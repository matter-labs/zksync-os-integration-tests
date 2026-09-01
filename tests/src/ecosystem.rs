use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::activity::{max_activity_chains, ActivityConfig, ACTIVITY_WALLET_KEYS};
use crate::chain::Chain;
use crate::server_runtime::ChainRuntime;
use crate::workdir::WorkDir;
use alloy::node_bindings::AnvilInstance;
use alloy::primitives::Address;
use alloy::signers::local::PrivateKeySigner;
use anyhow::{Context, Result};
use lib_server::{load_config_from_yaml, Server};

/// How long [`Ecosystem::start_activity`] waits for each enabled flow
/// to produce its first successful tick before giving up. Generous: the first
/// L1→L2 deposit needs an L1 tx plus its receipt, and tests run several servers
/// on one machine.
const ACTIVITY_WARMUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// A fully-resolved description of one chain to bring up: its identity, the
/// deployment-slice config layers to load (`[base, deployment.yaml]`), the
/// allocated [`ChainRuntime`] whose ports/paths are applied onto the loaded
/// `Config`, the genesis input path, and its test wallets (empty when the
/// snapshot funds none — e.g. the v30 fixture, driven through L1 admin keys).
pub(crate) struct ChainSpec {
    pub(crate) chain_id: u64,
    pub(crate) bridgehub: Address,
    pub(crate) config_paths: Vec<PathBuf>,
    pub(crate) runtime: ChainRuntime,
    pub(crate) genesis_path: PathBuf,
    pub(crate) wallets: Vec<PrivateKeySigner>,
}

/// A running ZKsync OS ecosystem: one Anvil L1 plus one or more L1-settling
/// chains, each with its own in-process server.
///
/// Owns the full process lifecycle (Anvil, servers, workdir). Chain handles
/// are pure RPC/wallet references — they do not own any processes.
///
/// Obtain via the `ecosystem` fixture (fresh deploy) or `fixtures::restore`
/// (committed snapshot).
pub struct Ecosystem {
    // Drop order matters (Rust drops fields top-to-bottom): chains first — each
    // owns its background-activity tasks, which must abort before the servers
    // they talk to — then servers, then Anvil, then workdir.
    chains: Vec<Chain>,
    _servers: Vec<Server>,
    _anvil: AnvilInstance,
    _workdir: Arc<WorkDir>,
}

impl Ecosystem {
    /// Bring up one server per spec, build the `Chain` handles, and take
    /// ownership of the processes. Branchless: both the deploy path and the
    /// restore path funnel through here once their specs are resolved.
    pub(crate) async fn assemble(
        anvil: AnvilInstance,
        workdir: Arc<WorkDir>,
        specs: Vec<ChainSpec>,
    ) -> Result<Ecosystem> {
        let l1_rpc = anvil.endpoint();
        let mut chains = Vec::with_capacity(specs.len());
        let mut servers = Vec::with_capacity(specs.len());
        for (i, spec) in specs.into_iter().enumerate() {
            // Load the deployment-slice layers into the typed Config, then apply
            // this run's runtime values (ports/paths/L1 URL/genesis) on top.
            let mut config = load_config_from_yaml(&spec.config_paths).await;
            spec.runtime
                .apply_to(&mut config, &l1_rpc, &spec.genesis_path);
            let server = Server::start(config)
                .await
                .with_context(|| format!("start server for chain {}", spec.chain_id))?;
            chains.push(Chain::new(
                spec.chain_id,
                spec.bridgehub,
                l1_rpc.clone(),
                spec.runtime.l2_rpc_url(),
                spec.wallets,
                i,
            ));
            servers.push(server);
        }
        assert_eq!(
            chains.len(),
            servers.len(),
            "chains and servers must be in 1:1 correspondence"
        );
        Ok(Self {
            chains,
            _servers: servers,
            _anvil: anvil,
            _workdir: workdir,
        })
    }

    // ── Accessors ─────────────────────────────────────────────────────────────

    /// The first chain (the single chain in one-chain ecosystems).
    pub fn chain(&self) -> &Chain {
        &self.chains[0]
    }

    /// All chains.
    pub fn chains(&self) -> impl Iterator<Item = &Chain> {
        self.chains.iter()
    }

    /// The workdir backing this ecosystem (forge IO, fixture outputs).
    pub fn workdir(&self) -> &Path {
        self._workdir.path()
    }

    /// Start background activity on every chain in this ecosystem and wait until
    /// each enabled flow has produced its first successful submission. Each chain
    /// owns its run (aborted, no verdict, when the ecosystem drops) — this is the
    /// "noise while a test runs" use case. For an explicit pass/fail verdict,
    /// drive a single chain via [`Chain::start_activity`] +
    /// [`Chain::finish_activity`].
    ///
    /// The warm-up wait matters: a fixture-driven test does not take the verdict
    /// itself, so without it a silently-dead noise loop would let a test pass
    /// against an idle chain. Panics if a flow fails or never ticks within
    /// [`ACTIVITY_WARMUP_TIMEOUT`].
    ///
    /// Panics if there are more chains than the activity-wallet pool can serve
    /// ([`max_activity_chains`]), since each chain needs its own two wallets.
    pub async fn start_activity(&self, config: ActivityConfig) {
        let max_chains = max_activity_chains(ACTIVITY_WALLET_KEYS.len());
        assert!(
            self.chains.len() <= max_chains,
            "cannot run background activity on {} chains — the wallet pool serves at most {}",
            self.chains.len(),
            max_chains
        );

        for chain in &self.chains {
            chain.start_activity(config.clone());
        }

        // Fail fast at fixture time if a flow can't get going.
        let deadline = std::time::Instant::now() + ACTIVITY_WARMUP_TIMEOUT;
        for chain in &self.chains {
            loop {
                assert!(
                    !chain.activity_failed(),
                    "background activity on chain {} failed during warm-up",
                    chain.chain_id()
                );
                let l2_ready =
                    config.l2_transfers.is_none() || chain.activity_transfers_submitted() >= 1;
                let l1_ready =
                    config.l1_deposits.is_none() || chain.activity_deposits_submitted() >= 1;
                if l2_ready && l1_ready {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "background activity on chain {} produced no first tick within {:?} \
                     (transfers={}, deposits={})",
                    chain.chain_id(),
                    ACTIVITY_WARMUP_TIMEOUT,
                    chain.activity_transfers_submitted(),
                    chain.activity_deposits_submitted()
                );
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
}
