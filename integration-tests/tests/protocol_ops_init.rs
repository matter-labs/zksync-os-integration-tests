//! Integration test: initialize L1 via protocol-ops, start server, execute batches.
//!
//! Two chains share one ecosystem: a gateway chain and a second chain migrated onto it via
//! `protocol_ops chain migrate-to-gateway`. The **same** gateway server process stays up through
//! `convert-to-gateway` and migration so its block replay DB stays consistent with L1 (a fresh
//! server would start with empty replay storage while L1 already has commits). Before migrating,
//! a short-lived L2 server for the second chain runs on L1 (Blobs) so priority-queue ops are
//! processed (`PriorityQueueNotFullyProcessed` otherwise). That server starts before the gateway
//! node so the two processes do not share conflicting auxiliary ports. After migration, a server
//! with `general.gateway_rpc_url` points at the gateway L2 RPC. Pre- and post-migration servers
//! for that chain share one RocksDB dir so replay state matches L1 after migration.
//!
//! Mirrors the L1 setup from update_server.py but uses protocol-ops instead of zkstack.
//! Genesis is generated via protocol-ops genesis gen (same as update_server).
//!
//! update_server.py steps the test does NOT do (they update the server repo, not needed for test):
//! - Build zkstack CLI (test uses protocol-ops)
//! - Update VK hash in proving_version.rs
//! - Regenerate contracts.json (factory deps) — server binary already has it
//!
//! Run with: `cargo test --test protocol_ops_init test_protocol_ops_init_full_flow -- --ignored --nocapture`

use anyhow::{Context, Result};
use integration_tests::anvil::Anvil;
use integration_tests::anvil_utils::fund_account;
use integration_tests::keys_from_seed::{
    era_validator_private_key, operator_commit_private_key, operator_execute_private_key,
    operator_prove_private_key,
};
use integration_tests::presets::{load_default_presets, Preset, RepoRef};
use integration_tests::protocol_ops::protocol_ops_logs_dir;
use integration_tests::server::{read_toolchain_from_dir, ServerBuilder};
use integration_tests::server_utils::{
    address_from_private_key, fund_l2_via_l1_deposit, wait_for_executed_batches_with_traffic,
    wait_for_l1_committed_equals_executed, DEFAULT_TEST_PRIVATE_KEY,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// Anvil account #0 (deployer/owner for ecosystem and chain init)
const DEFAULT_ANVIL_PRIVATE_KEY: &str =
    "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
/// L1 chain that becomes the gateway (settlement layer).
const GATEWAY_CHAIN_ID: u64 = 6565;
/// Second ZK chain created on the same bridgehub, then migrated to settle on `GATEWAY_CHAIN_ID`.
const MIGRATED_CHAIN_ID: u64 = 8787;
/// Seeds for `keys_from_seed::operator_*_private_key` — separate L1 operator keys per chain.
const OPERATOR_CHAIN_GATEWAY: &str = "gateway chain";
const OPERATOR_CHAIN_MIGRATED: &str = "chain 1";
/// Anvil-typical L1 gas price (wei) for AdminFunctions migration calldata.
const MIGRATE_L1_GAS_PRICE_WEI: u64 = 1_000_000_000;
/// Non-default ports so the migrated chain node can run next to the gateway node locally.
const MIGRATED_NODE_STATUS_SERVER_PORT: u16 = 3072;
const MIGRATED_NODE_PROVER_API_PORT: u16 = 3125;
const MIGRATED_NODE_PROMETHEUS_PORT: u16 = 3313;

fn extract_json_value(obj: &serde_json::Value, path: &str) -> Result<String> {
    let mut v = obj;
    for key in path.split('.') {
        v = v.get(key).ok_or_else(|| {
            anyhow::anyhow!(
                "Missing key {:?} in path {:?}\nAvailable keys: {:?}\nFull JSON:\n{}",
                key,
                path,
                v.as_object().map(|o| o.keys().collect::<Vec<_>>()),
                serde_json::to_string_pretty(obj).unwrap_or_default()
            )
        })?;
    }
    v.as_str()
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("Key {:?} is not a string", path))
}

fn get_v31_preset() -> Result<Preset> {
    let presets = load_default_presets().context("Failed to load presets")?;
    presets
        .get("v31_draft")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("v31_draft preset not found in presets.yaml"))
}

fn get_era_contracts_path(preset: &Preset) -> Result<PathBuf> {
    match &preset.era_contracts {
        RepoRef::Path(path) => Ok(path.clone()),
        RepoRef::DockerTag(_) => anyhow::bail!(
            "protocol_ops_init test requires local era-contracts path (v31_draft preset)"
        ),
    }
}

