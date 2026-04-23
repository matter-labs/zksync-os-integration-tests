//! Standalone tool to generate `l1-state.json` for integration tests.
//!
//! Replicates the L1 setup from `update_server.py` / `protocol_ops_init` test:
//!  1. Build contracts (local only — Docker image has pre-built artifacts)
//!     1a. Generate genesis.json (must run before any forge script — `DeployCTM`
//!     reads `genesis_root` from `configs/genesis/zksync-os/latest.json` and
//!     bakes it into the CTM on L1)
//!  2. Start Anvil with `--dump-state`
//!  3. Deploy L1 contracts via `protocol_ops ecosystem init`
//!  4. Register gateway chain via `protocol_ops chain init`
//!  5. Register gateway-settling chains (with `--pause-deposits --skip-priority-txs`)
//!  6. Register L1-settling chains
//!  7. Fund all operator accounts on L1
//!  8. Write per-chain config files and wallets.yaml
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

use integration_tests::anvil_utils::anvil_set_balance;
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

/// Large mint for ecosystem-wide ZK token balances (10^39 wei).
const ZK_MINT_AMOUNT_LARGE: &str = "1000000000000000000000000000000000000000";

/// Smaller mint for per-chain admin ZK token balances (10^24 wei, ~1M ZK).
const ZK_MINT_AMOUNT_ADMIN: &str = "1000000000000000000000000";

/// Forced ZK token price in USD for local dev/test configs.
const ZK_FORCED_PRICE_USD: u64 = 3000;

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
        let anvil_start = std::time::Instant::now();

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
        println!("Starting anvil took {:.2?}", anvil_start.elapsed());

        let rpc_url = format!("http://localhost:{}", port);

        integration_tests::server_utils::wait_for_chain_to_be_ready(
            &rpc_url,
            "Anvil",
            100,
            Duration::from_millis(100),
            None,
        )
        .context("Anvil did not become ready")?;

        println!("Anvil became ready after {:.2?}", anvil_start.elapsed());

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

    /// L1 accounts for this chain that need an ETH top-up on L1.
    ///
    /// The chain owner always needs L1 gas (admin/governance calls, including
    /// migration L1→L2 priority txs). The commit/prove/execute operators only
    /// need L1 gas when the chain settles on L1 — chains settling on the
    /// gateway run their operators against the gateway L2 only.
    fn l1_funded_addresses(&self, settles_on_l1: bool) -> Vec<&str> {
        if settles_on_l1 {
            vec![
                &self.owner_addr,
                &self.commit_addr,
                &self.prove_addr,
                &self.execute_addr,
            ]
        } else {
            vec![&self.owner_addr]
        }
    }
}

// ---------------------------------------------------------------------------
// Forge helpers
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
struct VotePrepOutput {
    diamond_cut_data: String,
    relayed_sl_da_validator: String,
}

// Genesis generation
// ---------------------------------------------------------------------------

