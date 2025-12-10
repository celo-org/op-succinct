use std::{
    str::FromStr,
    sync::Arc,
    time::Instant,
};

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

/// Gas configuration for L1 transactions.
#[derive(Clone, Debug, Default)]
pub struct GasConfig {
    /// Optional max fee per gas in wei. If not set, uses provider estimation.
    pub max_fee_per_gas: Option<u128>,
    /// Optional max priority fee (tip) per gas in wei. If not set, uses provider estimation.
    pub max_priority_fee_per_gas: Option<u128>,
}

impl GasConfig {
    /// Creates a new GasConfig from environment variables.
    ///
    /// Returns an error if an environment variable is set but contains an invalid value.
    /// The values must be valid u128 integers representing wei (not gwei or other units).
    pub fn from_env() -> Result<Self> {
        let max_fee_per_gas = Self::parse_env_var("MAX_FEE_PER_GAS")?;
        let max_priority_fee_per_gas = Self::parse_env_var("MAX_PRIORITY_FEE_PER_GAS")?;

        Ok(Self { max_fee_per_gas, max_priority_fee_per_gas })
    }

    /// Parses an environment variable as an optional u128.
    ///
    /// Returns:
    /// - `Ok(None)` if the environment variable is not set
    /// - `Ok(Some(value))` if the environment variable is set and can be parsed
    /// - `Err` if the environment variable is set but cannot be parsed as u128
    fn parse_env_var(var_name: &str) -> Result<Option<u128>> {
        match std::env::var(var_name) {
            Ok(value) => {
                let trimmed = value.trim();
                if trimmed.is_empty() {
                    return Ok(None);
                }
                trimmed.parse::<u128>().map(Some).with_context(|| {
                    format!(
                        "Failed to parse {} environment variable: '{}'. \
                         Value must be a valid integer in wei (not gwei or other units). \
                         Example: MAX_FEE_PER_GAS=100000000000 for 100 gwei",
                        var_name, value
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

/// Configuration for adaptive gas pricing strategy.
///
/// This config controls how gas prices are calculated when network gas prices
/// exceed a threshold. When gas is above the threshold, the filler uses
/// historical average baseFee (from `eth_feeHistory`) multiplied by a factor,
/// with gradual increases after a timeout.
#[derive(Clone, Debug)]
pub struct AdaptiveGasConfig {
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
    /// Percentage increase per timeout period.
    /// Default: 10
    pub fallback_increase_percent: u64,
}

impl Default for AdaptiveGasConfig {
    fn default() -> Self {
        Self {
            gas_price_threshold: 3_000_000_000,   // 3 gwei
            history_blocks: 900,                   // ~3 hours at 12s/block
            price_multiplier: 1.1,                 // 110%
            fallback_timeout_minutes: 30,
            fallback_increase_percent: 10,
        }
    }
}

impl AdaptiveGasConfig {
    /// Creates a new AdaptiveGasConfig from environment variables.
    ///
    /// All values are optional and fall back to defaults if not set.
    /// Returns an error if an environment variable is set but contains an invalid value.
    pub fn from_env() -> Result<Self> {
        let mut config = Self::default();

        if let Some(threshold) = Self::parse_env_var_u128("GAS_PRICE_THRESHOLD")? {
            config.gas_price_threshold = threshold;
        }
        if let Some(blocks) = Self::parse_env_var_u64("GAS_HISTORY_BLOCKS")? {
            config.history_blocks = blocks;
        }
        if let Some(multiplier) = Self::parse_env_var_f64("GAS_PRICE_MULTIPLIER")? {
            if multiplier <= 0.0 {
                anyhow::bail!("GAS_PRICE_MULTIPLIER must be positive, got: {}", multiplier);
            }
            config.price_multiplier = multiplier;
        }
        if let Some(timeout) = Self::parse_env_var_u64("GAS_FALLBACK_TIMEOUT_MINUTES")? {
            config.fallback_timeout_minutes = timeout;
        }
        if let Some(percent) = Self::parse_env_var_u64("GAS_FALLBACK_INCREASE_PERCENT")? {
            config.fallback_increase_percent = percent;
        }

        Ok(config)
    }

    fn parse_env_var_u128(var_name: &str) -> Result<Option<u128>> {
        match std::env::var(var_name) {
            Ok(value) => {
                let trimmed = value.trim();
                if trimmed.is_empty() {
                    return Ok(None);
                }
                trimmed.parse::<u128>().map(Some).with_context(|| {
                    format!(
                        "Failed to parse {} environment variable: '{}'. \
                         Value must be a valid integer in wei.",
                        var_name, value
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

    fn parse_env_var_u64(var_name: &str) -> Result<Option<u64>> {
        match std::env::var(var_name) {
            Ok(value) => {
                let trimmed = value.trim();
                if trimmed.is_empty() {
                    return Ok(None);
                }
                trimmed.parse::<u64>().map(Some).with_context(|| {
                    format!(
                        "Failed to parse {} environment variable: '{}'. \
                         Value must be a valid positive integer.",
                        var_name, value
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

    fn parse_env_var_f64(var_name: &str) -> Result<Option<f64>> {
        match std::env::var(var_name) {
            Ok(value) => {
                let trimmed = value.trim();
                if trimmed.is_empty() {
                    return Ok(None);
                }
                trimmed.parse::<f64>().map(Some).with_context(|| {
                    format!(
                        "Failed to parse {} environment variable: '{}'. \
                         Value must be a valid decimal number (e.g., 1.1).",
                        var_name, value
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
        Self {
            config,
            high_gas_start: Arc::new(RwLock::new(None)),
        }
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
        let increase_per_period = self.config.fallback_increase_percent as f64 / 100.0;

        // Compound increase: (1 + increase)^periods
        Some((1.0 + increase_per_period).powi(periods as i32))
    }

    /// Gets the recommended gas prices for a transaction.
    ///
    /// This method fetches current gas prices and historical fee data to
    /// calculate optimal gas prices based on the configured strategy.
    pub async fn get_gas_prices<P>(&self, provider: &P) -> Result<GasPriceEstimate>
    where
        P: Provider<Ethereum>,
    {
        // Get current gas price to compare against threshold
        let current_gas = provider
            .get_gas_price()
            .await
            .context("Failed to get current gas price")?;

        if current_gas <= self.config.gas_price_threshold {
            // Below threshold - reset timer, use current estimation
            self.reset_high_gas_timer().await;

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
        let fee_history = provider
            .get_fee_history(
                self.config.history_blocks,
                BlockNumberOrTag::Latest,
                &[50.0], // median priority fee
            )
            .await
            .context("Failed to get fee history")?;

        // Calculate average baseFee
        let base_fees: Vec<u128> = fee_history.base_fee_per_gas.to_vec();
        let avg_base_fee = if base_fees.is_empty() {
            current_gas // fallback to current if no history
        } else {
            base_fees.iter().sum::<u128>() / base_fees.len() as u128
        };

        // Calculate average priority fee from rewards
        let priority_fees: Vec<u128> = fee_history
            .reward
            .iter()
            .flatten()
            .flatten()
            .copied()
            .collect();
        let avg_priority_fee = if priority_fees.is_empty() {
            1_000_000_000u128 // 1 gwei default if no data
        } else {
            priority_fees.iter().sum::<u128>() / priority_fees.len() as u128
        };

        // Apply multiplier
        let mut max_fee = (avg_base_fee as f64 * self.config.price_multiplier) as u128;
        let mut priority_fee = (avg_priority_fee as f64 * self.config.price_multiplier) as u128;

        // Apply timeout increase if waiting too long
        if let Some(factor) = self.get_timeout_increase_factor().await {
            max_fee = (max_fee as f64 * factor) as u128;
            priority_fee = (priority_fee as f64 * factor) as u128;
        }

        Ok(GasPriceEstimate {
            max_fee_per_gas: max_fee,
            max_priority_fee_per_gas: priority_fee,
        })
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
}

/// Gas pricing strategy for transactions.
#[derive(Clone, Debug)]
pub enum GasPricingStrategy {
    /// No gas pricing override - use provider defaults.
    Default,
    /// Static gas prices (legacy behavior).
    Static(GasConfig),
    /// Adaptive gas pricing based on network conditions.
    Adaptive(AdaptiveGasOracle),
}

impl GasPricingStrategy {
    /// Creates a gas pricing strategy from environment variables.
    ///
    /// The strategy is determined by which environment variables are set:
    /// - If `GAS_PRICE_THRESHOLD` or other adaptive config vars are set, uses Adaptive
    /// - If `MAX_FEE_PER_GAS` or `MAX_PRIORITY_FEE_PER_GAS` are set, uses Static
    /// - Otherwise, uses Default (provider estimation)
    pub fn from_env() -> Result<Self> {
        // Check if any adaptive gas config vars are set
        let has_adaptive_config = std::env::var("GAS_PRICE_THRESHOLD").is_ok()
            || std::env::var("GAS_HISTORY_BLOCKS").is_ok()
            || std::env::var("GAS_PRICE_MULTIPLIER").is_ok()
            || std::env::var("GAS_FALLBACK_TIMEOUT_MINUTES").is_ok()
            || std::env::var("GAS_FALLBACK_INCREASE_PERCENT").is_ok();

        if has_adaptive_config {
            return Ok(Self::Adaptive(AdaptiveGasOracle::from_env()?));
        }

        // Check if static gas config is set
        let gas_config = GasConfig::from_env()?;
        if gas_config.max_fee_per_gas.is_some() || gas_config.max_priority_fee_per_gas.is_some() {
            return Ok(Self::Static(gas_config));
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

    /// Creates a new SignerLock with static gas configuration (legacy).
    pub fn new_with_gas_config(signer: Signer, gas_config: GasConfig) -> Self {
        let cached_address = signer.address();
        SignerLock {
            inner: Arc::new(Mutex::new(signer)),
            cached_address,
            gas_strategy: GasPricingStrategy::Static(gas_config),
        }
    }

    /// Creates a new SignerLock with adaptive gas pricing.
    pub fn new_with_adaptive_gas(signer: Signer, oracle: AdaptiveGasOracle) -> Self {
        let cached_address = signer.address();
        SignerLock {
            inner: Arc::new(Mutex::new(signer)),
            cached_address,
            gas_strategy: GasPricingStrategy::Adaptive(oracle),
        }
    }

    /// Creates a new SignerLock with a custom gas pricing strategy.
    pub fn new_with_strategy(signer: Signer, gas_strategy: GasPricingStrategy) -> Self {
        let cached_address = signer.address();
        SignerLock {
            inner: Arc::new(Mutex::new(signer)),
            cached_address,
            gas_strategy,
        }
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

    /// Returns the static gas configuration if using static pricing.
    /// Returns None if using adaptive or default pricing.
    pub fn gas_config(&self) -> Option<&GasConfig> {
        match &self.gas_strategy {
            GasPricingStrategy::Static(config) => Some(config),
            _ => None,
        }
    }

    /// Returns the adaptive gas oracle if using adaptive pricing.
    /// Returns None if using static or default pricing.
    pub fn gas_oracle(&self) -> Option<&AdaptiveGasOracle> {
        match &self.gas_strategy {
            GasPricingStrategy::Adaptive(oracle) => Some(oracle),
            _ => None,
        }
    }

    /// Sends a transaction request, signed by the configured signer.
    /// Transactions are serialized via a Mutex to prevent nonce conflicts.
    /// Applies gas pricing strategy based on configuration.
    pub async fn send_transaction_request(
        &self,
        l1_rpc: Url,
        mut transaction_request: TransactionRequest,
    ) -> Result<TransactionReceipt> {
        // Apply gas pricing strategy
        match &self.gas_strategy {
            GasPricingStrategy::Default => {
                // No modification - let provider estimate
            }
            GasPricingStrategy::Static(config) => {
                // Apply static gas prices if set
                if let Some(max_fee) = config.max_fee_per_gas {
                    transaction_request = transaction_request.max_fee_per_gas(max_fee);
                }
                if let Some(priority_fee) = config.max_priority_fee_per_gas {
                    transaction_request = transaction_request.max_priority_fee_per_gas(priority_fee);
                }
            }
            GasPricingStrategy::Adaptive(oracle) => {
                // Create a provider to query gas prices
                let provider = ProviderBuilder::new()
                    .network::<Ethereum>()
                    .connect_http(l1_rpc.clone());

                // Get adaptive gas prices
                let estimate = oracle.get_gas_prices(&provider).await?;
                transaction_request = oracle.apply_to_transaction(transaction_request, &estimate);
            }
        }

        let signer = self.inner.lock().await;
        signer.send_transaction_request(l1_rpc, transaction_request).await
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
    fn test_gas_config_default() {
        let config = GasConfig::default();
        assert_eq!(config.max_fee_per_gas, None);
        assert_eq!(config.max_priority_fee_per_gas, None);
    }

    #[test]
    fn test_gas_config_struct_construction() {
        // Test direct struct construction (doesn't rely on env vars)
        let config = GasConfig {
            max_fee_per_gas: Some(100_000_000_000u128),
            max_priority_fee_per_gas: Some(2_000_000_000u128),
        };
        assert_eq!(config.max_fee_per_gas, Some(100_000_000_000u128));
        assert_eq!(config.max_priority_fee_per_gas, Some(2_000_000_000u128));

        // Test partial values
        let config_partial =
            GasConfig { max_fee_per_gas: Some(50_000_000_000u128), max_priority_fee_per_gas: None };
        assert_eq!(config_partial.max_fee_per_gas, Some(50_000_000_000u128));
        assert_eq!(config_partial.max_priority_fee_per_gas, None);
    }

    #[test]
    fn test_signer_lock_new_has_default_strategy() {
        let signer = Signer::new_local_signer(
            "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
        )
        .unwrap();
        let signer_lock = SignerLock::new(signer);

        // Default strategy means no static gas_config
        assert!(signer_lock.gas_config().is_none());
        assert!(signer_lock.gas_oracle().is_none());
        assert!(matches!(signer_lock.gas_strategy(), GasPricingStrategy::Default));
    }

    #[test]
    fn test_signer_lock_new_with_gas_config() {
        let signer = Signer::new_local_signer(
            "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
        )
        .unwrap();
        let gas_config = GasConfig {
            max_fee_per_gas: Some(100_000_000_000u128),
            max_priority_fee_per_gas: Some(2_000_000_000u128),
        };
        let signer_lock = SignerLock::new_with_gas_config(signer, gas_config);

        let config = signer_lock.gas_config().expect("Should have static config");
        assert_eq!(config.max_fee_per_gas, Some(100_000_000_000u128));
        assert_eq!(config.max_priority_fee_per_gas, Some(2_000_000_000u128));
        assert!(matches!(signer_lock.gas_strategy(), GasPricingStrategy::Static(_)));
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
        assert!(signer_lock.gas_config().is_none());
        assert!(matches!(signer_lock.gas_strategy(), GasPricingStrategy::Adaptive(_)));
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
        assert_eq!(config.gas_price_threshold, 3_000_000_000); // 3 gwei
        assert_eq!(config.history_blocks, 900);
        assert!((config.price_multiplier - 1.1).abs() < f64::EPSILON);
        assert_eq!(config.fallback_timeout_minutes, 30);
        assert_eq!(config.fallback_increase_percent, 10);
    }

    #[test]
    fn test_adaptive_gas_oracle_creation() {
        let config = AdaptiveGasConfig {
            gas_price_threshold: 5_000_000_000,
            history_blocks: 1000,
            price_multiplier: 1.2,
            fallback_timeout_minutes: 45,
            fallback_increase_percent: 15,
        };
        let oracle = AdaptiveGasOracle::new(config.clone());

        assert_eq!(oracle.config().gas_price_threshold, 5_000_000_000);
        assert_eq!(oracle.config().history_blocks, 1000);
        assert!((oracle.config().price_multiplier - 1.2).abs() < f64::EPSILON);
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
            fallback_increase_percent: 10,
            ..Default::default()
        };
        let oracle = AdaptiveGasOracle::new(config);

        // Timer not started - no increase
        assert!(oracle.get_timeout_increase_factor().await.is_none());
    }

    // Note: Tests that modify environment variables should be run with --test-threads=1
    // to avoid race conditions. Example:
    // cargo test -p op-succinct-signer-utils -- --test-threads=1

    mod gas_config_env_tests {
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
        fn test_from_env_with_valid_values() {
            with_env_vars(
                &[
                    ("MAX_FEE_PER_GAS", Some("100000000000")),
                    ("MAX_PRIORITY_FEE_PER_GAS", Some("2000000000")),
                ],
                || {
                    let config = GasConfig::from_env().unwrap();
                    assert_eq!(config.max_fee_per_gas, Some(100_000_000_000u128));
                    assert_eq!(config.max_priority_fee_per_gas, Some(2_000_000_000u128));
                },
            );
        }

        #[test]
        fn test_from_env_with_no_vars_set() {
            with_env_vars(&[("MAX_FEE_PER_GAS", None), ("MAX_PRIORITY_FEE_PER_GAS", None)], || {
                let config = GasConfig::from_env().unwrap();
                assert_eq!(config.max_fee_per_gas, None);
                assert_eq!(config.max_priority_fee_per_gas, None);
            });
        }

        #[test]
        fn test_from_env_with_partial_values() {
            with_env_vars(
                &[("MAX_FEE_PER_GAS", Some("50000000000")), ("MAX_PRIORITY_FEE_PER_GAS", None)],
                || {
                    let config = GasConfig::from_env().unwrap();
                    assert_eq!(config.max_fee_per_gas, Some(50_000_000_000u128));
                    assert_eq!(config.max_priority_fee_per_gas, None);
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
                    let config = GasConfig::from_env().unwrap();
                    assert_eq!(config.max_fee_per_gas, Some(100_000_000_000u128));
                    assert_eq!(config.max_priority_fee_per_gas, Some(2_000_000_000u128));
                },
            );
        }

        #[test]
        fn test_from_env_empty_string_is_none() {
            with_env_vars(
                &[("MAX_FEE_PER_GAS", Some("")), ("MAX_PRIORITY_FEE_PER_GAS", Some("  "))],
                || {
                    let config = GasConfig::from_env().unwrap();
                    assert_eq!(config.max_fee_per_gas, None);
                    assert_eq!(config.max_priority_fee_per_gas, None);
                },
            );
        }

        #[test]
        fn test_from_env_rejects_invalid_max_fee() {
            with_env_vars(&[("MAX_FEE_PER_GAS", Some("100gwei"))], || {
                let result = GasConfig::from_env();
                assert!(result.is_err());
                let err_msg = result.unwrap_err().to_string();
                assert!(err_msg.contains("MAX_FEE_PER_GAS"));
                assert!(err_msg.contains("100gwei"));
                assert!(err_msg.contains("wei"));
            });
        }

        #[test]
        fn test_from_env_rejects_invalid_priority_fee() {
            with_env_vars(
                &[("MAX_FEE_PER_GAS", None), ("MAX_PRIORITY_FEE_PER_GAS", Some("2 gwei"))],
                || {
                    let result = GasConfig::from_env();
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
                let result = GasConfig::from_env();
                assert!(result.is_err());
            });
        }

        #[test]
        fn test_from_env_rejects_floating_point() {
            with_env_vars(&[("MAX_FEE_PER_GAS", Some("100.5"))], || {
                let result = GasConfig::from_env();
                assert!(result.is_err());
            });
        }

        #[test]
        fn test_from_env_rejects_hex_values() {
            with_env_vars(&[("MAX_FEE_PER_GAS", Some("0x174876e800"))], || {
                let result = GasConfig::from_env();
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
            assert_eq!(config.history_blocks, 900);
            assert!((config.price_multiplier - 1.1).abs() < f64::EPSILON);
            assert_eq!(config.fallback_timeout_minutes, 30);
            assert_eq!(config.fallback_increase_percent, 10);

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
                    assert_eq!(config.fallback_increase_percent, 20);
                },
            );
        }

        #[test]
        fn test_adaptive_config_rejects_invalid_multiplier() {
            with_env_vars(&[("GAS_PRICE_MULTIPLIER", Some("-1.0"))], || {
                let result = AdaptiveGasConfig::from_env();
                assert!(result.is_err());
                let err_msg = result.unwrap_err().to_string();
                assert!(err_msg.contains("positive"));
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
        fn test_gas_pricing_strategy_from_env_default() {
            let _lock = ENV_MUTEX.lock().unwrap();
            let saved = clear_all_gas_env_vars();

            let strategy = GasPricingStrategy::from_env().unwrap();
            assert!(matches!(strategy, GasPricingStrategy::Default));

            restore_env_vars(saved);
        }

        #[test]
        fn test_gas_pricing_strategy_from_env_static() {
            with_env_vars(
                &[
                    ("GAS_PRICE_THRESHOLD", None),
                    ("MAX_FEE_PER_GAS", Some("100000000000")),
                ],
                || {
                    let strategy = GasPricingStrategy::from_env().unwrap();
                    assert!(matches!(strategy, GasPricingStrategy::Static(_)));
                },
            );
        }

        #[test]
        fn test_gas_pricing_strategy_from_env_adaptive() {
            with_env_vars(
                &[
                    ("GAS_PRICE_THRESHOLD", Some("5000000000")),
                    ("MAX_FEE_PER_GAS", None),
                ],
                || {
                    let strategy = GasPricingStrategy::from_env().unwrap();
                    assert!(matches!(strategy, GasPricingStrategy::Adaptive(_)));
                },
            );
        }

        #[test]
        fn test_gas_pricing_strategy_adaptive_takes_precedence() {
            // When both adaptive and static configs are set, adaptive wins
            with_env_vars(
                &[
                    ("GAS_PRICE_THRESHOLD", Some("5000000000")),
                    ("MAX_FEE_PER_GAS", Some("100000000000")),
                ],
                || {
                    let strategy = GasPricingStrategy::from_env().unwrap();
                    assert!(matches!(strategy, GasPricingStrategy::Adaptive(_)));
                },
            );
        }
    }
}
