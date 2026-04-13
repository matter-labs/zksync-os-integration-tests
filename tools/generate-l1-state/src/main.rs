//! Standalone tool to generate `l1-state.json` for integration tests.
//!
//! Replicates the L1 setup from `update_server.py` / `protocol_ops_init` test:
//!  1. Build contracts (local only — Docker image has pre-built artifacts)
//!  2. Start Anvil with `--dump-state`
//!  3. Deploy L1 contracts via `protocol_ops ecosystem init`
//!  4. Register gateway chain via `protocol_ops chain init`
//!  5. Register gateway-settling chains (with `--pause-deposits --skip-priority-txs`)
//!  6. Register L1-settling chains
//!  7. Fund all operator accounts on L1
//!  8. Generate genesis.json, per-chain config files, and wallets.yaml
//!  9. Start gateway server
//! 10. Fund gateway L2 (test account + operators)
//! 11. Wait for gateway executed batches
//! 12. Convert gateway chain (forge + protocol_ops)
//! 13. Migrate gateway-settling chains, enable validators, fund operators
//! 14. Submit L1 deposits for L1-settling chains
//! 15. Stop gateway server, archive RocksDB as ephemeral state
//! 16. Stop Anvil (triggers state dump), write ecosystem.yaml + metadata.json
//!
//! Result: 1 gateway chain + N gateway-settling chains + M L1-settling chains.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;

use integration_tests::anvil_utils::fund_account;
use integration_tests::docker::docker_pull_image;
use integration_tests::l1_state::{ChainWallets, WalletsFile};
use integration_tests::presets::RepoRef;
use integration_tests::server::L1DepositBaseToken;
use integration_tests::server_utils::address_from_private_key;

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(
    name = "generate-l1-state",
    about = "Generate l1-state.json for integration tests"
)]
struct Args {
    /// Preset name from presets.yaml
    preset: String,
}

/// Settlement layer a chain commits batches to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SettlesOn {
    L1,
    Gateway,
}

/// Static description of one chain in the ecosystem: chain id, directory /
/// wallets.yaml key, and which layer it settles on. Adding a new chain =
/// adding one row to [`ECOSYSTEM_CHAINS`] (plus matching entries in
/// `wallets.yaml`).
#[derive(Clone, Copy, Debug)]
struct ChainSpec {
    id: u64,
    /// Directory name under the state output and wallets.yaml key.
    name: &'static str,
    settles_on: SettlesOn,
}

/// The gateway chain itself. Broken out of [`ECOSYSTEM_CHAINS`] because it
/// has a distinct role in the generation flow (it runs as a standalone
/// zksync-os-server during generation, every other chain settles either on
/// it or on L1).
const GATEWAY: ChainSpec = ChainSpec {
    id: 506,
    name: "gateway",
    settles_on: SettlesOn::L1,
};

/// Every non-gateway chain produced by this tool.
const ECOSYSTEM_CHAINS: &[ChainSpec] = &[
    ChainSpec {
        id: 6565,
        name: "l1_settling",
        settles_on: SettlesOn::L1,
    },
    ChainSpec {
        id: 6566,
        name: "gateway_settling_a",
        settles_on: SettlesOn::Gateway,
    },
    ChainSpec {
        id: 6567,
        name: "gateway_settling_b",
        settles_on: SettlesOn::Gateway,
    },
];

fn gateway_settling_chains() -> impl Iterator<Item = &'static ChainSpec> {
    ECOSYSTEM_CHAINS
        .iter()
        .filter(|c| c.settles_on == SettlesOn::Gateway)
}

fn l1_settling_chains() -> impl Iterator<Item = &'static ChainSpec> {
    ECOSYSTEM_CHAINS
        .iter()
        .filter(|c| c.settles_on == SettlesOn::L1)
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

use integration_tests::anvil::DEFAULT_ANVIL_PRIVATE_KEY;

/// Deployer + ecosystem owner keys, loaded from wallets.yaml.
struct KeySet {
    deployer_pk: String,
    deployer_addr: String,
    ecosystem_owner_pk: String,
    ecosystem_owner_addr: String,
}

impl KeySet {
    fn from_wallets(wallets: &WalletsFile) -> Result<Self> {
        Ok(Self {
            deployer_pk: wallets.ecosystem.deployer.private_key.clone(),
            deployer_addr: wallets.ecosystem.deployer.address.clone(),
            ecosystem_owner_pk: wallets.ecosystem.owner.private_key.clone(),
            ecosystem_owner_addr: wallets.ecosystem.owner.address.clone(),
        })
    }
}

/// CREATE2 salt for ZK token deployment.
const ERC20_SALT: &str = "0000000000000000000000000000000000000000000000000000000000000001";
/// Deterministic CREATE2 deployer address.
const CREATE2_DEPLOYER: &str = "0x4e59b44847b379578588920ca78fbf26c0b4956c";

/// Anvil-typical L1 gas price (wei) for migration calldata.
const MIGRATE_L1_GAS_PRICE_WEI: u64 = 1_000_000_000;

/// Longer timeout used while the gateway is draining its post-migration
/// priority queue; the queue can take noticeably longer to empty than a
/// normal batch wait.
const PRIORITY_QUEUE_DRAIN_TIMEOUT: Duration = Duration::from_secs(180);

/// Mirrors `L2DACommitmentScheme` on the chain contracts. Only the variants
/// actively used in this tool are listed; add more as validium / custom-DA
/// chains start being generated.
#[derive(Copy, Clone)]
enum L2DaCommitmentScheme {
    BlobsAndPubdataKeccak256 = 3,
}

impl L2DaCommitmentScheme {
    fn as_u8_str(self) -> &'static str {
        match self {
            Self::BlobsAndPubdataKeccak256 => "3",
        }
    }
}

// ---------------------------------------------------------------------------
// Era-contracts execution backend (local binary or Docker session)
// ---------------------------------------------------------------------------

use integration_tests::protocol_ops::EraContractsBackend;

// ---------------------------------------------------------------------------
// Repo path resolution
// ---------------------------------------------------------------------------

/// Load preset from presets.yaml.
fn load_preset(args: &Args) -> Result<integration_tests::presets::Preset> {
    let presets = integration_tests::presets::load_default_presets()
        .context("Failed to load presets.yaml")?;
    let preset = presets
        .get(&args.preset)
        .ok_or_else(|| anyhow::anyhow!("Preset '{}' not found in presets.yaml", args.preset))?
        .clone();
    Ok(preset)
}

/// Get the local era-contracts path if available, or None for Docker presets.
fn era_local_path(preset: &integration_tests::presets::Preset) -> Option<PathBuf> {
    match &preset.era_contracts {
        RepoRef::Path(p) => fs::canonicalize(p).ok(),
        RepoRef::DockerTag { .. } => None,
    }
}

// ---------------------------------------------------------------------------
// Anvil management (with --dump-state)
// ---------------------------------------------------------------------------

struct DumpStateAnvil {
    child: Child,
    rpc_url: String,
}