fn build_contracts(era_path: &Path) -> Result<()> {
    println!("\n=== Building Contracts ===");
    let status = Command::new("yarn")
        .args(["build-all-contracts"])
        .current_dir(era_path)
        .status()
        .context("Failed to run yarn build-all-contracts")?;
    if !status.success() {
        anyhow::bail!("yarn build-all-contracts failed");
    }
    println!("✓ Contracts build completed");
    Ok(())
}

fn run_genesis_gen(era_path: &Path, output_path: &Path) -> Result<()> {
    println!("\n=== Generating genesis.json ===");
    let genesis_tool_dir = era_path.join("tools/zksync-os-genesis-gen");
    let manifest = genesis_tool_dir.join("Cargo.toml");
    if !manifest.exists() {
        anyhow::bail!("zksync-os-genesis-gen not found at {}", manifest.display());
    }

    let mut build_cmd = Command::new("cargo");
    build_cmd
        .args([
            "build",
            "--release",
            "--manifest-path",
            manifest.to_str().unwrap(),
        ])
        .current_dir(&genesis_tool_dir);
    if let Some(toolchain) = read_toolchain_from_dir(&genesis_tool_dir) {
        build_cmd.env("RUSTUP_TOOLCHAIN", &toolchain);
    }
    let build_output = build_cmd
        .output()
        .context("Failed to run cargo build for zksync-os-genesis-gen")?;
    if !build_output.status.success() {
        let stderr = String::from_utf8_lossy(&build_output.stderr);
        anyhow::bail!("cargo build for zksync-os-genesis-gen failed:\n{}", stderr);
    }

    let binary = genesis_tool_dir
        .join("target/release/zksync-os-genesis-gen")
        .with_extension(std::env::consts::EXE_EXTENSION);
    let output = Command::new(&binary)
        .args(["--output-file", output_path.to_str().unwrap()])
        .current_dir(&genesis_tool_dir)
        .output()
        .with_context(|| format!("Failed to execute {}", binary.display()))?;
    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "zksync-os-genesis-gen failed with status: {}\n\nSTDOUT:\n{}\n\nSTDERR:\n{}",
            output.status,
            stdout,
            stderr
        );
    }
    println!("✓ Genesis generated at {}", output_path.display());
    Ok(())
}

fn l1_path_rel(trim_leading_slash: &str) -> &str {
    trim_leading_slash.trim_start_matches('/')
}

#[derive(serde::Deserialize)]
struct ForceDeploymentsDumpToml {
    force_deployments_data: String,
}

fn run_forge_deploy_and_set_gateway_transaction_filterer(
    l1_contracts_root: &Path,
    l1_rpc_url: &str,
    bridgehub: &str,
    chain_id: u64,
) -> Result<()> {
    println!("\n=== Forge: deploy and set gateway transaction filterer ===");
    let status = Command::new("forge")
        .current_dir(l1_contracts_root)
        .args([
            "script",
            "deploy-scripts/dev/DeployAndSetGatewayTransactionFilterer.s.sol:DeployAndSetGatewayTransactionFilterer",
            "--sig",
            "run(address,uint256)",
            bridgehub,
            &chain_id.to_string(),
            "--rpc-url",
            l1_rpc_url,
            "--broadcast",
            "--ffi",
            "--private-key",
            DEFAULT_ANVIL_PRIVATE_KEY,
        ])
        .status()
        .context("spawn forge DeployAndSetGatewayTransactionFilterer")?;
    if !status.success() {
        anyhow::bail!("forge DeployAndSetGatewayTransactionFilterer failed");
    }
    Ok(())
}

fn run_forge_dump_force_deployments(
    l1_contracts_root: &Path,
    l1_rpc_url: &str,
    ctm_proxy: &str,
    dump_toml_rel: &str,
) -> Result<()> {
    println!("\n=== Forge: dump force_deployments_data for gateway vote-prep ===");
    fs::create_dir_all(l1_contracts_root.join("script-out"))
        .context("create l1-contracts/script-out")?;
    let status = Command::new("forge")
        .current_dir(l1_contracts_root)
        .env("FORCE_DEPLOYMENTS_DUMP_TOML_REL_PATH", dump_toml_rel)
        .args([
            "script",
            "deploy-scripts/dev/DumpForceDeploymentsForGateway.s.sol:DumpForceDeploymentsForGateway",
            "--sig",
            "run(address)",
            ctm_proxy,
            "--rpc-url",
            l1_rpc_url,
        ])
        .status()
        .context("spawn forge script DumpForceDeploymentsForGateway")?;
    if !status.success() {
        anyhow::bail!("forge DumpForceDeploymentsForGateway failed");
    }
    Ok(())
}

fn read_force_deployments_hex(l1_contracts_root: &Path, dump_toml_rel: &str) -> Result<String> {
    let path = l1_contracts_root.join(l1_path_rel(dump_toml_rel));
    let raw =
        fs::read_to_string(&path).with_context(|| format!("read force dump {}", path.display()))?;
    let parsed: ForceDeploymentsDumpToml =
        toml::from_str(&raw).context("parse force deployments dump TOML")?;
    Ok(parsed.force_deployments_data)
}

