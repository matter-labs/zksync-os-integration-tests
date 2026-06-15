//! Per-server runtime resources: listen ports and scratch paths.
//!
//! Every in-process `zksync-os-server` needs four free ports and a set of
//! db/proof/dump directories under the test workdir. [`ChainRuntime`] owns
//! that allocation and applies it directly onto the loaded server [`Config`]
//! via [`ChainRuntime::apply_to`] — typed field assignments checked against the
//! real schema, mirroring zksync-os-server's own integration-test practice.
//! Deployment-fixed values (addresses, operator keys) come from the config
//! file layers; only the per-run runtime values are set here.

use std::path::{Path, PathBuf};
use std::time::Duration;

use alloy::primitives::address;
use lib_server::{Config, ExternalPriceApiClientConfig, ForcedPriceClientConfig};

use crate::locked_port::LockedPort;

pub struct ChainRuntime {
    pub rpc_port: u16,
    pub status_port: u16,
    pub prover_port: u16,
    pub prometheus_port: u16,
    pub db_path: PathBuf,
    pub proof_storage_path: PathBuf,
    pub block_dump_path: PathBuf,
    // Held until the server binds; prevents other test workers from stealing
    // the same port numbers during the allocation-to-bind window.
    _port_locks: [LockedPort; 4],
}

impl ChainRuntime {
    /// Allocate four free ports and derive db paths under `workdir`.
    /// `tag` keeps directories of co-hosted servers apart (use the chain ID).
    pub fn allocate(workdir: &Path, tag: &str) -> Self {
        let db_path = workdir.join(format!("server-{tag}")).join("db");
        let rpc = LockedPort::acquire_unused().expect("rpc port");
        let status = LockedPort::acquire_unused().expect("status port");
        let prover = LockedPort::acquire_unused().expect("prover port");
        let prometheus = LockedPort::acquire_unused().expect("prometheus port");
        Self {
            rpc_port: rpc.port,
            status_port: status.port,
            prover_port: prover.port,
            prometheus_port: prometheus.port,
            proof_storage_path: db_path.join("fri_proofs"),
            block_dump_path: db_path.join("block_dumps"),
            db_path,
            _port_locks: [rpc, status, prover, prometheus],
        }
    }

    pub fn l2_rpc_url(&self) -> String {
        format!("http://localhost:{}", self.rpc_port)
    }

    /// Apply this run's ports, scratch paths, L1 RPC URL and genesis input path
    /// onto a loaded server [`Config`].
    ///
    /// These are typed field assignments, so every key is checked against the
    /// real server config schema at compile time — a renamed server field is a
    /// build error here, not a silently-ignored YAML key at runtime. Runs after
    /// the config-file layers are loaded, so it wins over any committed/rendered
    /// values for these runtime concerns.
    pub fn apply_to(&self, config: &mut Config, l1_rpc: &str, genesis: &Path) {
        config.general_config.rocks_db_path = self.db_path.clone();
        config.l1_provider_config.rpc_url = l1_rpc.to_string();
        config.genesis_config.genesis_input_path = Some(genesis.to_path_buf());
        config.rpc_config.address = format!("0.0.0.0:{}", self.rpc_port);
        config.status_server_config.address = format!("0.0.0.0:{}", self.status_port);
        config.prover_api_config.address = format!("0.0.0.0:{}", self.prover_port);
        config.prover_api_config.proof_storage.path = self.proof_storage_path.clone();
        config.observability_config.prometheus.port = self.prometheus_port;
        config.sequencer_config.block_dump_path = self.block_dump_path.clone();

        // Production defaults are tuned for mainnet; override for local Anvil.
        config.l1_watcher_config.poll_interval = Duration::from_millis(100);
        // Default is 1 minute (mainnet epoch cadence); Anvil finalizes every ~0.5 s.
        config.l1_watcher_config.finalized_poll_interval = Duration::from_secs(1);
        config.l1_watcher_config.confirmations = 0;
        // Default is 7 s (alloy HTTP default); each L1 tx waits this long before
        // checking inclusion — 3 txns × 7 s = 21 s dead sleep per batch.
        config.l1_provider_config.rpc_poll_interval = Duration::from_millis(100);
        config.l1_sender_config.poll_interval = Duration::from_millis(100);
        config.batcher_config.batch_timeout = Duration::from_secs(1);
        config.prover_api_config.fake_fri_provers.enabled = true;
        // Defaults are min_age=3 s + compute_time=2 s; zero both for tests.
        config.prover_api_config.fake_fri_provers.min_age = Duration::ZERO;
        config.prover_api_config.fake_fri_provers.compute_time = Duration::ZERO;
        config.prover_api_config.fake_snark_provers.enabled = true;
        // Default is 10 s.
        config.prover_api_config.fake_snark_provers.max_batch_age = Duration::from_secs(1);
        // Skips the expensive ZK witness computation (the server's own integration
        // tests use the same setting: enable_input_generation=false).
        config.prover_input_generator_config.enable_input_generation = false;
        config
            .sequencer_config
            .revm_consistency_checker_revert_on_divergence = true;
        // Required by the base-token price updater on the Main Node.
        // Address 0x1 = ETH; 3000 USD matches the Anvil local-dev convention.
        config.external_price_api_client_config = Some(ExternalPriceApiClientConfig::Forced {
            forced: ForcedPriceClientConfig {
                prices: [(
                    address!("0000000000000000000000000000000000000001"),
                    3000.0_f64,
                )]
                .into(),
                ..Default::default()
            },
        });
    }
}
