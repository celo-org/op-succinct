use std::{str::FromStr, sync::Arc, time::Instant};

use alloy_consensus::TxEnvelope;
use alloy_eips::{BlockNumberOrTag, Decodable2718};
use alloy_network::{Ethereum, EthereumWallet, TransactionBuilder};
use alloy_primitives::{Address, Bytes, TxKind};
use alloy_provider::{Provider, ProviderBuilder, Web3Signer};
use alloy_rpc_types_eth::{TransactionReceipt, TransactionRequest};
use alloy_signer::Signer as AlloySigner;
use alloy_signer_gcp::{GcpKeyRingRef, GcpSigner, KeySpecifier};
use alloy_signer_local::PrivateKeySigner;
use alloy_transport_http::reqwest::Url;
use anyhow::{Context, Result};
use gcloud_sdk::{
    google::cloud::kms::v1::key_management_service_client::KeyManagementServiceClient, GoogleApi,
};
use tokio::{
    sync::{Mutex, RwLock},
    time::Duration,
};

pub const NUM_CONFIRMATIONS: u64 = 3;
pub const TIMEOUT_SECONDS: u64 = 60;

#[derive(Clone, Debug)]
/// The type of signer to use for signing transactions.
pub enum Signer {
    /// The signer URL and address.
    Web3Signer(Url, Address),
    /// The local signer.
    LocalSigner(PrivateKeySigner),
    /// Cloud HSM signer using Google.
    CloudHsmSigner(GcpSigner),
}

impl Signer {
    pub fn address(&self) -> Address {
        match self {
            Signer::Web3Signer(_, address) => *address,
            Signer::LocalSigner(signer) => signer.address(),
            Signer::CloudHsmSigner(signer) => signer.address(),
        }
    }

    /// Creates a new Web3 signer with the given URL and address.
    pub fn new_web3_signer(url: Url, address: Address) -> Self {
        Signer::Web3Signer(url, address)
    }

    /// Creates a new local signer from a private key string.
    pub fn new_local_signer(private_key_str: &str) -> Result<Self> {
        let private_key =
            PrivateKeySigner::from_str(private_key_str).context("Failed to parse private key")?;
        Ok(Signer::LocalSigner(private_key))
    }

    pub async fn from_env() -> Result<Self> {
        if let (Ok(project_id), Ok(location), Ok(keyring_name)) = (
            std::env::var("GOOGLE_PROJECT_ID"),
            std::env::var("GOOGLE_LOCATION"),
            std::env::var("GOOGLE_KEYRING"),
        ) {
            let key_name = std::env::var("HSM_KEY_NAME").expect("HSM_KEY_NAME");
            let key_version =
                std::env::var("HSM_KEY_VERSION").unwrap_or("1".to_string()).parse()?;

            let keyring = GcpKeyRingRef::new(&project_id, &location, &keyring_name);

            let key_specifier = KeySpecifier::new(keyring, &key_name, key_version);

            let client = GoogleApi::from_function(
                KeyManagementServiceClient::new,
                "https://cloudkms.googleapis.com",
                None,
            )
            .await?;
            let signer = GcpSigner::new(client, key_specifier, None).await?;

            Ok(Signer::CloudHsmSigner(signer))
        } else if let (Ok(signer_url_str), Ok(signer_address_str)) =
            (std::env::var("SIGNER_URL"), std::env::var("SIGNER_ADDRESS"))
        {
            let signer_url = Url::parse(&signer_url_str).context("Failed to parse SIGNER_URL")?;
            let signer_address =
                Address::from_str(&signer_address_str).context("Failed to parse SIGNER_ADDRESS")?;
            Ok(Signer::new_web3_signer(signer_url, signer_address))
        } else if let Ok(private_key_str) = std::env::var("PRIVATE_KEY") {
            Signer::new_local_signer(&private_key_str)
        } else {
            anyhow::bail!(
                "None of the required signer configurations are set in environment:\n\
                - For Cloud HSM: GOOGLE_PROJECT_ID, GOOGLE_LOCATION, GOOGLE_KEYRING\n\
                - For Web3Signer: SIGNER_URL and SIGNER_ADDRESS\n\
                - For Local: PRIVATE_KEY"
            )
        }
    }

    /// Sends a transaction request, signed by the configured `signer`.
    pub async fn send_transaction_request(
        &self,
        l1_rpc: Url,
        mut transaction_request: TransactionRequest,
    ) -> Result<TransactionReceipt> {
        match self {
            Signer::Web3Signer(signer_url, signer_address) => {
                // Set the from address to the signer address.
                transaction_request.set_from(*signer_address);

                // Fill the transaction request with all of the relevant gas and nonce information.
                let provider = ProviderBuilder::new().network::<Ethereum>().connect_http(l1_rpc);
                let filled_tx = provider.fill(transaction_request).await?;

                // Sign the transaction request using the Web3Signer.
                let web3_provider =
                    ProviderBuilder::new().network::<Ethereum>().connect_http(signer_url.clone());
                let signer = Web3Signer::new(web3_provider.clone(), *signer_address);

                let mut tx = filled_tx.as_builder().unwrap().clone();
                tx.normalize_data();

                let raw: Bytes =
                    signer.provider().client().request("eth_signTransaction", (tx,)).await?;

                let tx_envelope = TxEnvelope::decode_2718(&mut raw.as_ref()).unwrap();

                let receipt = provider
                    .send_tx_envelope(tx_envelope)
                    .await
                    .context("Failed to send transaction")?
                    .with_required_confirmations(NUM_CONFIRMATIONS)
                    .with_timeout(Some(Duration::from_secs(TIMEOUT_SECONDS)))
                    .get_receipt()
                    .await?;

                Ok(receipt)
            }
            Signer::LocalSigner(private_key) => {
                let provider = ProviderBuilder::new()
                    .network::<Ethereum>()
                    .wallet(EthereumWallet::new(private_key.clone()))
                    .connect_http(l1_rpc);

                // Ensure the request has a `from` address so the wallet filler can sign it.
                transaction_request.set_from(private_key.address());
                if transaction_request.to.is_none() {
                    // NOTE(fakedev9999): Anvil's wallet filler insists on a `to` field even for
                    // deployments. Mark the request as contract creation so it can be signed.
                    transaction_request.to = Some(TxKind::Create);
                }

                let receipt = provider
                    .send_transaction(transaction_request)
                    .await
                    .context("Failed to send transaction")?
                    .with_required_confirmations(NUM_CONFIRMATIONS)
                    .with_timeout(Some(Duration::from_secs(TIMEOUT_SECONDS)))
                    .get_receipt()
                    .await?;

                Ok(receipt)
            }
            Signer::CloudHsmSigner(signer) => {
                // Set the from address to HSM address
                transaction_request.set_from(signer.address());
                if transaction_request.to.is_none() {
                    // NOTE(fakedev9999): Anvil's wallet filler insists on a `to` field even for
                    // deployments. Mark the request as contract creation so it can be signed.
                    transaction_request.to = Some(TxKind::Create);
                }

                let wallet = EthereumWallet::new(signer.clone());
                let provider = ProviderBuilder::new()
                    .network::<Ethereum>()
                    .wallet(wallet)
                    .connect_http(l1_rpc);

                let receipt = provider
                    .send_transaction(transaction_request)
                    .await
                    .context("Failed to send KMS-signed transaction")?
                    .with_required_confirmations(NUM_CONFIRMATIONS)
                    .with_timeout(Some(Duration::from_secs(TIMEOUT_SECONDS)))
                    .get_receipt()
                    .await?;

                Ok(receipt)
            }
        }
    }
}

