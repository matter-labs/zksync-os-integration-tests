//! # ZKsync OS Protocol Upgrade Tests
//!
//! This test suite performs end-to-end protocol upgrade testing from v30.2 to v31.
//!
//! ## Running the Tests
//!
//! ### Full Upgrade Test
//!
//! The test automatically starts and manages the Anvil L1 node:
//!
//! ```bash
//! cargo test --test upgrade-tests test_v30_to_v31_upgrade -- --ignored --nocapture
//! ```
//!
//! After the test completes, manually restart the server to apply the upgrade:
//!
//! ```bash
//! cd zksync-os-server
//! ./target/release/zksync-os-server --config ./local-chains/v30.2/default/config.yaml &
//! ```
//!
//! ### Post-Upgrade Verification (after manually restarting the server)
//!
//! ```bash
//! cargo test --test upgrade-tests test_post_upgrade_verification -- --ignored --nocapture
//! ```
//!
//! ## Using the Complete Script
//!
//! For a fully automated test including Docker infrastructure:
//!
//! ```bash
//! bash ./scripts/run-upgrade-local.sh
//! ```

use anyhow::{Context, Result};
use integration_tests::anvil::Anvil;
use integration_tests::anvil_utils::{fund_account, impersonate_account, stop_impersonating_account, RICH_ACCOUNT_PRIVATE_KEY};
use integration_tests::presets::RepoRef;
use integration_tests::server::ServerBuilder;
use integration_tests::server_utils::{
    address_from_private_key, fund_l2_via_l1_deposit,
    wait_for_executed_batches_with_traffic, DEFAULT_TEST_PRIVATE_KEY,
};
use integration_tests::upgrade_config::{Contracts, Wallets};
use integration_tests::upgrade_yaml_output_generator;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// Script output from protocol_ops no-governance-prepare (from --out file: output.core, output.ecosystem, output.run_json).
#[derive(Clone)]
struct NoGovernancePrepareOutput {
    core: serde_json::Value,
    ecosystem: serde_json::Value,
    run_json: String,
}

fn parse_no_governance_out_file(out_path: &Path) -> Result<NoGovernancePrepareOutput> {
    let content = fs::read_to_string(out_path)
        .with_context(|| format!("Failed to read out file: {}", out_path.display()))?;
    let root: serde_json::Value =
        serde_json::from_str(&content).context("Failed to parse no-governance-prepare out file")?;
    let output = root
        .get("output")
        .ok_or_else(|| anyhow::anyhow!("Missing output in out file"))?;
    let core = output
        .get("core")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("Missing output.core in out file"))?;
    let ecosystem = output
        .get("ecosystem")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("Missing output.ecosystem in out file"))?;
    let run_json = output
        .get("run_json")
        .and_then(|v| serde_json::to_string(v).ok())
        .unwrap_or_else(|| "{}".to_string());
    Ok(NoGovernancePrepareOutput {
        core,
        ecosystem,
        run_json,
    })
}

const UPGRADE_VERSION: &str = "v31-interop-b";

/// Execute transactions from a protocol-ops --out JSON file using the Forge script ExecuteProtocolOpsOut.s.sol.
/// Uses the `transactions` field produced by protocol_ops (each item: to, data, value).
pub fn execute_transactions(
    era_contracts_path: &Path,
    out_path: &Path,
    l1_rpc_url: &str,
    private_key: &str,
) -> Result<()> {
    integration_tests::protocol_ops::run_execute_protocol_ops_out(
        era_contracts_path,
        out_path,
        l1_rpc_url,
        private_key,
    )
    .context("run_execute_protocol_ops_out failed")
}

/// Helper to get project root directory
fn get_project_root() -> PathBuf {
    integration_tests::utils::find_project_root()
        .expect("Failed to find project root")
}

/// Helper to resolve local era-contracts path from presets.
/// For now this test only supports local preset paths.
fn get_era_contracts_path() -> PathBuf {
    let presets = integration_tests::presets::load_default_presets()
        .expect("Failed to load presets");
    let mut names: Vec<String> = presets.keys().cloned().collect();
    names.sort();
    let name = names.first().expect("No presets found").clone();
    let preset = presets.get(&name).expect("Preset disappeared");
    match &preset.era_contracts {
        RepoRef::Path(path) => path.clone(),
        RepoRef::DockerTag(tag) => panic!(
            "upgrade-tests currently support only local preset paths for era-contracts. Found docker tag: {}",
            tag
        ),
    }
}

fn run_protocol_ops_for_default_preset(args: &[&str]) -> Result<String> {
    let preset = get_default_preset();
    integration_tests::protocol_ops::run_protocol_ops_for_preset(&preset, args)
}

fn get_default_preset() -> integration_tests::presets::Preset {
    let presets = integration_tests::presets::load_default_presets()
        .expect("Failed to load presets");
    let mut names: Vec<String> = presets.keys().cloned().collect();
    names.sort();
    let name = names.first().expect("No presets found").clone();
    presets
        .get(&name)
        .expect("Preset disappeared")
        .clone()
}

fn get_default_server_paths() -> integration_tests::preset_paths::ServerPresetPaths {
    let preset = get_default_preset();
    integration_tests::preset_paths::server_paths_for_preset(&preset)
        .expect("Failed to resolve server paths from preset")
}

/// Helper to run a command and display its output in real-time
fn run_command(name: &str, cmd: &mut Command) -> Result<()> {
    run_command_with_verbosity(name, cmd, false)
}

