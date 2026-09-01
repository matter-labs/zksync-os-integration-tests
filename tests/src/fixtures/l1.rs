use std::sync::Arc;

use rstest::fixture;
use zk_deployer::commands::apply::ApplyArgs;
use zk_deployer::commands::bootstrap::BootstrapArgs;
use zk_deployer::deployed::DeployedEcosystem;
use zk_deployer::intent::{ChainIntent, DaMode, IntentConfig, ValidiumDa, WalletsIntent};

use crate::chain::WALLET_KEYS;
use crate::ecosystem::{ChainSpec, Ecosystem};
use crate::server_runtime::ChainRuntime;
use crate::workdir::WorkDir;
use zk_deployer::anvil::{default_builder, save_state, spawn, spawn_from_file};

use super::cache;

/// Per-chain deployment shape for the [`ecosystem`] fixture.
#[derive(Debug, Clone)]
pub struct ChainDef {
    pub chain_id: u64,
    pub da_mode: DaMode,
}

impl ChainDef {
    pub fn rollup(chain_id: u64) -> Self {
        Self {
            chain_id,
            da_mode: DaMode::Rollup,
        }
    }

    /// Validium (LOGS_ONLY pubdata) posting via the chosen DA — see
    /// [`ValidiumDa`] for the flavors and their on-chain pricing-mode
    /// consequences.
    pub fn validium(chain_id: u64, da: ValidiumDa) -> Self {
        Self {
            chain_id,
            da_mode: DaMode::Validium(da),
        }
    }
}