fn write_gateway_vote_preparation_toml(
    l1_contracts_root: &Path,
    dest_rel: &str,
    force_deployments_hex: &str,
    refund_recipient: &str,
    gateway_chain_id: u64,
) -> Result<()> {
    let base = l1_contracts_root.join("script-config/config-deploy-ctm.toml");
    let mut body = fs::read_to_string(&base).with_context(|| format!("read {}", base.display()))?;
    let gateway_block = format!(
        r#"
refund_recipient = "{refund_recipient}"
gateway_chain_id = {gateway_chain_id}
gateway_settlement_fee = 1000000000
force_deployments_data = "{force_hex}"
"#,
        refund_recipient = refund_recipient,
        gateway_chain_id = gateway_chain_id,
        force_hex = force_deployments_hex,
    );
    let contracts_marker = "\n[contracts]";
    if let Some(idx) = body.find(contracts_marker) {
        body.insert_str(idx, &gateway_block);
    } else {
        body.push_str(&gateway_block);
    }
    let out = l1_contracts_root.join(l1_path_rel(dest_rel));
    fs::write(&out, body).with_context(|| format!("write {}", out.display()))?;
    Ok(())
}

fn run_convert_to_gateway(
    preset: &Preset,
    l1_rpc_url: &str,
    bridgehub: &str,
    gateway_chain_id: u64,
    governance_addr: &str,
    deployer_addr: &str,
    stm_tracker: &str,
    vote_input_rel: &str,
    vote_output_rel: &str,
) -> Result<()> {
    println!("\n=== protocol_ops chain convert-to-gateway (grant-whitelist) ===");
    let mut grant: Vec<String> = vec![
        "chain".into(),
        "convert-to-gateway".into(),
        "--stage".into(),
        "grant-whitelist".into(),
        "--l1-rpc-url".into(),
        l1_rpc_url.into(),
        "--private-key".into(),
        DEFAULT_ANVIL_PRIVATE_KEY.into(),
        "--bridgehub-proxy-address".into(),
        bridgehub.into(),
        "--gateway-chain-id".into(),
        gateway_chain_id.to_string(),
    ];
    for a in [governance_addr, deployer_addr, stm_tracker] {
        grant.push("--whitelist-grantees".into());
        grant.push(a.into());
    }
    let grant_ref: Vec<&str> = grant.iter().map(|s| s.as_str()).collect();
    integration_tests::protocol_ops::run_protocol_ops_for_preset(preset, &grant_ref)
        .context("convert-to-gateway grant-whitelist")?;

    println!("\n=== protocol_ops chain convert-to-gateway (vote-prepare) ===");
    let vote_prep: Vec<String> = vec![
        "chain".into(),
        "convert-to-gateway".into(),
        "--stage".into(),
        "vote-prepare".into(),
        "--l1-rpc-url".into(),
        l1_rpc_url.into(),
        "--private-key".into(),
        DEFAULT_ANVIL_PRIVATE_KEY.into(),
        "--bridgehub-proxy-address".into(),
        bridgehub.into(),
        "--gateway-chain-id".into(),
        gateway_chain_id.to_string(),
        "--ctm-representative-chain-id".into(),
        gateway_chain_id.to_string(),
        "--vote-preparation-input-path".into(),
        vote_input_rel.into(),
        "--vote-preparation-output-path".into(),
        vote_output_rel.into(),
    ];
    let vote_prep_ref: Vec<&str> = vote_prep.iter().map(|s| s.as_str()).collect();
    integration_tests::protocol_ops::run_protocol_ops_for_preset(preset, &vote_prep_ref)
        .context("convert-to-gateway vote-prepare")?;

    println!("\n=== protocol_ops chain convert-to-gateway (governance-execute) ===");
    let gov: Vec<String> = vec![
        "chain".into(),
        "convert-to-gateway".into(),
        "--stage".into(),
        "governance-execute".into(),
        "--l1-rpc-url".into(),
        l1_rpc_url.into(),
        "--private-key".into(),
        DEFAULT_ANVIL_PRIVATE_KEY.into(),
        "--bridgehub-proxy-address".into(),
        bridgehub.into(),
        "--gateway-chain-id".into(),
        gateway_chain_id.to_string(),
        "--governance-address".into(),
        governance_addr.into(),
        "--vote-preparation-output-path".into(),
        vote_output_rel.into(),
    ];
    let gov_ref: Vec<&str> = gov.iter().map(|s| s.as_str()).collect();
    integration_tests::protocol_ops::run_protocol_ops_for_preset(preset, &gov_ref)
        .context("convert-to-gateway governance-execute")?;

    println!("\n=== protocol_ops chain convert-to-gateway (revoke-whitelist) ===");
    let revoke: Vec<String> = vec![
        "chain".into(),
        "convert-to-gateway".into(),
        "--stage".into(),
        "revoke-whitelist".into(),
        "--l1-rpc-url".into(),
        l1_rpc_url.into(),
        "--private-key".into(),
        DEFAULT_ANVIL_PRIVATE_KEY.into(),
        "--bridgehub-proxy-address".into(),
        bridgehub.into(),
        "--gateway-chain-id".into(),
        gateway_chain_id.to_string(),
        "--revoke-address".into(),
        deployer_addr.into(),
    ];
    let revoke_ref: Vec<&str> = revoke.iter().map(|s| s.as_str()).collect();
    integration_tests::protocol_ops::run_protocol_ops_for_preset(preset, &revoke_ref)
        .context("convert-to-gateway revoke-whitelist")?;

    Ok(())
}

