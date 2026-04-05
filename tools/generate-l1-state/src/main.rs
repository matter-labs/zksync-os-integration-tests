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
use integration_tests::keys_from_seed::{
    operator_commit_private_key, operator_execute_private_key, operator_prove_private_key,
};
use integration_tests::presets::RepoRef;
use integration_tests::server_utils::{
    address_from_private_key, fund_l2_via_l1_deposit, wait_for_executed_batches_with_traffic,
};

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

const GATEWAY_CHAIN_ID: u64 = 506;
const GATEWAY_SETTLING_CHAIN_IDS: &[u64] = &[6566, 6567];
const L1_SETTLING_CHAIN_IDS: &[u64] = &[6565];

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

use integration_tests::anvil::DEFAULT_ANVIL_PRIVATE_KEY;

/// Anvil-typical L1 gas price (wei) for migration calldata.
const MIGRATE_L1_GAS_PRICE_WEI: u64 = 1_000_000_000;

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

        // Wait for Anvil to be ready
        std::thread::sleep(Duration::from_secs(2));
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
    commit_pk: String,
    prove_pk: String,
    execute_pk: String,
    commit_addr: String,
    prove_addr: String,
    execute_addr: String,
}

impl ChainOperators {
    fn new(chain_id: u64, seed_name: &str, dir_name: &str) -> Result<Self> {
        let commit_pk = operator_commit_private_key(seed_name);
        let prove_pk = operator_prove_private_key(seed_name);
        let execute_pk = operator_execute_private_key(seed_name);
        let commit_addr = address_from_private_key(&commit_pk)?;
        let prove_addr = address_from_private_key(&prove_pk)?;
        let execute_addr = address_from_private_key(&execute_pk)?;
        Ok(Self {
            chain_id,
            dir_name: dir_name.to_string(),
            commit_pk,
            prove_pk,
            execute_pk,
            commit_addr,
            prove_addr,
            execute_addr,
        })
    }

    fn all_addresses(&self) -> [&str; 3] {
        [&self.commit_addr, &self.prove_addr, &self.execute_addr]
    }
}

// ---------------------------------------------------------------------------
// Forge helpers
// ---------------------------------------------------------------------------

fn run_forge_deploy_and_set_gateway_transaction_filterer(
    contracts_backend: &EraContractsBackend,
    l1_rpc_url: &str,
    bridgehub: &str,
    chain_id: u64,
) -> Result<()> {
    println!("  Forge: DeployAndSetGatewayTransactionFilterer for chain {chain_id}");
    contracts_backend.forge_script(
        &[
            "deploy-scripts/dev/DeployAndSetGatewayTransactionFilterer.s.sol:DeployAndSetGatewayTransactionFilterer",
            "--sig", "run(address,uint256)",
            bridgehub,
            &chain_id.to_string(),
            "--rpc-url", l1_rpc_url,
            "--broadcast", "--ffi",
            "--private-key", DEFAULT_ANVIL_PRIVATE_KEY,
        ],
        &[],
    ).with_context(|| format!("DeployAndSetGatewayTransactionFilterer for chain {chain_id}"))?;
    Ok(())
}

#[derive(serde::Deserialize)]
struct ForceDeploymentsDumpToml {
    force_deployments_data: String,
}

#[derive(serde::Deserialize)]
struct VotePrepOutput {
    relayed_sl_da_validator: String,
}

/// Dump force deployments data.
/// Output lands in `work_dir/script-out/` for both local and Docker modes.
fn run_forge_dump_force_deployments(
    contracts_backend: &EraContractsBackend,
    l1_rpc_url: &str,
    ctm_proxy: &str,
) -> Result<String> {
    println!("  Forge: DumpForceDeploymentsForGateway");
    let dump_filename = "force_deployments_dump.toml";
    let dump_rel = format!("/script-out/{}", dump_filename);
    contracts_backend.forge_script(
        &[
            "deploy-scripts/dev/DumpForceDeploymentsForGateway.s.sol:DumpForceDeploymentsForGateway",
            "--sig", "run(address)",
            ctm_proxy,
            "--rpc-url", l1_rpc_url,
        ],
        &[("FORCE_DEPLOYMENTS_DUMP_TOML_REL_PATH", &dump_rel)],
    )?;
    let host_path = contracts_backend
        .work_dir()
        .join("script-out")
        .join(dump_filename);
    let raw =
        fs::read_to_string(&host_path).with_context(|| format!("read {}", host_path.display()))?;
    let parsed: ForceDeploymentsDumpToml = toml::from_str(&raw)?;
    Ok(parsed.force_deployments_data)
}