/// Configuration for adaptive gas pricing strategy.
///
/// This config controls how gas prices are calculated. There are two modes:
///
/// 1. **Override mode**: If `max_fee_override` or `max_priority_fee_override` are set, those values
///    are used directly, bypassing all adaptive logic.
///
/// 2. **Adaptive mode**: When gas prices exceed `gas_price_threshold`, uses historical average
///    baseFee (from `eth_feeHistory`) multiplied by a factor, with gradual increases after a
///    timeout.
#[derive(Clone, Debug)]
pub struct AdaptiveGasConfig {
    /// Optional hard override for max fee per gas (wei).
    /// When set, bypasses all adaptive logic and uses this value directly.
    /// Set via `MAX_FEE_PER_GAS` environment variable.
    pub max_fee_override: Option<u128>,
    /// Optional hard override for max priority fee per gas (wei).
    /// When set, bypasses all adaptive logic and uses this value directly.
    /// Set via `MAX_PRIORITY_FEE_PER_GAS` environment variable.
    pub max_priority_fee_override: Option<u128>,
    /// Threshold above which to apply historical smoothing (wei).
    /// Default: 3 gwei (3_000_000_000 wei)
    pub gas_price_threshold: u128,
    /// Number of blocks for history (~900 for 3 hours at 12s/block).
    /// Default: 900
    pub history_blocks: u64,
    /// Multiplier for historical average (e.g., 1.1 = 110%).
    /// Default: 1.1
    pub price_multiplier: f64,
    /// Timeout in minutes before gradual increase starts.
    /// Default: 30
    pub fallback_timeout_minutes: u64,
    /// Percentage increase per timeout period (e.g., 10.0 for 10%, 3.5 for 3.5%).
    /// Default: 10.0
    pub fallback_increase_percent: f64,
    /// Optional hard ceiling for max fee per gas (wei).
    /// Prevents runaway escalation regardless of timeout increases.
    /// Set via `MAX_GAS_PRICE_CAP` environment variable.
    pub max_gas_price_cap: Option<u128>,
    /// Optional threshold above which to skip submission entirely (wei).
    /// If current gas exceeds this, the transaction is not submitted.
    /// Set via `SKIP_IF_GAS_ABOVE` environment variable.
    pub skip_if_gas_above: Option<u128>,
    /// Percentile to use for historical base fee calculation (e.g., 50.0 for median).
    /// Default: 50.0 (median)
    pub gas_percentile: f64,
    /// Timeout in seconds before attempting RBF (Replace-By-Fee).
    /// Default: 60
    pub rbf_timeout_seconds: u64,
    /// Percentage increase for RBF attempts (must be >= 10.0 for most networks).
    /// Default: 12.0 (12% to ensure acceptance)
    pub rbf_price_bump_percent: f64,
    /// Maximum number of RBF retry attempts.
    /// Default: 5
    pub rbf_max_retries: u32,
}

impl Default for AdaptiveGasConfig {
    fn default() -> Self {
        Self {
            max_fee_override: None,
            max_priority_fee_override: None,
            gas_price_threshold: 3_000_000_000, // 3 gwei
            history_blocks: 3600,               // ~12 hours at 12s/block
            price_multiplier: 1.1,              // 110%
            fallback_timeout_minutes: 30,
            fallback_increase_percent: 10.0,
            max_gas_price_cap: Some(15_000_000_000), // 15 gwei
            skip_if_gas_above: Some(10_000_000_000), // 10 gwei
            gas_percentile: 50.0,                    // median
            rbf_timeout_seconds: 60,
            rbf_price_bump_percent: 12.0, // 12% to ensure RBF acceptance
            rbf_max_retries: 5,
        }
    }
}

impl AdaptiveGasConfig {
    /// Creates a new AdaptiveGasConfig from environment variables.
    ///
    /// All values are optional and fall back to defaults if not set.
    /// Returns an error if an environment variable is set but contains an invalid value.
    ///
    /// # Environment Variables
    ///
    /// **Override mode (bypass adaptive logic):**
    /// - `MAX_FEE_PER_GAS` - Fixed max fee per gas in wei
    /// - `MAX_PRIORITY_FEE_PER_GAS` - Fixed max priority fee per gas in wei
    ///
    /// **Adaptive mode:**
    /// - `GAS_PRICE_THRESHOLD` - Threshold in wei (default: 3 gwei)
    /// - `GAS_HISTORY_BLOCKS` - Blocks for averaging (default: 7200, ~24h)
    /// - `GAS_PRICE_MULTIPLIER` - Multiplier for avg (default: 1.1)
    /// - `GAS_FALLBACK_TIMEOUT_MINUTES` - Timeout before increase (default: 30)
    /// - `GAS_FALLBACK_INCREASE_PERCENT` - Increase % per period (default: 10.0)
    /// - `MAX_GAS_PRICE_CAP` - Hard ceiling for gas price in wei (optional)
    /// - `SKIP_IF_GAS_ABOVE` - Skip submission if gas exceeds this in wei (optional)
    /// - `GAS_PERCENTILE` - Percentile for base fee calculation (default: 50.0)
    ///
    /// **RBF (Replace-By-Fee) settings:**
    /// - `RBF_TIMEOUT_SECONDS` - Seconds before RBF attempt (default: 60)
    /// - `RBF_PRICE_BUMP_PERCENT` - Gas increase for RBF (default: 12.0)
    /// - `RBF_MAX_RETRIES` - Maximum RBF retry attempts (default: 5)
    pub fn from_env() -> Result<Self> {
        // Parse override values (these bypass adaptive logic when set)
        let mut config = Self {
            max_fee_override: Self::parse_env_var(
                "MAX_FEE_PER_GAS",
                "Value must be a valid integer in wei",
            )?,
            max_priority_fee_override: Self::parse_env_var(
                "MAX_PRIORITY_FEE_PER_GAS",
                "Value must be a valid integer in wei",
            )?,
            ..Default::default()
        };

        // Parse adaptive config values
        if let Some(threshold) =
            Self::parse_env_var("GAS_PRICE_THRESHOLD", "Value must be a valid integer in wei")?
        {
            config.gas_price_threshold = threshold;
        }
        if let Some(blocks) =
            Self::parse_env_var("GAS_HISTORY_BLOCKS", "Value must be a valid positive integer")?
        {
            config.history_blocks = blocks;
        }
        if let Some(multiplier) = Self::parse_env_var::<f64>(
            "GAS_PRICE_MULTIPLIER",
            "Value must be a valid decimal number (e.g., 1.1)",
        )? {
            if !multiplier.is_finite() || multiplier <= 0.0 {
                anyhow::bail!(
                    "GAS_PRICE_MULTIPLIER must be a finite positive number, got: {}",
                    multiplier
                );
            }
            config.price_multiplier = multiplier;
        }
        if let Some(timeout) = Self::parse_env_var(
            "GAS_FALLBACK_TIMEOUT_MINUTES",
            "Value must be a valid positive integer",
        )? {
            if timeout == 0 {
                anyhow::bail!(
                    "GAS_FALLBACK_TIMEOUT_MINUTES must be greater than 0, got: {}",
                    timeout
                );
            }
            config.fallback_timeout_minutes = timeout;
        }
        if let Some(percent) = Self::parse_env_var::<f64>(
            "GAS_FALLBACK_INCREASE_PERCENT",
            "Value must be a valid decimal number (e.g., 10.0, 3.5)",
        )? {
            if !percent.is_finite() || percent < 0.0 {
                anyhow::bail!(
                    "GAS_FALLBACK_INCREASE_PERCENT must be a finite non-negative number, got: {}",
                    percent
                );
            }
            config.fallback_increase_percent = percent;
        }

        // Parse new gas strategy config values (only override if env var is set)
        if let Some(cap) =
            Self::parse_env_var("MAX_GAS_PRICE_CAP", "Value must be a valid integer in wei")?
        {
            config.max_gas_price_cap = Some(cap);
        }
        if let Some(skip) =
            Self::parse_env_var("SKIP_IF_GAS_ABOVE", "Value must be a valid integer in wei")?
        {
            config.skip_if_gas_above = Some(skip);
        }

        if let Some(percentile) = Self::parse_env_var::<f64>(
            "GAS_PERCENTILE",
            "Value must be a valid decimal number (e.g., 50.0, 25.0)",
        )? {
            if !percentile.is_finite() || !(0.0..=100.0).contains(&percentile) {
                anyhow::bail!("GAS_PERCENTILE must be between 0 and 100, got: {}", percentile);
            }
            config.gas_percentile = percentile;
        }

        // Parse RBF config values
        if let Some(timeout) =
            Self::parse_env_var("RBF_TIMEOUT_SECONDS", "Value must be a valid positive integer")?
        {
            config.rbf_timeout_seconds = timeout;
        }
        if let Some(bump) = Self::parse_env_var::<f64>(
            "RBF_PRICE_BUMP_PERCENT",
            "Value must be a valid decimal number (e.g., 12.0)",
        )? {
            if !bump.is_finite() || bump < 0.0 {
                anyhow::bail!(
                    "RBF_PRICE_BUMP_PERCENT must be a finite non-negative number, got: {}",
                    bump
                );
            }
            config.rbf_price_bump_percent = bump;
        }
        if let Some(retries) =
            Self::parse_env_var("RBF_MAX_RETRIES", "Value must be a valid positive integer")?
        {
            config.rbf_max_retries = retries;
        }

        Ok(config)
    }

