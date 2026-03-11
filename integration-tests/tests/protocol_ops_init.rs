//! Integration test: initialize L1 via protocol-ops, start server, execute batches.
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
use integration_tests::presets::{load_default_presets, Preset, RepoRef};
use integration_tests::preset_paths::server_paths_for_preset;
use integration_tests::protocol_ops::protocol_ops_logs_dir;
use integration_tests::server::ServerBuilder;
use integration_tests::anvil_utils::fund_account;
use integration_tests::server_utils::{
    address_from_private_key, fund_l2_via_l1_deposit,
    wait_for_executed_batches_with_traffic, DEFAULT_TEST_PRIVATE_KEY,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// Anvil account #0 (deployer/owner for ecosystem and chain init)
const DEFAULT_ANVIL_PRIVATE_KEY: &str =
    "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
/// Anvil accounts #1 and #2 - operator keys must be different for commit/prove/execute
const OPERATOR_COMMIT_PRIVATE_KEY: &str =
    "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
const OPERATOR_PROVE_PRIVATE_KEY: &str =
    "0x5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a";
const OPERATOR_EXECUTE_PRIVATE_KEY: &str =
    "0x7c852118294e51e653712a81e05800f419141751be58f605c371e15141b007a6";
const CHAIN_ID: u64 = 6565;

fn extract_json_value(obj: &serde_json::Value, path: &str) -> Result<String> {
    let mut v = obj;
    for key in path.split('.') {
        v = v
            .get(key)
            .ok_or_else(|| anyhow::anyhow!("Missing key {:?} in JSON", key))?;
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

/// Execution version for genesis (v31 uses 6, same as update_server ZKSYNC_OS_EXECUTION_VERSION)
const GENESIS_EXECUTION_VERSION: u32 = 6;

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

/// Generate genesis.json via protocol-ops genesis gen (runs zksync-os-genesis-gen).
/// Falls back to copying latest.json if genesis gen fails (e.g. nightly Rust deps unavailable).
fn run_genesis_gen(preset: &Preset, output_path: &Path) -> Result<()> {
    println!("\n=== Generating genesis.json ===");
    let result = integration_tests::protocol_ops::run_protocol_ops_for_preset(
        preset,
        &[
            "genesis",
            "gen",
            "--output-file",
            output_path.to_str().unwrap(),
            "--execution-version",
            &GENESIS_EXECUTION_VERSION.to_string(),
        ],
    );
    match result {
        Ok(_) => {
            println!("✓ Genesis generated");
            Ok(())
        }
        Err(_) => {
            let era_path = get_era_contracts_path(preset)?;
            let genesis_src = era_path.join("configs/genesis/zksync-os/latest.json");
            println!(
                "⚠ protocol-ops genesis gen failed, falling back to copy from {}",
                genesis_src.display()
            );
            fs::copy(&genesis_src, output_path)
                .with_context(|| format!("Failed to copy genesis from {}", genesis_src.display()))?;
            Ok(())
        }
    }
}

async fn run_protocol_ops_init_test() -> Result<()> {
    let preset = get_v31_preset()?;
    let era_path = get_era_contracts_path(&preset)?;
    let paths = server_paths_for_preset(&preset)?;
    let server_root = paths.server_root.clone();

    let anvil = Anvil::spawn_fresh().await?;
    let l1_rpc_url = anvil.rpc_url_for(&preset.zksync_os_server);

    build_contracts(&era_path)?;

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
    let bridgehub = extract_json_value(output, "hub.bridgehub_proxy_addr")?;
    let ctm_proxy = extract_json_value(output, "ctm.state_transition_proxy_addr")?;
    // For ZKsync OS with BlobsZKSyncOS, use BlobsL1DAValidatorZKsyncOS (accepts 32-byte versioned hashes).
    // Rollup validator expects 65-byte CalldataDA format and reverts with OperatorDAInputTooSmall.
    let l1_da_validator = output
        .get("ctm")
        .and_then(|c| c.get("blobs_zksync_os_l1_da_validator_addr"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty() && *s != "0x0000000000000000000000000000000000000000")
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("blobs_zksync_os_l1_da_validator_addr not set or zero"))
        .or_else(|_| extract_json_value(output, "ctm.rollup_l1_da_validator_addr"))?;
    let bytecodes_supplier = extract_json_value(output, "ctm.bytecodes_supplier_addr")?;

    let commit_operator = address_from_private_key(OPERATOR_COMMIT_PRIVATE_KEY)?;
    let prove_operator = address_from_private_key(OPERATOR_PROVE_PRIVATE_KEY)?;
    let execute_operator = address_from_private_key(OPERATOR_EXECUTE_PRIVATE_KEY)?;

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
            "--commit-operator",
            &commit_operator,
            "--prove-operator",
            &prove_operator,
            "--execute-operator",
            &execute_operator,
            "--chain-id",
            &CHAIN_ID.to_string(),
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

    let config_dir = std::env::temp_dir().join(format!(
        "protocol_ops_init_{}",
        uuid::Uuid::new_v4().to_string().replace('-', "")
    ));
    fs::create_dir_all(&config_dir)?;

    let genesis_dst = config_dir.join("genesis.json");
    run_genesis_gen(&preset, &genesis_dst)?;

    let genesis_path = genesis_dst.canonicalize().unwrap_or(genesis_dst);

    let config_content = format!(
        r#"genesis:
  bridgehub_address: '{}'
  bytecode_supplier_address: '{}'
  genesis_input_path: '{}'
  chain_id: {}
l1_sender:
  operator_commit_sk: '{}'
  operator_prove_sk: '{}'
  operator_execute_sk: '{}'
external_price_api_client:
  source: Forced
  forced_prices:
    '0x0000000000000000000000000000000000000001': 3000
"#,
        bridgehub,
        bytecodes_supplier,
        genesis_path.display(),
        CHAIN_ID,
        OPERATOR_COMMIT_PRIVATE_KEY,
        OPERATOR_PROVE_PRIVATE_KEY,
        OPERATOR_EXECUTE_PRIVATE_KEY,
    );

    let config_path = config_dir.join("config.yaml");
    fs::write(&config_path, &config_content)?;

    let server = ServerBuilder::new(preset)
        .config_path(&config_path)
        .spawn(&anvil)
        .map_err(|e| anyhow::anyhow!("Failed to spawn server: {:?}", e))?;

    let l2_rpc_url = server.rpc_url();

    let test_address = address_from_private_key(DEFAULT_TEST_PRIVATE_KEY)?;
    fund_account(
        &test_address,
        "1ether",
        l1_rpc_url.as_str(),
        DEFAULT_ANVIL_PRIVATE_KEY,
    )
    .context("Failed to fund DEFAULT_TEST_PRIVATE_KEY on L1")?;

    println!("\n=== Funding L2 via deposit ===");
    fund_l2_via_l1_deposit(
        &server_root,
        l1_rpc_url.as_str(),
        l2_rpc_url.as_str(),
        &bridgehub,
        CHAIN_ID,
        DEFAULT_TEST_PRIVATE_KEY,
        0.1,
        Duration::from_secs(60),
    )?;

    println!("\n=== Waiting for executed batches ===");
    wait_for_executed_batches_with_traffic(
        l2_rpc_url.as_str(),
        l1_rpc_url.as_str(),
        &diamond_proxy,
        DEFAULT_TEST_PRIVATE_KEY,
        3,
        Duration::from_secs(120),
    )?;

    server.kill().map_err(|e| anyhow::anyhow!("Failed to kill server: {:?}", e))?;
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