fn run_migrate_to_gateway(
    preset: &Preset,
    l1_rpc_url: &str,
    bridgehub: &str,
    chain_id: u64,
    gateway_chain_id: u64,
    vote_output_rel: &str,
    refund_recipient: &str,
) -> Result<()> {
    let chain_id_s = chain_id.to_string();
    let gateway_chain_id_s = gateway_chain_id.to_string();
    let gas_s = MIGRATE_L1_GAS_PRICE_WEI.to_string();

    println!("\n=== protocol_ops chain migrate-to-gateway (pause-deposits) ===");
    let pause: Vec<String> = vec![
        "chain".into(),
        "migrate-to-gateway".into(),
        "--stage".into(),
        "pause-deposits".into(),
        "--l1-rpc-url".into(),
        l1_rpc_url.into(),
        "--private-key".into(),
        DEFAULT_ANVIL_PRIVATE_KEY.into(),
        "--bridgehub-proxy-address".into(),
        bridgehub.into(),
        "--chain-id".into(),
        chain_id_s.clone(),
    ];
    let pause_ref: Vec<&str> = pause.iter().map(|s| s.as_str()).collect();
    integration_tests::protocol_ops::run_protocol_ops_for_preset(preset, &pause_ref)
        .context("migrate-to-gateway pause-deposits")?;

    println!("\n=== protocol_ops chain migrate-to-gateway (migrate) ===");
    let migrate: Vec<String> = vec![
        "chain".into(),
        "migrate-to-gateway".into(),
        "--stage".into(),
        "migrate".into(),
        "--l1-rpc-url".into(),
        l1_rpc_url.into(),
        "--private-key".into(),
        DEFAULT_ANVIL_PRIVATE_KEY.into(),
        "--bridgehub-proxy-address".into(),
        bridgehub.into(),
        "--chain-id".into(),
        chain_id_s.clone(),
        "--gateway-chain-id".into(),
        gateway_chain_id_s,
        "--l1-gas-price".into(),
        gas_s,
        "--vote-preparation-output-path".into(),
        vote_output_rel.into(),
        "--refund-recipient".into(),
        refund_recipient.into(),
    ];
    let migrate_ref: Vec<&str> = migrate.iter().map(|s| s.as_str()).collect();
    integration_tests::protocol_ops::run_protocol_ops_for_preset(preset, &migrate_ref)
        .context("migrate-to-gateway migrate")?;

    println!("\n=== protocol_ops chain migrate-to-gateway (notify-server) ===");
    let notify: Vec<String> = vec![
        "chain".into(),
        "migrate-to-gateway".into(),
        "--stage".into(),
        "notify-server".into(),
        "--l1-rpc-url".into(),
        l1_rpc_url.into(),
        "--private-key".into(),
        DEFAULT_ANVIL_PRIVATE_KEY.into(),
        "--bridgehub-proxy-address".into(),
        bridgehub.into(),
        "--chain-id".into(),
        chain_id_s,
    ];
    let notify_ref: Vec<&str> = notify.iter().map(|s| s.as_str()).collect();
    integration_tests::protocol_ops::run_protocol_ops_for_preset(preset, &notify_ref)
        .context("migrate-to-gateway notify-server")?;

    Ok(())
}

fn yaml_gateway_node_config(
    bridgehub: &str,
    bytecodes_supplier: &str,
    genesis_path: &Path,
    chain_id: u64,
    commit_pk: &str,
    prove_pk: &str,
    execute_pk: &str,
) -> String {
    format!(
        r#"genesis:
  bridgehub_address: '{}'
  bytecode_supplier_address: '{}'
  genesis_input_path: '{}'
  chain_id: {}
l1_watcher:
  poll_interval: 100ms
  confirmations: 0
l1_sender:
  pubdata_mode: Blobs
  poll_interval: 100ms
  operator_commit_sk: '{}'
  operator_prove_sk: '{}'
  operator_execute_sk: '{}'
batcher:
  batch_timeout: 1s
prover_api:
  fake_fri_provers:
    enabled: true
  fake_snark_provers:
    enabled: true
sequencer:
  revm_consistency_checker_enabled: false
external_price_api_client:
  source: Forced
  forced_prices:
    '0x0000000000000000000000000000000000000001': 3000
"#,
        bridgehub,
        bytecodes_supplier,
        genesis_path.display(),
        chain_id,
        commit_pk,
        prove_pk,
        execute_pk,
    )
}