    /// Returns true if any override values are set.
    pub fn has_overrides(&self) -> bool {
        self.max_fee_override.is_some() || self.max_priority_fee_override.is_some()
    }

    fn parse_env_var<T>(var_name: &str, type_hint: &str) -> Result<Option<T>>
    where
        T: FromStr,
        T::Err: std::fmt::Display,
    {
        match std::env::var(var_name) {
            Ok(value) => {
                let trimmed = value.trim();
                if trimmed.is_empty() {
                    return Ok(None);
                }
                trimmed.parse::<T>().map(Some).map_err(|e| {
                    anyhow::anyhow!(
                        "Failed to parse {} environment variable: '{}'. {}: {}",
                        var_name,
                        value,
                        type_hint,
                        e
                    )
                })
            }
            Err(std::env::VarError::NotPresent) => Ok(None),
            Err(std::env::VarError::NotUnicode(os_str)) => {
                anyhow::bail!(
                    "{} environment variable contains invalid unicode: {:?}",
                    var_name,
                    os_str
                )
            }
        }
    }
}

/// Calculated gas prices from the adaptive gas oracle.
#[derive(Clone, Debug)]
pub struct GasPriceEstimate {
    /// The calculated max fee per gas in wei.
    pub max_fee_per_gas: u128,
    /// The calculated max priority fee per gas in wei.
    pub max_priority_fee_per_gas: u128,
}

/// An adaptive gas pricing utility that calculates optimal gas prices.
///
/// When gas prices are below the threshold, uses normal provider estimation.
/// When gas prices exceed the threshold, uses historical average baseFee
/// (fetched via `eth_feeHistory`) multiplied by a configurable factor.
///
/// If transactions remain pending for too long (timeout), the oracle
/// gradually increases gas prices to eventually get transactions through.
#[derive(Clone, Debug)]
pub struct AdaptiveGasOracle {
    config: AdaptiveGasConfig,
    /// When high-gas mode started (for timeout tracking).
    /// None means gas is currently below threshold.
    high_gas_start: Arc<RwLock<Option<Instant>>>,
}

impl AdaptiveGasOracle {
    /// Creates a new AdaptiveGasOracle with the given configuration.
    pub fn new(config: AdaptiveGasConfig) -> Self {
        Self { config, high_gas_start: Arc::new(RwLock::new(None)) }
    }

    /// Creates a new AdaptiveGasOracle with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(AdaptiveGasConfig::default())
    }

    /// Creates a new AdaptiveGasOracle from environment variables.
    pub fn from_env() -> Result<Self> {
        Ok(Self::new(AdaptiveGasConfig::from_env()?))
    }

    /// Returns the configuration.
    pub fn config(&self) -> &AdaptiveGasConfig {
        &self.config
    }

    /// Resets the high-gas timer (called when gas drops below threshold).
    async fn reset_high_gas_timer(&self) {
        let mut guard = self.high_gas_start.write().await;
        *guard = None;
    }

    /// Starts the high-gas timer if not already started.
    async fn start_high_gas_timer(&self) {
        let mut guard = self.high_gas_start.write().await;
        if guard.is_none() {
            *guard = Some(Instant::now());
        }
    }

    /// Calculates the timeout increase factor.
    ///
    /// Returns `Some(factor)` if we've been waiting longer than the timeout,
    /// where factor increases by `fallback_increase_percent` for each timeout period.
    /// Returns `None` if we haven't hit the timeout yet.
    pub async fn get_timeout_increase_factor(&self) -> Option<f64> {
        let guard = self.high_gas_start.read().await;
        let start = (*guard)?;
        let elapsed = start.elapsed();
        let timeout_duration = Duration::from_secs(self.config.fallback_timeout_minutes * 60);

        if elapsed < timeout_duration {
            return None;
        }

        // Calculate how many timeout periods have passed
        let periods = elapsed.as_secs() / timeout_duration.as_secs();
        let increase_per_period = self.config.fallback_increase_percent / 100.0;

        // Compound increase: (1 + increase)^periods
        Some((1.0 + increase_per_period).powi(periods as i32))
    }

    /// Gets the recommended gas prices for a transaction.
    ///
    /// This method implements the following logic:
    /// 1. If override values are set, use them directly (no RPC calls needed for overridden values)
    /// 2. Otherwise, if current gas is below threshold, use provider estimation
    /// 3. If current gas is above threshold, use historical average with multiplier
    pub async fn get_gas_prices<P>(&self, provider: &P) -> Result<GasPriceEstimate>
    where
        P: Provider<Ethereum>,
    {
        // Check for hard overrides first - these bypass all adaptive logic
        if let (Some(max_fee), Some(priority_fee)) =
            (self.config.max_fee_override, self.config.max_priority_fee_override)
        {
            // Both overrides set - use them directly, no RPC needed
            return Ok(GasPriceEstimate {
                max_fee_per_gas: max_fee,
                max_priority_fee_per_gas: priority_fee,
            });
        }

        // Get current gas price and provider estimates for any non-overridden values
        let current_gas =
            provider.get_gas_price().await.context("Failed to get current gas price")?;

        // If only one override is set, we need to get the other value
        if let Some(max_fee) = self.config.max_fee_override {
            // max_fee is overridden, get priority fee from provider
            let fees = provider
                .estimate_eip1559_fees()
                .await
                .context("Failed to estimate EIP-1559 fees")?;
            return Ok(GasPriceEstimate {
                max_fee_per_gas: max_fee,
                max_priority_fee_per_gas: fees.max_priority_fee_per_gas,
            });
        }

        if let Some(priority_fee) = self.config.max_priority_fee_override {
            // priority_fee is overridden, get max_fee from provider or adaptive logic
            let fees = provider
                .estimate_eip1559_fees()
                .await
                .context("Failed to estimate EIP-1559 fees")?;
            return Ok(GasPriceEstimate {
                max_fee_per_gas: fees.max_fee_per_gas,
                max_priority_fee_per_gas: priority_fee,
            });
        }

        // No overrides - use adaptive logic
        if current_gas <= self.config.gas_price_threshold {
            // Below threshold - use current estimation
            // Note: We do NOT reset the timer here. The timer should only be reset
            // when a transaction is successfully confirmed via notify_transaction_confirmed().
            // This prevents the timer from being reset when gas briefly drops during spikes.

            // Use EIP-1559 fee estimation
            let fees = provider
                .estimate_eip1559_fees()
                .await
                .context("Failed to estimate EIP-1559 fees")?;
            return Ok(GasPriceEstimate {
                max_fee_per_gas: fees.max_fee_per_gas,
                max_priority_fee_per_gas: fees.max_priority_fee_per_gas,
            });
        }

        // Above threshold - use historical average
        self.start_high_gas_timer().await;

        // Single RPC call to get all historical data
        // Request the configured percentile for priority fees
        let fee_history = provider
            .get_fee_history(
                self.config.history_blocks,
                BlockNumberOrTag::Latest,
                &[self.config.gas_percentile],
            )
            .await
            .context("Failed to get fee history")?;

        // Calculate percentile-based baseFee (more robust than average)
        let base_fees: Vec<u128> = fee_history.base_fee_per_gas.to_vec();
        let percentile_base_fee = if base_fees.is_empty() {
            current_gas // fallback to current if no history
        } else {
            Self::calculate_percentile(&base_fees, self.config.gas_percentile)
        };

        // Calculate percentile priority fee from rewards
        let priority_fees: Vec<u128> =
            fee_history.reward.iter().flatten().flatten().copied().collect();
        let percentile_priority_fee = if priority_fees.is_empty() {
            1_000_000_000u128 // 1 gwei default if no data
        } else {
            Self::calculate_percentile(&priority_fees, self.config.gas_percentile)
        };

        // Apply multiplier to base fee and priority fee
        let scaled_base_fee = (percentile_base_fee as f64 * self.config.price_multiplier) as u128;
        let mut priority_fee =
            (percentile_priority_fee as f64 * self.config.price_multiplier) as u128;

        // For EIP-1559 validity, max_fee_per_gas must be at least base_fee + priority_fee.
        // We include the priority fee in max_fee to ensure the transaction is includable.
        let mut max_fee = scaled_base_fee + priority_fee;

        // Apply timeout increase if waiting too long
        if let Some(factor) = self.get_timeout_increase_factor().await {
            max_fee = (max_fee as f64 * factor) as u128;
            priority_fee = (priority_fee as f64 * factor) as u128;
        }

        // Apply max gas price cap to prevent runaway escalation
        if let Some(cap) = self.config.max_gas_price_cap {
            max_fee = max_fee.min(cap);
            // Also cap priority fee to ensure it doesn't exceed max_fee
            priority_fee = priority_fee.min(max_fee);
        }

        Ok(GasPriceEstimate { max_fee_per_gas: max_fee, max_priority_fee_per_gas: priority_fee })
    }

    /// Calculates the percentile value from a slice of values.
    ///
    /// Uses linear interpolation for non-integer indices.
    fn calculate_percentile(values: &[u128], percentile: f64) -> u128 {
        if values.is_empty() {
            return 0;
        }
        if values.len() == 1 {
            return values[0];
        }

        let mut sorted = values.to_vec();
        sorted.sort_unstable();

        // Calculate the index for the percentile
        let idx = (percentile / 100.0) * (sorted.len() - 1) as f64;
        let lower = idx.floor() as usize;
        let upper = idx.ceil() as usize;

        if lower == upper {
            sorted[lower]
        } else {
            // Linear interpolation
            let fraction = idx - lower as f64;
            let lower_val = sorted[lower] as f64;
            let upper_val = sorted[upper] as f64;
            (lower_val + fraction * (upper_val - lower_val)) as u128
        }
    }

    /// Applies the gas estimate to a transaction request.
    pub fn apply_to_transaction(
        &self,
        mut tx: TransactionRequest,
        estimate: &GasPriceEstimate,
    ) -> TransactionRequest {
        tx = tx.max_fee_per_gas(estimate.max_fee_per_gas);
        tx = tx.max_priority_fee_per_gas(estimate.max_priority_fee_per_gas);
        tx
    }

    /// Checks if submission should be skipped due to high gas prices.
    ///
    /// Returns `true` if `skip_if_gas_above` is configured and current gas exceeds it.
    /// This allows callers to delay transactions when gas is extremely high.
    pub async fn should_skip_submission<P>(&self, provider: &P) -> Result<bool>
    where
        P: Provider<Ethereum>,
    {
        let Some(threshold) = self.config.skip_if_gas_above else {
            return Ok(false);
        };

        let current_gas =
            provider.get_gas_price().await.context("Failed to get current gas price")?;

        Ok(current_gas > threshold)
    }

    /// Notifies the oracle that a transaction was successfully confirmed.
    ///
    /// This resets the high-gas timer, as the transaction is no longer pending.
    pub async fn notify_transaction_confirmed(&self) {
        self.reset_high_gas_timer().await;
    }
}