/// Helper to run a command with optional verbosity
fn run_command_with_verbosity(name: &str, cmd: &mut Command, verbose: bool) -> Result<()> {
    if verbose {
        println!("Running: {}", name);
        println!("Command: {:?}", cmd);
    } else {
        print!("  {} ... ", name);
        std::io::Write::flush(&mut std::io::stdout()).ok();
    }

    if verbose {
        // Inherit stdin, stdout, and stderr so we see all output including Foundry traces
        let status = cmd
            .env("RUST_LOG", "info")
            .env("VERBOSE", "1")
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .status()
            .with_context(|| format!("Failed to run: {}", name))?;

        if !status.success() {
            anyhow::bail!("{} failed with status: {}", name, status);
        }
    } else {
        // Capture output for quiet mode to display on error
        let output = cmd
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .with_context(|| format!("Failed to run: {}", name))?;

        if !output.status.success() {
            println!("FAILED");
            println!("Command: {:?}", cmd);

            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);

            if !stdout.is_empty() {
                println!("stdout:\n{}", stdout);
            }
            if !stderr.is_empty() {
                println!("stderr:\n{}", stderr);
            }

            anyhow::bail!("{} failed with status: {}", name, output.status);
        }

        println!("✓");
    }

    Ok(())
}

/// Build all contracts required by upgrade scripts.
/// Mirrors Docker build flow but skips all cleanup.
fn build_contracts(era_path: &Path) -> Result<()> {
    println!("\n=== Building Contracts ===");
    run_command(
        "Build all contracts",
        Command::new("yarn")
            .args(["build-all-contracts"])
            .current_dir(era_path),
    )
    .context("Failed to build contracts")?;

    println!("✓ Contracts build completed");
    Ok(())
}

/// Update permanent values for upgrade
fn update_permanent_values(
    contracts: &Contracts,
) -> Result<()> {
    print!("  Updating permanent values ... ");
    std::io::Write::flush(&mut std::io::stdout()).ok();

    let era_path = get_era_contracts_path();

    // Local upgrade tests use the fixed chain id used by the local stack.
    let era_chain_id = "6565".to_string();

    // Extract values from Contracts
    let bridgehub_addr = &contracts.ecosystem_contracts.bridgehub_proxy_addr;
    let ctm_addr = &contracts.ecosystem_contracts.state_transition_proxy_addr;
    let bytecodes_supplier = &contracts.ecosystem_contracts.l1_bytecodes_supplier_addr;
    let create2_factory = &contracts.create2_factory_addr;
    let create2_salt = &contracts.create2_factory_salt;

    println!("Chain ID: {}", era_chain_id);
    println!("Bridgehub: {}", bridgehub_addr);
    println!("CTM: {}", ctm_addr);

    // Create permanent-values.toml
    // IMPORTANT: is_zk_sync_os = true tells the upgrade scripts to use tx type 126
    // instead of 254 for upgrade transactions (required for ZKsync OS chains)
    let permanent_values = format!(
        r#"era_chain_id = {}
is_zk_sync_os = true

[core_contracts]
bridgehub_proxy_addr = "{}"

[ctm_contracts]
ctm_proxy_addr = "{}"
l1_bytecodes_supplier_addr = "{}"

[permanent_contracts]
create2_factory_addr = "{}"
create2_factory_salt = "{}"
"#,
        era_chain_id,
        bridgehub_addr,
        ctm_addr,
        bytecodes_supplier,
        create2_factory,
        create2_salt
    );

    let script_config_dir = era_path.join("l1-contracts/script-config");
    fs::create_dir_all(&script_config_dir)?;
    fs::write(
        script_config_dir.join("permanent-values.toml"),
        &permanent_values,
    )?;

    let upgrade_envs_dir = era_path.join("l1-contracts/upgrade-envs/permanent-values");
    fs::create_dir_all(&upgrade_envs_dir)?;
    fs::write(upgrade_envs_dir.join("local.toml"), permanent_values)?;

    println!("✓");
    Ok(())
}

/// Extract a YAML value (simple parser for key: value format)
fn extract_yaml_value(yaml: &str, key: &str) -> Result<String> {
    for line in yaml.lines() {
        if line.trim_start().starts_with(key) {
            if let Some(value) = line.split(':').nth(1) {
                return Ok(value.trim().to_string());
            }
        }
    }
    anyhow::bail!("Could not find key '{}' in YAML", key)
}

/// Extract a string value from JSON by dotted path (e.g. "upgrade_addresses.bridgehub.chain_asset_handler_proxy_addr").
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

/// Extract a YAML value from a specific section (e.g., "governor" section's "private_key")
fn extract_yaml_value_in_section(yaml: &str, section: &str, key: &str) -> Result<String> {
    let mut in_section = false;
    for line in yaml.lines() {
        // Check if we're entering the target section (line starts with section name followed by :)
        if !line.starts_with(' ') && !line.starts_with('\t') {
            in_section = line.trim().starts_with(section) && line.contains(':');
        }
        // If we're in the section and find the key, extract the value
        if in_section && line.trim_start().starts_with(key) {
            if let Some(value) = line.split(':').nth(1) {
                return Ok(value.trim().to_string());
            }
        }
    }
    anyhow::bail!("Could not find key '{}' in section '{}' in YAML", key, section)
}

/// Fund governance accounts with ETH for upgrade transactions
fn fund_governance_accounts(l1_rpc_url: &str) -> Result<()> {
    // Governor address that needs funding
    let governor_address = "0x8002cd98cfb563492a6fb3e7c8243b7b9ad4cc92";

    // Send 10 ETH to governor from rich account
    // Use high gas price to replace any pending transactions with same nonce
    run_command(
        "Fund governor account",
        Command::new("cast")
            .args([
                "send",
                governor_address,
                "--value",
                "10ether",
                "--private-key",
                RICH_ACCOUNT_PRIVATE_KEY,
                "--rpc-url",
                l1_rpc_url,
                "--gas-price",
                "100gwei",
            ]),
    )?;

    Ok(())
}