fn yaml_migrated_node_config(
    gateway_rpc_url: &str,
    gateway_chain_id: u64,
    bridgehub: &str,
    bytecodes_supplier: &str,
    genesis_path: &Path,
    migrated_chain_id: u64,
    commit_pk: &str,
    prove_pk: &str,
    execute_pk: &str,
) -> String {
    format!(
        r#"general:
  gateway_rpc_url: '{}'
  gateway_chain_id: {}
genesis:
  bridgehub_address: '{}'
  bytecode_supplier_address: '{}'
  genesis_input_path: '{}'
  chain_id: {}
l1_watcher:
  poll_interval: 100ms
  confirmations: 0
l1_sender:
  pubdata_mode: RelayedL2Calldata
  poll_interval: 100ms
  operator_commit_sk: '{}'
  operator_prove_sk: '{}'
  operator_execute_sk: '{}'
batcher:
  batch_timeout: 1s
status_server:
  address: 0.0.0.0:{}
prover_api:
  address: 0.0.0.0:{}
  fake_fri_provers:
    enabled: true
  fake_snark_provers:
    enabled: true
observability:
  prometheus:
    port: {}
sequencer:
  revm_consistency_checker_enabled: false
external_price_api_client:
  source: Forced
  forced_prices:
    '0x0000000000000000000000000000000000000001': 3000
"#,
        gateway_rpc_url,
        gateway_chain_id,
        bridgehub,
        bytecodes_supplier,
        genesis_path.display(),
        migrated_chain_id,
        commit_pk,
        prove_pk,
        execute_pk,
        MIGRATED_NODE_STATUS_SERVER_PORT,
        MIGRATED_NODE_PROVER_API_PORT,
        MIGRATED_NODE_PROMETHEUS_PORT,
    )
}