/// Gas pricing strategy for transactions.
#[derive(Clone, Debug)]
pub enum GasPricingStrategy {
    /// No gas pricing override - use provider defaults.
    Default,
    /// Adaptive gas pricing based on network conditions.
    /// Also handles static overrides via `max_fee_override` and `max_priority_fee_override`.
    Adaptive(Box<AdaptiveGasOracle>),
}

impl GasPricingStrategy {
    /// Creates a gas pricing strategy from environment variables.
    ///
    /// Uses Adaptive strategy if any gas configuration is set:
    /// - `MAX_FEE_PER_GAS` / `MAX_PRIORITY_FEE_PER_GAS` (static overrides)
    /// - `GAS_PRICE_THRESHOLD`, `GAS_HISTORY_BLOCKS`, etc. (adaptive config)
    ///
    /// Otherwise uses Default (provider estimation).
    pub fn from_env() -> Result<Self> {
        // Check if any gas config vars are set
        let has_gas_config = std::env::var("MAX_FEE_PER_GAS").is_ok() ||
            std::env::var("MAX_PRIORITY_FEE_PER_GAS").is_ok() ||
            std::env::var("GAS_PRICE_THRESHOLD").is_ok() ||
            std::env::var("GAS_HISTORY_BLOCKS").is_ok() ||
            std::env::var("GAS_PRICE_MULTIPLIER").is_ok() ||
            std::env::var("GAS_FALLBACK_TIMEOUT_MINUTES").is_ok() ||
            std::env::var("GAS_FALLBACK_INCREASE_PERCENT").is_ok();

        if has_gas_config {
            return Ok(Self::Adaptive(Box::new(AdaptiveGasOracle::from_env()?)));
        }

        Ok(Self::Default)
    }
}

/// Wrapper around Signer that provides thread-safe transaction sending.
/// Transactions are serialized via a Mutex to prevent nonce conflicts.
#[derive(Clone, Debug)]
pub struct SignerLock {
    inner: Arc<Mutex<Signer>>,
    cached_address: Address,
    gas_strategy: GasPricingStrategy,
}

impl SignerLock {
    /// Creates a new SignerLock wrapping the given Signer with default gas pricing.
    pub fn new(signer: Signer) -> Self {
        let cached_address = signer.address();
        SignerLock {
            inner: Arc::new(Mutex::new(signer)),
            cached_address,
            gas_strategy: GasPricingStrategy::Default,
        }
    }

    /// Creates a new SignerLock with adaptive gas pricing.
    pub fn new_with_adaptive_gas(signer: Signer, oracle: AdaptiveGasOracle) -> Self {
        let cached_address = signer.address();
        SignerLock {
            inner: Arc::new(Mutex::new(signer)),
            cached_address,
            gas_strategy: GasPricingStrategy::Adaptive(Box::new(oracle)),
        }
    }

    /// Creates a new SignerLock with a custom gas pricing strategy.
    pub fn new_with_strategy(signer: Signer, gas_strategy: GasPricingStrategy) -> Self {
        let cached_address = signer.address();
        SignerLock { inner: Arc::new(Mutex::new(signer)), cached_address, gas_strategy }
    }

    /// Creates a SignerLock from environment variables.
    ///
    /// Detects the gas pricing strategy from environment:
    /// - Adaptive if `GAS_PRICE_THRESHOLD` etc. are set
    /// - Static if `MAX_FEE_PER_GAS` etc. are set
    /// - Default otherwise
    pub async fn from_env() -> Result<Self> {
        let signer = Signer::from_env().await?;
        let gas_strategy = GasPricingStrategy::from_env()?;
        Ok(SignerLock::new_with_strategy(signer, gas_strategy))
    }

    /// Returns the address of the signer without acquiring a lock.
    pub fn address(&self) -> Address {
        self.cached_address
    }

    /// Returns the gas pricing strategy.
    pub fn gas_strategy(&self) -> &GasPricingStrategy {
        &self.gas_strategy
    }

