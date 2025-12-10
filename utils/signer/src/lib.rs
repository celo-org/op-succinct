use std::{str::FromStr, sync::Arc};

use alloy_consensus::TxEnvelope;
use alloy_eips::Decodable2718;
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
use tokio::{sync::Mutex, time::Duration};

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

/// Wrapper around Signer that provides thread-safe transaction sending.
/// Transactions are serialized via a Mutex to prevent nonce conflicts.
#[derive(Clone, Debug)]
pub struct SignerLock {
    inner: Arc<Mutex<Signer>>,
    cached_address: Address,
    gas_config: GasConfig,
}

impl SignerLock {
    /// Creates a new SignerLock wrapping the given Signer.
    pub fn new(signer: Signer) -> Self {
        let cached_address = signer.address();
        SignerLock {
            inner: Arc::new(Mutex::new(signer)),
            cached_address,
            gas_config: GasConfig::default(),
        }
    }

    /// Creates a new SignerLock with custom gas configuration.
    pub fn new_with_gas_config(signer: Signer, gas_config: GasConfig) -> Self {
        let cached_address = signer.address();
        SignerLock { inner: Arc::new(Mutex::new(signer)), cached_address, gas_config }
    }

    /// Creates a SignerLock from environment variables.
    pub async fn from_env() -> Result<Self> {
        let signer = Signer::from_env().await?;
        let gas_config = GasConfig::from_env()?;
        Ok(SignerLock::new_with_gas_config(signer, gas_config))
    }

    /// Returns the address of the signer without acquiring a lock.
    pub fn address(&self) -> Address {
        self.cached_address
    }

    /// Returns the gas configuration.
    pub fn gas_config(&self) -> &GasConfig {
        &self.gas_config
    }

    /// Sends a transaction request, signed by the configured signer.
    /// Transactions are serialized via a Mutex to prevent nonce conflicts.
    /// Applies gas configuration if set.
    pub async fn send_transaction_request(
        &self,
        l1_rpc: Url,
        mut transaction_request: TransactionRequest,
    ) -> Result<TransactionReceipt> {
        // Apply gas config if set
        if let Some(max_fee) = self.gas_config.max_fee_per_gas {
            transaction_request = transaction_request.max_fee_per_gas(max_fee);
        }
        if let Some(priority_fee) = self.gas_config.max_priority_fee_per_gas {
            transaction_request = transaction_request.max_priority_fee_per_gas(priority_fee);
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
    fn test_signer_lock_new_has_default_gas_config() {
        let signer = Signer::new_local_signer(
            "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
        )
        .unwrap();
        let signer_lock = SignerLock::new(signer);

        assert_eq!(signer_lock.gas_config().max_fee_per_gas, None);
        assert_eq!(signer_lock.gas_config().max_priority_fee_per_gas, None);
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

        assert_eq!(signer_lock.gas_config().max_fee_per_gas, Some(100_000_000_000u128));
        assert_eq!(signer_lock.gas_config().max_priority_fee_per_gas, Some(2_000_000_000u128));
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
}