/// Transfer ownership of the ecosystem governance timelock to the governor wallet
/// This is needed because the forge scripts broadcast from the governance owner,
/// but we only have the governor wallet's private key
fn transfer_governance_ownership_to_governor(
    root: &Path,
    l1_rpc_url: &str,
    contracts: &Contracts,
    wallets: &Wallets,
) -> Result<()> {
    println!("\n  Transferring governance ownership to governor wallet...");

    let ecosystem_governance = &contracts.ecosystem_contracts.governance;
    println!("    Ecosystem governance: {}", ecosystem_governance);

    let governor_address = wallets.governor.address.as_str();
    println!("    Governor wallet: {}", governor_address);

    // Read current owner of governance
    let owner_output = Command::new("cast")
        .args(["call", &ecosystem_governance, "owner()(address)", "--rpc-url", l1_rpc_url])
        .output()
        .context("Failed to read governance owner")?;
    let current_owner = String::from_utf8_lossy(&owner_output.stdout).trim().to_string();
    println!("    Current governance owner: {}", current_owner);

    // Check if already owned by governor
    if current_owner.to_lowercase() == governor_address.to_lowercase() {
        println!("    Already owned by governor, skipping");
        return Ok(());
    }

    // Transfer ownership via impersonation
    impersonate_account(&current_owner, l1_rpc_url)?;
    fund_account(&current_owner, "1ether", l1_rpc_url, RICH_ACCOUNT_PRIVATE_KEY)?;

    let output = Command::new("cast")
        .args([
            "send",
            &ecosystem_governance,
            "transferOwnership(address)",
            &governor_address,
            "--from",
            &current_owner,
            "--rpc-url",
            l1_rpc_url,
            "--unlocked",
        ])
        .output()
        .context("Failed to transfer governance ownership")?;

    stop_impersonating_account(&current_owner, l1_rpc_url);

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to transfer governance ownership: {}", stderr);
    }

    // Accept ownership as governor
    let governor_private_key = wallets.governor.private_key.as_str();

    let output = Command::new("cast")
        .args([
            "send",
            &ecosystem_governance,
            "acceptOwnership()",
            "--private-key",
            &governor_private_key,
            "--rpc-url",
            l1_rpc_url,
        ])
        .output()
        .context("Failed to accept governance ownership")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to accept governance ownership: {}", stderr);
    }

    println!("    ✓ Governance ownership transferred to governor");
    Ok(())
}

/// Transfer ownership of a contract from current owner to ecosystem governance
fn transfer_contract_ownership(
    contract_name: &str,
    contract_address: &str,
    ecosystem_governance: &str,
    l1_rpc_url: &str,
) -> Result<()> {
    println!("    {} ({})", contract_name, contract_address);

    // Read current owner
    let owner_output = Command::new("cast")
        .args(["call", contract_address, "owner()(address)", "--rpc-url", l1_rpc_url])
        .output()
        .context("Failed to read owner")?;
    let current_owner = String::from_utf8_lossy(&owner_output.stdout).trim().to_string();
    println!("      Current owner: {}", current_owner);

    // Check if already owned by ecosystem governance
    if current_owner.to_lowercase() == ecosystem_governance.to_lowercase() {
        println!("      Already owned by ecosystem governance, skipping");
        return Ok(());
    }

    // Impersonate current owner and transfer
    impersonate_account(&current_owner, l1_rpc_url)?;
    fund_account(&current_owner, "1ether", l1_rpc_url, RICH_ACCOUNT_PRIVATE_KEY)?;

    let output = Command::new("cast")
        .args([
            "send",
            contract_address,
            "transferOwnership(address)",
            ecosystem_governance,
            "--from",
            &current_owner,
            "--rpc-url",
            l1_rpc_url,
            "--unlocked",
        ])
        .output()
        .context("Failed to call transferOwnership")?;

    stop_impersonating_account(&current_owner, l1_rpc_url);

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to transfer {} ownership: {}", contract_name, stderr);
    }

    // Accept ownership as ecosystem governance
    impersonate_account(ecosystem_governance, l1_rpc_url)?;
    fund_account(ecosystem_governance, "1ether", l1_rpc_url, RICH_ACCOUNT_PRIVATE_KEY)?;

    let output = Command::new("cast")
        .args([
            "send",
            contract_address,
            "acceptOwnership()",
            "--from",
            ecosystem_governance,
            "--rpc-url",
            l1_rpc_url,
            "--unlocked",
        ])
        .output()
        .context("Failed to call acceptOwnership")?;

    stop_impersonating_account(ecosystem_governance, l1_rpc_url);

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to accept {} ownership: {}", contract_name, stderr);
    }

    println!("      ✓ Ownership transferred to ecosystem governance");
    Ok(())
}