    /// Returns the adaptive gas oracle if using adaptive pricing.
    /// Returns None if using default pricing.
    pub fn gas_oracle(&self) -> Option<&AdaptiveGasOracle> {
        match &self.gas_strategy {
            GasPricingStrategy::Adaptive(oracle) => Some(oracle.as_ref()),
            GasPricingStrategy::Default => None,
        }
    }

    /// Returns the adaptive gas config if using adaptive pricing.
    /// Returns None if using default pricing.
    pub fn gas_config(&self) -> Option<&AdaptiveGasConfig> {
        self.gas_oracle().map(|o| o.config())
    }

    /// Sends a transaction request, signed by the configured signer.
    /// Transactions are serialized via a Mutex to prevent nonce conflicts.
    /// Applies gas pricing strategy based on configuration.
    ///
    /// If using adaptive pricing with RBF enabled, will attempt to replace
    /// stuck transactions with higher gas prices.
    pub async fn send_transaction_request(
        &self,
        l1_rpc: Url,
        mut transaction_request: TransactionRequest,
    ) -> Result<TransactionReceipt> {
        // Apply gas pricing strategy
        let rbf_config = match &self.gas_strategy {
            GasPricingStrategy::Default => {
                // No modification - let provider estimate, no RBF
                let signer = self.inner.lock().await;
                return signer.send_transaction_request(l1_rpc, transaction_request).await;
            }
            GasPricingStrategy::Adaptive(oracle) => {
                // Create a provider to query gas prices
                let provider =
                    ProviderBuilder::new().network::<Ethereum>().connect_http(l1_rpc.clone());

                // Get gas prices (handles both static overrides and adaptive pricing)
                let estimate = oracle.get_gas_prices(&provider).await?;
                transaction_request = oracle.apply_to_transaction(transaction_request, &estimate);

                // Extract RBF config
                (
                    oracle.config().rbf_timeout_seconds,
                    oracle.config().rbf_price_bump_percent,
                    oracle.config().rbf_max_retries,
                    estimate.max_fee_per_gas,
                    estimate.max_priority_fee_per_gas,
                )
            }
        };

        let (
            rbf_timeout_secs,
            rbf_bump_percent,
            max_retries,
            mut current_max_fee,
            mut current_priority_fee,
        ) = rbf_config;

        // Acquire lock BEFORE fetching nonce to prevent race conditions.
        // This ensures concurrent callers serialize properly and each gets a unique nonce.
        let signer = self.inner.lock().await;

        // Get the nonce explicitly so we can reuse it for RBF
        let provider = ProviderBuilder::new().network::<Ethereum>().connect_http(l1_rpc.clone());
        let nonce = provider
            .get_transaction_count(self.cached_address)
            .await
            .context("Failed to get nonce")?;
        transaction_request = transaction_request.nonce(nonce);
        let mut attempts = 0u32;

        loop {
            attempts += 1;

            // Clone the request for this attempt with current gas prices
            let mut attempt_request = transaction_request.clone();
            attempt_request = attempt_request.max_fee_per_gas(current_max_fee);
            attempt_request = attempt_request.max_priority_fee_per_gas(current_priority_fee);

            tracing::debug!(
                attempt = attempts,
                max_fee = current_max_fee,
                priority_fee = current_priority_fee,
                nonce = nonce,
                "Sending transaction"
            );

            // Try to send with RBF timeout
            let send_result = tokio::time::timeout(
                Duration::from_secs(rbf_timeout_secs),
                signer.send_transaction_request(l1_rpc.clone(), attempt_request),
            )
            .await;

            match send_result {
                Ok(Ok(receipt)) => {
                    // Transaction confirmed! Notify oracle and return
                    if let GasPricingStrategy::Adaptive(oracle) = &self.gas_strategy {
                        oracle.notify_transaction_confirmed().await;
                    }
                    return Ok(receipt);
                }
                Ok(Err(e)) => {
                    // Transaction failed (not timeout) - might be replaceable
                    let err_str = e.to_string().to_lowercase();
                    if err_str.contains("replacement") || err_str.contains("underpriced") {
                        // This is likely a "replacement transaction underpriced" error
                        // Bump gas and retry
                        tracing::warn!(
                            attempt = attempts,
                            error = %e,
                            "Transaction underpriced, bumping gas for RBF"
                        );
                    } else if attempts >= max_retries {
                        return Err(e);
                    } else {
                        // Other error, but we can still try RBF
                        tracing::warn!(
                            attempt = attempts,
                            error = %e,
                            "Transaction failed, attempting RBF"
                        );
                    }
                }
                Err(_) => {
                    // Timeout - transaction might be pending, try RBF
                    tracing::warn!(
                        attempt = attempts,
                        timeout_secs = rbf_timeout_secs,
                        "Transaction timed out, attempting RBF"
                    );
                }
            }

            if attempts >= max_retries {
                anyhow::bail!(
                    "Transaction failed after {} RBF attempts. Last gas: max_fee={}, priority_fee={}",
                    attempts,
                    current_max_fee,
                    current_priority_fee
                );
            }

            // Bump gas prices for RBF (must be at least 10% higher, we use configured bump)
            let bump_factor = 1.0 + (rbf_bump_percent / 100.0);
            current_max_fee = (current_max_fee as f64 * bump_factor) as u128;
            current_priority_fee = (current_priority_fee as f64 * bump_factor) as u128;

            tracing::info!(
                attempt = attempts + 1,
                new_max_fee = current_max_fee,
                new_priority_fee = current_priority_fee,
                "Bumping gas for RBF retry"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_eips::BlockId;
    use alloy_primitives::{address, U256};
    use op_succinct_host_utils::OPSuccinctL2OutputOracle::OPSuccinctL2OutputOracleInstance as OPSuccinctL2OOContract;

    use super::*;

    #[tokio::test]
    #[ignore]
    async fn test_send_transaction_request_web3() {
        let proposer_signer = SignerLock::new(Signer::new_web3_signer(
            "http://localhost:9000".parse().unwrap(),
            "0x9b3F173823E944d183D532ed236Ee3B83Ef15E1d".parse().unwrap(),
        ));

        let provider = ProviderBuilder::new()
            .network::<Ethereum>()
            .connect_http("http://localhost:8545".parse().unwrap());

        let l2oo_contract = OPSuccinctL2OOContract::new(
            address!("0xDafA1019F21AB8B27b319B1085f93673F02A69B7"),
            provider.clone(),
        );

        let latest_header = provider.get_block(BlockId::latest()).await.unwrap().unwrap();

        let transaction_request = l2oo_contract
            .checkpointBlockHash(U256::from(latest_header.header.number))
            .into_transaction_request();

        let receipt = proposer_signer
            .send_transaction_request("http://localhost:8545".parse().unwrap(), transaction_request)
            .await
            .unwrap();

        println!("Signed transaction receipt: {receipt:?}");
    }

    #[tokio::test]
    #[ignore]
    // This test is meant to be ran locally to test various signers implementations,
    // depending of the envvars set.
    async fn test_send_transaction_request() {
        dotenv::dotenv().ok();

        rustls::crypto::aws_lc_rs::default_provider()
            .install_default()
            .expect("Failed to install default crypto provider");
        let signer = SignerLock::from_env().await.unwrap();

        println!("Signer: {}", signer.address());

        let transaction_request = TransactionRequest::default()
            .to(Address::from([
                1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20,
            ]))
            .value(U256::from(100000u64))
            .from(signer.address());
        let receipt = signer
            .send_transaction_request("http://localhost:8545".parse().unwrap(), transaction_request)
            .await
            .unwrap();
        println!("Signed transaction receipt: {receipt:?}");
    }

    #[test]
    fn test_signer_lock_new_has_default_strategy() {
        let signer = Signer::new_local_signer(
            "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
        )
        .unwrap();
        let signer_lock = SignerLock::new(signer);

        // Default strategy means no gas_config or oracle
        assert!(signer_lock.gas_config().is_none());
        assert!(signer_lock.gas_oracle().is_none());
        assert!(matches!(signer_lock.gas_strategy(), GasPricingStrategy::Default));
    }

    #[test]
    fn test_signer_lock_new_with_adaptive_gas() {
        let signer = Signer::new_local_signer(
            "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
        )
        .unwrap();
        let oracle = AdaptiveGasOracle::with_defaults();
        let signer_lock = SignerLock::new_with_adaptive_gas(signer, oracle);

        assert!(signer_lock.gas_oracle().is_some());
        assert!(signer_lock.gas_config().is_some()); // gas_config() now returns the adaptive config
        assert!(matches!(signer_lock.gas_strategy(), GasPricingStrategy::Adaptive(_)));
    }

    #[test]
    fn test_signer_lock_with_override_config() {
        let signer = Signer::new_local_signer(
            "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
        )
        .unwrap();
        let config = AdaptiveGasConfig {
            max_fee_override: Some(100_000_000_000u128),
            max_priority_fee_override: Some(2_000_000_000u128),
            ..Default::default()
        };
        let oracle = AdaptiveGasOracle::new(config);
        let signer_lock = SignerLock::new_with_adaptive_gas(signer, oracle);

        let config = signer_lock.gas_config().expect("Should have config");
        assert_eq!(config.max_fee_override, Some(100_000_000_000u128));
        assert_eq!(config.max_priority_fee_override, Some(2_000_000_000u128));
        assert!(config.has_overrides());
    }

    #[test]
    fn test_signer_lock_address_cached() {
        let signer = Signer::new_local_signer(
            "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
        )
        .unwrap();
        let expected_address = signer.address();
        let signer_lock = SignerLock::new(signer);

        // Address should be cached and accessible without lock
        assert_eq!(signer_lock.address(), expected_address);
    }

    #[test]
    fn test_adaptive_gas_config_default() {
        let config = AdaptiveGasConfig::default();
        assert_eq!(config.max_fee_override, None);
        assert_eq!(config.max_priority_fee_override, None);
        assert_eq!(config.gas_price_threshold, 3_000_000_000); // 3 gwei
        assert_eq!(config.history_blocks, 3600); // 12 hours
        assert!((config.price_multiplier - 1.1).abs() < f64::EPSILON);
        assert_eq!(config.fallback_timeout_minutes, 30);
        assert!((config.fallback_increase_percent - 10.0).abs() < f64::EPSILON);
        assert!(!config.has_overrides());
        // New fields
        assert_eq!(config.max_gas_price_cap, Some(15_000_000_000)); // 15 gwei
        assert_eq!(config.skip_if_gas_above, Some(10_000_000_000)); // 10 gwei
        assert!((config.gas_percentile - 50.0).abs() < f64::EPSILON);
        assert_eq!(config.rbf_timeout_seconds, 60);
        assert!((config.rbf_price_bump_percent - 12.0).abs() < f64::EPSILON);
        assert_eq!(config.rbf_max_retries, 5);
    }

    #[test]
    fn test_adaptive_gas_config_with_overrides() {
        let config = AdaptiveGasConfig {
            max_fee_override: Some(100_000_000_000u128),
            max_priority_fee_override: Some(2_000_000_000u128),
            ..Default::default()
        };
        assert!(config.has_overrides());
        assert_eq!(config.max_fee_override, Some(100_000_000_000u128));
        assert_eq!(config.max_priority_fee_override, Some(2_000_000_000u128));
    }

    #[test]
    fn test_adaptive_gas_oracle_creation() {
        let config = AdaptiveGasConfig {
            max_fee_override: None,
            max_priority_fee_override: None,
            gas_price_threshold: 5_000_000_000,
            history_blocks: 1000,
            price_multiplier: 1.2,
            fallback_timeout_minutes: 45,
            fallback_increase_percent: 15.0,
            max_gas_price_cap: Some(500_000_000_000), // 500 gwei cap
            skip_if_gas_above: Some(200_000_000_000), // skip if > 200 gwei
            gas_percentile: 25.0,                     // 25th percentile
            rbf_timeout_seconds: 120,
            rbf_price_bump_percent: 15.0,
            rbf_max_retries: 3,
        };
        let oracle = AdaptiveGasOracle::new(config.clone());

        assert_eq!(oracle.config().gas_price_threshold, 5_000_000_000);
        assert_eq!(oracle.config().history_blocks, 1000);
        assert!((oracle.config().price_multiplier - 1.2).abs() < f64::EPSILON);
        assert_eq!(oracle.config().max_gas_price_cap, Some(500_000_000_000));
        assert_eq!(oracle.config().skip_if_gas_above, Some(200_000_000_000));
        assert!((oracle.config().gas_percentile - 25.0).abs() < f64::EPSILON);
        assert_eq!(oracle.config().rbf_timeout_seconds, 120);
        assert!((oracle.config().rbf_price_bump_percent - 15.0).abs() < f64::EPSILON);
        assert_eq!(oracle.config().rbf_max_retries, 3);
    }

    #[test]
    fn test_gas_price_estimate_struct() {
        let estimate = GasPriceEstimate {
            max_fee_per_gas: 100_000_000_000u128,
            max_priority_fee_per_gas: 2_000_000_000u128,
        };
        assert_eq!(estimate.max_fee_per_gas, 100_000_000_000u128);
        assert_eq!(estimate.max_priority_fee_per_gas, 2_000_000_000u128);
    }

    #[test]
    fn test_apply_to_transaction() {
        let oracle = AdaptiveGasOracle::with_defaults();
        let estimate = GasPriceEstimate {
            max_fee_per_gas: 50_000_000_000u128,
            max_priority_fee_per_gas: 1_000_000_000u128,
        };
        let tx = TransactionRequest::default();
        let tx = oracle.apply_to_transaction(tx, &estimate);

        assert_eq!(tx.max_fee_per_gas, Some(50_000_000_000u128));
        assert_eq!(tx.max_priority_fee_per_gas, Some(1_000_000_000u128));
    }

    #[tokio::test]
    async fn test_timeout_increase_factor_before_timeout() {
        let config = AdaptiveGasConfig {
            fallback_timeout_minutes: 30,
            fallback_increase_percent: 10.0,
            ..Default::default()
        };
        let oracle = AdaptiveGasOracle::new(config);

        // Timer not started - no increase
        assert!(oracle.get_timeout_increase_factor().await.is_none());
    }

    #[test]
    fn test_calculate_percentile() {
        // Test median (50th percentile)
        let values = vec![10, 20, 30, 40, 50];
        assert_eq!(AdaptiveGasOracle::calculate_percentile(&values, 50.0), 30);

        // Test 25th percentile
        assert_eq!(AdaptiveGasOracle::calculate_percentile(&values, 25.0), 20);

        // Test 75th percentile
        assert_eq!(AdaptiveGasOracle::calculate_percentile(&values, 75.0), 40);

        // Test 0th percentile (minimum)
        assert_eq!(AdaptiveGasOracle::calculate_percentile(&values, 0.0), 10);

        // Test 100th percentile (maximum)
        assert_eq!(AdaptiveGasOracle::calculate_percentile(&values, 100.0), 50);

        // Test with single value
        assert_eq!(AdaptiveGasOracle::calculate_percentile(&[100], 50.0), 100);

        // Test empty slice
        assert_eq!(AdaptiveGasOracle::calculate_percentile(&[], 50.0), 0);
    }

    #[test]
    fn test_config_with_gas_cap_and_skip() {
        let config = AdaptiveGasConfig {
            max_gas_price_cap: Some(100_000_000_000), // 100 gwei cap
            skip_if_gas_above: Some(50_000_000_000),  // skip if > 50 gwei
            ..Default::default()
        };

        assert_eq!(config.max_gas_price_cap, Some(100_000_000_000));
        assert_eq!(config.skip_if_gas_above, Some(50_000_000_000));
    }

    #[test]
    fn test_config_rbf_settings() {
        let config = AdaptiveGasConfig {
            rbf_timeout_seconds: 120,
            rbf_price_bump_percent: 15.0,
            rbf_max_retries: 10,
            ..Default::default()
        };

        assert_eq!(config.rbf_timeout_seconds, 120);
        assert!((config.rbf_price_bump_percent - 15.0).abs() < f64::EPSILON);
        assert_eq!(config.rbf_max_retries, 10);
    }

    // Note: Tests that modify environment variables should be run with --test-threads=1
    // to avoid race conditions. Example:
    // cargo test -p op-succinct-signer-utils -- --test-threads=1

    mod override_env_tests {
        use super::*;
        use std::sync::Mutex;

        // Use a mutex to serialize env var tests
        static ENV_MUTEX: Mutex<()> = Mutex::new(());

        fn with_env_vars<F, T>(vars: &[(&str, Option<&str>)], f: F) -> T
        where
            F: FnOnce() -> T,
        {
            let _lock = ENV_MUTEX.lock().unwrap();

            // Save and set env vars
            // SAFETY: We hold a mutex to ensure single-threaded access to env vars in tests
            let saved: Vec<_> = vars
                .iter()
                .map(|(key, value)| {
                    let saved = std::env::var(key).ok();
                    match value {
                        Some(v) => unsafe { std::env::set_var(key, v) },
                        None => unsafe { std::env::remove_var(key) },
                    }
                    (*key, saved)
                })
                .collect();

            let result = f();

            // Restore env vars
            // SAFETY: We hold a mutex to ensure single-threaded access to env vars in tests
            for (key, saved) in saved {
                match saved {
                    Some(v) => unsafe { std::env::set_var(key, v) },
                    None => unsafe { std::env::remove_var(key) },
                }
            }

            result
        }

        #[test]
        fn test_from_env_with_override_values() {
            with_env_vars(
                &[
                    ("MAX_FEE_PER_GAS", Some("100000000000")),
                    ("MAX_PRIORITY_FEE_PER_GAS", Some("2000000000")),
                ],
                || {
                    let config = AdaptiveGasConfig::from_env().unwrap();
                    assert_eq!(config.max_fee_override, Some(100_000_000_000u128));
                    assert_eq!(config.max_priority_fee_override, Some(2_000_000_000u128));
                    assert!(config.has_overrides());
                },
            );
        }

        #[test]
        fn test_from_env_with_no_override_vars_set() {
            with_env_vars(&[("MAX_FEE_PER_GAS", None), ("MAX_PRIORITY_FEE_PER_GAS", None)], || {
                let config = AdaptiveGasConfig::from_env().unwrap();
                assert_eq!(config.max_fee_override, None);
                assert_eq!(config.max_priority_fee_override, None);
                assert!(!config.has_overrides());
            });
        }

        #[test]
        fn test_from_env_with_partial_override_values() {
            with_env_vars(
                &[("MAX_FEE_PER_GAS", Some("50000000000")), ("MAX_PRIORITY_FEE_PER_GAS", None)],
                || {
                    let config = AdaptiveGasConfig::from_env().unwrap();
                    assert_eq!(config.max_fee_override, Some(50_000_000_000u128));
                    assert_eq!(config.max_priority_fee_override, None);
                    assert!(config.has_overrides());
                },
            );
        }

        #[test]
        fn test_from_env_trims_whitespace() {
            with_env_vars(
                &[
                    ("MAX_FEE_PER_GAS", Some("  100000000000  ")),
                    ("MAX_PRIORITY_FEE_PER_GAS", Some("\t2000000000\n")),
                ],
                || {
                    let config = AdaptiveGasConfig::from_env().unwrap();
                    assert_eq!(config.max_fee_override, Some(100_000_000_000u128));
                    assert_eq!(config.max_priority_fee_override, Some(2_000_000_000u128));
                },
            );
        }

        #[test]
        fn test_from_env_empty_string_is_none() {
            with_env_vars(
                &[("MAX_FEE_PER_GAS", Some("")), ("MAX_PRIORITY_FEE_PER_GAS", Some("  "))],
                || {
                    let config = AdaptiveGasConfig::from_env().unwrap();
                    assert_eq!(config.max_fee_override, None);
                    assert_eq!(config.max_priority_fee_override, None);
                },
            );
        }

        #[test]
        fn test_from_env_rejects_invalid_max_fee() {
            with_env_vars(&[("MAX_FEE_PER_GAS", Some("100gwei"))], || {
                let result = AdaptiveGasConfig::from_env();
                assert!(result.is_err());
                let err_msg = result.unwrap_err().to_string();
                assert!(err_msg.contains("MAX_FEE_PER_GAS"));
                assert!(err_msg.contains("100gwei"));
            });
        }

        #[test]
        fn test_from_env_rejects_invalid_priority_fee() {
            with_env_vars(
                &[("MAX_FEE_PER_GAS", None), ("MAX_PRIORITY_FEE_PER_GAS", Some("2 gwei"))],
                || {
                    let result = AdaptiveGasConfig::from_env();
                    assert!(result.is_err());
                    let err_msg = result.unwrap_err().to_string();
                    assert!(err_msg.contains("MAX_PRIORITY_FEE_PER_GAS"));
                    assert!(err_msg.contains("2 gwei"));
                },
            );
        }

        #[test]
        fn test_from_env_rejects_negative_values() {
            with_env_vars(&[("MAX_FEE_PER_GAS", Some("-100"))], || {
                let result = AdaptiveGasConfig::from_env();
                assert!(result.is_err());
            });
        }

        #[test]
        fn test_from_env_rejects_floating_point() {
            with_env_vars(&[("MAX_FEE_PER_GAS", Some("100.5"))], || {
                let result = AdaptiveGasConfig::from_env();
                assert!(result.is_err());
            });
        }

        #[test]
        fn test_from_env_rejects_hex_values() {
            with_env_vars(&[("MAX_FEE_PER_GAS", Some("0x174876e800"))], || {
                let result = AdaptiveGasConfig::from_env();
                assert!(result.is_err());
            });
        }
    }

    mod adaptive_gas_config_env_tests {
        use super::*;
        use std::sync::Mutex;

        static ENV_MUTEX: Mutex<()> = Mutex::new(());

        fn with_env_vars<F, T>(vars: &[(&str, Option<&str>)], f: F) -> T
        where
            F: FnOnce() -> T,
        {
            let _lock = ENV_MUTEX.lock().unwrap();

            // SAFETY: We hold a mutex to ensure single-threaded access
            let saved: Vec<_> = vars
                .iter()
                .map(|(key, value)| {
                    let saved = std::env::var(key).ok();
                    match value {
                        Some(v) => unsafe { std::env::set_var(key, v) },
                        None => unsafe { std::env::remove_var(key) },
                    }
                    (*key, saved)
                })
                .collect();

            let result = f();

            for (key, saved) in saved {
                match saved {
                    Some(v) => unsafe { std::env::set_var(key, v) },
                    None => unsafe { std::env::remove_var(key) },
                }
            }

            result
        }

        fn clear_all_gas_env_vars() -> Vec<(&'static str, Option<String>)> {
            let vars = [
                "GAS_PRICE_THRESHOLD",
                "GAS_HISTORY_BLOCKS",
                "GAS_PRICE_MULTIPLIER",
                "GAS_FALLBACK_TIMEOUT_MINUTES",
                "GAS_FALLBACK_INCREASE_PERCENT",
                "MAX_FEE_PER_GAS",
                "MAX_PRIORITY_FEE_PER_GAS",
                "MAX_GAS_PRICE_CAP",
                "SKIP_IF_GAS_ABOVE",
                "GAS_PERCENTILE",
                "RBF_TIMEOUT_SECONDS",
                "RBF_PRICE_BUMP_PERCENT",
                "RBF_MAX_RETRIES",
            ];
            vars.iter()
                .map(|&key| {
                    let saved = std::env::var(key).ok();
                    unsafe { std::env::remove_var(key) };
                    (key, saved)
                })
                .collect()
        }

        fn restore_env_vars(saved: Vec<(&'static str, Option<String>)>) {
            for (key, value) in saved {
                match value {
                    Some(v) => unsafe { std::env::set_var(key, v) },
                    None => unsafe { std::env::remove_var(key) },
                }
            }
        }

        #[test]
        fn test_adaptive_config_from_env_defaults() {
            let _lock = ENV_MUTEX.lock().unwrap();
            let saved = clear_all_gas_env_vars();

            let config = AdaptiveGasConfig::from_env().unwrap();
            assert_eq!(config.gas_price_threshold, 3_000_000_000);
            assert_eq!(config.history_blocks, 3600); // 12 hours default
            assert!((config.price_multiplier - 1.1).abs() < f64::EPSILON);
            assert_eq!(config.fallback_timeout_minutes, 30);
            assert!((config.fallback_increase_percent - 10.0).abs() < f64::EPSILON);
            // New fields defaults
            assert_eq!(config.max_gas_price_cap, Some(15_000_000_000)); // 15 gwei
            assert_eq!(config.skip_if_gas_above, Some(10_000_000_000)); // 10 gwei
            assert!((config.gas_percentile - 50.0).abs() < f64::EPSILON);
            assert_eq!(config.rbf_timeout_seconds, 60);
            assert!((config.rbf_price_bump_percent - 12.0).abs() < f64::EPSILON);
            assert_eq!(config.rbf_max_retries, 5);

            restore_env_vars(saved);
        }

        #[test]
        fn test_adaptive_config_from_env_custom_values() {
            with_env_vars(
                &[
                    ("GAS_PRICE_THRESHOLD", Some("5000000000")),
                    ("GAS_HISTORY_BLOCKS", Some("1800")),
                    ("GAS_PRICE_MULTIPLIER", Some("1.5")),
                    ("GAS_FALLBACK_TIMEOUT_MINUTES", Some("60")),
                    ("GAS_FALLBACK_INCREASE_PERCENT", Some("20")),
                ],
                || {
                    let config = AdaptiveGasConfig::from_env().unwrap();
                    assert_eq!(config.gas_price_threshold, 5_000_000_000);
                    assert_eq!(config.history_blocks, 1800);
                    assert!((config.price_multiplier - 1.5).abs() < f64::EPSILON);
                    assert_eq!(config.fallback_timeout_minutes, 60);
                    assert!((config.fallback_increase_percent - 20.0).abs() < f64::EPSILON);
                },
            );
        }

        #[test]
        fn test_adaptive_config_rejects_invalid_multiplier() {
            with_env_vars(&[("GAS_PRICE_MULTIPLIER", Some("-1.0"))], || {
                let result = AdaptiveGasConfig::from_env();
                assert!(result.is_err());
                let err_msg = result.unwrap_err().to_string();
                assert!(err_msg.contains("finite positive"));
            });
        }

        #[test]
        fn test_adaptive_config_rejects_zero_multiplier() {
            with_env_vars(&[("GAS_PRICE_MULTIPLIER", Some("0"))], || {
                let result = AdaptiveGasConfig::from_env();
                assert!(result.is_err());
            });
        }

        #[test]
        fn test_adaptive_config_rejects_nan_multiplier() {
            with_env_vars(&[("GAS_PRICE_MULTIPLIER", Some("NaN"))], || {
                let result = AdaptiveGasConfig::from_env();
                assert!(result.is_err());
                let err_msg = result.unwrap_err().to_string();
                assert!(err_msg.contains("finite positive"));
            });
        }

        #[test]
        fn test_adaptive_config_rejects_infinity_multiplier() {
            with_env_vars(&[("GAS_PRICE_MULTIPLIER", Some("inf"))], || {
                let result = AdaptiveGasConfig::from_env();
                assert!(result.is_err());
                let err_msg = result.unwrap_err().to_string();
                assert!(err_msg.contains("finite positive"));
            });
        }

        #[test]
        fn test_adaptive_config_rejects_zero_fallback_timeout() {
            with_env_vars(&[("GAS_FALLBACK_TIMEOUT_MINUTES", Some("0"))], || {
                let result = AdaptiveGasConfig::from_env();
                assert!(result.is_err());
                let err_msg = result.unwrap_err().to_string();
                assert!(err_msg.contains("GAS_FALLBACK_TIMEOUT_MINUTES"));
                assert!(err_msg.contains("greater than 0"));
            });
        }

        #[test]
        fn test_adaptive_config_new_env_vars() {
            with_env_vars(
                &[
                    ("MAX_GAS_PRICE_CAP", Some("500000000000")),
                    ("SKIP_IF_GAS_ABOVE", Some("200000000000")),
                    ("GAS_PERCENTILE", Some("25.0")),
                    ("RBF_TIMEOUT_SECONDS", Some("120")),
                    ("RBF_PRICE_BUMP_PERCENT", Some("15.0")),
                    ("RBF_MAX_RETRIES", Some("10")),
                ],
                || {
                    let config = AdaptiveGasConfig::from_env().unwrap();
                    assert_eq!(config.max_gas_price_cap, Some(500_000_000_000));
                    assert_eq!(config.skip_if_gas_above, Some(200_000_000_000));
                    assert!((config.gas_percentile - 25.0).abs() < f64::EPSILON);
                    assert_eq!(config.rbf_timeout_seconds, 120);
                    assert!((config.rbf_price_bump_percent - 15.0).abs() < f64::EPSILON);
                    assert_eq!(config.rbf_max_retries, 10);
                },
            );
        }

        #[test]
        fn test_adaptive_config_rejects_invalid_percentile() {
            with_env_vars(&[("GAS_PERCENTILE", Some("150.0"))], || {
                let result = AdaptiveGasConfig::from_env();
                assert!(result.is_err());
                let err_msg = result.unwrap_err().to_string();
                assert!(err_msg.contains("GAS_PERCENTILE"));
                assert!(err_msg.contains("between 0 and 100"));
            });
        }

        #[test]
        fn test_gas_pricing_strategy_from_env_default() {
            let _lock = ENV_MUTEX.lock().unwrap();
            let saved = clear_all_gas_env_vars();

            let strategy = GasPricingStrategy::from_env().unwrap();
            assert!(matches!(strategy, GasPricingStrategy::Default));

            restore_env_vars(saved);
        }

        #[test]
        fn test_gas_pricing_strategy_from_env_with_override() {
            // When override vars are set, strategy should be Adaptive (which now handles overrides)
            with_env_vars(
                &[("GAS_PRICE_THRESHOLD", None), ("MAX_FEE_PER_GAS", Some("100000000000"))],
                || {
                    let strategy = GasPricingStrategy::from_env().unwrap();
                    assert!(matches!(strategy, GasPricingStrategy::Adaptive(_)));
                    if let GasPricingStrategy::Adaptive(oracle) = strategy {
                        assert_eq!(oracle.config().max_fee_override, Some(100_000_000_000));
                    }
                },
            );
        }

        #[test]
        fn test_gas_pricing_strategy_from_env_adaptive() {
            with_env_vars(
                &[("GAS_PRICE_THRESHOLD", Some("5000000000")), ("MAX_FEE_PER_GAS", None)],
                || {
                    let strategy = GasPricingStrategy::from_env().unwrap();
                    assert!(matches!(strategy, GasPricingStrategy::Adaptive(_)));
                },
            );
        }

        #[test]
        fn test_gas_pricing_strategy_with_both_override_and_adaptive() {
            // When both override and adaptive configs are set, both should be in the config
            with_env_vars(
                &[
                    ("GAS_PRICE_THRESHOLD", Some("5000000000")),
                    ("MAX_FEE_PER_GAS", Some("100000000000")),
                ],
                || {
                    let strategy = GasPricingStrategy::from_env().unwrap();
                    assert!(matches!(strategy, GasPricingStrategy::Adaptive(_)));
                    if let GasPricingStrategy::Adaptive(oracle) = strategy {
                        assert_eq!(oracle.config().gas_price_threshold, 5_000_000_000);
                        assert_eq!(oracle.config().max_fee_override, Some(100_000_000_000));
                    }
                },
            );
        }
    }
}