/// Generate genesis.json into `work_dir/genesis.json`.
fn run_genesis_gen(contracts_backend: &EraContractsBackend) -> Result<PathBuf> {
    println!("Generating genesis.json...");
    let filename = "genesis.json";

    let local_binary = contracts_backend.tool_binary("zksync-os-genesis-gen")?;
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

    let local_binary = contracts_backend.tool_binary("wallets-gen")?;

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
                ZK_MINT_AMOUNT_LARGE,
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

/// ETH base token address (0x0...01).
const ETH_BASE_TOKEN: &str = "0x0000000000000000000000000000000000000001";

// ---------------------------------------------------------------------------
// Generation flow (Steps 3–15)
// ---------------------------------------------------------------------------

struct FlowResult {}

#[allow(clippy::too_many_arguments)]
async fn run_generation_flow(
    contracts_backend: &EraContractsBackend,
    l1_rpc_url: &str,
    keys: &KeySet,
    gw_ops: &ChainOperators,
    gw_settling_ops: &[ChainOperators],
    l1_settling_ops: &[ChainOperators],
    output_dir: &Path,
    preset: &integration_tests::presets::Preset,
    anvil_port: u16,
    genesis_path: &Path,
) -> Result<FlowResult> {
    // ----------------------------------------------------------------
    // Step 3a: Fund all L1 accounts used by the flow
    //   - Deployer / ecosystem owner (4000 ETH each)
    //   - Per-chain owners + validator operators (100 ETH each)
    // Validators don't spend L1 gas until after chain init, but we fund
    // them here so there is a single funding pass rather than one for
    // owners at the start and another for validators later.
    //
    // Uses `anvil_setBalance` (direct state mutation — no tx, no gas, no
    // nonce) via a native reqwest JSON-RPC call, issued in parallel.
    // ----------------------------------------------------------------
    println!("\n=== Funding L1 accounts ===");
    let funding_start = std::time::Instant::now();
    const ETH: u128 = 1_000_000_000_000_000_000;
    // (chain ops, settles_on_l1). Gateway itself settles on L1; its operators
    // commit/prove/execute against L1, so they need L1 gas too.
    let l1_funding_targets: Vec<(&ChainOperators, bool)> = std::iter::once((gw_ops, true))
        .chain(gw_settling_ops.iter().map(|o| (o, false)))
        .chain(l1_settling_ops.iter().map(|o| (o, true)))
        .collect();
    let mut funding_jobs: Vec<(String, u128, String)> = vec![
        (keys.deployer_addr.clone(), 4000 * ETH, "deployer".into()),
        (
            keys.ecosystem_owner_addr.clone(),
            4000 * ETH,
            "ecosystem owner".into(),
        ),
    ];
    for (ops, settles_on_l1) in &l1_funding_targets {
        for addr in ops.l1_funded_addresses(*settles_on_l1) {
            funding_jobs.push((
                addr.to_string(),
                100 * ETH,
                format!("L1 account {addr} for chain {}", ops.chain_id),
            ));
        }
    }
    let num_accounts = funding_jobs.len();
    let mut join_set: tokio::task::JoinSet<Result<()>> = tokio::task::JoinSet::new();
    for (addr, wei, label) in funding_jobs {
        let rpc = l1_rpc_url.to_string();
        join_set.spawn(async move {
            anvil_set_balance(&addr, wei, &rpc)
                .await
                .with_context(|| format!("fund {label}"))
        });
    }
    while let Some(res) = join_set.join_next().await {
        res.context("funding task join")??;
    }
    println!(
        "Funded {num_accounts} L1 accounts in {:.2?}",
        funding_start.elapsed()
    );

    // ----------------------------------------------------------------
    // Step 3: Ecosystem init
    // ----------------------------------------------------------------
    println!("\n=== protocol_ops ecosystem init ===");
    // `ecosystem init` is prepare-only (auto-forks L1 via anvil, emits Safe
    // bundles). We apply each bundle under the deployer key so the
    // deployments land on the real anvil and subsequent steps see them.
    let eco_out_dir = "ecosystem_init";
    let eco_out_dir_arg = contracts_backend.work_path(eco_out_dir);
    contracts_backend.protocol_ops(&[
        "ecosystem",
        "init",
        "--deployer-address",
        &keys.deployer_addr,
        "--owner",
        &keys.ecosystem_owner_addr,
        "--l1-rpc-url",
        l1_rpc_url,
        "--out",
        &eco_out_dir_arg,
    ])?;
    // Apply each Safe bundle: deployer / ecosystem-owner targets sign with
    // their keys. Eco-init bundles only have these two as targets, so plain
    // `apply` covers everything.
    contracts_backend
        .parse_safe_bundles(eco_out_dir, l1_rpc_url)?
        .apply(&[&keys.deployer_pk, &keys.ecosystem_owner_pk])?;

    // Read deployed addresses from the per-command metadata block in the
    // bundle dir's `manifest.json`. The first (and only) entry's `.output`
    // matches the old `ecosystem_init_out.json`'s `.output` shape.
    let manifest_json: serde_json::Value = serde_json::from_str(
        &contracts_backend.read_protocol_ops_output(&format!("{eco_out_dir}/manifest.json"))?,
    )?;
    let output = manifest_json
        .get("metadata")
        .and_then(|m| m.as_array())
        .and_then(|arr| arr.first())
        .and_then(|entry| entry.get("output"))
        .ok_or_else(|| {
            anyhow::anyhow!("Missing metadata[0].output in ecosystem init manifest.json")
        })?;

    let bridgehub = extract_json_value(
        output,
        "hub.deployed_addresses.bridgehub.bridgehub_proxy_addr",
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
    let _deployer_addr = extract_json_value(output, "hub.deployer_addr")?;
    let governance_addr = extract_json_value(output, "hub.deployed_addresses.governance_addr")?;

    if l1_da_validator.is_empty() || l1_da_validator == "0x0000000000000000000000000000000000000000"
    {
        anyhow::bail!("L1 DA validator address is empty or zero");
    }

    println!("  bridgehub = {bridgehub}");
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
    //
    // TODO(interop): register each chain's base token on every other chain via
    // `register_on_all_chains` (see zksync-era's `chain register-on-all-chains`
    // equivalent). The current interop test only verifies L2→L1 message
    // inclusion, so cross-chain base-token transfers would need this added.
    // ----------------------------------------------------------------
    // Write ecosystem.yaml early — downstream `chain init` / `chain gateway *`
    // invocations consume it via `--ecosystem <path>`.
    let eco_yaml_path = contracts_backend.work_dir().join("ecosystem.yaml");
    let eco_config = integration_tests::l1_state::EcosystemConfig {
        bridgehub: bridgehub.clone(),
        // Bake the ecosystem deployer EOA into ecosystem.yaml so downstream
        // protocol-ops invocations (and CI workflows that wrap them) can
        // pick it up without a separate env var. External users can override
        // by passing `--deployer-address` at invocation time.
        deployer: Some(keys.deployer_addr.clone()),
        chains: {
            let mut chains = std::collections::BTreeMap::new();
            chains.insert(GATEWAY.name.to_string(), GATEWAY.id);
            for spec in gateway_settling_chains() {
                chains.insert(spec.name.to_string(), spec.id);
            }
            for spec in l1_settling_chains() {
                chains.insert(spec.name.to_string(), spec.id);
            }
            chains
        },
    };
    fs::write(&eco_yaml_path, serde_yaml::to_string(&eco_config)?)?;
    println!("  ecosystem.yaml -> {}", eco_yaml_path.display());

    let eco_path = contracts_backend.work_path("ecosystem.yaml");

    // Helper to resolve a chain's diamond proxy from the bridgehub.
    let resolve_diamond = |chain_id: u64| -> Result<String> {
        let chain_id_str = chain_id.to_string();
        let addr = contracts_backend
            .cast(&[
                "call",
                &bridgehub,
                "getZKChain(uint256)(address)",
                &chain_id_str,
                "--rpc-url",
                l1_rpc_url,
            ])?
            .trim()
            .to_string();
        Ok(addr)
    };

    // Helper: run `chain init` directly via protocol-ops, then apply the
    // emitted Safe bundles with the three signers every chain init needs
    // (deployer + ecosystem owner + chain owner).
    //
    // `extra_flags` is appended verbatim — used by gateway-settling chains
    // to pass `--pause-deposits true --skip-priority-txs true`.
    let run_chain_init = |work_subdir: &str,
                          chain_name: &str,
                          chain_id: u64,
                          base_token: &str,
                          chain_ops: &ChainOperators,
                          extra_flags: &[&str]|
     -> Result<()> {
        let chain_id_str = chain_id.to_string();
        let safe_rel = format!("{work_subdir}/safe");
        let safe_abs = contracts_backend.work_path(&safe_rel);
        let mut args: Vec<&str> = vec![
            "chain",
            "init",
            "--l1-rpc-url",
            l1_rpc_url,
            "--bridgehub",
            &bridgehub,
            "--chain-id",
            &chain_id_str,
            "--deployer-address",
            &keys.deployer_addr,
            "--l1-da-validator",
            &l1_da_validator,
            "--base-token-addr",
            base_token,
            "--owner",
            &chain_ops.owner_addr,
            "--commit-operator",
            &chain_ops.commit_addr,
            "--prove-operator",
            &chain_ops.prove_addr,
            "--execute-operator",
            &chain_ops.execute_addr,
            "--out",
            &safe_abs,
        ];
        args.extend_from_slice(extra_flags);
        contracts_backend
            .protocol_ops(&args)
            .with_context(|| format!("chain init failed for {chain_name} (id={chain_id})"))?;
        contracts_backend
            .parse_safe_bundles(&safe_rel, l1_rpc_url)?
            .apply(&[
                &keys.deployer_pk,
                &keys.ecosystem_owner_pk,
                &chain_ops.owner_pk,
            ])
            .with_context(|| format!("apply chain init bundles for {chain_name}"))?;
        let _ = chain_name;
        Ok(())
    };

    // Gateway chain (custom ZK base token)
    println!("\n=== chain init: gateway chain {} ===", GATEWAY.id);
    run_chain_init(
        "generate_l1_state/chain_init_gateway",
        GATEWAY.name,
        GATEWAY.id,
        &zk_token_address,
        gw_ops,
        &[],
    )?;
    let gw_diamond_proxy = resolve_diamond(GATEWAY.id)?;

    // Gateway-settling chains (pause deposits + skip priority txs)
    for ops in gw_settling_ops {
        println!(
            "\n=== chain init: gateway-settling chain {} ===",
            ops.chain_id
        );
        run_chain_init(
            &format!("generate_l1_state/chain_init_{}", ops.chain_id),
            &ops.dir_name,
            ops.chain_id,
            ETH_BASE_TOKEN,
            ops,
            &["--pause-deposits", "true", "--skip-priority-txs", "true"],
        )?;
    }

    // L1-settling chains (default: ETH base token, deposits live)
    for ops in l1_settling_ops {
        println!("\n=== chain init: L1-settling chain {} ===", ops.chain_id);
        run_chain_init(
            &format!("generate_l1_state/chain_init_{}", ops.chain_id),
            &ops.dir_name,
            ops.chain_id,
            ETH_BASE_TOKEN,
            ops,
            &[],
        )?;
    }

    // ----------------------------------------------------------------
    // Step 7: Fund chain admins with ZK tokens for migration L1→L2 priority tx gas.
    // (Operator/owner L1 ETH funding already happened in Step 3a.)
    // ----------------------------------------------------------------
    println!("\n=== Funding chain admins with ZK tokens ===");
    for ops in gateway_settling_chains().chain(l1_settling_chains()) {
        let diamond_proxy = resolve_diamond(ops.id)?;
        let admin = contracts_backend
            .cast(&[
                "call",
                &diamond_proxy,
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
                ZK_MINT_AMOUNT_ADMIN,
                "--private-key",
                &keys.deployer_pk,
                "--rpc-url",
                l1_rpc_url,
            ])
            .with_context(|| format!("mint ZK to chain admin {admin}"))?;
    }

    // ----------------------------------------------------------------
    // Step 8: Write per-chain config files into output directory
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
            genesis_path,
            ops.chain_id,
            &ops.commit_pk,
            &ops.prove_pk,
            &ops.execute_pk,
        );
        if ops.chain_id == GATEWAY.id || settles_on_gateway {
            builder = builder.forced_price(&zk_token_address, ZK_FORCED_PRICE_USD);
        }
        if settles_on_gateway {
            builder = builder.gateway("RUNTIME", GATEWAY.id);
        }
        fs::write(
            contracts_backend
                .work_dir()
                .join(format!("{}.yaml", ops.dir_name)),
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

    // wallets.yaml was already copied next to ecosystem.yaml before chain-init.

    // ----------------------------------------------------------------
    // Step 9: Start gateway server
    // ----------------------------------------------------------------
    println!("\n=== Starting gateway server (chain {}) ===", GATEWAY.id);
    let gw_config_path = contracts_backend
        .work_dir()
        .join(format!("{}.yaml", gw_ops.dir_name));

    let anvil_handle = integration_tests::anvil::Anvil::wrap_external(anvil_port);
    let gw_rocks_db = contracts_backend.work_dir().join("gateway_rocksdb");
    // Remove stale RocksDB from a previous run so the server starts fresh
    // against the newly-deployed L1 contracts.
    if gw_rocks_db.exists() {
        fs::remove_dir_all(&gw_rocks_db).context("remove stale gateway_rocksdb")?;
    }
    let gw_server = integration_tests::server::ServerBuilder::new(preset.clone(), GATEWAY.name)
        .config_path(&gw_config_path)
        .rocks_db_path(&gw_rocks_db)
        .spawn(&anvil_handle)
        .context("Failed to start gateway server")?;
    let gw_l2_rpc = gw_server.rpc_url();
    println!("  Gateway server ready at {gw_l2_rpc}");

    // ----------------------------------------------------------------
    // Step 10: Fund gateway L2 (test account + gateway operators)
    // ----------------------------------------------------------------
    println!("\n=== Funding gateway L2 ===");

    // The gateway uses ZK base token. Mint ZK tokens to the test account
    // and approve the NTV so L1→L2 deposits can pay in ZK.
    let test_address = address_from_private_key(DEFAULT_ANVIL_PRIVATE_KEY)?;
    contracts_backend
        .cast(&[
            "send",
            &zk_token_address,
            "mint(address,uint256)",
            &test_address,
            ZK_MINT_AMOUNT_LARGE,
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
            ZK_MINT_AMOUNT_LARGE,
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
        .wait_for_traffic_tx_executed_on_l1()
        .context("gateway executed batches")?;

    // ----------------------------------------------------------------
    // Step 12: Convert gateway chain
    // ----------------------------------------------------------------
    println!("\n=== Converting chain {} to gateway ===", GATEWAY.id);

    // Direct `chain gateway convert` — one protocol-ops invocation runs all
    // five stages (deploy-filterer + grant-whitelist + vote-prepare +
    // governance-execute + revoke-whitelist) against a single anvil fork and
    // emits one Safe bundle directory. Must be under `script-out/…` — the
    // forge script has fs_permissions restricted to that subtree.
    let vote_output_path_rel = "script-out/gateway_vote_prep_out.toml".to_string();
    {
        let gateway_id_str = GATEWAY.id.to_string();
        let convert_rel = format!("generate_l1_state/gateway_convert_{}", GATEWAY.id);
        let convert_safe_rel = format!("{convert_rel}/safe");
        let convert_safe_abs = contracts_backend.work_path(&convert_safe_rel);
        contracts_backend.protocol_ops(&[
            "chain",
            "gateway",
            "convert",
            "--l1-rpc-url",
            l1_rpc_url,
            "--ecosystem",
            &eco_path,
            "--chain",
            "gateway",
            "--gateway-deployer",
            &keys.deployer_addr,
            "--ctm-representative-chain-id",
            &gateway_id_str,
            "--vote-preparation-toml",
            &vote_output_path_rel,
            "--out",
            &convert_safe_abs,
        ])?;
        contracts_backend
            .parse_safe_bundles(&convert_safe_rel, l1_rpc_url)?
            .apply(&[
                &gw_ops.owner_pk,
                &keys.deployer_pk,
                &keys.ecosystem_owner_pk,
            ])
            .context("apply gateway convert Safe bundles")?;
    }

    // ----------------------------------------------------------------
    // Step 13: Gateway-settling chains — migrate, finalize, enable validators
    //
    // Three `chain gateway migrate-to` phase commands, one per phase:
    //   phase-1-submit:      notify-server + submit       (chain admin)
    //   phase-2-finalize:    finalize                     (deployer)
    //   phase-3-validators:  enable-validators + set-da   (chain admin)
    //
    // Phases 1+2 run per-chain (bundle 1 is applied to real L1 before
    // bundle 2 is simulated — finalize needs the submit priority tx on L1).
    // Phase 3 runs per-chain after the gateway has stabilised.
    // ----------------------------------------------------------------

    // Read deployment artifacts from the vote preparation output
    // (written by GatewayVotePreparation.s.sol during convert-to-gateway).
    let gw_vote_toml =
        contracts_backend.read_repo_file("l1-contracts/script-out/gateway_vote_prep_out.toml")?;
    let vote_prep: VotePrepOutput =
        toml::from_str(&gw_vote_toml).context("parse vote preparation output TOML")?;
    let relayed_sl_da_validator = vote_prep.relayed_sl_da_validator;
    println!("  Gateway relayed SL DA validator: {relayed_sl_da_validator}");

    // Save diamond cut data to a file so tests can pass it via
    // --l1-diamond-cut-data (Anvil state dumps don't preserve historical
    // events, so the auto-resolution from NewUpgradeCutData events doesn't
    // work). The format matches what resolve_l1_diamond_cut_data returns.
    let diamond_cut_data_path = contracts_backend.work_dir().join("diamond_cut_data.hex");
    fs::write(&diamond_cut_data_path, &vote_prep.diamond_cut_data)?;
    println!(
        "  diamond_cut_data.hex -> {}",
        diamond_cut_data_path.display()
    );

    // Cache the full vote-prep TOML so skip-generate tests (e.g. live
    // migrate-to-gateway) can stage it into the era-contracts script-out
    // directory before invoking the migrate-to phase commands.
    let vote_prep_toml_path = contracts_backend
        .work_dir()
        .join("gateway_vote_prep_out.toml");
    fs::write(&vote_prep_toml_path, &gw_vote_toml)?;
    println!(
        "  gateway_vote_prep_out.toml -> {}",
        vote_prep_toml_path.display()
    );

    // 13a: Phases 1 + 2 per chain.
    let gateway_chain_id_str = GATEWAY.id.to_string();
    let l1_gas_price_str = MIGRATE_L1_GAS_PRICE_WEI.to_string();
    for ops in gw_settling_ops {
        let chain_id = ops.chain_id;
        println!("\n=== Migrating chain {} to gateway ===", chain_id);
        let migrate_dir = format!("migrate_{chain_id}");
        let signers: &[&str] = &[&ops.owner_pk, &keys.deployer_pk];

        // Phase 1: notify-server → submit (chain admin), one Safe bundle
        // emitted directly by `chain gateway migrate-to phase-1-submit`.
        let phase1_safe_rel = format!("{migrate_dir}/phase1/safe");
        let phase1_safe_abs = contracts_backend.work_path(&phase1_safe_rel);
        contracts_backend.protocol_ops(&[
            "chain",
            "gateway",
            "migrate-to",
            "phase-1-submit",
            "--l1-rpc-url",
            l1_rpc_url,
            "--ecosystem",
            &eco_path,
            "--chain",
            &ops.dir_name,
            "--gateway-chain-id",
            &gateway_chain_id_str,
            "--l1-gas-price",
            &l1_gas_price_str,
            "--vote-preparation-toml",
            &vote_output_path_rel,
            "--refund-recipient",
            &keys.deployer_addr,
            "--out",
            &phase1_safe_abs,
        ])?;
        contracts_backend
            .parse_safe_bundles(&phase1_safe_rel, l1_rpc_url)?
            .apply(signers)
            .context("apply migrate phase 1 bundles")?;

        // Phase 2: finalize (deployer) — forks real L1 post-phase-1.
        let phase2_safe_rel = format!("{migrate_dir}/phase2/safe");
        let phase2_safe_abs = contracts_backend.work_path(&phase2_safe_rel);
        contracts_backend.protocol_ops(&[
            "chain",
            "gateway",
            "migrate-to",
            "phase-2-finalize",
            "--l1-rpc-url",
            l1_rpc_url,
            "--ecosystem",
            &eco_path,
            "--chain",
            &ops.dir_name,
            "--deployer-address",
            &keys.deployer_addr,
            "--gateway-rpc-url",
            &gw_l2_rpc,
            "--vote-preparation-toml",
            &vote_output_path_rel,
            "--out",
            &phase2_safe_abs,
        ])?;
        contracts_backend
            .parse_safe_bundles(&phase2_safe_rel, l1_rpc_url)?
            .apply(signers)
            .context("apply migrate phase 2 bundles")?;
    }

    // 13b: Wait for gateway to process all migration L1->L2 priority txs
    println!("\n=== Waiting for gateway to process chain migrations ===");
    gw_server
        .wait_for_traffic_tx_executed_on_l1()
        .context("gateway batches after migration")?;

    // 13d: Phase 3 per chain — enable validators + set DA validator pairs,
    // then fund operators on gateway L2.
    let l1_gas_price_str = MIGRATE_L1_GAS_PRICE_WEI.to_string();
    for ops in gw_settling_ops {
        let chain_id = ops.chain_id;
        println!(
            "\n=== Enabling validators for chain {} on gateway ===",
            chain_id
        );
        let migrate_dir = format!("migrate_{chain_id}");

        let phase3_safe_rel = format!("{migrate_dir}/phase3/safe");
        let phase3_safe_abs = contracts_backend.work_path(&phase3_safe_rel);
        contracts_backend.protocol_ops(&[
            "chain",
            "gateway",
            "migrate-to",
            "phase-3-validators",
            "--l1-rpc-url",
            l1_rpc_url,
            "--ecosystem",
            &eco_path,
            "--chain",
            &ops.dir_name,
            "--gateway-rpc-url",
            &gw_l2_rpc,
            "--commit-operator",
            &ops.commit_addr,
            "--prove-operator",
            &ops.prove_addr,
            "--execute-operator",
            &ops.execute_addr,
            "--l1-da-validator",
            &relayed_sl_da_validator,
            "--l2-da-commitment-scheme",
            "blobs-and-pubdata-keccak256",
            "--l1-gas-price",
            &l1_gas_price_str,
            "--out",
            &phase3_safe_abs,
        ])?;
        contracts_backend
            .parse_safe_bundles(&phase3_safe_rel, l1_rpc_url)?
            .apply(&[&ops.owner_pk, &keys.deployer_pk])
            .context("apply migrate phase 3 bundles")?;

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
        let deadline = std::time::Instant::now() + integration_tests::DEFAULT_WAIT_TIMEOUT;
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
            println!("  Gateway priority queue size: {queue_size}, waiting...");
            std::thread::sleep(Duration::from_secs(1));
        }
        // Now wait for enough batches to be executed on L1 so the state is visible
        gw_server
            .wait_for_traffic_tx_executed_on_l1()
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
    //
    // Write the archive directly to the persistent cache dir (`output_dir`),
    // not to the transient work_dir: gateway.yaml embeds the archive path
    // below, and `run-tests.sh` wipes work_dir contents between runs — if
    // the archive lived in work_dir, a subsequent `--skip-generate` test run
    // would find a dangling reference. `output_dir` is the same cache dir
    // whose files survive across runs and back `resolve_ecosystem_dir`.
    let gw_state_archive = output_dir.join("l1-state.gateway-state.tar.gz");
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
        contracts_backend
            .work_dir()
            .join(format!("{}.yaml", gw_ops.dir_name)),
        integration_tests::server_config::ServerConfigBuilder::new(
            &bridgehub,
            &bytecodes_supplier,
            genesis_path,
            GATEWAY.id,
            &gw_ops.commit_pk,
            &gw_ops.prove_pk,
            &gw_ops.execute_pk,
        )
        .ephemeral(gw_state_archive_abs.to_string_lossy())
        .forced_price(&zk_token_address, ZK_FORCED_PRICE_USD)
        .build(),
    )?;
    println!("  Updated {}.yaml with ephemeral state", gw_ops.dir_name);

    // ecosystem.yaml is already written and doesn't need further updates —
    // the gateway ephemeral state path lives in the gateway's chain config YAML.
    println!("  ecosystem.yaml finalized");

    Ok(FlowResult {})
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let preset = load_preset(&args)?;
    integration_tests::server::get_or_create_run_id("generate_l1_state");

    // ── Determine output directory ───────────────────────────────────────
    let output_dir = integration_tests::l1_state::cache_dir_for_preset(&preset)?;
    let metadata_path = output_dir.join("metadata.json");

    // ── Cache check ──────────────────────────────────────────────────────
    // Only use cached state when both repo refs are non-local (DockerTag),
    // since local paths may have changed since the last generation.
    let cacheable = matches!(preset.era_contracts, RepoRef::DockerTag { .. })
        && matches!(preset.zksync_os_server, RepoRef::DockerTag { .. });
    if cacheable && metadata_path.exists() {
        // Verify the cached state was generated with the same image tags.
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

    // Create the era-contracts execution backend (local or Docker session).
    let contracts_backend = EraContractsBackend::from_preset(&preset, "generate_l1_state", &[])?;
    let output_path = contracts_backend.work_dir().join("l1-state.json");
    let work_dir = contracts_backend.work_dir();
    println!("Work directory: {}", work_dir.display());

    // Clean l1-contracts/script-out/ in local mode. Forge scripts write TOML/JSON
    // dumps there (e.g. force-deployments-dump.toml), and leftover files from an
    // earlier run can be picked up by the current run, producing subtle
    // cross-run contamination (e.g. genesis-hash mismatches). Docker mode starts
    // a fresh container each run, so script-out is already pristine there.
    if let Some(era_path) = contracts_backend.era_path() {
        let script_out = era_path.join("l1-contracts").join("script-out");
        if script_out.exists() {
            for entry in fs::read_dir(&script_out)? {
                let entry = entry?;
                if entry.file_name() == ".gitkeep" {
                    continue;
                }
                let path = entry.path();
                if path.is_dir() {
                    fs::remove_dir_all(&path)?;
                } else {
                    fs::remove_file(&path)?;
                }
            }
        }
    }

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
        let w = wallets
            .chains
            .get(spec.name)
            .ok_or_else(|| anyhow::anyhow!("wallets.yaml missing chain '{}'", spec.name))?;
        ChainOperators::from_wallets(spec.id, spec.name, w)
    };
    let gw_ops = ops_for(&GATEWAY)?;
    let gw_settling_ops: Vec<ChainOperators> = gateway_settling_chains()
        .map(ops_for)
        .collect::<Result<_>>()?;
    let l1_settling_ops: Vec<ChainOperators> =
        l1_settling_chains().map(ops_for).collect::<Result<_>>()?;

    // Contracts (`yarn build-all-contracts`) and server / protocol_ops
    // binaries are built by `integration-tests/build.rs` at cargo compile
    // time — by the time this tool starts, everything is up-to-date.
    if era_local_path(&preset).is_some() {
        println!("\n=== Contracts built by integration-tests/build.rs ===");
    } else {
        println!("\n=== Using Docker image for contracts ===");
    }

    // Regenerate configs/genesis/zksync-os/latest.json from current bytecodes
    // *before* any forge script runs. `DeployCTM.s.sol` reads `genesis_root`
    // out of that file and bakes it into the CTM on L1; if genesis-gen ran
    // after ecosystem init, L1 would be registered with a stale root and the
    // server's freshly-computed root would mismatch on startup.
    println!("\n=== Generating genesis.json ===");
    let genesis_path = run_genesis_gen(&contracts_backend)?.canonicalize()?;

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
        &preset,
        anvil_port,
        &genesis_path,
    )
    .await;

    // ----------------------------------------------------------------
    // Step 16: Stop Anvil (triggers state dump)
    // ----------------------------------------------------------------
    println!("\n=== Stopping Anvil (dumping state) ===");
    anvil.terminate()?;

    // Propagate any error from the main flow
    let _flow = result?;

    if !output_path.exists() {
        anyhow::bail!("Anvil did not write state file: {}", output_path.display());
    }
    let file_size = fs::metadata(&output_path)?.len();
    println!(
        "\nState file: {} ({:.1} MB)",
        output_path.display(),
        file_size as f64 / 1_048_576.0
    );

    // Copy cacheable artifacts from the transient work_dir into output_dir.
    // Subdirectories (safe-bundle dirs, rocksdb, etc.) are ephemeral — only
    // top-level regular files are persisted.
    let work_dir = contracts_backend.work_dir();
    for entry in fs::read_dir(work_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let src = entry.path();
        let dst = output_dir.join(entry.file_name());
        fs::copy(&src, &dst)
            .with_context(|| format!("copy {} to {}", src.display(), dst.display()))?;
    }

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