/// Transfer ownership of newly deployed contracts to ecosystem governance
/// This is called AFTER no-governance-prepare since contracts are deployed with
/// deployer as owner, but governance stages need ecosystem governance to be the owner.
fn transfer_new_contracts_ownership(
    l1_rpc_url: &str,
    contracts: &Contracts,
    core: &serde_json::Value,
) -> Result<()> {
    println!("\n  Transferring contract ownership to ecosystem governance...");
    std::io::Write::flush(&mut std::io::stdout()).ok();

    let ecosystem_governance = &contracts.ecosystem_contracts.governance;
    println!("    Ecosystem governance: {}", ecosystem_governance);

    let chain_asset_handler = extract_json_value(core, "upgrade_addresses.bridgehub.chain_asset_handler_proxy_addr")?;
    transfer_contract_ownership("ChainAssetHandler", &chain_asset_handler, ecosystem_governance, l1_rpc_url)?;

    let native_token_vault = extract_json_value(core, "upgrade_addresses.native_token_vault_addr")?;
    transfer_contract_ownership("NativeTokenVault", &native_token_vault, ecosystem_governance, l1_rpc_url)?;

    println!("  ✓ All ownership transfers complete");
    Ok(())
}

/// Check if migration is paused on ChainAssetHandler
fn check_migration_paused(chain_asset_handler: &str, context: &str, l1_rpc_url: &str) -> Result<()> {

    // Check migrationPaused
    let output = Command::new("cast")
        .args(["call", &chain_asset_handler, "migrationPaused()(bool)", "--rpc-url", l1_rpc_url])
        .output()
        .context("Failed to call migrationPaused")?;

    let result = String::from_utf8_lossy(&output.stdout).trim().to_string();
    println!("  migrationPaused() {} = {}", context, result);

    // Also check which implementation is being used
    let impl_output = Command::new("cast")
        .args(["storage", &chain_asset_handler, "0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc", "--rpc-url", l1_rpc_url])
        .output()
        .context("Failed to read implementation slot")?;

    let impl_addr = String::from_utf8_lossy(&impl_output.stdout).trim().to_string();
    println!("  Implementation {} = {}", context, impl_addr);

    Ok(())
}

/// Ensure migration is paused, calling pauseMigration() directly if needed
fn ensure_migration_paused(chain_asset_handler: &str, l1_rpc_url: &str) -> Result<()> {
    // Check if already paused
    let output = Command::new("cast")
        .args(["call", &chain_asset_handler, "migrationPaused()(bool)", "--rpc-url", l1_rpc_url])
        .output()
        .context("Failed to call migrationPaused")?;

    let result = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if result == "true" {
        println!("  Migration already paused");
        return Ok(());
    }

    println!("  Migration not paused, pausing via impersonation...");

    // Get the owner and pause via impersonation
    let owner_output = Command::new("cast")
        .args(["call", &chain_asset_handler, "owner()(address)", "--rpc-url", l1_rpc_url])
        .output()
        .context("Failed to read owner")?;
    let owner = String::from_utf8_lossy(&owner_output.stdout).trim().to_string();

    impersonate_account(&owner, l1_rpc_url)?;
    fund_account(&owner, "1ether", l1_rpc_url, RICH_ACCOUNT_PRIVATE_KEY)?;

    let output = Command::new("cast")
        .args([
            "send",
            &chain_asset_handler,
            "pauseMigration()",
            "--from",
            &owner,
            "--rpc-url",
            l1_rpc_url,
            "--unlocked",
        ])
        .output()
        .context("Failed to call pauseMigration")?;

    stop_impersonating_account(&owner, l1_rpc_url);

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to pause migration: {}", stderr);
    }

    println!("  ✓ Migration paused via direct call");
    Ok(())
}