async fn run_protocol_ops_init_test() -> Result<()> {
    let preset = get_v31_preset()?;
    let era_path = get_era_contracts_path(&preset)?;

    let anvil = Anvil::spawn_fresh().await?;
    let l1_rpc_url = anvil.rpc_url_for(&preset.zksync_os_server);

    build_contracts(&era_path)?;

    let era_pk = era_validator_private_key();
    let gateway_commit_pk = operator_commit_private_key(OPERATOR_CHAIN_GATEWAY);
    let gateway_prove_pk = operator_prove_private_key(OPERATOR_CHAIN_GATEWAY);
    let gateway_execute_pk = operator_execute_private_key(OPERATOR_CHAIN_GATEWAY);
    let migrated_commit_pk = operator_commit_private_key(OPERATOR_CHAIN_MIGRATED);
    let migrated_prove_pk = operator_prove_private_key(OPERATOR_CHAIN_MIGRATED);
    let migrated_execute_pk = operator_execute_private_key(OPERATOR_CHAIN_MIGRATED);

    let logs_dir = protocol_ops_logs_dir()
        .ok_or_else(|| anyhow::anyhow!("Could not resolve protocol_ops logs dir"))?;
    let ecosystem_out = logs_dir.join("ecosystem_init_out.json");
    let chain_out = logs_dir.join("chain_init_out.json");

    println!("\n=== protocol_ops ecosystem init ===");
    integration_tests::protocol_ops::run_protocol_ops_for_preset(
        &preset,
        &[
            "ecosystem",
            "init",
            "--l1-rpc-url",
            l1_rpc_url.as_str(),
            "--private-key",
            DEFAULT_ANVIL_PRIVATE_KEY,
            "--out",
            ecosystem_out.to_str().unwrap(),
        ],
    )?;

    let ecosystem_json: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(&ecosystem_out).context("Failed to read ecosystem init output")?,
    )
    .context("Failed to parse ecosystem init output")?;

    let output = ecosystem_json
        .get("output")
        .ok_or_else(|| anyhow::anyhow!("Missing output in ecosystem init JSON"))?;
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
    if l1_da_validator.is_empty() || l1_da_validator == "0x0000000000000000000000000000000000000000"
    {
        anyhow::bail!("L1 DA validator address is empty or zero");
    }
    let bytecodes_supplier = extract_json_value(
        output,
        "ctm.deployed_addresses.state_transition.bytecodes_supplier_addr",
    )?;

    let era_validator_operator = address_from_private_key(&era_pk)?;
    let gateway_commit_operator = address_from_private_key(&gateway_commit_pk)?;
    let gateway_prove_operator = address_from_private_key(&gateway_prove_pk)?;
    let gateway_execute_operator = address_from_private_key(&gateway_execute_pk)?;
    let migrated_commit_operator = address_from_private_key(&migrated_commit_pk)?;
    let migrated_prove_operator = address_from_private_key(&migrated_prove_pk)?;
    let migrated_execute_operator = address_from_private_key(&migrated_execute_pk)?;

    println!("\n=== protocol_ops chain init ===");
    integration_tests::protocol_ops::run_protocol_ops_for_preset(
        &preset,
        &[
            "chain",
            "init",
            "--ctm-proxy",
            &ctm_proxy,
            "--l1-da-validator",
            &l1_da_validator,
            "--era-validator-operator",
            &era_validator_operator,
            "--commit-operator",
            &gateway_commit_operator,
            "--prove-operator",
            &gateway_prove_operator,
            "--execute-operator",
            &gateway_execute_operator,
            "--chain-id",
            &GATEWAY_CHAIN_ID.to_string(),
            "--l1-rpc-url",
            l1_rpc_url.as_str(),
            "--private-key",
            DEFAULT_ANVIL_PRIVATE_KEY,
            "--out",
            chain_out.to_str().unwrap(),
        ],
    )?;

    let chain_json: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(&chain_out).context("Failed to read chain init output")?,
    )
    .context("Failed to parse chain init output")?;

    let chain_output = chain_json
        .get("output")
        .ok_or_else(|| anyhow::anyhow!("Missing output in chain init JSON"))?;
    let diamond_proxy = extract_json_value(chain_output, "diamond_proxy_addr")?;

    let chain_out_migrated = logs_dir.join("chain_init_migrated_out.json");
    println!("\n=== protocol_ops chain init (second chain, later migrated to gateway) ===");
    integration_tests::protocol_ops::run_protocol_ops_for_preset(
        &preset,
        &[
            "chain",
            "init",
            "--ctm-proxy",
            &ctm_proxy,
            "--l1-da-validator",
            &l1_da_validator,
            "--era-validator-operator",
            &era_validator_operator,
            "--commit-operator",
            &migrated_commit_operator,
            "--prove-operator",
            &migrated_prove_operator,
            "--execute-operator",
            &migrated_execute_operator,
            "--chain-id",
            &MIGRATED_CHAIN_ID.to_string(),
            "--l1-rpc-url",
            l1_rpc_url.as_str(),
            "--private-key",
            DEFAULT_ANVIL_PRIVATE_KEY,
            "--out",
            chain_out_migrated.to_str().unwrap(),
        ],
    )?;

    let chain_json_migrated: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(&chain_out_migrated)
            .context("Failed to read second chain init output")?,
    )
    .context("Failed to parse second chain init output")?;
    let chain_output_migrated = chain_json_migrated
        .get("output")
        .ok_or_else(|| anyhow::anyhow!("Missing output in second chain init JSON"))?;
    let diamond_proxy_migrated = extract_json_value(chain_output_migrated, "diamond_proxy_addr")?;

    let config_dir = std::env::temp_dir().join(format!(
        "protocol_ops_init_{}",
        uuid::Uuid::new_v4().to_string().replace('-', "")
    ));
    fs::create_dir_all(&config_dir)?;
    let migrated_chain_rocksdb = config_dir.join("migrated_chain_rocksdb");

    let genesis_dst = config_dir.join("genesis.json");
    run_genesis_gen(&era_path, &genesis_dst)?;

    let genesis_path = genesis_dst.canonicalize().unwrap_or(genesis_dst);

    let config_content = yaml_gateway_node_config(
        &bridgehub,
        &bytecodes_supplier,
        genesis_path.as_path(),
        GATEWAY_CHAIN_ID,
        &gateway_commit_pk,
        &gateway_prove_pk,
        &gateway_execute_pk,
    );

    let config_path = config_dir.join("config.yaml");
    fs::write(&config_path, &config_content)?;

    println!("\n=== Funding L1 operator accounts (L1 sender gas) ===");
    for (name, addr) in [
        ("era_validator", era_validator_operator.as_str()),
        ("gateway commit", gateway_commit_operator.as_str()),
        ("gateway prove", gateway_prove_operator.as_str()),
        ("gateway execute", gateway_execute_operator.as_str()),
        ("migrated commit", migrated_commit_operator.as_str()),
        ("migrated prove", migrated_prove_operator.as_str()),
        ("migrated execute", migrated_execute_operator.as_str()),
    ] {
        fund_account(
            addr,
            "100ether",
            l1_rpc_url.as_str(),
            DEFAULT_ANVIL_PRIVATE_KEY,
        )
        .with_context(|| format!("Failed to fund L1 {name} operator {addr}"))?;
    }

    let test_address = address_from_private_key(DEFAULT_TEST_PRIVATE_KEY)?;
    fund_account(
        &test_address,
        "1ether",
        l1_rpc_url.as_str(),
        DEFAULT_ANVIL_PRIVATE_KEY,
    )
    .context("Failed to fund DEFAULT_TEST_PRIVATE_KEY on L1")?;

    let config_migrated_pre_path = config_dir.join("config_migrated_pre_l1.yaml");
    let migrated_pre_yaml = yaml_gateway_node_config(
        &bridgehub,
        &bytecodes_supplier,
        genesis_path.as_path(),
        MIGRATED_CHAIN_ID,
        &migrated_commit_pk,
        &migrated_prove_pk,
        &migrated_execute_pk,
    );
    fs::write(&config_migrated_pre_path, &migrated_pre_yaml)?;

    println!("\n=== Pre-migration server (second chain on L1; drain priority queue) ===");
    let server_migrated_pre = ServerBuilder::new(preset.clone())
        .config_path(&config_migrated_pre_path)
        .rocks_db_path(&migrated_chain_rocksdb)
        .spawn(&anvil)
        .map_err(|e| anyhow::anyhow!("Failed to spawn pre-migration server: {:?}", e))?;
    let migrated_pre_l2_rpc = server_migrated_pre.rpc_url();
    let migrated_pre_server_logs = server_migrated_pre.logs_path();

    fund_l2_via_l1_deposit(
        l1_rpc_url.as_str(),
        migrated_pre_l2_rpc.as_str(),
        &bridgehub,
        MIGRATED_CHAIN_ID,
        DEFAULT_TEST_PRIVATE_KEY,
        0.1,
        Duration::from_secs(120),
        Some(migrated_pre_server_logs.as_path()),
    )
    .context("fund L2 pre-migration (second chain)")?;
    wait_for_executed_batches_with_traffic(
        migrated_pre_l2_rpc.as_str(),
        l1_rpc_url.as_str(),
        &diamond_proxy_migrated,
        DEFAULT_TEST_PRIVATE_KEY,
        3,
        Duration::from_secs(120),
    )
    .context("pre-migration batches (drain priority queue)")?;
    wait_for_l1_committed_equals_executed(
        l1_rpc_url.as_str(),
        &diamond_proxy_migrated,
        Duration::from_secs(120),
    )
    .context("pre-migration: wait committed==executed before migrate")?;

    server_migrated_pre
        .kill()
        .map_err(|e| anyhow::anyhow!("Failed to kill pre-migration server: {:?}", e))?;

    let server_gateway = ServerBuilder::new(preset.clone())
        .config_path(&config_path)
        .spawn(&anvil)
        .map_err(|e| anyhow::anyhow!("Failed to spawn gateway server: {:?}", e))?;

    let l2_rpc_url = server_gateway.rpc_url();
    let gateway_server_logs = server_gateway.logs_path();

    println!("\n=== Funding L2 via deposit ===");
    fund_l2_via_l1_deposit(
        l1_rpc_url.as_str(),
        l2_rpc_url.as_str(),
        &bridgehub,
        GATEWAY_CHAIN_ID,
        DEFAULT_TEST_PRIVATE_KEY,
        0.1,
        Duration::from_secs(60),
        Some(gateway_server_logs.as_path()),
    )?;

    // Migrated-chain L1 senders use the gateway L2 RPC as settlement layer; operators need L2 ETH there.
    println!(
        "\n=== Funding gateway L2 for batch operators (L1 sender gas on settlement layer) ==="
    );
    for (name, pk) in [
        ("commit", gateway_commit_pk.as_str()),
        ("prove", gateway_prove_pk.as_str()),
        ("execute", gateway_execute_pk.as_str()),
    ] {
        fund_l2_via_l1_deposit(
            l1_rpc_url.as_str(),
            l2_rpc_url.as_str(),
            &bridgehub,
            GATEWAY_CHAIN_ID,
            pk,
            5.0,
            Duration::from_secs(120),
            Some(gateway_server_logs.as_path()),
        )
        .with_context(|| format!("fund gateway L2 for operator {name}"))?;
    }

    println!("\n=== Waiting for executed batches ===");
    wait_for_executed_batches_with_traffic(
        l2_rpc_url.as_str(),
        l1_rpc_url.as_str(),
        &diamond_proxy,
        DEFAULT_TEST_PRIVATE_KEY,
        3,
        Duration::from_secs(120),
    )?;

    let l1_contracts_root = era_path.join("l1-contracts");
    let flow_id = uuid::Uuid::new_v4().to_string().replace('-', "");
    let force_dump_rel = format!("/script-out/force_dep_gateway_{flow_id}.toml");
    let gw_vote_input_rel = format!("/script-config/gateway_vote_prep_{flow_id}.toml");
    let gw_vote_output_rel = format!("/script-out/gateway_vote_prep_out_{flow_id}.toml");

    run_forge_dump_force_deployments(
        &l1_contracts_root,
        l1_rpc_url.as_str(),
        &ctm_proxy,
        &force_dump_rel,
    )?;
    let force_hex = read_force_deployments_hex(&l1_contracts_root, &force_dump_rel)?;
    let deployer_addr = extract_json_value(output, "hub.deployer_addr")?;
    let governance_addr = extract_json_value(output, "hub.deployed_addresses.governance_addr")?;
    let stm_tracker = extract_json_value(
        output,
        "hub.deployed_addresses.bridgehub.ctm_deployment_tracker_proxy_addr",
    )?;
    write_gateway_vote_preparation_toml(
        &l1_contracts_root,
        &gw_vote_input_rel,
        &force_hex,
        &deployer_addr,
        GATEWAY_CHAIN_ID,
    )?;
    run_forge_deploy_and_set_gateway_transaction_filterer(
        &l1_contracts_root,
        l1_rpc_url.as_str(),
        &bridgehub,
        GATEWAY_CHAIN_ID,
    )?;
    run_convert_to_gateway(
        &preset,
        l1_rpc_url.as_str(),
        &bridgehub,
        GATEWAY_CHAIN_ID,
        &governance_addr,
        &deployer_addr,
        &stm_tracker,
        &gw_vote_input_rel,
        &gw_vote_output_rel,
    )?;

    let gateway_l2_rpc = server_gateway.rpc_url();

    run_forge_deploy_and_set_gateway_transaction_filterer(
        &l1_contracts_root,
        l1_rpc_url.as_str(),
        &bridgehub,
        MIGRATED_CHAIN_ID,
    )?;

    run_migrate_to_gateway(
        &preset,
        l1_rpc_url.as_str(),
        &bridgehub,
        MIGRATED_CHAIN_ID,
        GATEWAY_CHAIN_ID,
        &gw_vote_output_rel,
        &deployer_addr,
    )?;

    // Migrated node settles via gateway; L1 sender uses settlement-layer RPC and needs native ETH
    // on gateway L2 for the migrated-chain operator keys (L1 Anvil balance is not used).
    println!("\n=== Funding gateway L2 for migrated-chain operators ===");
    for (name, pk) in [
        ("commit", migrated_commit_pk.as_str()),
        ("prove", migrated_prove_pk.as_str()),
        ("execute", migrated_execute_pk.as_str()),
    ] {
        fund_l2_via_l1_deposit(
            l1_rpc_url.as_str(),
            gateway_l2_rpc.as_str(),
            &bridgehub,
            GATEWAY_CHAIN_ID,
            pk,
            5.0,
            Duration::from_secs(120),
            Some(gateway_server_logs.as_path()),
        )
        .with_context(|| format!("fund gateway L2 for migrated-chain operator {name}"))?;
    }

    let config_migrated_path = config_dir.join("config_migrated.yaml");
    let migrated_yaml = yaml_migrated_node_config(
        &gateway_l2_rpc,
        GATEWAY_CHAIN_ID,
        &bridgehub,
        &bytecodes_supplier,
        genesis_path.as_path(),
        MIGRATED_CHAIN_ID,
        &migrated_commit_pk,
        &migrated_prove_pk,
        &migrated_execute_pk,
    );
    fs::write(&config_migrated_path, &migrated_yaml)?;

    println!("\n=== Migrated-chain server (settles via gateway) ===");
    let server_migrated = ServerBuilder::new(preset.clone())
        .config_path(&config_migrated_path)
        .rocks_db_path(&migrated_chain_rocksdb)
        .spawn(&anvil)
        .map_err(|e| anyhow::anyhow!("Failed to spawn migrated-chain server: {:?}", e))?;
    let migrated_l2_rpc = server_migrated.rpc_url();

    // L1 mailbox keeps `depositsPaused` during/after gateway migration until async L1 confirm.
    // Same RocksDB as pre-migration already holds L2 funds from the earlier deposit.
    println!("\n=== Waiting for executed batches (migrated chain) ===");
    wait_for_executed_batches_with_traffic(
        migrated_l2_rpc.as_str(),
        l1_rpc_url.as_str(),
        &diamond_proxy_migrated,
        DEFAULT_TEST_PRIVATE_KEY,
        3,
        Duration::from_secs(180),
    )?;

    server_migrated
        .kill()
        .map_err(|e| anyhow::anyhow!("Failed to kill migrated server: {:?}", e))?;
    server_gateway
        .kill()
        .map_err(|e| anyhow::anyhow!("Failed to kill gateway server: {:?}", e))?;
    anvil.kill()?;

    println!("\n✓ test_protocol_ops_init_full_flow completed successfully");
    Ok(())
}

#[tokio::test]
#[ignore]
async fn test_protocol_ops_init_full_flow() {
    run_protocol_ops_init_test()
        .await
        .expect("protocol_ops_init test failed");
}
