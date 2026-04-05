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
    batcher: BatcherSection,
    prover_api: ProverApiSection,
    #[serde(skip_serializing_if = "Option::is_none")]
    status_server: Option<AddressSection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    observability: Option<ObservabilitySection>,
    sequencer: SequencerSection,
    external_price_api_client: ExternalPriceSection,
}

#[derive(Serialize)]
struct GeneralSection {
    #[serde(skip_serializing_if = "Option::is_none")]
    ephemeral: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ephemeral_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    gateway_rpc_url: Option<String>,
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
    operator_commit_sk: String,
    operator_prove_sk: String,
    operator_execute_sk: String,
}

#[derive(Serialize)]
struct BatcherSection {
    batch_timeout: String,
}

#[derive(Serialize)]
struct FakeProverSection {
    enabled: bool,
}

#[derive(Serialize)]
struct ProverApiSection {
    #[serde(skip_serializing_if = "Option::is_none")]
    address: Option<String>,
    fake_fri_provers: FakeProverSection,
    fake_snark_provers: FakeProverSection,
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

    pub fn build(&self) -> String {
        let general = if self.ephemeral || self.gateway_rpc_url.is_some() {
            Some(GeneralSection {
                ephemeral: if self.ephemeral { Some(true) } else { None },
                ephemeral_state: self.ephemeral_state.clone(),
                gateway_rpc_url: self.gateway_rpc_url.clone(),
                gateway_chain_id: self.gateway_chain_id,
            })
        } else {
            None
        };

        let mut forced_prices = BTreeMap::new();
        forced_prices.insert(
            "0x0000000000000000000000000000000000000001".to_string(),
            3000,
        );

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
                operator_commit_sk: self.commit_sk.clone(),
                operator_prove_sk: self.prove_sk.clone(),
                operator_execute_sk: self.execute_sk.clone(),
            },
            batcher: BatcherSection {
                batch_timeout: "1s".to_string(),
            },
            prover_api: ProverApiSection {
                address: self.prover_api_port.map(|p| format!("0.0.0.0:{p}")),
                fake_fri_provers: FakeProverSection { enabled: true },
                fake_snark_provers: FakeProverSection { enabled: true },
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
                forced_prices,
            },
        };

        serde_yaml::to_string(&config).expect("failed to serialize server config")
    }
}