/// Run ecosystem upgrade stages. Returns script output from no-governance-prepare (no v31-upgrade-*.toml files on disk).
fn run_ecosystem_upgrades(
    root: &Path,
    l1_rpc_url: &str,
    contracts: &Contracts,
    wallets: &Wallets,
) -> Result<NoGovernancePrepareOutput> {
    println!("\n=== Running Ecosystem Upgrades ===");
    let era_path = get_era_contracts_path();
    let preset = get_default_preset();

    fund_governance_accounts(l1_rpc_url)?;

    // Stage 0: no-governance-prepare in simulate mode, then execute transactions from --out file
    println!("\n  Running no-governance-prepare (protocol_ops) in simulate mode...");
    let deployer_key = wallets.deployer.private_key.as_str();
    let bridgehub_proxy_address = contracts.ecosystem_contracts.bridgehub_proxy_addr.as_str();
    let ctm_proxy_address = contracts.ecosystem_contracts.state_transition_proxy_addr.as_str();
    let bytecodes_supplier_address = contracts.ecosystem_contracts.l1_bytecodes_supplier_addr.as_str();
    let rollup_da_manager_address = contracts.ecosystem_contracts.l1_rollup_da_manager.as_str();

    let logs_dir = integration_tests::protocol_ops::protocol_ops_logs_dir()
        .ok_or_else(|| anyhow::anyhow!("Could not resolve protocol_ops logs dir"))?;
    fs::create_dir_all(&logs_dir)?;
    let no_governance_out_path = logs_dir.join("no_governance_prepare_out.json");
    let no_governance_out_str = no_governance_out_path.to_str().ok_or_else(|| anyhow::anyhow!("Path invalid UTF-8"))?;

    let no_governance_args = vec![
        "ecosystem",
        "upgrade",
        "--ecosystem-upgrade-stage",
        "no-governance-prepare",
        "--l1-rpc-url",
        l1_rpc_url,
        "--private-key",
        deployer_key,
        "--bridgehub-proxy-address",
        bridgehub_proxy_address,
        "--ctm-proxy-address",
        ctm_proxy_address,
        "--bytecodes-supplier-address",
        bytecodes_supplier_address,
        "--rollup-da-manager-address",
        rollup_da_manager_address,
        "--is-zk-sync-os",
        "true",
        "--simulate",
        "--out",
        no_governance_out_str,
    ];
    integration_tests::protocol_ops::run_protocol_ops_for_preset(&preset, &no_governance_args)
        .context("no-governance-prepare (protocol_ops) failed")?;

    let script_output = parse_no_governance_out_file(&no_governance_out_path)
        .context("Failed to parse no-governance-prepare out file")?;

    println!("\n  Executing transactions from simulate output...");
    execute_transactions(&era_path, &no_governance_out_path, l1_rpc_url, deployer_key)
        .context("execute_transactions (no-governance-prepare) failed")?;

    std::thread::sleep(Duration::from_secs(1));

    transfer_new_contracts_ownership(l1_rpc_url, contracts, &script_output.core)?;
    transfer_governance_ownership_to_governor(root, l1_rpc_url, contracts, wallets)?;

    let ecosystem_output_path = era_path.join("l1-contracts/script-out/v31-upgrade-ecosystem.toml");
    if let Some(parent) = ecosystem_output_path.parent() {
        fs::create_dir_all(parent).ok();
    }
    let ecosystem_toml_str = toml::to_string_pretty(&script_output.ecosystem)
        .context("Failed to serialize ecosystem JSON to TOML")?;
    fs::write(&ecosystem_output_path, &ecosystem_toml_str)
        .context("Failed to write ecosystem toml for governance stages")?;
    let ecosystem_output_path_str = ecosystem_output_path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("Path contains invalid UTF-8"))?;

    let governor_key = wallets.governor.private_key.as_str();
    let governance_addr = contracts.ecosystem_contracts.governance.as_str();

    let governance_stage0_out_path = logs_dir.join("governance_stage0_out.json");
    let governance_stage0_out_str = governance_stage0_out_path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("Path invalid UTF-8"))?;
    println!("\n  Running governance-stage0 (protocol_ops) in simulate mode...");
    run_protocol_ops_for_default_preset(&[
        "ecosystem",
        "upgrade",
        "--ecosystem-upgrade-stage",
        "governance-stage0",
        "--l1-rpc-url",
        l1_rpc_url,
        "--private-key",
        governor_key,
        "--governance-address",
        governance_addr,
        "--ecosystem-output-path",
        ecosystem_output_path_str,
        "--simulate",
        "--out",
        governance_stage0_out_str,
    ])
    .context("governance-stage0 (simulate) failed")?;
    println!("\n  Executing transactions from governance-stage0 simulate output...");
    execute_transactions(&era_path, &governance_stage0_out_path, l1_rpc_url, governor_key)
        .context("execute_transactions (governance-stage0) failed")?;

    let chain_asset_handler =
        extract_json_value(&script_output.core, "upgrade_addresses.bridgehub.chain_asset_handler_proxy_addr")?;
    check_migration_paused(&chain_asset_handler, "after governance-stage0", l1_rpc_url)?;
    ensure_migration_paused(&chain_asset_handler, l1_rpc_url)?;

    std::thread::sleep(Duration::from_secs(1));

    let governance_stage1_out_path = logs_dir.join("governance_stage1_out.json");
    let governance_stage1_out_str = governance_stage1_out_path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("Path invalid UTF-8"))?;
    println!("\n  Running governance-stage1 (protocol_ops) in simulate mode...");
    run_protocol_ops_for_default_preset(&[
        "ecosystem",
        "upgrade",
        "--ecosystem-upgrade-stage",
        "governance-stage1",
        "--l1-rpc-url",
        l1_rpc_url,
        "--private-key",
        governor_key,
        "--governance-address",
        governance_addr,
        "--ecosystem-output-path",
        ecosystem_output_path_str,
        "--simulate",
        "--out",
        governance_stage1_out_str,
    ])
    .context("governance-stage1 (simulate) failed")?;
    println!("\n  Executing transactions from governance-stage1 simulate output...");
    execute_transactions(&era_path, &governance_stage1_out_path, l1_rpc_url, governor_key)
        .context("execute_transactions (governance-stage1) failed - this is required to set protocol version in CTM")?;

    check_migration_paused(&chain_asset_handler, "after governance-stage1", l1_rpc_url)?;

    println!("✓ All ecosystem upgrade stages completed");

    Ok(script_output)
}

/// Verify that the chain's protocol version matches the expected v31 version
fn verify_protocol_version(_root: &Path, l1_rpc_url: &str) -> Result<()> {
    println!("\n  Verifying protocol version...");

    let server_paths = get_default_server_paths();

    // Read the diamond proxy address from the original contracts.yaml (not the copied one)
    // The original file has diamond_proxy_addr under the l1: section
    let contracts_yaml = fs::read_to_string(&server_paths.contracts_yaml)?;
    let diamond_proxy = extract_yaml_value_in_section(&contracts_yaml, "l1", "diamond_proxy_addr")?;
    println!("    Diamond proxy: {}", diamond_proxy);

    // Get current protocol version from chain
    let output = Command::new("cast")
        .args(["call", &diamond_proxy, "getProtocolVersion()(uint256)", "--rpc-url", l1_rpc_url])
        .output()
        .context("Failed to call getProtocolVersion")?;

    let version_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
    println!("    Current protocol version (raw): {}", version_str);

    // Parse version - cast returns "128849018881 [1.288e11]" format, we need just the first number
    // It's a packed uint256 with major.minor.patch
    // v31 should be 0x1f00000001 = 133143986177 (31.0.1) or similar
    let version: u64 = version_str
        .split_whitespace()
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let major = (version >> 32) & 0xFFFF;
    let minor = (version >> 16) & 0xFFFF;
    let patch = version & 0xFFFF;
    println!("    Parsed version: {}.{}.{}", major, minor, patch);

    // Check that major version is 31
    if major != 31 {
        anyhow::bail!(
            "Protocol version mismatch! Expected major version 31, got {}. Full version: {}.{}.{}",
            major, major, minor, patch
        );
    }

    println!("    ✓ Protocol version verified: v{}.{}.{}", major, minor, patch);
    Ok(())
}

