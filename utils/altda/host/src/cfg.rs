//! Configuration types for the AltDA host.
//!
//! Contains the [`AltDAChainHost`] configuration struct, [`AltDAExtendedHintType`] hint type
//! wrapper, and [`AltDAChainProviders`] provider set. Wraps celo-kona's [`CeloSingleChainHost`]
//! so standard L1/L2 hints are served with Celo semantics (Celo block/transaction decoding,
//! Celo rollup-config / Espresso settings), matching the celo-fied AltDA client, and adds the
//! AltDA commitment hint on top.

use std::{str::FromStr, sync::Arc, time::Duration};

use anyhow::Result;
use async_trait::async_trait;
use celo_host::single::{
    CeloConfigBackend, CeloSingleChainHost, CeloSingleChainProviders, CeloVerifyingPreimageFetcher,
};
use clap::Parser;
use kona_host::{
    single::SingleChainHostError, OfflineHostBackend, OnlineHostBackend, OnlineHostBackendCfg,
    PreimageServer,
};
use kona_preimage::{Channel, HintReader, OracleServer};
use kona_proof::errors::HintParsingError;
use op_succinct_host_utils::host::PreimageServerStarter;
use serde::Serialize;
use tokio::task::{self, JoinHandle};

use crate::handler::AltDAHintHandler;

/// The hint type celo-kona's [`CeloSingleChainHost`] serves under the same config.
///
/// This is kona's `HintType` when celo-host is built without `eigenda` and hokulea's
/// `ExtendedHintType` when it is built with it (the workspace default). Aliasing it — rather than
/// naming a concrete type — keeps the AltDA host correct under either feature set; the only spots
/// that would otherwise hard-code one form are `Standard`'s payload below and the high-level hint,
/// both of which go through this alias or `FromStr`.
type CeloHintType = <CeloSingleChainHost as OnlineHostBackendCfg>::HintType;

/// Extended hint type that wraps the Celo-served hint type and adds the `AltDACommitment` variant.
///
/// The client-side [`AltDAHintType`](op_succinct_altda_client_utils::hint::AltDAHintType) produces
/// the hint string `"altda-commitment"` via its `Display` impl. This type parses that string back
/// via `FromStr`, routing it to the AltDA-specific hint handler on the host side.
///
/// Standard kona hint types (L1BlockHeader, L1Transactions, etc.) are wrapped in the `Standard`
/// variant and delegated to celo-kona's [`CeloSingleChainHintHandler`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AltDAExtendedHintType {
    /// A standard kona hint type, served by the Celo hint handler.
    Standard(CeloHintType),
    /// An AltDA commitment hint. The hint data contains the encoded commitment:
    /// `[commitment_type_byte][commitment_data...]`
    AltDACommitment,
}

impl core::str::FromStr for AltDAExtendedHintType {
    type Err = HintParsingError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "altda-commitment" => Ok(Self::AltDACommitment),
            _ => Ok(Self::Standard(CeloHintType::from_str(s)?)),
        }
    }
}

impl core::fmt::Display for AltDAExtendedHintType {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            AltDAExtendedHintType::Standard(hint) => write!(f, "{hint}"),
            AltDAExtendedHintType::AltDACommitment => write!(f, "altda-commitment"),
        }
    }
}

/// The host configuration for AltDA-backed Celo chains.
///
/// Wraps celo-kona's [`CeloSingleChainHost`] and adds the AltDA server URL. The DA server is the
/// standard OP Stack `op-alt-da` server that stores batch data and serves it by commitment.
#[derive(Default, Parser, Serialize, Clone, Debug)]
pub struct AltDAChainHost {
    /// The inner Celo single-chain host configuration.
    #[clap(flatten)]
    pub celo_host: CeloSingleChainHost,

    /// URL of the AltDA server (e.g., `http://127.0.0.1:8080`).
    ///
    /// The host fetches batch data from this server using the endpoint:
    /// `GET {altda_server_url}/get/0x{hex(encoded_commitment)}`
    #[clap(long, env = "ALTDA_SERVER_URL")]
    pub altda_server_url: Option<String>,
}

