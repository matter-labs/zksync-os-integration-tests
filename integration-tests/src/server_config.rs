//! Builder for zksync-os-server config YAML files.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Serialize;

/// Settlement mode for a chain's batch pubdata.
pub enum PubdataMode {
    /// L1 blobs (for L1-settling and gateway chains)
    Blobs,
    /// Relayed L2 calldata (for gateway-settling chains)
    RelayedL2Calldata,
}

/// Builder that produces a zksync-os-server config YAML string.
pub struct ServerConfigBuilder {
    // Required
    bridgehub: String,
    bytecodes_supplier: String,
    genesis_path: String,
    chain_id: u64,
    commit_sk: String,
    prove_sk: String,
    execute_sk: String,
    pubdata_mode: PubdataMode,

    // Optional
    ephemeral: bool,
    ephemeral_state: Option<String>,
    gateway_rpc_url: Option<String>,
    gateway_chain_id: Option<u64>,
    status_port: Option<u16>,
    prover_api_port: Option<u16>,
    prometheus_port: Option<u16>,
    extra_forced_prices: Vec<(String, u64)>,
}

// ---------------------------------------------------------------------------
// Serializable config structs
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct ServerConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    general: Option<GeneralSection>,
    genesis: GenesisSection,
    l1_watcher: L1WatcherSection,
    l1_sender: L1SenderSection,
    gateway_sender: GatewaySenderSection,
    gateway_provider: GatewayProviderConfigSection,
    batcher: BatcherSection,
    prover_input_generator: ProverInputGeneratorSection,
    prover_api: ProverApiSection,
    #[serde(skip_serializing_if = "Option::is_none")]
    status_server: Option<AddressSection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    observability: Option<ObservabilitySection>,
    sequencer: SequencerSection,
    external_price_api_client: ExternalPriceSection,
    base_token_price_updater: BaseTokenPriceUpdaterSection,
}

#[derive(Serialize)]
struct GeneralSection {
    #[serde(skip_serializing_if = "Option::is_none")]
    ephemeral: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ephemeral_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    gateway_chain_id: Option<u64>,
}

#[derive(Serialize)]
struct GenesisSection {
    bridgehub_address: String,
    bytecode_supplier_address: String,
    genesis_input_path: String,
    chain_id: u64,
}

#[derive(Serialize)]
struct L1WatcherSection {
    poll_interval: String,
    confirmations: u32,
}

#[derive(Serialize)]
struct L1SenderSection {
    pubdata_mode: String,
    poll_interval: String,
    #[serde(flatten)]
    operator_keys: OperatorKeysSection,
}

#[derive(Clone, Serialize)]
struct OperatorKeysSection {
    operator_commit_sk: String,
    operator_prove_sk: String,
    operator_execute_sk: String,
}

#[derive(Serialize)]
struct GatewaySenderSection {
    poll_interval: String,
    operator_commit_sk: String,
    operator_prove_sk: String,
    operator_execute_sk: String,
}

#[derive(Serialize)]
struct GatewayProviderConfigSection {
    #[serde(skip_serializing_if = "Option::is_none")]
    rpc_url: Option<String>,
    poll_interval: String,
}

#[derive(Serialize)]
struct BatcherSection {
    batch_timeout: String,
}

#[derive(Serialize)]
struct ProverInputGeneratorSection {
    /// Skip RiscV witness generation when fake provers are enabled — the
    /// witness gets thrown away anyway. Saves several seconds per batch on
    /// the commit→prove pipeline cold-start.
    enable_input_generation: bool,
}

/// Fake FRI prover override: turn the artificial pacing knobs to zero so
/// proofs are produced as fast as the pipeline can hand jobs over. The
/// upstream defaults (`compute_time: 2s`, `min_age: 3s`) exist to give real
/// provers a head start on shared environments — irrelevant for tests.
#[derive(Serialize)]
struct FakeFriProverSection {
    enabled: bool,
    compute_time: String,
    min_age: String,
}

/// Fake SNARK prover override: same idea — `max_batch_age: 10s` is the
/// upstream "wait-for-real-prover" knob and adds straight wall time per
/// batch in tests that have no real prover at all.
#[derive(Serialize)]
struct FakeSnarkProverSection {
    enabled: bool,
    max_batch_age: String,
}

#[derive(Serialize)]
struct ProverApiSection {
    #[serde(skip_serializing_if = "Option::is_none")]
    address: Option<String>,
    fake_fri_provers: FakeFriProverSection,
    fake_snark_provers: FakeSnarkProverSection,
}

#[derive(Serialize)]
struct AddressSection {
    address: String,
}

#[derive(Serialize)]
struct ObservabilitySection {
    prometheus: PrometheusSection,
}

#[derive(Serialize)]
struct PrometheusSection {
    port: u16,
}

#[derive(Serialize)]
struct SequencerSection {
    revm_consistency_checker_enabled: bool,
}

#[derive(Serialize)]
struct ExternalPriceSection {
    source: String,
    forced_prices: BTreeMap<String, u64>,
}

#[derive(Serialize)]
struct BaseTokenPriceUpdaterSection {
    fallback_prices: BTreeMap<String, u64>,
}

// ---------------------------------------------------------------------------
// Builder impl
// ---------------------------------------------------------------------------