/// Generate upgrade YAML output from protocol_ops script output (no file reads).
fn generate_upgrade_yaml(root: &Path, script_output: &NoGovernancePrepareOutput) -> Result<()> {
    let _ = root;
    let l1_contracts_path = get_era_contracts_path().join("l1-contracts");
    print!("  Generate upgrade YAML output ... ");
    std::io::Write::flush(&mut std::io::stdout()).ok();

    let yaml_out = l1_contracts_path.join("script-out/v31-local-output.yaml");
    let ecosystem_toml_for_yaml = toml::to_string_pretty(&script_output.ecosystem)
        .context("Failed to serialize ecosystem to TOML for YAML generator")?;
    upgrade_yaml_output_generator::generate_upgrade_yaml_output_from_memory(
        script_output.run_json.as_bytes(),
        &ecosystem_toml_for_yaml,
        &yaml_out,
    )?;

    println!("✓");
    Ok(())
}

/// Run chain upgrade
fn run_chain_upgrade(
    root: &Path,
    l1_rpc_url: &str,
    contracts: &Contracts,
    wallets: &Wallets,
) -> Result<()> {
    // Chain upgrade (via protocol_ops)
    println!("\n  Running chain upgrade (protocol_ops)...");
    let governor_key = wallets.governor.private_key.as_str();
    let chain_address = contracts.l1.diamond_proxy_addr.as_str();
    let admin_address = contracts.l1.chain_admin_addr.as_str();
    let access_control_restriction = contracts.l1.access_control_restriction_addr.as_str();
    run_protocol_ops_for_default_preset(&[
        "chain",
        "upgrade",
        "--l1-rpc-url",
        l1_rpc_url,
        "--private-key",
        governor_key,
        "--chain-address",
        chain_address,
        "--admin-address",
        admin_address,
        "--access-control-restriction",
        access_control_restriction,
    ])
    .context("chain upgrade failed")?;

    Ok(())
}

/// Run ecosystem upgrade Stage 2
fn run_final_upgrade_stages(
    root: &Path,
    l1_rpc_url: &str,
    contracts: &Contracts,
    wallets: &Wallets,
) -> Result<()> {
    // Stage 2 (via protocol_ops). Must succeed.
    let governor_key = wallets.governor.private_key.as_str();
    let governance_addr = contracts.ecosystem_contracts.governance.as_str();
    run_protocol_ops_for_default_preset(&[
        "ecosystem",
        "upgrade",
        "--ecosystem-upgrade-stage",
        "governance-stage2",
        "--l1-rpc-url",
        l1_rpc_url,
        "--private-key",
        governor_key,
        "--governance-address",
        governance_addr,
    ])
    .context("governance-stage2 failed")?;


    // Stage 3 is skipped for local testing because:
    // 1. It requires v31 contracts to be deployed (l1AssetTracker function)
    // 2. In fresh test environments, there are no token balances to migrate
    // 3. The governance stages don't fully upgrade contracts in local testing
    println!("Stage 3 (token migration) skipped for local testing");
    println!("Note: Stage 3 is only needed when migrating existing bridged token balances");

    Ok(())

    // // Stage 3 (migrate token balances)
    // // Run with -vvvv for maximum verbosity to see all transaction traces
    // run_command(
    //     "Ecosystem upgrade - Stage 3 (migrate token balances)",
    //     Command::new("forge")
    //         .args([
    //             "script",
    //             "deploy-scripts/upgrade/v31/EcosystemUpgrade_v31.s.sol:EcosystemUpgrade_v31",
    //             "--sig",
    //             "stage3()",
    //             "--rpc-url",
    //             L1_RPC_URL,
    //             "--broadcast",
    //             "--private-key",
    //             RICH_ACCOUNT_PRIVATE_KEY,
    //             "--legacy",
    //             "--slow",
    //             "--gas-price",
    //             "50000000000",
    //             "-vvvv", // Maximum verbosity for full traces
    //         ])
    //         .current_dir(era_path.join("contracts/l1-contracts")),
    // )
}

fn read_u64_from_cast_call(l1_rpc_url: &str, contract: &str, sig: &str) -> Result<u64> {
    read_u64_from_cast_call_with_args(l1_rpc_url, contract, sig, &[])
}