impl AltDAChainHost {
    /// Starts the preimage server, communicating with the client over the provided channels.
    ///
    /// Mirrors [`CeloSingleChainHost::start_server`]: the backend is wrapped in
    /// [`CeloConfigBackend`] (serves the Celo rollup config, Espresso settings included) and
    /// [`CeloVerifyingPreimageFetcher`] (verifies authenticated global keys), and standard hints
    /// route through [`CeloSingleChainHintHandler`] via [`AltDAHintHandler`]. In offline mode it
    /// serves preimages from the key-value store only.
    pub async fn start_server<C>(
        &self,
        hint: C,
        preimage: C,
    ) -> Result<JoinHandle<Result<(), SingleChainHostError>>, SingleChainHostError>
    where
        C: Channel + Send + Sync + 'static,
    {
        let kv_store = self.celo_host.create_key_value_store()?;
        // The rollup config (Celo Espresso settings included) that `CeloConfigBackend` serves under
        // `L2_ROLLUP_CONFIG_KEY`. `None` when no rollup config is configured, in which case the
        // request is delegated to the inner backend (yielding `KeyNotFound`), matching upstream.
        let rollup_config_json = self
            .celo_host
            .read_rollup_config()?
            .and_then(|cfg| serde_json::to_vec(&cfg).ok())
            .map(Arc::new);

        let task_handle = if self.is_offline() {
            task::spawn(async {
                PreimageServer::new(
                    OracleServer::new(preimage),
                    HintReader::new(hint),
                    Arc::new(CeloVerifyingPreimageFetcher::new(CeloConfigBackend::new(
                        OfflineHostBackend::new(kv_store),
                        rollup_config_json,
                    ))),
                )
                .start()
                .await
                .map_err(SingleChainHostError::from)
            })
        } else {
            let providers = self.create_providers().await?;
            // The L2 payload witness is the high-level hint that drives execution. Build it via
            // `from_str` so it stays correct whether celo-host resolves `Standard`'s inner type to
            // kona's `HintType` or hokulea's `ExtendedHintType` (which depends on celo-host's
            // `eigenda` feature).
            let high_level_hint = AltDAExtendedHintType::from_str("l2-payload-witness")
                .expect("\"l2-payload-witness\" is a valid hint type");
            let backend = CeloVerifyingPreimageFetcher::new(CeloConfigBackend::new(
                OnlineHostBackend::new(self.clone(), kv_store.clone(), providers, AltDAHintHandler)
                    .with_high_level_hint(high_level_hint),
                rollup_config_json,
            ));

            task::spawn(async {
                PreimageServer::new(
                    OracleServer::new(preimage),
                    HintReader::new(hint),
                    Arc::new(backend),
                )
                .start()
                .await
                .map_err(SingleChainHostError::from)
            })
        };

        Ok(task_handle)
    }

    /// Returns `true` if the host is running in offline mode.
    pub const fn is_offline(&self) -> bool {
        self.celo_host.is_offline()
    }

    /// Creates the providers required for the host backend.
    ///
    /// Extends celo-kona's [`CeloSingleChainProviders`] with an HTTP client and DA server URL for
    /// fetching AltDA commitment data.
    async fn create_providers(&self) -> Result<AltDAChainProviders, SingleChainHostError> {
        let inner_providers = self.celo_host.create_providers().await?;

        let da_server_url = self
            .altda_server_url
            .clone()
            .ok_or(SingleChainHostError::Other("AltDA server URL must be set"))?;

        Ok(AltDAChainProviders {
            inner_providers,
            da_server_url,
            http_client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("failed to build HTTP client"),
        })
    }
}

impl OnlineHostBackendCfg for AltDAChainHost {
    type HintType = AltDAExtendedHintType;
    type Providers = AltDAChainProviders;
}

/// The providers required for the AltDA host.
///
/// Extends celo-kona's [`CeloSingleChainProviders`] with an HTTP client for fetching batch data
/// from the DA server.
#[derive(Debug, Clone)]
pub struct AltDAChainProviders {
    /// The Celo single-chain providers (L1, L2, beacon).
    pub inner_providers: CeloSingleChainProviders,
    /// The URL of the AltDA server.
    pub da_server_url: String,
    /// HTTP client for making requests to the DA server.
    pub http_client: reqwest::Client,
}

#[async_trait]
impl PreimageServerStarter for AltDAChainHost {
    async fn start_server<C>(
        &self,
        hint: C,
        preimage: C,
    ) -> Result<JoinHandle<Result<(), SingleChainHostError>>, SingleChainHostError>
    where
        C: Channel + Send + Sync + 'static,
    {
        self.start_server(hint, preimage).await
    }
}
