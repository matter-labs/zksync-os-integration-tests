use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::chain::Chain;
use crate::server_runtime::ChainRuntime;
use crate::workdir::WorkDir;
use alloy::node_bindings::AnvilInstance;
use alloy::primitives::Address;
use alloy::signers::local::PrivateKeySigner;
use anyhow::{Context, Result};
use lib_server::{load_config_from_yaml, Server};

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
    chains: Vec<Chain>,
    /// Kept so chains can be added to a running ecosystem (see [`Ecosystem::add_chain`]).
    specs: Vec<ChainSpec>,
    l1_rpc: String,

    // Drop order matters: servers first, then Anvil, then workdir.
    servers: Vec<Server>,
    _anvil: AnvilInstance,
    workdir: Arc<WorkDir>,
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
        for spec in &specs {
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
                spec.wallets.clone(),
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
            specs,
            l1_rpc,
            servers,
            _anvil: anvil,
            workdir,
        })
    }

    /// Bring up one more chain in this running ecosystem: start a server for the
    /// already-registered `chain_id` and hand back its [`Chain`] handle.
    ///
    /// The chain must already exist on L1 (registered and initialized) — this only adds the node
    /// side, which is what a test that grows an ecosystem after a protocol upgrade needs.
    pub async fn add_chain(
        &mut self,
        chain_id: u64,
        bridgehub: Address,
        server_config: PathBuf,
        genesis_path: PathBuf,
        wallets: Vec<PrivateKeySigner>,
    ) -> Result<()> {
        let runtime = ChainRuntime::allocate(self.workdir(), &chain_id.to_string());
        let spec = ChainSpec {
            chain_id,
            bridgehub,
            config_paths: vec![server_config],
            runtime,
            genesis_path,
            wallets,
        };

        let mut config = load_config_from_yaml(&spec.config_paths).await;
        spec.runtime
            .apply_to(&mut config, &self.l1_rpc, &spec.genesis_path);
        let server = Server::start(config)
            .await
            .with_context(|| format!("start server for chain {chain_id}"))?;

        self.chains.push(Chain::new(
            spec.chain_id,
            spec.bridgehub,
            self.l1_rpc.clone(),
            spec.runtime.l2_rpc_url(),
            spec.wallets.clone(),
        ));
        self.servers.push(server);
        self.specs.push(spec);
        Ok(())
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
        self.workdir.path()
    }
}