impl DumpStateAnvil {
    fn spawn(port: u16, dump_state_path: &Path) -> Result<Self> {
        let child = Command::new("anvil")
            .args([
                "--preserve-historical-states",
                "--disable-block-gas-limit",
                "--host",
                "0.0.0.0",
                "--dump-state",
                dump_state_path.to_str().unwrap(),
                "--port",
                &port.to_string(),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("Failed to start anvil")?;

        let rpc_url = format!("http://localhost:{}", port);

        integration_tests::server_utils::wait_for_chain_to_be_ready(
            &rpc_url,
            "Anvil",
            10,
            Duration::from_secs(1),
            None,
        )
        .context("Anvil did not become ready")?;

        Ok(Self { child, rpc_url })
    }

    fn rpc_url(&self) -> &str {
        &self.rpc_url
    }

    /// Gracefully terminate Anvil so it writes the dump-state file.
    fn terminate(mut self) -> Result<()> {
        println!("Stopping Anvil (pid={})...", self.child.id());
        #[cfg(unix)]
        {
            use nix::sys::signal::{self, Signal};
            use nix::unistd::Pid;
            let _ = signal::kill(Pid::from_raw(self.child.id() as i32), Signal::SIGTERM);
        }
        #[cfg(not(unix))]
        {
            let _ = self.child.kill();
        }
        // Poll for exit with timeout
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {
                    if std::time::Instant::now() >= deadline {
                        eprintln!("Anvil did not exit in time, sending SIGKILL");
                        let _ = self.child.kill();
                        let _ = self.child.wait();
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(_) => break,
            }
        }
        Ok(())
    }
}

impl Drop for DumpStateAnvil {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---------------------------------------------------------------------------
// JSON helpers
// ---------------------------------------------------------------------------

fn extract_json_value(obj: &serde_json::Value, path: &str) -> Result<String> {
    let mut v = obj;
    for key in path.split('.') {
        v = v.get(key).ok_or_else(|| {
            anyhow::anyhow!(
                "Missing key {:?} in path {:?}\nAvailable keys: {:?}",
                key,
                path,
                v.as_object().map(|o| o.keys().collect::<Vec<_>>()),
            )
        })?;
    }
    v.as_str()
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("Key {:?} is not a string", path))
}

// ---------------------------------------------------------------------------
// Chain operator context
// ---------------------------------------------------------------------------

struct ChainOperators {
    chain_id: u64,
    /// Human-readable chain name, also used as subdirectory for config files.
    dir_name: String,
    owner_pk: String,
    owner_addr: String,
    commit_pk: String,
    prove_pk: String,
    execute_pk: String,
    commit_addr: String,
    prove_addr: String,
    execute_addr: String,
}

impl ChainOperators {
    fn from_wallets(chain_id: u64, dir_name: &str, w: &ChainWallets) -> Result<Self> {
        Ok(Self {
            chain_id,
            dir_name: dir_name.to_string(),
            owner_pk: w.owner.private_key.clone(),
            owner_addr: w.owner.address.clone(),
            commit_pk: w.commit_operator.private_key.clone(),
            prove_pk: w.prove_operator.private_key.clone(),
            execute_pk: w.execute_operator.private_key.clone(),
            commit_addr: w.commit_operator.address.clone(),
            prove_addr: w.prove_operator.address.clone(),
            execute_addr: w.execute_operator.address.clone(),
        })
    }

    /// All L1 accounts belonging to this chain that need an ETH top-up on L1:
    /// the chain owner (gas for governance/admin txs) plus the three
    /// validator operators (gas for commit/prove/execute txs).
    fn l1_funded_addresses(&self) -> [&str; 4] {
        [
            &self.owner_addr,
            &self.commit_addr,
            &self.prove_addr,
            &self.execute_addr,
        ]
    }
}

// ---------------------------------------------------------------------------
// Forge helpers
// ---------------------------------------------------------------------------

fn run_deploy_gateway_transaction_filterer(
    contracts_backend: &EraContractsBackend,
    l1_rpc_url: &str,
    bridgehub: &str,
    chain_id: u64,
    chain_owner_pk: &str,
) -> Result<()> {
    println!("  gateway convert deploy-filterer for chain {chain_id}");
    contracts_backend
        .protocol_ops(&[
            "chain",
            "gateway",
            "convert",
            "deploy-filterer",
            "--l1-rpc-url",
            l1_rpc_url,
            "--private-key",
            chain_owner_pk,
            "--bridgehub",
            bridgehub,
            "--gateway-chain-id",
            &chain_id.to_string(),
        ])
        .with_context(|| format!("gateway convert deploy-filterer for chain {chain_id}"))?;
    Ok(())
}

#[derive(serde::Deserialize)]
struct VotePrepOutput {
    relayed_sl_da_validator: String,
}

// ---------------------------------------------------------------------------
// protocol_ops wrappers
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn run_convert_to_gateway(
    contracts_backend: &EraContractsBackend,
    l1_rpc_url: &str,
    keys: &KeySet,
    gw_owner_pk: &str,
    bridgehub: &str,
    gateway_chain_id: u64,
    governance_addr: &str,
    stm_tracker: &str,
    ctm_proxy: &str,
    vote_output_path: &str,
) -> Result<()> {
    let gw_str = gateway_chain_id.to_string();

    println!("  gateway convert: grant-whitelist");
    contracts_backend
        .protocol_ops(&[
            "chain",
            "gateway",
            "convert",
            "grant-whitelist",
            "--l1-rpc-url",
            l1_rpc_url,
            "--private-key",
            gw_owner_pk,
            "--bridgehub",
            bridgehub,
            "--gateway-chain-id",
            &gw_str,
            "--whitelist-grantees",
            governance_addr,
            "--whitelist-grantees",
            &keys.deployer_addr,
            "--whitelist-grantees",
            stm_tracker,
        ])
        .context("gateway convert grant-whitelist")?;

    println!("  gateway convert: vote-prepare");
    contracts_backend
        .protocol_ops(&[
            "chain",
            "gateway",
            "convert",
            "vote-prepare",
            "--l1-rpc-url",
            l1_rpc_url,
            "--private-key",
            &keys.deployer_pk,
            "--bridgehub",
            bridgehub,
            "--gateway-chain-id",
            &gw_str,
            "--ctm-representative-chain-id",
            &gw_str,
            "--ctm-proxy",
            ctm_proxy,
            "--refund-recipient",
            &keys.deployer_addr,
            "--testnet-verifier",
            "--zksync-os",
            "--vote-preparation-toml",
            vote_output_path,
        ])
        .context("gateway convert vote-prepare")?;

    println!("  gateway convert: governance-execute");
    contracts_backend
        .protocol_ops(&[
            "chain",
            "gateway",
            "convert",
            "governance-execute",
            "--l1-rpc-url",
            l1_rpc_url,
            "--private-key",
            &keys.ecosystem_owner_pk,
            "--bridgehub",
            bridgehub,
            "--gateway-chain-id",
            &gw_str,
            "--governance-address",
            governance_addr,
            "--vote-preparation-toml",
            vote_output_path,
        ])
        .context("gateway convert governance-execute")?;

    println!("  gateway convert: revoke-whitelist");
    contracts_backend
        .protocol_ops(&[
            "chain",
            "gateway",
            "convert",
            "revoke-whitelist",
            "--l1-rpc-url",
            l1_rpc_url,
            "--private-key",
            gw_owner_pk,
            "--bridgehub",
            bridgehub,
            "--gateway-chain-id",
            &gw_str,
            "--revoke-address",
            &keys.deployer_addr,
        ])
        .context("gateway convert revoke-whitelist")?;

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_migrate_to_gateway(
    contracts_backend: &EraContractsBackend,
    l1_rpc_url: &str,
    chain_owner_pk: &str,
    bridgehub: &str,
    chain_id: u64,
    gateway_chain_id: u64,
    vote_output_rel: &str,
    refund_recipient: &str,
    deposits_already_paused: bool,
) -> Result<()> {
    let chain_str = chain_id.to_string();
    let gw_str = gateway_chain_id.to_string();
    let gas_str = MIGRATE_L1_GAS_PRICE_WEI.to_string();

    if !deposits_already_paused {
        println!("  gateway migrate chain {chain_id}: pause-deposits");
        contracts_backend
            .protocol_ops(&[
                "chain",
                "gateway",
                "migrate",
                "pause-deposits",
                "--l1-rpc-url",
                l1_rpc_url,
                "--private-key",
                chain_owner_pk,
                "--bridgehub",
                bridgehub,
                "--chain-id",
                &chain_str,
            ])
            .context("gateway migrate pause-deposits")?;
    }

    // Production order: notify the server before submitting the migration
    // so listeners see the event pre-migration. This is inert here (no L2
    // server is listening during state generation) but keeps the sequence
    // faithful to the real flow.
    println!("  gateway migrate chain {chain_id}: notify-server");
    contracts_backend
        .protocol_ops(&[
            "chain",
            "gateway",
            "migrate",
            "notify-server",
            "--l1-rpc-url",
            l1_rpc_url,
            "--private-key",
            chain_owner_pk,
            "--bridgehub",
            bridgehub,
            "--chain-id",
            &chain_str,
        ])
        .context("gateway migrate notify-server")?;

    println!("  gateway migrate chain {chain_id}: submit");
    contracts_backend
        .protocol_ops(&[
            "chain",
            "gateway",
            "migrate",
            "submit",
            "--l1-rpc-url",
            l1_rpc_url,
            "--private-key",
            chain_owner_pk,
            "--bridgehub",
            bridgehub,
            "--chain-id",
            &chain_str,
            "--gateway-chain-id",
            &gw_str,
            "--l1-gas-price",
            &gas_str,
            "--vote-preparation-toml",
            vote_output_rel,
            "--refund-recipient",
            refund_recipient,
        ])
        .context("gateway migrate submit")?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Genesis generation
// ---------------------------------------------------------------------------

/// Build a Rust tool from the era-contracts tree (local mode only, no-op for Docker).
fn build_contracts_tool(
    contracts_backend: &EraContractsBackend,
    tool_subdir: &str,
) -> Result<Option<PathBuf>> {
    let era_path = match contracts_backend.era_path() {
        Some(p) => p,
        None => return Ok(None), // Docker: pre-built in image
    };
    let tool_dir = era_path.join(tool_subdir);
    let manifest = tool_dir.join("Cargo.toml");
    anyhow::ensure!(
        manifest.exists(),
        "{} not found at {}",
        tool_subdir,
        manifest.display()
    );
    let mut build_cmd = Command::new("cargo");
    build_cmd
        .args([
            "build",
            "--release",
            "--manifest-path",
            manifest.to_str().unwrap(),
        ])
        .current_dir(&tool_dir);
    if let Some(toolchain) = integration_tests::server::read_toolchain_from_dir(&tool_dir) {
        build_cmd.env("RUSTUP_TOOLCHAIN", &toolchain);
    }
    let status = build_cmd
        .status()
        .with_context(|| format!("cargo build {}", tool_subdir))?;
    if !status.success() {
        anyhow::bail!("cargo build {} failed with status: {}", tool_subdir, status);
    }
    let bin_name = tool_dir.file_name().unwrap().to_string_lossy().to_string();
    let binary = tool_dir
        .join("target/release")
        .join(&bin_name)
        .with_extension(std::env::consts::EXE_EXTENSION);
    Ok(Some(binary))
}

/// Generate genesis.json into `work_dir/genesis.json`.
fn run_genesis_gen(contracts_backend: &EraContractsBackend) -> Result<PathBuf> {
    println!("Generating genesis.json...");
    let filename = "genesis.json";

    let local_binary = build_contracts_tool(contracts_backend, "tools/zksync-os-genesis-gen")?;
    let cmd_name = local_binary
        .as_ref()
        .map(|b| b.to_string_lossy().to_string())
        .unwrap_or_else(|| "zksync-os-genesis-gen".to_string());

    // The tool resolves ../../configs/genesis/ relative to cwd, so we
    // must run from tools/zksync-os-genesis-gen in both modes.
    let workdir = contracts_backend.repo_path("tools/zksync-os-genesis-gen");
    let output_arg = contracts_backend.work_path(filename);

    contracts_backend
        .run(&[&cmd_name, "--output-file", &output_arg], Some(&workdir))
        .context("zksync-os-genesis-gen")?;

    let result = contracts_backend.work_dir().join(filename);
    println!("  genesis.json -> {}", result.display());
    Ok(result)
}

/// Generate wallets.yaml into `work_dir/wallets.yaml`.
fn run_wallets_gen(contracts_backend: &EraContractsBackend, chains_arg: &str) -> Result<PathBuf> {
    let filename = "wallets.yaml";

    let local_binary = build_contracts_tool(contracts_backend, "tools/wallets-gen")?;

    let output_arg = contracts_backend.work_path(filename);
    let cmd_name = local_binary
        .as_ref()
        .map(|b| b.to_string_lossy().to_string())
        .unwrap_or_else(|| "wallets-gen".to_string());

    contracts_backend
        .run(
            &[&cmd_name, "--chains", chains_arg, "--output", &output_arg],
            None,
        )
        .context("wallets-gen")?;

    Ok(contracts_backend.work_dir().join(filename))
}

// ---------------------------------------------------------------------------
// ZK token deployment
// ---------------------------------------------------------------------------

/// Deploy a TestnetERC20Token ("ZK") via CREATE2, mint to deployer and
/// governance. Returns the token address.
fn deploy_zk_token(
    contracts_backend: &EraContractsBackend,
    l1_rpc_url: &str,
    keys: &KeySet,
    governance_addr: &str,
) -> Result<String> {
    println!("\n=== Deploying ZK token ===");

    let erc20_bytecode = contracts_backend
        .forge(&[
            "inspect",
            "contracts/dev-contracts/TestnetERC20Token.sol:TestnetERC20Token",
            "bytecode",
        ])
        .context("forge inspect ERC20 bytecode")?;

    let constructor_args = contracts_backend
        .cast(&[
            "abi-encode",
            "constructor(string,string,uint8)",
            "ZK",
            "ZK",
            "18",
        ])
        .context("abi-encode ERC20 constructor")?;
    let constructor_args = constructor_args.trim();

    let deploy_data = format!(
        "0x{}{}{}",
        ERC20_SALT,
        &erc20_bytecode[2..],
        &constructor_args[2..]
    );
    contracts_backend
        .cast(&[
            "send",
            CREATE2_DEPLOYER,
            &deploy_data,
            "--private-key",
            &keys.deployer_pk,
            "--rpc-url",
            l1_rpc_url,
        ])
        .context("CREATE2 deploy ERC20")?;

    let init_code = format!("0x{}{}", &erc20_bytecode[2..], &constructor_args[2..]);
    let zk_token_address = contracts_backend
        .cast(&[
            "create2",
            &format!("--salt=0x{}", ERC20_SALT),
            &format!("--init-code={}", init_code),
            &format!("--deployer={}", CREATE2_DEPLOYER),
        ])
        .context("compute CREATE2 address")?;
    let zk_token_address = zk_token_address.trim().to_string();
    println!("  ZK token: {zk_token_address}");

    let mint_amount = "1000000000000000000000000000000000000000";
    for (name, addr) in [
        ("ecosystem_owner", keys.ecosystem_owner_addr.as_str()),
        ("deployer", keys.deployer_addr.as_str()),
        ("governance", governance_addr),
    ] {
        contracts_backend
            .cast(&[
                "send",
                &zk_token_address,
                "mint(address,uint256)",
                addr,
                mint_amount,
                "--private-key",
                &keys.deployer_pk,
                "--rpc-url",
                l1_rpc_url,
            ])
            .with_context(|| format!("mint ZK to {name}"))?;
    }

    Ok(zk_token_address)
}

// ---------------------------------------------------------------------------
// Chain init variants
// ---------------------------------------------------------------------------

enum ChainInitKind<'a> {
    /// Gateway chain: ZK-based base token, no deposit pause.
    Gateway { base_token_addr: &'a str },
    /// Chain that will later migrate to settle on the gateway.
    /// `--pause-deposits + --skip-priority-txs` keep the priority queue empty
    /// so migrate-to-gateway doesn't fail with PriorityQueueNotFullyProcessed,
    /// avoiding the need for a pre-migration server to drain the queue.
    GatewaySettling,
    /// Plain chain that settles on L1. ETH base token, deposits stay live.
    L1Settling,
}

#[allow(clippy::too_many_arguments)]
fn run_chain_init(
    contracts_backend: &EraContractsBackend,
    l1_rpc_url: &str,
    keys: &KeySet,
    ops: &ChainOperators,
    ctm_proxy: &str,
    l1_da_validator: &str,
    create2_factory: &str,
    kind: ChainInitKind<'_>,
    out_filename: &str,
) -> Result<()> {
    let chain_id = ops.chain_id.to_string();
    let out_arg = contracts_backend.work_path(out_filename);

    let mut args: Vec<&str> = vec![
        "chain",
        "init",
        "--ctm-proxy",
        ctm_proxy,
        "--l1-da-validator",
        l1_da_validator,
        "--owner",
        &ops.owner_addr,
        "--commit-operator",
        &ops.commit_addr,
        "--prove-operator",
        &ops.prove_addr,
        "--execute-operator",
        &ops.execute_addr,
        "--chain-id",
        &chain_id,
        "--vm-type",
        "zksyncos",
        "--l1-rpc-url",
        l1_rpc_url,
        "--private-key",
        &keys.deployer_pk,
        "--owner-pk",
        &ops.owner_pk,
        "--bridgehub-admin-pk",
        &keys.ecosystem_owner_pk,
        "--create2-factory-addr",
        create2_factory,
    ];

    match &kind {
        ChainInitKind::Gateway { base_token_addr } => {
            args.extend_from_slice(&["--base-token-addr", base_token_addr]);
        }
        ChainInitKind::GatewaySettling => {
            args.extend_from_slice(&["--pause-deposits", "--skip-priority-txs"]);
        }
        ChainInitKind::L1Settling => {}
    }

    args.extend_from_slice(&["--out", &out_arg]);

    contracts_backend.protocol_ops(&args)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Generation flow (Steps 3–15)
// ---------------------------------------------------------------------------

struct FlowResult {
    bridgehub: String,
    bytecodes_supplier: String,
    gw_diamond_proxy: String,
    gw_settling_diamond_proxies: Vec<String>,
    l1_settling_diamond_proxies: Vec<String>,
    gateway_ephemeral_state: PathBuf,
}

#[allow(clippy::too_many_arguments)]
async fn run_generation_flow(
    contracts_backend: &EraContractsBackend,
    l1_rpc_url: &str,
    keys: &KeySet,
    gw_ops: &ChainOperators,
    gw_settling_ops: &[ChainOperators],
    l1_settling_ops: &[ChainOperators],
    output_dir: &Path,
    output_path: &Path,
    preset: &integration_tests::presets::Preset,
    anvil_port: u16,
) -> Result<FlowResult> {
    // ----------------------------------------------------------------
    // Step 3a: Fund all L1 accounts used by the flow
    //   - Deployer / ecosystem owner (4000 ETH each, from Anvil default)
    //   - Per-chain owners + validator operators (100 ETH each, from deployer)
    // Validators don't spend L1 gas until after chain init, but we fund
    // them here so there is a single funding pass rather than one for
    // owners at the start and another for validators later.
    // ----------------------------------------------------------------
    println!("\n=== Funding deployer and ecosystem owner ===");
    fund_account(
        &keys.deployer_addr,
        "4000ether",
        l1_rpc_url,
        DEFAULT_ANVIL_PRIVATE_KEY,
    )
    .context("fund deployer")?;
    fund_account(
        &keys.ecosystem_owner_addr,
        "4000ether",
        l1_rpc_url,
        DEFAULT_ANVIL_PRIVATE_KEY,
    )
    .context("fund ecosystem owner")?;
    println!("\n=== Funding L1 owner + operator accounts ===");
    for ops in std::iter::once(gw_ops)
        .chain(gw_settling_ops.iter())
        .chain(l1_settling_ops.iter())
    {
        for addr in ops.l1_funded_addresses() {
            fund_account(addr, "100ether", l1_rpc_url, &keys.deployer_pk)
                .with_context(|| format!("fund L1 account {addr} for chain {}", ops.chain_id))?;
        }
    }

    // ----------------------------------------------------------------
    // Step 3: Ecosystem init
    // ----------------------------------------------------------------
    println!("\n=== protocol_ops ecosystem init ===");
    let ecosystem_out_arg = contracts_backend.work_path("ecosystem_init_out.json");
    contracts_backend.protocol_ops(&[
        "ecosystem",
        "init",
        "--owner",
        &keys.ecosystem_owner_addr,
        "--private-key",
        &keys.deployer_pk,
        "--owner-pk",
        &keys.ecosystem_owner_pk,
        "--l1-rpc-url",
        l1_rpc_url,
        "--out",
        &ecosystem_out_arg,
    ])?;

    let ecosystem_json: serde_json::Value = serde_json::from_str(
        &contracts_backend.read_protocol_ops_output("ecosystem_init_out.json")?,
    )?;
    let output = ecosystem_json
        .get("output")
        .ok_or_else(|| anyhow::anyhow!("Missing 'output' in ecosystem init JSON"))?;

    let bridgehub = extract_json_value(
        output,
        "hub.deployed_addresses.bridgehub.bridgehub_proxy_addr",
    )?;
    let ctm_proxy = extract_json_value(
        output,
        "ctm.deployed_addresses.state_transition.state_transition_proxy_addr",
    )?;
    let l1_da_validator = extract_json_value(
        output,
        "ctm.deployed_addresses.blobs_zksync_os_l1_da_validator_addr",
    )
    .or_else(|_| {
        extract_json_value(output, "ctm.deployed_addresses.rollup_l1_da_validator_addr")
    })?;
    let bytecodes_supplier = extract_json_value(
        output,
        "ctm.deployed_addresses.state_transition.bytecodes_supplier_addr",
    )?;
    let deployer_addr = extract_json_value(output, "hub.deployer_addr")?;
    let governance_addr = extract_json_value(output, "hub.deployed_addresses.governance_addr")?;
    let stm_tracker = extract_json_value(
        output,
        "hub.deployed_addresses.bridgehub.ctm_deployment_tracker_proxy_addr",
    )?;
    let create2_factory = extract_json_value(output, "hub.contracts.create2_factory_addr")?;

    if l1_da_validator.is_empty() || l1_da_validator == "0x0000000000000000000000000000000000000000"
    {
        anyhow::bail!("L1 DA validator address is empty or zero");
    }

    println!("  bridgehub = {bridgehub}");
    println!("  ctm_proxy = {ctm_proxy}");
    println!("  bytecodes_supplier = {bytecodes_supplier}");

    // ----------------------------------------------------------------
    // Step 3b: Deploy ZK token, register on NTV
    // ----------------------------------------------------------------
    let zk_token_address = deploy_zk_token(contracts_backend, l1_rpc_url, keys, &governance_addr)?;

    let shared_bridge = contracts_backend
        .cast(&[
            "call",
            &bridgehub,
            "sharedBridge()(address)",
            "--rpc-url",
            l1_rpc_url,
        ])?
        .trim()
        .to_string();
    let ntv = contracts_backend
        .cast(&[
            "call",
            &shared_bridge,
            "nativeTokenVault()(address)",
            "--rpc-url",
            l1_rpc_url,
        ])?
        .trim()
        .to_string();

    contracts_backend
        .cast(&[
            "send",
            &ntv,
            "registerToken(address)",
            &zk_token_address,
            "--private-key",
            &keys.deployer_pk,
            "--rpc-url",
            l1_rpc_url,
        ])
        .context("register ZK token on NTV")?;

    // ----------------------------------------------------------------
    // Steps 4-6: Chain init
    //
    // The three variants differ only by a handful of flags:
    //   - gateway chain: --base-token-addr (ZK), no pause
    //   - gateway-settling: ETH base token, --pause-deposits + --skip-priority-txs
    //   - L1-settling: ETH base token, no pause
    // Everything else (CTM, DA validator, operators, keys, VM type, factory)
    // is identical, so we drive all three variants through one helper.
    // ----------------------------------------------------------------
    let gw_chain_out_name = "chain_init_gateway.json";
    println!(
        "\n=== protocol_ops chain init: gateway chain {} ===",
        GATEWAY.id
    );
    run_chain_init(
        contracts_backend,
        l1_rpc_url,
        keys,
        gw_ops,
        &ctm_proxy,
        &l1_da_validator,
        &create2_factory,
        ChainInitKind::Gateway {
            base_token_addr: &zk_token_address,
        },
        gw_chain_out_name,
    )?;
    let gw_chain_json: serde_json::Value = serde_json::from_str(
        &contracts_backend.read_protocol_ops_output(gw_chain_out_name)?,
    )?;
    let gw_chain_output = gw_chain_json
        .get("output")
        .ok_or_else(|| anyhow::anyhow!("Missing output"))?;
    let gw_diamond_proxy = extract_json_value(gw_chain_output, "diamond_proxy_addr")?;

    let mut gw_settling_diamond_proxies = Vec::new();
    for ops in gw_settling_ops {
        println!(
            "\n=== protocol_ops chain init: gateway-settling chain {} ===",
            ops.chain_id
        );
        let chain_out_name = format!("chain_init_{}.json", ops.chain_id);
        run_chain_init(
            contracts_backend,
            l1_rpc_url,
            keys,
            ops,
            &ctm_proxy,
            &l1_da_validator,
            &create2_factory,
            ChainInitKind::GatewaySettling,
            &chain_out_name,
        )?;
        let chain_json: serde_json::Value =
            serde_json::from_str(&contracts_backend.read_protocol_ops_output(&chain_out_name)?)?;
        let chain_output = chain_json
            .get("output")
            .ok_or_else(|| anyhow::anyhow!("Missing output"))?;
        gw_settling_diamond_proxies.push(extract_json_value(chain_output, "diamond_proxy_addr")?);
    }

    let mut l1_settling_diamond_proxies = Vec::new();
    for ops in l1_settling_ops {
        println!(
            "\n=== protocol_ops chain init: L1-settling chain {} ===",
            ops.chain_id
        );
        let chain_out_name = format!("chain_init_{}.json", ops.chain_id);
        run_chain_init(
            contracts_backend,
            l1_rpc_url,
            keys,
            ops,
            &ctm_proxy,
            &l1_da_validator,
            &create2_factory,
            ChainInitKind::L1Settling,
            &chain_out_name,
        )?;
        let chain_json: serde_json::Value =
            serde_json::from_str(&contracts_backend.read_protocol_ops_output(&chain_out_name)?)?;
        let chain_output = chain_json
            .get("output")
            .ok_or_else(|| anyhow::anyhow!("Missing output"))?;
        l1_settling_diamond_proxies.push(extract_json_value(chain_output, "diamond_proxy_addr")?);
    }

    // ----------------------------------------------------------------
    // Step 7: Fund chain admins with ZK tokens for migration L1→L2 priority tx gas.
    // (Operator/owner L1 ETH funding already happened in Step 3a.)
    // ----------------------------------------------------------------
    println!("\n=== Funding chain admins with ZK tokens ===");
    let zk_mint_amount = "1000000000000000000000000";
    for proxies in [&gw_settling_diamond_proxies, &l1_settling_diamond_proxies] {
        for diamond_proxy in proxies {
            let admin = contracts_backend
                .cast(&[
                    "call",
                    diamond_proxy,
                    "getAdmin()(address)",
                    "--rpc-url",
                    l1_rpc_url,
                ])?
                .trim()
                .to_string();
            contracts_backend
                .cast(&[
                    "send",
                    &zk_token_address,
                    "mint(address,uint256)",
                    &admin,
                    zk_mint_amount,
                    "--private-key",
                    &keys.deployer_pk,
                    "--rpc-url",
                    l1_rpc_url,
                ])
                .with_context(|| format!("mint ZK to chain admin {admin}"))?;
        }
    }

    // ----------------------------------------------------------------
    // Step 8: Generate genesis.json into output directory
    // ----------------------------------------------------------------
    let work_genesis = run_genesis_gen(contracts_backend)?;
    let genesis_path = output_dir.join("genesis.json");
    fs::copy(&work_genesis, &genesis_path)
        .with_context(|| format!("copy genesis to {}", genesis_path.display()))?;
    let genesis_path = genesis_path.canonicalize()?;

    // ----------------------------------------------------------------
    // Step 8b: Write per-chain config files into output directory
    //   gateway.yaml, gateway_settling_a.yaml, gateway_settling_b.yaml, l1_settling.yaml
    // ----------------------------------------------------------------
    println!("\n=== Writing chain configs ===");
    // All three chain variants share the same base ServerConfigBuilder args;
    // they only differ in whether `gateway()` / `forced_price()` apply.
    //   - gateway           : forced ZK price, no gateway route
    //   - gateway-settling  : forced ZK price + gateway route (RPC at runtime)
    //   - L1-settling       : neither
    let write_chain_config = |ops: &ChainOperators, settles_on_gateway: bool, label: &str| {
        use integration_tests::server_config::ServerConfigBuilder;
        let mut builder = ServerConfigBuilder::new(
            &bridgehub,
            &bytecodes_supplier,
            &genesis_path,
            ops.chain_id,
            &ops.commit_pk,
            &ops.prove_pk,
            &ops.execute_pk,
        );
        if ops.chain_id == GATEWAY.id || settles_on_gateway {
            builder = builder.forced_price(&zk_token_address, 3000);
        }
        if settles_on_gateway {
            builder = builder.gateway("RUNTIME", GATEWAY.id);
        }
        fs::write(
            output_dir.join(format!("{}.yaml", ops.dir_name)),
            builder.build(),
        )?;
        println!("  {}.yaml ({label})", ops.dir_name);
        Ok::<_, anyhow::Error>(())
    };

    write_chain_config(gw_ops, false, "gateway")?;
    for ops in gw_settling_ops {
        write_chain_config(ops, true, "gateway-settling")?;
    }
    for ops in l1_settling_ops {
        write_chain_config(ops, false, "L1-settling")?;
    }

    // ----------------------------------------------------------------
    // Step 8c: Copy wallets.yaml (already generated before the flow)
    // ----------------------------------------------------------------
    let wallets_path = output_dir.join("wallets.yaml");
    let work_wallets = contracts_backend.work_dir().join("wallets.yaml");
    fs::copy(&work_wallets, &wallets_path)
        .with_context(|| format!("copy wallets to {}", wallets_path.display()))?;
    println!("  wallets.yaml -> {}", wallets_path.display());

    // ----------------------------------------------------------------
    // Step 9: Start gateway server
    // ----------------------------------------------------------------
    println!(
        "\n=== Starting gateway server (chain {}) ===",
        GATEWAY.id
    );
    let gw_config_path = output_dir.join(format!("{}.yaml", gw_ops.dir_name));

    let anvil_handle = integration_tests::anvil::Anvil::wrap_external(anvil_port);
    let gw_rocks_db = contracts_backend.work_dir().join("gateway_rocksdb");
    // Remove stale RocksDB from a previous run so the server starts fresh
    // against the newly-deployed L1 contracts.
    if gw_rocks_db.exists() {
        fs::remove_dir_all(&gw_rocks_db).context("remove stale gateway_rocksdb")?;
    }
    let logs_dir = output_dir.join("logs");
    fs::create_dir_all(&logs_dir).context("create logs dir for state generation")?;
    let gw_server =
        integration_tests::server::ServerBuilder::new(preset.clone(), "generate_l1_state")
            .chain_name(GATEWAY.name)
            .config_path(&gw_config_path)
            .rocks_db_path(&gw_rocks_db)
            .logs_dir(&logs_dir)
            .diamond_proxy_addr(&gw_diamond_proxy)
            .bridgehub_addr(&bridgehub)
            .chain_id(GATEWAY.id)
            .spawn(&anvil_handle)
            .context("Failed to start gateway server")?;
    let gw_l2_rpc = gw_server.rpc_url();
    println!("  Gateway server ready at {gw_l2_rpc}");

    // ----------------------------------------------------------------
    // Step 10: Fund gateway L2 (test account + gateway operators)
    // ----------------------------------------------------------------
    println!("\n=== Funding gateway L2 ===");

    // The gateway uses ZK base token. Mint ZK tokens to the test account
    // and approve the bridgehub so L1→L2 deposits can pay in ZK.
    let test_address = address_from_private_key(DEFAULT_ANVIL_PRIVATE_KEY)?;
    let zk_mint = "1000000000000000000000000000000000000000";
    contracts_backend
        .cast(&[
            "send",
            &zk_token_address,
            "mint(address,uint256)",
            &test_address,
            zk_mint,
            "--private-key",
            &keys.deployer_pk,
            "--rpc-url",
            l1_rpc_url,
        ])
        .context("mint ZK to test account")?;
    // Only approve the NativeTokenVault — it is the contract that ultimately
    // pulls the base token via transferFrom. Approving the bridgehub or
    // shared bridge as well would hide any unintended deposit-path change
    // (e.g. a regression where one of them starts transferring directly).
    contracts_backend
        .cast(&[
            "send",
            &zk_token_address,
            "approve(address,uint256)",
            ntv.as_str(),
            zk_mint,
            "--private-key",
            DEFAULT_ANVIL_PRIVATE_KEY,
            "--rpc-url",
            l1_rpc_url,
        ])
        .context("approve NTV for ZK tokens")?;

    gw_server
        .fund_account_via_l1_deposit(&test_address, 0.1, L1DepositBaseToken::PreApprovedCustom)
        .await
        .context("fund gateway L2 test account")?;

    for addr_name in ["commit", "prove", "execute"] {
        let addr = match addr_name {
            "commit" => &gw_ops.commit_addr,
            "prove" => &gw_ops.prove_addr,
            "execute" => &gw_ops.execute_addr,
            _ => unreachable!(),
        };
        gw_server
            .fund_account_via_l1_deposit(addr, 5.0, L1DepositBaseToken::PreApprovedCustom)
            .await
            .with_context(|| format!("fund gateway L2 for operator {addr_name}"))?;
    }

    // ----------------------------------------------------------------
    // Step 11: Wait for gateway batches
    // ----------------------------------------------------------------
    println!("\n=== Waiting for gateway batches ===");
    gw_server
        .wait_for_executed_batches_with_traffic()
        .context("gateway executed batches")?;

    // ----------------------------------------------------------------
    // Step 12: Convert gateway chain
    // ----------------------------------------------------------------
    println!("\n=== Converting chain {} to gateway ===", GATEWAY.id);

    // Must be /script-out/... — protocol_ops strips the leading "/" and passes
    // the remainder to forge which checks it against fs_permissions (only
    // "script-out" relative to the project root is whitelisted).
    let vote_output_path_rel = "/script-out/gateway_vote_prep_out.toml".to_string();
    run_deploy_gateway_transaction_filterer(
        contracts_backend,
        l1_rpc_url,
        &bridgehub,
        GATEWAY.id,
        &gw_ops.owner_pk,
    )?;
    run_convert_to_gateway(
        contracts_backend,
        l1_rpc_url,
        keys,
        &gw_ops.owner_pk,
        &bridgehub,
        GATEWAY.id,
        &governance_addr,
        &stm_tracker,
        &ctm_proxy,
        &vote_output_path_rel,
    )?;

    // ----------------------------------------------------------------
    // Step 13: Gateway-settling chains — migrate, finalize, enable validators
    // ----------------------------------------------------------------

    // 13a: Migrate + confirm transfer for all chains
    for ops in gw_settling_ops {
        let chain_id = ops.chain_id;
        println!("\n=== Migrating chain {} to gateway ===", chain_id);
        run_migrate_to_gateway(
            contracts_backend,
            l1_rpc_url,
            &ops.owner_pk,
            &bridgehub,
            chain_id,
            GATEWAY.id,
            &vote_output_path_rel,
            &deployer_addr,
            true,
        )?;
        // Confirm transfer on L1 (finishMigrateChainToGateway only, no validators yet).
        // This proves inclusion of the migration priority tx and does not require
        // any owner authority, so we pay with the deployer key rather than the
        // chain owner — using owner_pk here would be misleading about the
        // command's actual authorization.
        println!("  Confirming transfer for chain {chain_id}");
        contracts_backend
            .protocol_ops(&[
                "chain",
                "gateway",
                "migrate",
                "finalize",
                "--bridgehub",
                &bridgehub,
                "--chain-id",
                &chain_id.to_string(),
                "--gateway-chain-id",
                &GATEWAY.id.to_string(),
                "--gateway-rpc-url",
                &gw_l2_rpc,
                "--gateway-diamond-proxy",
                &gw_diamond_proxy,
                "--l1-rpc-url",
                l1_rpc_url,
                "--private-key",
                &keys.deployer_pk,
                "--vote-preparation-toml",
                &vote_output_path_rel,
                // No --commit/prove/execute-operator: skip validator enablement for now
                "--commit-operator",
                "0x0000000000000000000000000000000000000000",
                "--prove-operator",
                "0x0000000000000000000000000000000000000000",
                "--execute-operator",
                "0x0000000000000000000000000000000000000000",
                "--gateway-validator-timelock",
                "0x0000000000000000000000000000000000000000",
            ])
            .with_context(|| format!("finalize migration (confirm) for chain {chain_id}"))?;
    }

    // 13b: Wait for gateway to process all migration L1->L2 priority txs
    println!("\n=== Waiting for gateway to process chain migrations ===");
    gw_server
        .wait_for_executed_batches_with_traffic()
        .context("gateway batches after migration")?;

    // 13c: Resolve gateway ValidatorTimelock (now chains are registered on gateway)
    //
    // Note on `validatorTimelockPostV29`: protocol v29 introduced a new
    // ValidatorTimelock contract alongside the legacy one; CTM exposes both
    // via `validatorTimelock()` (pre-v29) and `validatorTimelockPostV29()`
    // (v29+). Gateway-settling chains run v29+, so we read the post-v29
    // address here.
    let gw_validator_timelock = {
        let first_chain_id = gw_settling_ops[0].chain_id;
        let ctm = contracts_backend
            .cast(&[
                "call",
                "0x0000000000000000000000000000000000010002",
                "chainTypeManager(uint256)(address)",
                &first_chain_id.to_string(),
                "--rpc-url",
                &gw_l2_rpc,
            ])
            .context("query gateway L2 chainTypeManager")?;
        let ctm = ctm.trim().to_string();
        anyhow::ensure!(
            !ctm.is_empty() && ctm != "0x0000000000000000000000000000000000000000",
            "chain {} not registered on gateway yet",
            first_chain_id
        );
        let vtl = contracts_backend
            .cast(&[
                "call",
                &ctm,
                "validatorTimelockPostV29()(address)",
                "--rpc-url",
                &gw_l2_rpc,
            ])
            .context("query validatorTimelockPostV29")?;
        vtl.trim().to_string()
    };
    println!("  Gateway L2 ValidatorTimelock: {gw_validator_timelock}");

    // Read gateway's relayed SL DA validator from vote preparation output.
    // protocol_ops writes to l1-contracts/script-out/ (symlinked to work_dir
    // in Docker).
    let gw_vote_toml =
        contracts_backend.read_repo_file("l1-contracts/script-out/gateway_vote_prep_out.toml")?;
    let vote_prep: VotePrepOutput =
        toml::from_str(&gw_vote_toml).context("parse vote preparation output TOML")?;
    let relayed_sl_da_validator = vote_prep.relayed_sl_da_validator;
    println!("  Gateway relayed SL DA validator: {relayed_sl_da_validator}");

    // 13d: Enable validators + fund operators for each chain
    // Call AdminFunctions.enableValidatorViaGateway directly via forge
    for (i, ops) in gw_settling_ops.iter().enumerate() {
        let chain_id = ops.chain_id;
        println!("\n=== Enabling validators for chain {} ===", chain_id);
        let mut seen = std::collections::HashSet::new();
        for (name, addr) in [
            ("commit", &ops.commit_addr),
            ("prove", &ops.prove_addr),
            ("execute", &ops.execute_addr),
        ] {
            if !seen.insert(addr.clone()) {
                continue;
            }
            println!("  Enabling {name} operator {addr}");
            let chain_id_str = chain_id.to_string();
            let gw_chain_id_str = GATEWAY.id.to_string();
            contracts_backend.forge_script(&[
                "deploy-scripts/AdminFunctions.s.sol",
                "--sig", "enableValidatorViaGateway(address,uint256,uint256,uint256,address,address,address,bool)",
                &bridgehub,
                "1000000000",
                &chain_id_str,
                &gw_chain_id_str,
                addr,
                &gw_validator_timelock,
                &deployer_addr,
                "true",
                "--rpc-url", l1_rpc_url,
                "--broadcast", "--ffi",
                "--private-key", &ops.owner_pk,
            ], &[])
            .with_context(|| format!("enableValidatorViaGateway for {name} operator {addr} on chain {chain_id}"))?;
        }

        // Set DA validator pair via gateway
        println!("  Setting DA validator pair via gateway for chain {chain_id}");
        let chain_diamond_on_gw = {
            let out = contracts_backend
                .cast(&[
                    "call",
                    "0x0000000000000000000000000000000000010002",
                    "getZKChain(uint256)(address)",
                    &chain_id.to_string(),
                    "--rpc-url",
                    &gw_l2_rpc,
                ])
                .context("query chain diamond on gateway")?;
            out.trim().to_string()
        };
        // Update with the post-migration gateway address (replaces the
        // pre-migration L1 address captured during chain init).
        println!(
            "  Chain {} diamond proxy on gateway: {}",
            chain_id, chain_diamond_on_gw
        );
        gw_settling_diamond_proxies[i] = chain_diamond_on_gw.clone();
        let chain_id_str = chain_id.to_string();
        let gw_chain_id_str = GATEWAY.id.to_string();
        // All currently-generated chains are rollups; switch to a validium
        // variant here when adding validium-on-gateway coverage.
        let da_scheme = L2DaCommitmentScheme::BlobsAndPubdataKeccak256;
        contracts_backend.forge_script(&[
            "deploy-scripts/AdminFunctions.s.sol",
            "--sig", "setDAValidatorPairWithGateway(address,uint256,uint256,uint256,address,uint8,address,address,bool)",
            &bridgehub,
            "1000000000",
            &chain_id_str,
            &gw_chain_id_str,
            &relayed_sl_da_validator,
            da_scheme.as_u8_str(),
            &chain_diamond_on_gw,
            &deployer_addr,
            "true",
            "--rpc-url", l1_rpc_url,
            "--broadcast", "--ffi",
            "--private-key", &ops.owner_pk,
        ], &[])
        .with_context(|| format!("setDAValidatorPairWithGateway for chain {chain_id}"))?;

        println!("  Funding gateway L2 for chain {} operators", chain_id);
        for addr in [&ops.commit_addr, &ops.prove_addr, &ops.execute_addr] {
            gw_server
                .fund_account_via_l1_deposit(addr, 5.0, L1DepositBaseToken::PreApprovedCustom)
                .await
                .context("fund gateway L2 for migrated-chain operator")?;
        }
    }

    // 13e: Wait for gateway to drain its priority queue (validator + DA validator txs)
    println!("\n=== Waiting for gateway to drain priority queue ===");
    {
        let deadline = std::time::Instant::now() + PRIORITY_QUEUE_DRAIN_TIMEOUT;
        loop {
            let raw = contracts_backend.cast(&[
                "call",
                &gw_diamond_proxy,
                "getPriorityQueueSize()(uint256)",
                "--rpc-url",
                l1_rpc_url,
            ])?;
            let queue_size = raw.trim().parse::<u64>().unwrap_or(u64::MAX);
            if queue_size == 0 {
                println!("  Gateway priority queue drained");
                break;
            }
            if std::time::Instant::now() > deadline {
                anyhow::bail!(
                    "Timed out waiting for gateway priority queue to drain (size={})",
                    queue_size
                );
            }
            println!("  Gateway priority queue size: {queue_size}, sending traffic...");
            let _ = gw_server.send_traffic_tx();
            std::thread::sleep(Duration::from_secs(3));
        }
        // Now wait for enough batches to be executed on L1 so the state is visible
        gw_server
            .wait_for_executed_batches_with_traffic()
            .context("gateway batches after priority queue drain")?;
    }

    // ----------------------------------------------------------------
    // Step 14a: Fund test account on gateway-settling chains via L1 deposit.
    // The gateway is running, so L1→gateway→chain deposits are processed.
    // ----------------------------------------------------------------
    {
        let test_addr = address_from_private_key(DEFAULT_ANVIL_PRIVATE_KEY)?;
        for ops in gw_settling_ops {
            let chain_id = ops.chain_id;
            println!("\n=== Funding test account on gateway-settling chain {chain_id} ===");
            integration_tests::l1_l2_deposit::submit_l1_to_l2_deposit_to(
                l1_rpc_url,
                &bridgehub,
                chain_id,
                DEFAULT_ANVIL_PRIVATE_KEY,
                10.0,
                Some(&test_addr),
            )
            .await
            .with_context(|| format!("L1 deposit for gateway-settling chain {chain_id}"))?;
        }
    }

    // ----------------------------------------------------------------
    // Step 14b: Fund test account on L1-settling chains via L1 deposit.
    // The deposit sits in the L1 priority queue until a test spawns the
    // server for that chain and it processes the queued tx in its first
    // batch. This mirrors the gateway-settling pre-funding above so that
    // tests can rely on pre-generated state instead of duplicating
    // setup.
    // ----------------------------------------------------------------
    {
        let test_addr = address_from_private_key(DEFAULT_ANVIL_PRIVATE_KEY)?;
        for ops in l1_settling_ops {
            let chain_id = ops.chain_id;
            println!("\n=== Funding test account on L1-settling chain {chain_id} ===");
            integration_tests::l1_l2_deposit::submit_l1_to_l2_deposit_to(
                l1_rpc_url,
                &bridgehub,
                chain_id,
                DEFAULT_ANVIL_PRIVATE_KEY,
                10.0,
                Some(&test_addr),
            )
            .await
            .with_context(|| format!("L1 deposit for L1-settling chain {chain_id}"))?;
        }
    }

    // ----------------------------------------------------------------
    // Step 15: Stop gateway server
    // ----------------------------------------------------------------
    println!("\n=== Stopping gateway server ===");
    gw_server.kill().context("kill gateway server")?;

    // Archive gateway RocksDB so tests can load it via ephemeral_state.
    // The server's unpack_ephemeral_state strips the first path component
    // (expects a wrapping directory like `node/`), so we wrap everything
    // under a `node/` prefix.
    let gw_state_archive = output_path.with_extension("gateway-state.tar.gz");
    println!(
        "Archiving gateway RocksDB -> {}",
        gw_state_archive.display()
    );
    {
        let tar_file = fs::File::create(&gw_state_archive)?;
        let enc = flate2::write::GzEncoder::new(tar_file, flate2::Compression::fast());
        let mut tar = tar::Builder::new(enc);
        tar.append_dir_all("node", &gw_rocks_db)?;
        tar.finish()?;
    }

    // Rewrite gateway config with ephemeral mode now that the archive exists
    let gw_state_archive_abs = gw_state_archive
        .canonicalize()
        .unwrap_or_else(|_| gw_state_archive.clone());
    fs::write(
        output_dir.join(format!("{}.yaml", gw_ops.dir_name)),
        integration_tests::server_config::ServerConfigBuilder::new(
            &bridgehub,
            &bytecodes_supplier,
            &genesis_path,
            GATEWAY.id,
            &gw_ops.commit_pk,
            &gw_ops.prove_pk,
            &gw_ops.execute_pk,
        )
        .ephemeral(gw_state_archive_abs.to_string_lossy())
        .forced_price(&zk_token_address, 3000)
        .build(),
    )?;
    println!("  Updated {}.yaml with ephemeral state", gw_ops.dir_name);

    Ok(FlowResult {
        bridgehub,
        bytecodes_supplier,
        gw_diamond_proxy,
        gw_settling_diamond_proxies,
        l1_settling_diamond_proxies,
        gateway_ephemeral_state: gw_state_archive,
    })
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let preset = load_preset(&args)?;

    // ── Determine output directory ───────────────────────────────────────
    let output_dir = integration_tests::l1_state::cache_dir_for_preset(&preset)?;
    let metadata_path = output_dir.join("metadata.json");

    // ── Cache check ──────────────────────────────────────────────────────
    // Only use cached state when both repo refs are non-local (DockerTag),
    // since local paths may have changed since the last generation.
    let cacheable = matches!(preset.era_contracts, RepoRef::DockerTag { .. })
        && matches!(preset.zksync_os_server, RepoRef::DockerTag { .. });
    if cacheable && metadata_path.exists() {
        // Verify the cached state was generated with the same image SHAs.
        // If a newer image has become available since the fallback, regenerate.
        let meta: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&metadata_path).unwrap_or_default())
                .unwrap_or_default();
        let era_tag_matches = meta
            .get("era_contracts_tag")
            .and_then(|v| v.as_str())
            .is_some_and(|cached| {
                if let RepoRef::DockerTag { tag, .. } = &preset.era_contracts {
                    cached == tag
                } else {
                    false
                }
            });
        let server_tag_matches = meta
            .get("zksync_os_server_tag")
            .and_then(|v| v.as_str())
            .is_some_and(|cached| {
                if let RepoRef::DockerTag { tag, .. } = &preset.zksync_os_server {
                    cached == tag
                } else {
                    false
                }
            });
        if era_tag_matches && server_tag_matches {
            println!("Cache hit: {}", output_dir.display());
            return Ok(());
        }
        println!(
            "Cache stale (newer image available), regenerating: {}",
            output_dir.display()
        );
    }

    // Remove stale or incomplete output from a previous run
    if output_dir.exists() {
        println!("Removing previous output: {}", output_dir.display());
        fs::remove_dir_all(&output_dir)?;
    }

    println!(
        "Cache not available, generating fresh l1-state.json in {}",
        output_dir.display()
    );

    // ── Pull Docker images upfront (not counted in generation time) ──────
    if let RepoRef::DockerTag { tag, .. } = &preset.era_contracts {
        let image = format!("ghcr.io/matter-labs/protocol-ops:{}", tag);
        println!("Pulling protocol-ops image (if not available): {}", image);
        docker_pull_image(&image)
            .map_err(|e| anyhow::anyhow!("Failed to pull protocol-ops image: {:?}", e))?;
    }
    if let RepoRef::DockerTag { tag, .. } = &preset.zksync_os_server {
        let image = format!("ghcr.io/matter-labs/zksync-os-server:{}", tag);
        println!("Pulling zksync-os-server image: {}", image);
        docker_pull_image(&image)
            .map_err(|e| anyhow::anyhow!("Failed to pull zksync-os-server image: {:?}", e))?;
    }

    let generation_start = std::time::Instant::now();
    fs::create_dir_all(&output_dir)?;
    let output_dir = fs::canonicalize(&output_dir)?;
    let output_path = output_dir.join("l1-state.json");

    // Create the era-contracts execution backend (local or Docker session).
    let contracts_backend = EraContractsBackend::from_preset(&preset, "generate_l1_state", &[])?;
    println!("Work directory: {}", contracts_backend.work_dir().display());
    if let EraContractsBackend::Docker { ref session, .. } = contracts_backend {
        println!(
            "Started era-contracts Docker session: {}",
            session.container_name()
        );
    }

    // Generate wallets.yaml early — we need the keys before ecosystem init.
    let all_chain_names: Vec<&str> = std::iter::once(GATEWAY.name)
        .chain(ECOSYSTEM_CHAINS.iter().map(|c| c.name))
        .collect();
    let chains_arg = all_chain_names.join(",");
    println!("\n=== Generating wallets.yaml ===");
    let work_wallets = run_wallets_gen(&contracts_backend, &chains_arg)?;
    let wallets: WalletsFile = serde_yaml::from_str(
        &fs::read_to_string(&work_wallets)
            .with_context(|| format!("read {}", work_wallets.display()))?,
    )
    .context("parse wallets.yaml")?;

    let keys = KeySet::from_wallets(&wallets)?;
    println!(
        "  deployer = {}, ecosystem_owner = {}",
        keys.deployer_addr, keys.ecosystem_owner_addr
    );

    // Chain operator contexts (from wallets.yaml)
    let ops_for = |spec: &ChainSpec| -> Result<ChainOperators> {
        let w = wallets.chains.get(spec.name).ok_or_else(|| {
            anyhow::anyhow!("wallets.yaml missing chain '{}'", spec.name)
        })?;
        ChainOperators::from_wallets(spec.id, spec.name, w)
    };
    let gw_ops = ops_for(&GATEWAY)?;
    let gw_settling_ops: Vec<ChainOperators> = gateway_settling_chains()
        .map(ops_for)
        .collect::<Result<_>>()?;
    let l1_settling_ops: Vec<ChainOperators> = l1_settling_chains()
        .map(ops_for)
        .collect::<Result<_>>()?;

    // ----------------------------------------------------------------
    // Step 1: Build contracts (local only — Docker image has pre-built artifacts)
    // ----------------------------------------------------------------
    if let Some(era_path) = era_local_path(&preset) {
        println!("\n=== Building contracts ===");
        let status = Command::new("yarn")
            .args(["build-all-contracts"])
            .current_dir(&era_path)
            .status()
            .context("yarn build-all-contracts")?;
        if !status.success() {
            anyhow::bail!("yarn build-all-contracts failed");
        }
        println!("  Contracts built");
    } else {
        println!("\n=== Skipping contract build (using Docker image) ===");
    }

    // ----------------------------------------------------------------
    // Step 2: Start Anvil with --dump-state
    // ----------------------------------------------------------------
    println!(
        "\n=== Starting Anvil (dump-state -> {}) ===",
        output_path.display()
    );
    let anvil_port = integration_tests::find_ports::pick_unused_port_sync()?;
    let anvil = DumpStateAnvil::spawn(anvil_port, &output_path)?;
    let l1_rpc_url = anvil.rpc_url().to_string();
    println!("  Anvil ready at {}", l1_rpc_url);

    // Run Steps 3–15; terminate Anvil even on error.
    let result = run_generation_flow(
        &contracts_backend,
        &l1_rpc_url,
        &keys,
        &gw_ops,
        &gw_settling_ops,
        &l1_settling_ops,
        &output_dir,
        &output_path,
        &preset,
        anvil_port,
    )
    .await;

    // ----------------------------------------------------------------
    // Step 16: Stop Anvil (triggers state dump)
    // ----------------------------------------------------------------
    println!("\n=== Stopping Anvil (dumping state) ===");
    anvil.terminate()?;

    // Propagate any error from the main flow
    let flow = result?;

    if !output_path.exists() {
        anyhow::bail!("Anvil did not write state file: {}", output_path.display());
    }
    let file_size = fs::metadata(&output_path)?.len();
    println!(
        "\nState file: {} ({:.1} MB)",
        output_path.display(),
        file_size as f64 / 1_048_576.0
    );

    // Write ecosystem.yaml alongside the state file
    let eco_path = output_dir.join("ecosystem.yaml");
    let eco_config = integration_tests::l1_state::EcosystemConfig {
        l1_state: output_path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string(),
        bridgehub: flow.bridgehub,
        bytecodes_supplier: flow.bytecodes_supplier,
        gateway: integration_tests::l1_state::GatewayMeta {
            chain_id: GATEWAY.id,
            diamond_proxy: flow.gw_diamond_proxy,
            ephemeral_state: flow.gateway_ephemeral_state.to_string_lossy().to_string(),
            name: GATEWAY.name.to_string(),
        },
        gateway_settling_chains: gateway_settling_chains()
            .enumerate()
            .map(|(i, spec)| integration_tests::l1_state::ChainMeta {
                chain_id: spec.id,
                diamond_proxy: flow.gw_settling_diamond_proxies[i].clone(),
                ephemeral_state: None,
                name: spec.name.to_string(),
            })
            .collect(),
        l1_settling_chains: l1_settling_chains()
            .enumerate()
            .map(|(i, spec)| integration_tests::l1_state::ChainMeta {
                chain_id: spec.id,
                diamond_proxy: flow.l1_settling_diamond_proxies[i].clone(),
                ephemeral_state: None,
                name: spec.name.to_string(),
            })
            .collect(),
    };
    fs::write(&eco_path, serde_yaml::to_string(&eco_config)?)?;
    println!("Ecosystem config: {}", eco_path.display());

    // Write metadata.json last — its presence marks the cache entry as complete.
    // Store the actual image SHAs so we can detect when a newer image is available.
    let era_tag = match &preset.era_contracts {
        RepoRef::DockerTag { tag, .. } => Some(tag.as_str()),
        _ => None,
    };
    let server_tag = match &preset.zksync_os_server {
        RepoRef::DockerTag { tag, .. } => Some(tag.as_str()),
        _ => None,
    };
    let metadata = serde_json::json!({
        "generated_at": chrono::Utc::now().to_rfc3339(),
        "era_contracts_tag": era_tag,
        "zksync_os_server_tag": server_tag,
    });
    let metadata_path = output_dir.join("metadata.json");
    fs::write(&metadata_path, serde_json::to_string_pretty(&metadata)?)?;

    let elapsed = generation_start.elapsed();
    println!(
        "Done! L1 state generation took {:.1}s",
        elapsed.as_secs_f64()
    );
    Ok(())
}