/// Full setup for N L1-settling chains on one Anvil L1: bootstrap + apply
/// (which funds every [`WALLET_KEYS`] wallet on each chain via L1→L2 deposits)
/// + one server per chain.
///
/// The deployment half (everything `apply` does, including the deposits) is
/// cached on disk under `.zkos-test-cache/` — see [`cache`] for the key
/// inputs and the `ZKOS_CACHE` knob. Servers always start fresh.
pub(super) async fn setup_l1_chains(chains: &[ChainDef]) -> Ecosystem {
    assert!(!chains.is_empty(), "need at least one chain");
    super::init_logging();

    let workdir = Arc::new(WorkDir::new().expect("create workdir"));
    let ecosystem_dir = workdir.path().join("ecosystem");
    std::fs::create_dir_all(&ecosystem_dir).expect("create ecosystem dir");
    let l1_state_path = ecosystem_dir.join("l1-state.json");

    // ── Intent + cache key (l1_rpc_url is filled in on the miss path) ───────
    let mut intent = IntentConfig {
        schema_version: 1,
        l1_rpc_url: None,
        wallets: WalletsIntent {
            ecosystem_seed: Some("test-ecosystem".to_string()),
            path: None,
        },
        chains: chains
            .iter()
            .map(|def| ChainIntent {
                chain_id: def.chain_id,
                base_token: None,
                da_mode: def.da_mode.clone(),
            })
            .collect(),
    };
    let mode = cache::CacheMode::from_env();
    let components = cache::KeyComponents::compute(&intent)
        .await
        .expect("compute cache key");
    let key = components.key();

    // `l1_rpc` is only needed on the miss path (it's written into the intent so
    // bootstrap/apply target this Anvil); `assemble` derives its own endpoint
    // from `anvil` for server startup.
    let (anvil, deployed);
    if let Some(hit) = cache::lookup(mode, &key) {
        // ── Cache hit: restore the deployment snapshot ───────────────────────
        // Deposits are already mined into the restored L1 state; the cached
        // intent.yaml carries the previous run's (dead) l1_rpc_url, which
        // nothing reads post-deployment.
        eprintln!("[tests] deployment cache hit ({key})");
        hit.restore_into(&ecosystem_dir).expect("restore snapshot");
        anvil = spawn_from_file(default_builder(), &l1_state_path)
            .await
            .expect("spawn anvil from cached state");
        deployed = DeployedEcosystem::load(&ecosystem_dir).expect("load deployed ecosystem");
    } else {
        // ── Cache miss: full bootstrap + apply + deposits, then snapshot ─────
        eprintln!("[tests] deployment cache miss ({key}) — running full setup");
        super::ensure_contracts_built().await;

        anvil = spawn(default_builder()).await.expect("spawn anvil");
        let l1_rpc = anvil.endpoint();
        intent.l1_rpc_url = Some(l1_rpc.clone());
        let intent_yaml = serde_yaml::to_string(&intent).expect("serialize intent");
        std::fs::write(ecosystem_dir.join("intent.yaml"), &intent_yaml).expect("write intent.yaml");

        // All forge script IO is scoped under a per-run subdir inside the
        // contracts checkout (script-config/<subdir>/ etc.), so concurrent
        // setups don't collide.
        let subdir = workdir
            .path()
            .file_name()
            .and_then(|n| n.to_str())
            .expect("tempdir basename")
            .trim_start_matches('.')
            .to_string();

        zk_deployer::commands::bootstrap::run(BootstrapArgs {
            intent: ecosystem_dir.join("intent.yaml"),
            state: ecosystem_dir.join("state.json"),
            out: ecosystem_dir.join("out"),
            private_key: super::DEPLOYER_KEY.parse().expect("deployer key"),
            wallets_out: ecosystem_dir.join("wallets.yaml"),
            genesis_out: ecosystem_dir.join("genesis.json"),
            broadcast: true,
            l1_state: l1_state_path.clone(),
            subdir: Some(subdir.clone()),
        })
        .await
        .expect("bootstrap");

        zk_deployer::commands::apply::run(ApplyArgs {
            intent: ecosystem_dir.join("intent.yaml"),
            state: ecosystem_dir.join("state.json"),
            wallets: ecosystem_dir.join("wallets.yaml"),
            out: ecosystem_dir.join("out"),
            private_key: super::DEPLOYER_KEY.parse().expect("deployer key"),
            broadcast: true,
            l1_state: l1_state_path.clone(),
            subdir: Some(subdir),
            no_fund_l2: false,
        })
        .await
        .expect("apply");

        deployed = DeployedEcosystem::load(&ecosystem_dir).expect("load deployed ecosystem");

        // `apply` has already queued the L1→L2 deposits that fund every
        // [`WALLET_KEYS`] wallet on each chain (priority txs, mined into batch 1
        // once each server starts below).

        // Snapshot the deployment (anvil state incl. deposits + workdir).
        save_state(&anvil, &l1_state_path)
            .await
            .expect("dump anvil state");
        if let Err(e) = cache::save(mode, &key, &components, &ecosystem_dir) {
            eprintln!("[tests] failed to save deployment cache ({key}): {e:#}");
        }
    }

    let genesis_path = ecosystem_dir.join("genesis.json");

    let mut specs: Vec<ChainSpec> = Vec::with_capacity(chains.len());

    for &ChainDef { chain_id, .. } in chains {
        let rt = ChainRuntime::allocate(workdir.path(), &chain_id.to_string());
        // Deployment slice only — ports/paths/L1 URL are applied onto the typed
        // Config by `rt` during `Ecosystem::assemble`.
        let chain_yaml = deployed
            .server_config_yaml(chain_id)
            .unwrap_or_else(|e| panic!("render server config for chain {chain_id}: {e}"));

        let server_dir = workdir.path().join(format!("server-{chain_id}"));
        std::fs::create_dir_all(&server_dir).expect("create server dir");
        let chain_yaml_path = server_dir.join("server.yaml");
        std::fs::write(&chain_yaml_path, &chain_yaml).expect("write server yaml");

        specs.push(ChainSpec {
            chain_id,
            bridgehub: deployed.bridgehub,
            config_paths: vec![chain_yaml_path],
            runtime: rt,
            genesis_path: genesis_path.clone(),
            wallets: WALLET_KEYS
                .iter()
                .enumerate()
                .map(|(i, k)| k.parse().unwrap_or_else(|_| panic!("wallet {i}")))
                .collect(),
        });
    }

    let eco = Ecosystem::assemble(anvil, workdir, specs)
        .await
        .expect("assemble ecosystem");

    // ── Wait for batch 1 on every chain (deposits processed) ────────────────
    // wait_for_batch() reads current=finalized_block() then waits for current+1,
    // which races: while waiting on chain A, chain B may already advance past its
    // batch 1, causing wait_for_batch() on B to wait for a batch that never comes.
    // wait_for_block_finalized(1) is idempotent (returns immediately if already >= 1).
    futures::future::try_join_all(eco.chains().map(|chain| chain.wait_for_block_finalized(1)))
        .await
        .expect("wait for batch 1 on all chains");

    eco
}

/// N L1-settling chains on one Anvil L1, each fully bootstrapped with three
/// pre-funded wallets.
///
/// **Default**: a single rollup chain with ID 6565. Override with
/// `#[with(vec![...])]` to choose how many chains, their IDs, and their shape
/// (see [`ChainDef`]):
///
/// ```ignore
/// #[rstest]
/// #[tokio::test(flavor = "multi_thread")]
/// async fn two_chains(
///     #[future]
///     #[with(vec![ChainDef::rollup(6565), ChainDef::validium(6566, ValidiumDa::Blobs)])]
///     ecosystem: Ecosystem,
/// ) -> anyhow::Result<()> {
///     let eco = ecosystem.await;
///     for chain in eco.chains() {
///         let hash = chain.ping().await?;
///         chain.wait_for_tx_finalized(hash).await?;
///     }
///     Ok(())
/// }
/// ```
///
/// Batch 1 is already finalized on every chain when the fixture returns —
/// wallets are funded and the chains are ready for test operations.
#[fixture]
pub async fn ecosystem(
    #[default(vec![ChainDef::rollup(super::TEST_CHAIN_ID)])] chains: Vec<ChainDef>,
) -> Ecosystem {
    setup_l1_chains(&chains).await
}