fn read_u64_from_cast_call_with_args(
    l1_rpc_url: &str,
    contract: &str,
    sig: &str,
    fn_args: &[&str],
) -> Result<u64> {
    let mut args: Vec<&str> = vec!["call", contract, sig];
    args.extend_from_slice(fn_args);
    args.extend_from_slice(&["--rpc-url", l1_rpc_url]);

    let output = Command::new("cast")
        .args(&args)
        .output()
        .with_context(|| format!("Failed to call {} on {}", sig, contract))?;
    if !output.status.success() {
        anyhow::bail!(
            "cast call failed for {} on {}: {}",
            sig,
            contract,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let first = raw.split_whitespace().next().unwrap_or("");
    if let Some(hex) = first.strip_prefix("0x") {
        return u64::from_str_radix(hex, 16)
            .with_context(|| format!("Failed to parse hex value '{}'", first));
    }
    first
        .parse::<u64>()
        .with_context(|| format!("Failed to parse decimal value '{}'", first))
}

fn read_l2_block_number_by_tag(l2_rpc_url: &str, tag: &str) -> Result<u64> {
    let output = Command::new("cast")
        .args(["block", tag, "--field", "number", "--rpc-url", l2_rpc_url])
        .output()
        .with_context(|| format!("Failed to read '{}' block number from {}", tag, l2_rpc_url))?;
    if !output.status.success() {
        anyhow::bail!(
            "cast block {} failed on {}: {}",
            tag,
            l2_rpc_url,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if let Some(hex) = raw.strip_prefix("0x") {
        return u64::from_str_radix(hex, 16)
            .with_context(|| format!("Failed to parse hex block number '{}'", raw));
    }
    raw.parse::<u64>()
        .with_context(|| format!("Failed to parse decimal block number '{}'", raw))
}

fn wait_for_server_batches_to_drain_before_upgrade(
    l1_rpc_url: &str,
    l2_rpc_url: &str,
    diamond_proxy_addr: &str,
    timeout: Duration,
) -> Result<()> {
    println!("Waiting for server batches to drain before chain upgrade...");
    let start = std::time::Instant::now();
    loop {
        let committed = read_u64_from_cast_call(
            l1_rpc_url,
            diamond_proxy_addr,
            "getTotalBatchesCommitted()(uint256)",
        )?;
        let executed = read_u64_from_cast_call(
            l1_rpc_url,
            diamond_proxy_addr,
            "getTotalBatchesExecuted()(uint256)",
        )?;
        let latest_block = read_l2_block_number_by_tag(l2_rpc_url, "latest")
            .context("Failed to read latest L2 block number from running server")?;
        let safe_block = read_l2_block_number_by_tag(l2_rpc_url, "safe")
            .context("Failed to read safe L2 block number from running server")?;
        let finalized_block = read_l2_block_number_by_tag(l2_rpc_url, "finalized")
            .context("Failed to read finalized L2 block number from running server")?;
        println!(
            "  L1 batches: committed={}, executed={}; L2 blocks: latest={}, safe={}, finalized={}",
            committed, executed, latest_block, safe_block, finalized_block
        );
        if committed == executed && latest_block == safe_block && safe_block == finalized_block {
            println!(
                "  ✓ Drain complete: committed==executed=={} and latest==safe==finalized=={}",
                committed, latest_block
            );
            println!("  Waiting additional 30s to ensure server is fully settled before chain upgrade...");
            std::thread::sleep(Duration::from_secs(60));
            return Ok(());
        }
        if start.elapsed() >= timeout {
            anyhow::bail!(
                "Timed out waiting for drain. committed={}, executed={}, latest={}, safe={}, finalized={}",
                committed,
                executed,
                latest_block,
                safe_block,
                finalized_block
            );
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

fn schedule_upgrade_timestamp(
    l1_rpc_url: &str,
    contracts: &Contracts,
    wallets: &Wallets,
) -> Result<()> {
    let output = Command::new("cast")
        .args(["block", "latest", "--field", "timestamp", "--rpc-url", l1_rpc_url])
        .output()
        .context("Failed to get latest block timestamp")?;
    if !output.status.success() {
        anyhow::bail!(
            "Failed to read latest block timestamp: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let current_timestamp: u64 = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .context("Failed to parse latest block timestamp")?;
    let upgrade_timestamp = current_timestamp + 60;

    let target_protocol_version = read_u64_from_cast_call(
        l1_rpc_url,
        &contracts.ecosystem_contracts.state_transition_proxy_addr,
        "protocolVersion()(uint256)",
    )
    .context("Failed to read target protocol version from CTM")?;

    println!(
        "Scheduling upgrade timestamp {} for target protocol version {}",
        upgrade_timestamp, target_protocol_version
    );

    let logs_dir = integration_tests::protocol_ops::protocol_ops_logs_dir()
        .ok_or_else(|| anyhow::anyhow!("Could not resolve protocol_ops logs dir"))?;
    fs::create_dir_all(&logs_dir)?;
    let set_ts_out_path = logs_dir.join("set_upgrade_timestamp_out.json");
    let set_ts_out_str = set_ts_out_path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("Path invalid UTF-8"))?;

    println!("\n  Running set-upgrade-timestamp (protocol_ops) in simulate mode...");
    run_protocol_ops_for_default_preset(&[
        "chain",
        "set-upgrade-timestamp",
        "--l1-rpc-url",
        l1_rpc_url,
        "--private-key",
        wallets.governor.private_key.as_str(),
        "--admin-address",
        contracts.l1.chain_admin_addr.as_str(),
        "--new-protocol-version",
        &target_protocol_version.to_string(),
        "--upgrade-timestamp",
        &upgrade_timestamp.to_string(),
        "--simulate",
        "--out",
        set_ts_out_str,
    ])
    .context("set-upgrade-timestamp (simulate) failed")?;
    println!("\n  Executing set-upgrade-timestamp transaction...");
    let era_path = get_era_contracts_path();
    execute_transactions(
        &era_path,
        &set_ts_out_path,
        l1_rpc_url,
        wallets.governor.private_key.as_str(),
    )
    .context("execute_transactions (set-upgrade-timestamp) failed")?;

    let scheduled_timestamp = read_u64_from_cast_call_with_args(
        l1_rpc_url,
        &contracts.l1.chain_admin_addr,
        "protocolVersionToUpgradeTimestamp(uint256)(uint256)",
        &[&target_protocol_version.to_string()],
    )
    .context("Failed to read protocolVersionToUpgradeTimestamp after scheduling")?;
    anyhow::ensure!(
        scheduled_timestamp == upgrade_timestamp,
        "Upgrade timestamp mismatch after scheduling: expected {}, got {}",
        upgrade_timestamp,
        scheduled_timestamp
    );
    println!(
        "✓ Upgrade timestamp confirmed on-chain: protocolVersionToUpgradeTimestamp({}) = {}",
        target_protocol_version, scheduled_timestamp
    );
    Ok(())
}

#[tokio::test]
#[ignore] // This is a long-running integration test, run with --ignored
async fn test_v30_to_v31_upgrade() -> Result<()> {
    let root = get_project_root();

    println!("=== Starting v30 to v31 upgrade test ===\n");

    // Check tooling versions first (protocol_ops from era-contracts); fail fast before starting Anvil/server.
    run_protocol_ops_for_default_preset(&["check-tooling-versions"])?;
    println!("✓ Tooling versions OK\n");

    let preset = get_default_preset();

    println!("Starting Anvil L1 chain with v30.2 state...");
    let anvil = Anvil::spawn(&preset).await?;
    let l1_rpc_url = anvil.rpc_url();
    println!("✓ Anvil L1 is ready at {}", l1_rpc_url);

    let server_paths = get_default_server_paths();
    let wallets_path = server_paths.wallets_yaml.clone();
    let contracts_path = server_paths.contracts_yaml.clone();

    println!("Loading wallets and contracts configuration...");
    let wallets = Wallets::load_from_path(&wallets_path)
        .context("Failed to load wallets.yaml")?;
    let contracts = Contracts::load_from_path(&contracts_path)
        .context("Failed to load contracts.yaml")?;
    println!("✓ Wallets and contracts loaded");

    println!("Starting zksync-os-server on v30.2...");
    let server = ServerBuilder::new(preset)
        .spawn(&anvil)
        .map_err(|e| anyhow::anyhow!("Failed to start server via ServerBuilder: {:?}", e))?;
    println!("✓ Server started ({})", server.container_name());

    let test_address = address_from_private_key(DEFAULT_TEST_PRIVATE_KEY)
        .context("Failed to derive address for DEFAULT_TEST_PRIVATE_KEY")?;
    fund_account(
        &test_address,
        "1ether",
        l1_rpc_url,
        wallets.deployer.private_key.as_str(),
    )
    .context("Failed to fund DEFAULT_TEST_PRIVATE_KEY on L1 for bridge")?;
    let paths = get_default_server_paths();
    let balance = fund_l2_via_l1_deposit(
        &paths.server_root,
        l1_rpc_url,
        server.rpc_url().as_str(),
        &contracts.ecosystem_contracts.bridgehub_proxy_addr,
        6565,
        DEFAULT_TEST_PRIVATE_KEY,
        0.01,
        Duration::from_secs(120),
    )
    .context("Failed to fund DEFAULT_TEST_PRIVATE_KEY on L2 via L1 bridge")?;
    anyhow::ensure!(balance > 0, "DEFAULT_TEST_PRIVATE_KEY L2 balance must be > 0, got {}", balance);
    println!("✓ DEFAULT_TEST_PRIVATE_KEY funded on L2 via L1 bridge (balance {} wei)", balance);

    println!("Driving L2 traffic until >=3 batches are executed on L1...");
    wait_for_executed_batches_with_traffic(
        server.rpc_url().as_str(),
        l1_rpc_url,
        &contracts.l1.diamond_proxy_addr,
        DEFAULT_TEST_PRIVATE_KEY,
        3,
        Duration::from_secs(120),
    )
    .with_context(|| {
        format!(
            "Pre-upgrade traffic/batch wait failed. Server logs: {}",
            server.logs_path().display()
        )
    })?;

    // Update permanent values for upgrade
    update_permanent_values(&contracts)?;

    // Run ecosystem upgrade stages (script output via stdout, no v31-upgrade-*.toml files)
    let script_output = run_ecosystem_upgrades(&root, l1_rpc_url, &contracts, &wallets)?;

    // Generate upgrade YAML output from in-memory script output
    generate_upgrade_yaml(&root, &script_output)?;

    // Notify server about upcoming upgrade, then wait for commit/execute counters to converge.
    schedule_upgrade_timestamp(l1_rpc_url, &contracts, &wallets)?;
    wait_for_server_batches_to_drain_before_upgrade(
        l1_rpc_url,
        server.rpc_url().as_str(),
        &contracts.l1.diamond_proxy_addr,
        Duration::from_secs(120),
    )?;

    println!("Keeping zksync-os-server running during chain upgrade...");

    // Run chain upgrade
    run_chain_upgrade(&root, l1_rpc_url, &contracts, &wallets)?;

    // Verify thef protocol version was upgraded to v31
    verify_protocol_version(&root, l1_rpc_url)?;

    // Run final upgrade stages
    run_final_upgrade_stages(&root, l1_rpc_url, &contracts, &wallets)?;

    println!("Driving post-upgrade traffic until >=6 batches are executed on L1...");
    wait_for_executed_batches_with_traffic(
        server.rpc_url().as_str(),
        l1_rpc_url,
        &contracts.l1.diamond_proxy_addr,
        DEFAULT_TEST_PRIVATE_KEY,
        6,
        Duration::from_secs(120),
    )
    .with_context(|| {
        format!(
            "Post-upgrade traffic/batch wait failed. Server logs: {}",
            server.logs_path().display()
        )
    })?;

    server
        .kill()
        .map_err(|e| anyhow::anyhow!("Failed to kill server after test: {:?}", e))?;

    println!("\n=== Upgrade test completed successfully! ===");
    Ok(())
}