// ---------------------------------------------------------------------------
// protocol_ops wrappers
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn run_convert_to_gateway(
    contracts_backend: &EraContractsBackend,
    l1_rpc_url: &str,
    bridgehub: &str,
    gateway_chain_id: u64,
    governance_addr: &str,
    deployer_addr: &str,
    stm_tracker: &str,
    force_deployments_data: &str,
    vote_output_path: &str,
) -> Result<()> {
    let gw_str = gateway_chain_id.to_string();

    println!("  convert-to-gateway: grant-whitelist");
    contracts_backend
        .protocol_ops(&[
            "chain",
            "convert-to-gateway",
            "--stage",
            "grant-whitelist",
            "--l1-rpc-url",
            l1_rpc_url,
            "--private-key",
            DEFAULT_ANVIL_PRIVATE_KEY,
            "--bridgehub-proxy-address",
            bridgehub,
            "--gateway-chain-id",
            &gw_str,
            "--whitelist-grantees",
            governance_addr,
            "--whitelist-grantees",
            deployer_addr,
            "--whitelist-grantees",
            stm_tracker,
        ])
        .context("convert-to-gateway grant-whitelist")?;

    println!("  convert-to-gateway: vote-prepare");
    contracts_backend
        .protocol_ops(&[
            "chain",
            "convert-to-gateway",
            "--stage",
            "vote-prepare",
            "--l1-rpc-url",
            l1_rpc_url,
            "--private-key",
            DEFAULT_ANVIL_PRIVATE_KEY,
            "--bridgehub-proxy-address",
            bridgehub,
            "--gateway-chain-id",
            &gw_str,
            "--ctm-representative-chain-id",
            &gw_str,
            "--force-deployments-data",
            force_deployments_data,
            "--refund-recipient",
            deployer_addr,
            "--testnet-verifier",
            "true",
            "--is-zk-sync-os",
            "true",
            "--vote-preparation-output-path",
            vote_output_path,
        ])
        .context("convert-to-gateway vote-prepare")?;

    println!("  convert-to-gateway: governance-execute");
    contracts_backend
        .protocol_ops(&[
            "chain",
            "convert-to-gateway",
            "--stage",
            "governance-execute",
            "--l1-rpc-url",
            l1_rpc_url,
            "--private-key",
            DEFAULT_ANVIL_PRIVATE_KEY,
            "--bridgehub-proxy-address",
            bridgehub,
            "--gateway-chain-id",
            &gw_str,
            "--governance-address",
            governance_addr,
            "--vote-preparation-output-path",
            vote_output_path,
        ])
        .context("convert-to-gateway governance-execute")?;

    println!("  convert-to-gateway: revoke-whitelist");
    contracts_backend
        .protocol_ops(&[
            "chain",
            "convert-to-gateway",
            "--stage",
            "revoke-whitelist",
            "--l1-rpc-url",
            l1_rpc_url,
            "--private-key",
            DEFAULT_ANVIL_PRIVATE_KEY,
            "--bridgehub-proxy-address",
            bridgehub,
            "--gateway-chain-id",
            &gw_str,
            "--revoke-address",
            deployer_addr,
        ])
        .context("convert-to-gateway revoke-whitelist")?;

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_migrate_to_gateway(
    contracts_backend: &EraContractsBackend,
    l1_rpc_url: &str,
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
        println!("  migrate-to-gateway chain {chain_id}: pause-deposits");
        contracts_backend
            .protocol_ops(&[
                "chain",
                "migrate-to-gateway",
                "--stage",
                "pause-deposits",
                "--l1-rpc-url",
                l1_rpc_url,
                "--private-key",
                DEFAULT_ANVIL_PRIVATE_KEY,
                "--bridgehub-proxy-address",
                bridgehub,
                "--chain-id",
                &chain_str,
            ])
            .context("migrate-to-gateway pause-deposits")?;
    }

    println!("  migrate-to-gateway chain {chain_id}: migrate");
    contracts_backend
        .protocol_ops(&[
            "chain",
            "migrate-to-gateway",
            "--stage",
            "migrate",
            "--l1-rpc-url",
            l1_rpc_url,
            "--private-key",
            DEFAULT_ANVIL_PRIVATE_KEY,
            "--bridgehub-proxy-address",
            bridgehub,
            "--chain-id",
            &chain_str,
            "--gateway-chain-id",
            &gw_str,
            "--l1-gas-price",
            &gas_str,
            "--vote-preparation-output-path",
            vote_output_rel,
            "--refund-recipient",
            refund_recipient,
        ])
        .context("migrate-to-gateway migrate")?;

    println!("  migrate-to-gateway chain {chain_id}: notify-server");
    contracts_backend
        .protocol_ops(&[
            "chain",
            "migrate-to-gateway",
            "--stage",
            "notify-server",
            "--l1-rpc-url",
            l1_rpc_url,
            "--private-key",
            DEFAULT_ANVIL_PRIVATE_KEY,
            "--bridgehub-proxy-address",
            bridgehub,
            "--chain-id",
            &chain_str,
        ])
        .context("migrate-to-gateway notify-server")?;

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
    let output = build_cmd
        .output()
        .with_context(|| format!("cargo build {}", tool_subdir))?;
    if !output.status.success() {
        anyhow::bail!(
            "cargo build {} failed:\n{}",
            tool_subdir,
            String::from_utf8_lossy(&output.stderr)
        );
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
fn run_generation_flow(
    contracts_backend: &EraContractsBackend,
    l1_rpc_url: &str,
    gw_ops: &ChainOperators,
    gw_settling_ops: &[ChainOperators],
    l1_settling_ops: &[ChainOperators],
    output_dir: &Path,
    output_path: &Path,
    preset: &integration_tests::presets::Preset,
    anvil_port: u16,
) -> Result<FlowResult> {
    // ----------------------------------------------------------------
    // Step 3: Ecosystem init
    // ----------------------------------------------------------------
    println!("\n=== protocol_ops ecosystem init ===");
    let ecosystem_out_arg = contracts_backend.work_path("ecosystem_init_out.json");
    contracts_backend.protocol_ops(&[
        "ecosystem",
        "init",
        "--l1-rpc-url",
        l1_rpc_url,
        "--private-key",
        DEFAULT_ANVIL_PRIVATE_KEY,
        "--out",
        &ecosystem_out_arg,
    ])?;

    let ecosystem_json: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(contracts_backend.work_dir().join("ecosystem_init_out.json"))
            .context("read ecosystem init output")?,
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

    if l1_da_validator.is_empty()
        || l1_da_validator == "0x0000000000000000000000000000000000000000"
    {
        anyhow::bail!("L1 DA validator address is empty or zero");
    }

    println!("  bridgehub = {bridgehub}");
    println!("  ctm_proxy = {ctm_proxy}");
    println!("  bytecodes_supplier = {bytecodes_supplier}");

    // ----------------------------------------------------------------
    // Step 4: Chain init — gateway chain
    // ----------------------------------------------------------------
    println!(
        "\n=== protocol_ops chain init: gateway chain {} ===",
        GATEWAY_CHAIN_ID
    );
    let gw_chain_out_arg = contracts_backend.work_path("chain_init_gateway.json");
    contracts_backend.protocol_ops(&[
        "chain",
        "init",
        "--ctm-proxy",
        &ctm_proxy,
        "--l1-da-validator",
        &l1_da_validator,
        "--commit-operator",
        &gw_ops.commit_addr,
        "--prove-operator",
        &gw_ops.prove_addr,
        "--execute-operator",
        &gw_ops.execute_addr,
        "--chain-id",
        &GATEWAY_CHAIN_ID.to_string(),
        "--l1-rpc-url",
        l1_rpc_url,
        "--private-key",
        DEFAULT_ANVIL_PRIVATE_KEY,
        "--out",
        &gw_chain_out_arg,
    ])?;
    let gw_chain_json: serde_json::Value = serde_json::from_str(&fs::read_to_string(
        contracts_backend.work_dir().join("chain_init_gateway.json"),
    )?)?;
    let gw_chain_output = gw_chain_json
        .get("output")
        .ok_or_else(|| anyhow::anyhow!("Missing output"))?;
    let gw_diamond_proxy = extract_json_value(gw_chain_output, "diamond_proxy_addr")?;

    // ----------------------------------------------------------------
    // Step 5: Chain init — gateway-settling chains
    // ----------------------------------------------------------------
    let mut gw_settling_diamond_proxies = Vec::new();
    for ops in gw_settling_ops {
        println!(
            "\n=== protocol_ops chain init: gateway-settling chain {} ===",
            ops.chain_id
        );
        let chain_out_name = format!("chain_init_{}.json", ops.chain_id);
        let chain_out_arg = contracts_backend.work_path(&chain_out_name);
        // --pause-deposits + --skip-priority-txs: keep the priority queue empty
        // so migrate-to-gateway doesn't fail with PriorityQueueNotFullyProcessed.
        // This avoids needing a pre-migration server to drain the queue.
        contracts_backend.protocol_ops(&[
            "chain",
            "init",
            "--ctm-proxy",
            &ctm_proxy,
            "--l1-da-validator",
            &l1_da_validator,
            "--commit-operator",
            &ops.commit_addr,
            "--prove-operator",
            &ops.prove_addr,
            "--execute-operator",
            &ops.execute_addr,
            "--chain-id",
            &ops.chain_id.to_string(),
            "--l1-rpc-url",
            l1_rpc_url,
            "--private-key",
            DEFAULT_ANVIL_PRIVATE_KEY,
            "--pause-deposits",
            "--skip-priority-txs",
            "--out",
            &chain_out_arg,
        ])?;
        let chain_json: serde_json::Value = serde_json::from_str(&fs::read_to_string(
            contracts_backend.work_dir().join(&chain_out_name),
        )?)?;
        let chain_output = chain_json
            .get("output")
            .ok_or_else(|| anyhow::anyhow!("Missing output"))?;
        gw_settling_diamond_proxies
            .push(extract_json_value(chain_output, "diamond_proxy_addr")?);
    }

    // ----------------------------------------------------------------
    // Step 6: Chain init — L1-settling chains
    // ----------------------------------------------------------------
    let mut l1_settling_diamond_proxies = Vec::new();
    for ops in l1_settling_ops {
        println!(
            "\n=== protocol_ops chain init: L1-settling chain {} ===",
            ops.chain_id
        );
        let chain_out_name = format!("chain_init_{}.json", ops.chain_id);
        let chain_out_arg = contracts_backend.work_path(&chain_out_name);
        contracts_backend.protocol_ops(&[
            "chain",
            "init",
            "--ctm-proxy",
            &ctm_proxy,
            "--l1-da-validator",
            &l1_da_validator,
            "--commit-operator",
            &ops.commit_addr,
            "--prove-operator",
            &ops.prove_addr,
            "--execute-operator",
            &ops.execute_addr,
            "--chain-id",
            &ops.chain_id.to_string(),
            "--l1-rpc-url",
            l1_rpc_url,
            "--private-key",
            DEFAULT_ANVIL_PRIVATE_KEY,
            "--out",
            &chain_out_arg,
        ])?;
        let chain_json: serde_json::Value = serde_json::from_str(&fs::read_to_string(
            contracts_backend.work_dir().join(&chain_out_name),
        )?)?;
        let chain_output = chain_json
            .get("output")
            .ok_or_else(|| anyhow::anyhow!("Missing output"))?;
        l1_settling_diamond_proxies
            .push(extract_json_value(chain_output, "diamond_proxy_addr")?);
    }

    // ----------------------------------------------------------------
    // Step 7: Fund all operator accounts on L1
    // ----------------------------------------------------------------
    println!("\n=== Funding L1 operator accounts ===");
    let all_ops: Vec<&ChainOperators> = std::iter::once(gw_ops)
        .chain(gw_settling_ops.iter())
        .chain(l1_settling_ops.iter())
        .collect();
    for ops in &all_ops {
        for addr in ops.all_addresses() {
            fund_account(addr, "100ether", l1_rpc_url, DEFAULT_ANVIL_PRIVATE_KEY)
                .with_context(|| {
                    format!("fund operator {} for chain {}", addr, ops.chain_id)
                })?;
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
    // Gateway config
    fs::write(
        output_dir.join(format!("{}.yaml", gw_ops.dir_name)),
        integration_tests::server_config::ServerConfigBuilder::new(
            &bridgehub,
            &bytecodes_supplier,
            &genesis_path,
            GATEWAY_CHAIN_ID,
            &gw_ops.commit_pk,
            &gw_ops.prove_pk,
            &gw_ops.execute_pk,
        )
        .build(),
    )?;
    println!("  {}.yaml (gateway)", gw_ops.dir_name);

    // Gateway-settling chain configs (gateway_rpc_url set at runtime via env var)
    for ops in gw_settling_ops {
        fs::write(
            output_dir.join(format!("{}.yaml", ops.dir_name)),
            integration_tests::server_config::ServerConfigBuilder::new(
                &bridgehub,
                &bytecodes_supplier,
                &genesis_path,
                ops.chain_id,
                &ops.commit_pk,
                &ops.prove_pk,
                &ops.execute_pk,
            )
            .gateway("RUNTIME", GATEWAY_CHAIN_ID)
            .build(),
        )?;
        println!("  {}.yaml (gateway-settling)", ops.dir_name);
    }

    // L1-settling chain configs
    for ops in l1_settling_ops {
        fs::write(
            output_dir.join(format!("{}.yaml", ops.dir_name)),
            integration_tests::server_config::ServerConfigBuilder::new(
                &bridgehub,
                &bytecodes_supplier,
                &genesis_path,
                ops.chain_id,
                &ops.commit_pk,
                &ops.prove_pk,
                &ops.execute_pk,
            )
            .build(),
        )?;
        println!("  {}.yaml (L1-settling)", ops.dir_name);
    }

    // ----------------------------------------------------------------
    // Step 8c: Generate wallets.yaml
    // ----------------------------------------------------------------
    println!("\n=== Generating wallets.yaml ===");
    let all_chain_names: Vec<&str> = std::iter::once(gw_ops.dir_name.as_str())
        .chain(gw_settling_ops.iter().map(|o| o.dir_name.as_str()))
        .chain(l1_settling_ops.iter().map(|o| o.dir_name.as_str()))
        .collect();
    let chains_arg = all_chain_names.join(",");
    let work_wallets = run_wallets_gen(contracts_backend, &chains_arg)?;
    let wallets_path = output_dir.join("wallets.yaml");
    fs::copy(&work_wallets, &wallets_path)
        .with_context(|| format!("copy wallets to {}", wallets_path.display()))?;
    println!("  wallets.yaml -> {}", wallets_path.display());

    // ----------------------------------------------------------------
    // Step 9: Start gateway server
    // ----------------------------------------------------------------
    println!(
        "\n=== Starting gateway server (chain {}) ===",
        GATEWAY_CHAIN_ID
    );
    let gw_config_path = output_dir.join(format!("{}.yaml", gw_ops.dir_name));

    let anvil_handle = integration_tests::anvil::Anvil::wrap_external(anvil_port);
    let gw_rocks_db = contracts_backend.work_dir().join("gateway_rocksdb");
    let logs_dir = output_dir.join("logs");
    fs::create_dir_all(&logs_dir).context("create logs dir for state generation")?;
    let gw_server =
        integration_tests::server::ServerBuilder::new(preset.clone(), "generate_l1_state")
            .config_path(&gw_config_path)
            .rocks_db_path(&gw_rocks_db)
            .logs_dir(&logs_dir)
            .spawn(&anvil_handle)
            .context("Failed to start gateway server")?;
    let gw_l2_rpc = gw_server.rpc_url();
    println!("  Gateway server ready at {gw_l2_rpc}");

    // ----------------------------------------------------------------
    // Step 10: Fund gateway L2 (test account + gateway operators)
    // ----------------------------------------------------------------
    println!("\n=== Funding gateway L2 ===");
    // Flush docker logs to host before deposit calls so the diagnostic
    // reader can find the file if the server crashes mid-operation.
    let _ = gw_server.save_logs();
    let test_address = address_from_private_key(DEFAULT_ANVIL_PRIVATE_KEY)?;
    fund_l2_via_l1_deposit(
        l1_rpc_url,
        &gw_l2_rpc,
        &bridgehub,
        GATEWAY_CHAIN_ID,
        &test_address,
        0.1,
        Duration::from_secs(120),
        Some(gw_server.logs_path().as_path()),
    )
    .context("fund gateway L2 test account")?;

    for addr_name in ["commit", "prove", "execute"] {
        let addr = match addr_name {
            "commit" => &gw_ops.commit_addr,
            "prove" => &gw_ops.prove_addr,
            "execute" => &gw_ops.execute_addr,
            _ => unreachable!(),
        };
        let _ = gw_server.save_logs();
        fund_l2_via_l1_deposit(
            l1_rpc_url,
            &gw_l2_rpc,
            &bridgehub,
            GATEWAY_CHAIN_ID,
            addr,
            5.0,
            Duration::from_secs(120),
            Some(gw_server.logs_path().as_path()),
        )
        .with_context(|| format!("fund gateway L2 for operator {addr_name}"))?;
    }

    // ----------------------------------------------------------------
    // Step 11: Wait for gateway batches
    // ----------------------------------------------------------------
    println!("\n=== Waiting for gateway batches ===");
    wait_for_executed_batches_with_traffic(
        &gw_l2_rpc,
        l1_rpc_url,
        &gw_diamond_proxy,
        DEFAULT_ANVIL_PRIVATE_KEY,
        3,
        Duration::from_secs(120),
    )
    .context("gateway executed batches")?;

    // ----------------------------------------------------------------
    // Step 12: Convert gateway chain
    // ----------------------------------------------------------------
    println!("\n=== Converting chain {} to gateway ===", GATEWAY_CHAIN_ID);

    let force_hex =
        run_forge_dump_force_deployments(contracts_backend, l1_rpc_url, &ctm_proxy)?;
    let vote_output_path_rel = "/script-out/gateway_vote_prep_out.toml".to_string();
    run_forge_deploy_and_set_gateway_transaction_filterer(
        contracts_backend,
        l1_rpc_url,
        &bridgehub,
        GATEWAY_CHAIN_ID,
    )?;
    run_convert_to_gateway(
        contracts_backend,
        l1_rpc_url,
        &bridgehub,
        GATEWAY_CHAIN_ID,
        &governance_addr,
        &deployer_addr,
        &stm_tracker,
        &force_hex,
        &vote_output_path_rel,
    )?;

    // ----------------------------------------------------------------
    // Step 13: Gateway-settling chains — migrate, finalize, enable validators
    // ----------------------------------------------------------------

    // 13a: Migrate + confirm transfer for all chains
    for ops in gw_settling_ops {
        let chain_id = ops.chain_id;
        println!("\n=== Migrating chain {} to gateway ===", chain_id);
        run_forge_deploy_and_set_gateway_transaction_filterer(
            contracts_backend,
            l1_rpc_url,
            &bridgehub,
            chain_id,
        )?;
        run_migrate_to_gateway(
            contracts_backend,
            l1_rpc_url,
            &bridgehub,
            chain_id,
            GATEWAY_CHAIN_ID,
            &vote_output_path_rel,
            &deployer_addr,
            true,
        )?;
        // Confirm transfer on L1 (finishMigrateChainToGateway only, no validators yet)
        println!("  Confirming transfer for chain {chain_id}");
        contracts_backend
            .protocol_ops(&[
                "chain",
                "finalize-migration-to-gateway",
                "--bridgehub-proxy-address",
                &bridgehub,
                "--chain-id",
                &chain_id.to_string(),
                "--gateway-chain-id",
                &GATEWAY_CHAIN_ID.to_string(),
                "--gateway-rpc-url",
                &gw_l2_rpc,
                "--gateway-diamond-proxy",
                &gw_diamond_proxy,
                "--l1-rpc-url",
                l1_rpc_url,
                "--private-key",
                DEFAULT_ANVIL_PRIVATE_KEY,
                "--vote-preparation-output-path",
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
    {
        let cast_out = contracts_backend.cast(&[
            "call",
            &gw_diamond_proxy,
            "getTotalBatchesExecuted()(uint256)",
            "--rpc-url",
            l1_rpc_url,
        ])?;
        let current = cast_out.trim().parse::<u64>().unwrap_or(0);
        wait_for_executed_batches_with_traffic(
            &gw_l2_rpc,
            l1_rpc_url,
            &gw_diamond_proxy,
            DEFAULT_ANVIL_PRIVATE_KEY,
            current + 3,
            Duration::from_secs(120),
        )
        .context("gateway batches after migration")?;
    }

    // 13c: Resolve gateway ValidatorTimelock (now chains are registered on gateway)
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
    // Both modes write to work_dir/script-out/ (local via sync, Docker via mount).
    let gw_vote_output_file = contracts_backend
        .work_dir()
        .join("script-out/gateway_vote_prep_out.toml");
    let gw_vote_toml = fs::read_to_string(&gw_vote_output_file)
        .with_context(|| format!("read vote prep output {}", gw_vote_output_file.display()))?;
    let vote_prep: VotePrepOutput = toml::from_str(&gw_vote_toml)
        .context("parse vote preparation output TOML")?;
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
            let gw_chain_id_str = GATEWAY_CHAIN_ID.to_string();
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
                "--private-key", DEFAULT_ANVIL_PRIVATE_KEY,
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
        // L2DACommitmentScheme: 3 = BLOBS_AND_PUBDATA_KECCAK256
        let chain_id_str = chain_id.to_string();
        let gw_chain_id_str = GATEWAY_CHAIN_ID.to_string();
        contracts_backend.forge_script(&[
            "deploy-scripts/AdminFunctions.s.sol",
            "--sig", "setDAValidatorPairWithGateway(address,uint256,uint256,uint256,address,uint8,address,address,bool)",
            &bridgehub,
            "1000000000",
            &chain_id_str,
            &gw_chain_id_str,
            &relayed_sl_da_validator,
            "3",
            &chain_diamond_on_gw,
            &deployer_addr,
            "true",
            "--rpc-url", l1_rpc_url,
            "--broadcast", "--ffi",
            "--private-key", DEFAULT_ANVIL_PRIVATE_KEY,
        ], &[])
        .with_context(|| format!("setDAValidatorPairWithGateway for chain {chain_id}"))?;

        println!("  Funding gateway L2 for chain {} operators", chain_id);
        for addr in [&ops.commit_addr, &ops.prove_addr, &ops.execute_addr] {
            let _ = gw_server.save_logs();
            fund_l2_via_l1_deposit(
                l1_rpc_url,
                &gw_l2_rpc,
                &bridgehub,
                GATEWAY_CHAIN_ID,
                addr,
                5.0,
                Duration::from_secs(120),
                Some(gw_server.logs_path().as_path()),
            )
            .context("fund gateway L2 for migrated-chain operator")?;
        }
    }

    // 13e: Wait for gateway to drain its priority queue (validator + DA validator txs)
    println!("\n=== Waiting for gateway to drain priority queue ===");
    {
        let deadline = std::time::Instant::now() + Duration::from_secs(180);
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
            let _ = contracts_backend.cast(&[
                "send",
                "0x0000000000000000000000000000000000000001",
                "--value",
                "1",
                "--private-key",
                DEFAULT_ANVIL_PRIVATE_KEY,
                "--rpc-url",
                &gw_l2_rpc,
            ]);
            std::thread::sleep(Duration::from_secs(3));
        }
        // Now wait for enough batches to be executed on L1 so the state is visible
        let cast_out = contracts_backend.cast(&[
            "call",
            &gw_diamond_proxy,
            "getTotalBatchesExecuted()(uint256)",
            "--rpc-url",
            l1_rpc_url,
        ])?;
        let current = cast_out.trim().parse::<u64>().unwrap_or(0);
        wait_for_executed_batches_with_traffic(
            &gw_l2_rpc,
            l1_rpc_url,
            &gw_diamond_proxy,
            DEFAULT_ANVIL_PRIVATE_KEY,
            current + 3,
            Duration::from_secs(120),
        )
        .context("gateway batches after priority queue drain")?;
    }

    // ----------------------------------------------------------------
    // Step 14: L1-settling chains — submit L1 deposit (no server needed)
    //
    // Unlike gateway-settling chains, L1-settling chains do not require
    // a running server during state generation. We only submit an L1->L2
    // deposit so the priority queue has a transaction ready for the
    // server when the test starts it later.
    // ----------------------------------------------------------------
    for ops in l1_settling_ops {
        let chain_id = ops.chain_id;
        println!("\n=== L1-settling chain {chain_id}: submitting L1 deposit ===");
        let test_addr = address_from_private_key(DEFAULT_ANVIL_PRIVATE_KEY)?;
        integration_tests::l1_l2_deposit::submit_l1_to_l2_deposit_to(
            l1_rpc_url,
            &bridgehub,
            chain_id,
            DEFAULT_ANVIL_PRIVATE_KEY,
            0.1,
            Some(&test_addr),
        )
        .with_context(|| format!("L1 deposit for L1-settling chain {chain_id}"))?;
    }

    // ----------------------------------------------------------------
    // Step 15: Stop gateway server
    // ----------------------------------------------------------------
    println!("\n=== Stopping gateway server ===");
    gw_server
        .kill()
        .context("kill gateway server")?;

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
            GATEWAY_CHAIN_ID,
            &gw_ops.commit_pk,
            &gw_ops.prove_pk,
            &gw_ops.execute_pk,
        )
        .ephemeral(gw_state_archive_abs.to_string_lossy())
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

fn main() -> Result<()> {
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

    let work_name = format!(
        "generate_l1_state_{}",
        uuid::Uuid::new_v4().to_string().get(..8).unwrap_or("run")
    );

    // Create the era-contracts execution backend (local or Docker session).
    let contracts_backend = EraContractsBackend::from_preset(&preset, &work_name, &[])?;
    println!("Work directory: {}", contracts_backend.work_dir().display());
    if let EraContractsBackend::Docker { ref session, .. } = contracts_backend {
        println!(
            "Started era-contracts Docker session: {}",
            session.container_name()
        );
    }

    // Chain operator contexts
    let gw_ops = ChainOperators::new(GATEWAY_CHAIN_ID, "gateway chain", "gateway")?;
    let gw_settling_dir_names = ["gateway_settling_a", "gateway_settling_b"];
    let gw_settling_ops: Vec<ChainOperators> = GATEWAY_SETTLING_CHAIN_IDS
        .iter()
        .enumerate()
        .map(|(i, &id)| {
            ChainOperators::new(id, &format!("chain {}", i + 1), gw_settling_dir_names[i])
        })
        .collect::<Result<_>>()?;
    let l1_settling_ops: Vec<ChainOperators> = L1_SETTLING_CHAIN_IDS
        .iter()
        .enumerate()
        .map(|(i, &id)| ChainOperators::new(id, &format!("l1 chain {}", i + 1), "l1_settling"))
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
        &gw_ops,
        &gw_settling_ops,
        &l1_settling_ops,
        &output_dir,
        &output_path,
        &preset,
        anvil_port,
    );

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
            chain_id: GATEWAY_CHAIN_ID,
            diamond_proxy: flow.gw_diamond_proxy,
            ephemeral_state: flow.gateway_ephemeral_state.to_string_lossy().to_string(),
            name: "gateway".to_string(),
        },
        gateway_settling_chains: GATEWAY_SETTLING_CHAIN_IDS
            .iter()
            .enumerate()
            .map(|(i, &id)| integration_tests::l1_state::ChainMeta {
                chain_id: id,
                diamond_proxy: flow.gw_settling_diamond_proxies[i].clone(),
                ephemeral_state: None,
                name: gw_settling_dir_names[i].to_string(),
            })
            .collect(),
        l1_settling_chains: L1_SETTLING_CHAIN_IDS
            .iter()
            .enumerate()
            .map(|(i, &id)| integration_tests::l1_state::ChainMeta {
                chain_id: id,
                diamond_proxy: flow.l1_settling_diamond_proxies[i].clone(),
                ephemeral_state: None,
                name: "l1_settling".to_string(),
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