impl ServerConfigBuilder {
    pub fn new(
        bridgehub: impl Into<String>,
        bytecodes_supplier: impl Into<String>,
        genesis_path: &Path,
        chain_id: u64,
        commit_sk: impl Into<String>,
        prove_sk: impl Into<String>,
        execute_sk: impl Into<String>,
    ) -> Self {
        Self {
            bridgehub: bridgehub.into(),
            bytecodes_supplier: bytecodes_supplier.into(),
            genesis_path: genesis_path.display().to_string(),
            chain_id,
            commit_sk: commit_sk.into(),
            prove_sk: prove_sk.into(),
            execute_sk: execute_sk.into(),
            pubdata_mode: PubdataMode::Blobs,
            ephemeral: false,
            ephemeral_state: None,
            gateway_rpc_url: None,
            gateway_chain_id: None,
            status_port: None,
            prover_api_port: None,
            prometheus_port: None,
            extra_forced_prices: Vec::new(),
        }
    }

    pub fn pubdata_mode(mut self, mode: PubdataMode) -> Self {
        self.pubdata_mode = mode;
        self
    }

    pub fn ephemeral(mut self, state_path: impl Into<String>) -> Self {
        self.ephemeral = true;
        self.ephemeral_state = Some(state_path.into());
        self
    }

    pub fn gateway(mut self, rpc_url: impl Into<String>, chain_id: u64) -> Self {
        self.gateway_rpc_url = Some(rpc_url.into());
        self.gateway_chain_id = Some(chain_id);
        self.pubdata_mode = PubdataMode::RelayedL2Calldata;
        self
    }

    pub fn status_port(mut self, port: u16) -> Self {
        self.status_port = Some(port);
        self
    }

    pub fn prover_api_port(mut self, port: u16) -> Self {
        self.prover_api_port = Some(port);
        self
    }

    pub fn prometheus_port(mut self, port: u16) -> Self {
        self.prometheus_port = Some(port);
        self
    }

    /// Add a forced price for a token address (e.g. ZK base token).
    pub fn forced_price(mut self, token_addr: impl Into<String>, price_usd: u64) -> Self {
        self.extra_forced_prices
            .push((token_addr.into(), price_usd));
        self
    }

    pub fn build(&self) -> String {
        let operator_keys = OperatorKeysSection {
            operator_commit_sk: self.commit_sk.clone(),
            operator_prove_sk: self.prove_sk.clone(),
            operator_execute_sk: self.execute_sk.clone(),
        };

        let general = Some(GeneralSection {
            ephemeral: if self.ephemeral { Some(true) } else { None },
            ephemeral_state: self.ephemeral_state.clone(),
            gateway_chain_id: self.gateway_chain_id,
        });

        let mut forced_prices = BTreeMap::new();
        forced_prices.insert(
            "0x0000000000000000000000000000000000000001".to_string(),
            3000,
        );
        for (addr, price) in &self.extra_forced_prices {
            forced_prices.insert(addr.to_lowercase(), *price);
        }

        let config = ServerConfig {
            general,
            genesis: GenesisSection {
                bridgehub_address: self.bridgehub.clone(),
                bytecode_supplier_address: self.bytecodes_supplier.clone(),
                genesis_input_path: self.genesis_path.clone(),
                chain_id: self.chain_id,
            },
            l1_watcher: L1WatcherSection {
                poll_interval: "100ms".to_string(),
                confirmations: 0,
            },
            l1_sender: L1SenderSection {
                pubdata_mode: match self.pubdata_mode {
                    PubdataMode::Blobs => "Blobs".to_string(),
                    PubdataMode::RelayedL2Calldata => "RelayedL2Calldata".to_string(),
                },
                poll_interval: "100ms".to_string(),
                operator_keys: operator_keys.clone(),
            },
            gateway_sender: GatewaySenderSection {
                poll_interval: "100ms".to_string(),
                operator_commit_sk: self.commit_sk.clone(),
                operator_prove_sk: self.prove_sk.clone(),
                operator_execute_sk: self.execute_sk.clone(),
            },
            gateway_provider: GatewayProviderConfigSection {
                rpc_url: self.gateway_rpc_url.clone(),
                poll_interval: "100ms".to_string(),
            },
            batcher: BatcherSection {
                batch_timeout: "1s".to_string(),
            },
            prover_input_generator: ProverInputGeneratorSection {
                enable_input_generation: false,
            },
            prover_api: ProverApiSection {
                address: self.prover_api_port.map(|p| format!("0.0.0.0:{p}")),
                fake_fri_provers: FakeFriProverSection {
                    enabled: true,
                    compute_time: "0ms".to_string(),
                    min_age: "0ms".to_string(),
                },
                fake_snark_provers: FakeSnarkProverSection {
                    enabled: true,
                    max_batch_age: "0ms".to_string(),
                },
            },
            status_server: self.status_port.map(|p| AddressSection {
                address: format!("0.0.0.0:{p}"),
            }),
            observability: self.prometheus_port.map(|p| ObservabilitySection {
                prometheus: PrometheusSection { port: p },
            }),
            sequencer: SequencerSection {
                revm_consistency_checker_enabled: false,
            },
            external_price_api_client: ExternalPriceSection {
                source: "Forced".to_string(),
                forced_prices: forced_prices.clone(),
            },
            base_token_price_updater: BaseTokenPriceUpdaterSection {
                fallback_prices: forced_prices,
            },
        };

        serde_yaml::to_string(&config).expect("failed to serialize server config")
    }
}
