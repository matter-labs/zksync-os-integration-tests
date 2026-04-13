use anyhow::{Context, Result};
use integration_tests::anvil::Anvil;
use integration_tests::anvil::DEFAULT_ANVIL_PRIVATE_KEY;
use integration_tests::anvil_utils::{
    fund_account, impersonate_account, stop_impersonating_account, RICH_ACCOUNT_PRIVATE_KEY,
};
use integration_tests::protocol_ops::EraContractsBackend;
use integration_tests::server::{L1DepositBaseToken, ServerBuilder};
use integration_tests::server_utils::address_from_private_key;
use integration_tests::upgrade_config::{Contracts, WalletsFile};
use integration_tests::upgrade_yaml_output_generator;
use std::fs;
use std::path::{Path, PathBuf};
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

/// Helper to get project root directory
fn get_project_root() -> PathBuf {
    integration_tests::utils::find_project_root().expect("Failed to find project root")
}

fn get_default_preset() -> integration_tests::presets::Preset {
    integration_tests::presets::load_current_preset().expect("Failed to load preset")
}

fn create_era_backend() -> Result<EraContractsBackend> {
    let preset = get_default_preset();
    EraContractsBackend::from_preset(&preset, "upgrade_v30_to_v31", &[])
}

fn get_default_chain_dir() -> PathBuf {
    integration_tests::preset_paths::chain_dir_for_version("v30.2")
        .expect("Failed to resolve chain dir for v30.2")
}

fn get_default_version_dir() -> PathBuf {
    get_default_chain_dir()
        .parent()
        .expect("chain_dir has no parent")
        .to_path_buf()
}

/// Build the v30.2 default-chain `config.yaml` from the ecosystem contracts +
/// per-chain wallets instead of keeping a hand-maintained file in tree. Uses
/// the same `ServerConfigBuilder` that `generate-l1-state` uses for freshly
/// generated chains. The file is written next to `genesis.json` in
/// `chain_dir`; `server.rs` (docker mode) mounts that directory and the
/// server reads both files from there.
fn write_upgrade_server_config(
    chain_dir: &Path,
    contracts: &Contracts,
    wallets: &WalletsFile,
    chain_id: u64,
) -> Result<PathBuf> {
    use integration_tests::server_config::ServerConfigBuilder;
    let chain_wallets = wallets
        .chains
        .get("default")
        .ok_or_else(|| anyhow::anyhow!("missing 'default' chain in wallets.yaml"))?;
    let genesis_path = chain_dir.join("genesis.json");
    // For v30.2 the on-chain `COMMITTER_ROLE` on the ValidatorTimelock
    // (0xcf4f9d6b…) is held by `blob_operator`'s address (0xb591…), not by
    // `commit_operator`'s address (0x9d46…). The wallets.yaml labels are
    // inconsistent with the v30.2 state generation — in pre-v31 the commit
    // tx *is* the blob-submitting tx, so the one key doubled as both. The
    // server expects `operator_commit_sk` to be the key whose address holds
    // COMMITTER_ROLE, so use blob_operator's key here.
    let yaml = ServerConfigBuilder::new(
        &contracts.ecosystem_contracts.bridgehub_proxy_addr,
        &contracts.ecosystem_contracts.l1_bytecodes_supplier_addr,
        &genesis_path,
        chain_id,
        &chain_wallets.blob_operator.private_key,
        &chain_wallets.prove_operator.private_key,
        &chain_wallets.execute_operator.private_key,
    )
    .build();
    let config_path = chain_dir.join("config.yaml");
    fs::write(&config_path, yaml)
        .with_context(|| format!("write config to {}", config_path.display()))?;
    Ok(config_path)
}

/// Write the `permanent-values.toml` input that era-contracts' upgrade and
/// CTM-deployment scripts read. Consumers include
/// `l1-contracts/deploy-scripts/ctm/DeployCTM.s.sol`,
/// `l1-contracts/scripts/upgrade-script-utils.ts` (reads `upgrade-envs/
/// permanent-values/<env>.toml`), and the foundry `.env`
/// `PERMANENT_VALUES_INPUT` variable. We write both `script-config/
/// permanent-values.toml` (used by forge scripts) and
/// `upgrade-envs/permanent-values/local.toml` (used by the TypeScript
/// upgrade helpers).
fn update_permanent_values(
    contracts_backend: &EraContractsBackend,
    contracts: &Contracts,
) -> Result<()> {
    print!("  Updating permanent values ... ");
    std::io::Write::flush(&mut std::io::stdout()).ok();

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
        era_chain_id, bridgehub_addr, ctm_addr, bytecodes_supplier, create2_factory, create2_salt
    );

    // Write via the backend so it lands in the right place regardless of
    // whether era-contracts is a local checkout or a docker image.
    contracts_backend.write_repo_file(
        "l1-contracts/script-config/permanent-values.toml",
        &permanent_values,
    )?;
    contracts_backend.write_repo_file(
        "l1-contracts/upgrade-envs/permanent-values/local.toml",
        &permanent_values,
    )?;

    println!("✓");
    Ok(())
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
    anyhow::bail!(
        "Could not find key '{}' in section '{}' in YAML",
        key,
        section
    )
}

/// Fund governance accounts with ETH for upgrade transactions
fn fund_governance_accounts(
    contracts_backend: &EraContractsBackend,
    l1_rpc_url: &str,
) -> Result<()> {
    // Governor address that needs funding
    let governor_address = "0x8002cd98cfb563492a6fb3e7c8243b7b9ad4cc92";

    // Send 10 ETH to governor from rich account
    // Use high gas price to replace any pending transactions with same nonce
    contracts_backend
        .cast(&[
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
        ])
        .context("Fund governor account")?;

    Ok(())
}

/// Transfer ownership of the ecosystem governance timelock to the governor wallet
/// This is needed because the forge scripts broadcast from the governance owner,
/// but we only have the governor wallet's private key
fn transfer_governance_ownership_to_governor(
    contracts_backend: &EraContractsBackend,
    _root: &Path,
    l1_rpc_url: &str,
    contracts: &Contracts,
    wallets: &WalletsFile,
) -> Result<()> {
    println!("\n  Transferring governance ownership to governor wallet...");

    let ecosystem_governance = &contracts.ecosystem_contracts.governance;
    println!("    Ecosystem governance: {}", ecosystem_governance);

    let governor_address = wallets.ecosystem.governor.address.as_str();
    println!("    Governor wallet: {}", governor_address);

    // Read current owner of governance
    let current_owner = contracts_backend
        .cast(&[
            "call",
            ecosystem_governance,
            "owner()(address)",
            "--rpc-url",
            l1_rpc_url,
        ])
        .context("Failed to read governance owner")?;
    let current_owner = current_owner.trim().to_string();
    println!("    Current governance owner: {}", current_owner);

    // Check if already owned by governor
    if current_owner.to_lowercase() == governor_address.to_lowercase() {
        println!("    Already owned by governor, skipping");
        return Ok(());
    }

    // Transfer ownership via impersonation
    impersonate_account(&current_owner, l1_rpc_url)?;
    fund_account(
        &current_owner,
        "1ether",
        l1_rpc_url,
        RICH_ACCOUNT_PRIVATE_KEY,
    )?;

    contracts_backend
        .cast(&[
            "send",
            ecosystem_governance,
            "transferOwnership(address)",
            governor_address,
            "--from",
            &current_owner,
            "--rpc-url",
            l1_rpc_url,
            "--unlocked",
        ])
        .context("Failed to transfer governance ownership")?;

    stop_impersonating_account(&current_owner, l1_rpc_url);

    // Accept ownership as governor
    let governor_private_key = wallets.ecosystem.governor.private_key.as_str();

    contracts_backend
        .cast(&[
            "send",
            ecosystem_governance,
            "acceptOwnership()",
            "--private-key",
            governor_private_key,
            "--rpc-url",
            l1_rpc_url,
        ])
        .context("Failed to accept governance ownership")?;

    println!("    ✓ Governance ownership transferred to governor");
    Ok(())
}

/// Transfer ownership of a contract from current owner to ecosystem governance
fn transfer_contract_ownership(
    contracts_backend: &EraContractsBackend,
    contract_name: &str,
    contract_address: &str,
    ecosystem_governance: &str,
    l1_rpc_url: &str,
) -> Result<()> {
    println!("    {} ({})", contract_name, contract_address);

    // Read current owner
    let current_owner = contracts_backend
        .cast(&[
            "call",
            contract_address,
            "owner()(address)",
            "--rpc-url",
            l1_rpc_url,
        ])
        .context("Failed to read owner")?;
    let current_owner = current_owner.trim().to_string();
    println!("      Current owner: {}", current_owner);

    // Check if already owned by ecosystem governance
    if current_owner.to_lowercase() == ecosystem_governance.to_lowercase() {
        println!("      Already owned by ecosystem governance, skipping");
        return Ok(());
    }

    // Impersonate current owner and transfer
    impersonate_account(&current_owner, l1_rpc_url)?;
    fund_account(
        &current_owner,
        "1ether",
        l1_rpc_url,
        RICH_ACCOUNT_PRIVATE_KEY,
    )?;

    contracts_backend
        .cast(&[
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
        .context(format!("Failed to transfer {} ownership", contract_name))?;

    stop_impersonating_account(&current_owner, l1_rpc_url);

    // Accept ownership as ecosystem governance
    impersonate_account(ecosystem_governance, l1_rpc_url)?;
    fund_account(
        ecosystem_governance,
        "1ether",
        l1_rpc_url,
        RICH_ACCOUNT_PRIVATE_KEY,
    )?;

    contracts_backend
        .cast(&[
            "send",
            contract_address,
            "acceptOwnership()",
            "--from",
            ecosystem_governance,
            "--rpc-url",
            l1_rpc_url,
            "--unlocked",
        ])
        .context(format!("Failed to accept {} ownership", contract_name))?;

    stop_impersonating_account(ecosystem_governance, l1_rpc_url);

    println!("      ✓ Ownership transferred to ecosystem governance");
    Ok(())
}

/// Transfer ownership of newly deployed contracts to ecosystem governance.
/// This is called AFTER no-governance-prepare since contracts are deployed
/// with the deployer as owner, but governance stages need ecosystem
/// governance to be the owner.
///
/// FIXME(#7, Stanislav): this is only correct if no-governance-prepare
/// actually deploys a fresh NTV in v31 (the key lives under
/// `upgrade_addresses.native_token_vault_addr`, but NTV predates v31).
/// Verify that the address here is a newly deployed contract and not a
/// pre-existing one whose ownership we shouldn't be touching.
///
/// FIXME(#7, Stanislav, follow-up PR by Kalman): on mainnet we can't
/// `acceptOwnership()` via a raw PK, and `transferOwnership` should be
/// produced as part of no-governance-prepare rather than executed here.
/// After no-gov-prepare runs we should be fully equipped to conduct the
/// upgrade without further owner-keyed actions.
fn transfer_new_contracts_ownership(
    contracts_backend: &EraContractsBackend,
    l1_rpc_url: &str,
    contracts: &Contracts,
    core: &serde_json::Value,
) -> Result<()> {
    println!("\n  Transferring contract ownership to ecosystem governance...");
    std::io::Write::flush(&mut std::io::stdout()).ok();

    let ecosystem_governance = &contracts.ecosystem_contracts.governance;
    println!("    Ecosystem governance: {}", ecosystem_governance);

    let chain_asset_handler = extract_json_value(
        core,
        "upgrade_addresses.bridgehub.chain_asset_handler_proxy_addr",
    )?;
    transfer_contract_ownership(
        contracts_backend,
        "ChainAssetHandler",
        &chain_asset_handler,
        ecosystem_governance,
        l1_rpc_url,
    )?;

    let native_token_vault = extract_json_value(core, "upgrade_addresses.native_token_vault_addr")?;
    transfer_contract_ownership(
        contracts_backend,
        "NativeTokenVault",
        &native_token_vault,
        ecosystem_governance,
        l1_rpc_url,
    )?;

    println!("  ✓ All ownership transfers complete");
    Ok(())
}

/// Check if migration is paused on ChainAssetHandler
fn check_migration_paused(
    contracts_backend: &EraContractsBackend,
    chain_asset_handler: &str,
    context: &str,
    l1_rpc_url: &str,
) -> Result<()> {
    // Check migrationPaused
    let result = contracts_backend
        .cast(&[
            "call",
            chain_asset_handler,
            "migrationPaused()(bool)",
            "--rpc-url",
            l1_rpc_url,
        ])
        .context("Failed to call migrationPaused")?;
    let result = result.trim();
    println!("  migrationPaused() {} = {}", context, result);

    // Also check which implementation is being used
    let impl_addr = contracts_backend
        .cast(&[
            "storage",
            chain_asset_handler,
            "0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc",
            "--rpc-url",
            l1_rpc_url,
        ])
        .context("Failed to read implementation slot")?;
    let impl_addr = impl_addr.trim();
    println!("  Implementation {} = {}", context, impl_addr);

    Ok(())
}

/// Ensure migration is paused, calling pauseMigration() directly if needed
fn ensure_migration_paused(
    contracts_backend: &EraContractsBackend,
    chain_asset_handler: &str,
    l1_rpc_url: &str,
) -> Result<()> {
    // Check if already paused
    let result = contracts_backend
        .cast(&[
            "call",
            chain_asset_handler,
            "migrationPaused()(bool)",
            "--rpc-url",
            l1_rpc_url,
        ])
        .context("Failed to call migrationPaused")?;
    if result.trim() == "true" {
        println!("  Migration already paused");
        return Ok(());
    }

    println!("  Migration not paused, pausing via impersonation...");

    // Get the owner and pause via impersonation
    let owner = contracts_backend
        .cast(&[
            "call",
            chain_asset_handler,
            "owner()(address)",
            "--rpc-url",
            l1_rpc_url,
        ])
        .context("Failed to read owner")?;
    let owner = owner.trim().to_string();

    impersonate_account(&owner, l1_rpc_url)?;
    fund_account(&owner, "1ether", l1_rpc_url, RICH_ACCOUNT_PRIVATE_KEY)?;

    contracts_backend
        .cast(&[
            "send",
            chain_asset_handler,
            "pauseMigration()",
            "--from",
            &owner,
            "--rpc-url",
            l1_rpc_url,
            "--unlocked",
        ])
        .context("Failed to pause migration")?;

    stop_impersonating_account(&owner, l1_rpc_url);

    println!("  ✓ Migration paused via direct call");
    Ok(())
}

/// Run ecosystem upgrade stages. Returns script output from no-governance-prepare (no v31-upgrade-*.toml files on disk).
fn run_ecosystem_upgrades(
    contracts_backend: &EraContractsBackend,
    root: &Path,
    l1_rpc_url: &str,
    contracts: &Contracts,
    wallets: &WalletsFile,
) -> Result<NoGovernancePrepareOutput> {
    println!("\n=== Running Ecosystem Upgrades ===");

    fund_governance_accounts(contracts_backend, l1_rpc_url)?;

    // Stage 0: no-governance-prepare in simulate mode, then execute transactions from --out file
    println!("\n  Running no-governance-prepare (protocol_ops) in simulate mode...");
    let deployer_key = wallets.ecosystem.deployer.private_key.as_str();
    let bridgehub_proxy_address = contracts.ecosystem_contracts.bridgehub_proxy_addr.as_str();
    let ctm_proxy_address = contracts
        .ecosystem_contracts
        .state_transition_proxy_addr
        .as_str();
    let bytecodes_supplier_address = contracts
        .ecosystem_contracts
        .l1_bytecodes_supplier_addr
        .as_str();
    let rollup_da_manager_address = contracts.ecosystem_contracts.l1_rollup_da_manager.as_str();

    let no_governance_out = "no_governance_prepare_out.json";
    let no_governance_out_arg = contracts_backend.work_path(no_governance_out);

    let no_governance_args = vec![
        "ecosystem",
        "upgrade",
        "--ecosystem-upgrade-stage",
        "no-governance-prepare",
        "--l1-rpc-url",
        l1_rpc_url,
        "--private-key",
        deployer_key,
        "--bridgehub",
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
        &no_governance_out_arg,
    ];
    contracts_backend
        .protocol_ops(&no_governance_args)
        .context("no-governance-prepare (protocol_ops) failed")?;

    let script_output =
        parse_no_governance_out_file(&contracts_backend.work_dir().join(no_governance_out))
            .context("Failed to parse no-governance-prepare out file")?;

    println!("\n  Executing transactions from simulate output...");
    contracts_backend
        .execute_protocol_ops_out(no_governance_out, l1_rpc_url, deployer_key)
        .context("execute_transactions (no-governance-prepare) failed")?;

    std::thread::sleep(Duration::from_secs(1));

    transfer_new_contracts_ownership(
        contracts_backend,
        l1_rpc_url,
        contracts,
        &script_output.core,
    )?;
    transfer_governance_ownership_to_governor(
        contracts_backend,
        root,
        l1_rpc_url,
        contracts,
        wallets,
    )?;

    // Write v31-upgrade-ecosystem.toml via the backend so it is visible to
    // governance-stage0/1 regardless of whether era-contracts is a local
    // checkout or a docker image. Pass the container-side path to
    // protocol_ops (same as host-side path in local mode).
    let ecosystem_output_rel = "l1-contracts/script-out/v31-upgrade-ecosystem.toml";
    let ecosystem_toml_str = toml::to_string_pretty(&script_output.ecosystem)
        .context("Failed to serialize ecosystem JSON to TOML")?;
    contracts_backend
        .write_repo_file(ecosystem_output_rel, &ecosystem_toml_str)
        .context("Failed to write ecosystem toml for governance stages")?;
    let ecosystem_output_path_str = contracts_backend.repo_path(ecosystem_output_rel);
    let ecosystem_output_path_str = ecosystem_output_path_str.as_str();

    let governor_key = wallets.ecosystem.governor.private_key.as_str();
    let governance_addr = contracts.ecosystem_contracts.governance.as_str();

    let governance_stage0_out = "governance_stage0_out.json";
    let governance_stage0_out_arg = contracts_backend.work_path(governance_stage0_out);
    println!("\n  Running governance-stage0 (protocol_ops) in simulate mode...");
    contracts_backend
        .protocol_ops(&[
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
            &governance_stage0_out_arg,
        ])
        .context("governance-stage0 (simulate) failed")?;
    println!("\n  Executing transactions from governance-stage0 simulate output...");
    contracts_backend
        .execute_protocol_ops_out(governance_stage0_out, l1_rpc_url, governor_key)
        .context("execute_transactions (governance-stage0) failed")?;

    let chain_asset_handler = extract_json_value(
        &script_output.core,
        "upgrade_addresses.bridgehub.chain_asset_handler_proxy_addr",
    )?;
    check_migration_paused(
        contracts_backend,
        &chain_asset_handler,
        "after governance-stage0",
        l1_rpc_url,
    )?;
    ensure_migration_paused(contracts_backend, &chain_asset_handler, l1_rpc_url)?;

    std::thread::sleep(Duration::from_secs(1));

    let governance_stage1_out = "governance_stage1_out.json";
    let governance_stage1_out_arg = contracts_backend.work_path(governance_stage1_out);
    println!("\n  Running governance-stage1 (protocol_ops) in simulate mode...");
    contracts_backend
        .protocol_ops(&[
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
            &governance_stage1_out_arg,
        ])
        .context("governance-stage1 (simulate) failed")?;
    println!("\n  Executing transactions from governance-stage1 simulate output...");
    contracts_backend.execute_protocol_ops_out(governance_stage1_out, l1_rpc_url, governor_key)
        .context("execute_transactions (governance-stage1) failed - this is required to set protocol version in CTM")?;

    check_migration_paused(
        contracts_backend,
        &chain_asset_handler,
        "after governance-stage1",
        l1_rpc_url,
    )?;

    println!("✓ All ecosystem upgrade stages completed");

    Ok(script_output)
}

/// Verify that the chain's protocol version matches the expected v31 version
fn verify_protocol_version(
    contracts_backend: &EraContractsBackend,
    _root: &Path,
    l1_rpc_url: &str,
) -> Result<()> {
    println!("\n  Verifying protocol version...");

    let chain_dir = get_default_chain_dir();

    // Read the diamond proxy address from the original contracts.yaml (not the copied one)
    // The original file has diamond_proxy_addr under the l1: section
    let contracts_yaml = fs::read_to_string(chain_dir.join("contracts.yaml"))?;
    let diamond_proxy = extract_yaml_value_in_section(&contracts_yaml, "l1", "diamond_proxy_addr")?;
    println!("    Diamond proxy: {}", diamond_proxy);

    // Get current protocol version from chain
    let version_str = contracts_backend
        .cast(&[
            "call",
            &diamond_proxy,
            "getProtocolVersion()(uint256)",
            "--rpc-url",
            l1_rpc_url,
        ])
        .context("Failed to call getProtocolVersion")?;
    let version_str = version_str.trim().to_string();
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
            major,
            major,
            minor,
            patch
        );
    }

    println!(
        "    ✓ Protocol version verified: v{}.{}.{}",
        major, minor, patch
    );
    Ok(())
}

/// Generate upgrade YAML output from protocol_ops script output. The
/// generated YAML is a test-inspection artifact (not consumed by any
/// downstream script in the currently-enabled steps), so we drop it in
/// the backend's host-side `work_dir` — that works for both local and
/// docker era-contracts and keeps the file with the rest of the run's
/// artifacts.
fn generate_upgrade_yaml(
    contracts_backend: &EraContractsBackend,
    script_output: &NoGovernancePrepareOutput,
) -> Result<()> {
    print!("  Generate upgrade YAML output ... ");
    std::io::Write::flush(&mut std::io::stdout()).ok();

    let yaml_out = contracts_backend.work_dir().join("v31-local-output.yaml");
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
    contracts_backend: &EraContractsBackend,
    _root: &Path,
    l1_rpc_url: &str,
    contracts: &Contracts,
    wallets: &WalletsFile,
) -> Result<()> {
    // Chain upgrade (via protocol_ops)
    println!("\n  Running chain upgrade (protocol_ops)...");
    let governor_key = wallets.ecosystem.governor.private_key.as_str();
    let chain_address = contracts.l1.diamond_proxy_addr.as_str();
    let admin_address = contracts.l1.chain_admin_addr.as_str();
    let access_control_restriction = contracts.l1.access_control_restriction_addr.as_str();
    contracts_backend
        .protocol_ops(&[
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
    contracts_backend: &EraContractsBackend,
    _root: &Path,
    l1_rpc_url: &str,
    contracts: &Contracts,
    wallets: &WalletsFile,
) -> Result<()> {
    // Stage 2 (via protocol_ops). Must succeed.
    let governor_key = wallets.ecosystem.governor.private_key.as_str();
    let governance_addr = contracts.ecosystem_contracts.governance.as_str();
    contracts_backend
        .protocol_ops(&[
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

fn read_u64_from_cast_call(
    contracts_backend: &EraContractsBackend,
    l1_rpc_url: &str,
    contract: &str,
    sig: &str,
) -> Result<u64> {
    read_u64_from_cast_call_with_args(contracts_backend, l1_rpc_url, contract, sig, &[])
}

fn read_u64_from_cast_call_with_args(
    contracts_backend: &EraContractsBackend,
    l1_rpc_url: &str,
    contract: &str,
    sig: &str,
    fn_args: &[&str],
) -> Result<u64> {
    let mut args: Vec<&str> = vec!["call", contract, sig];
    args.extend_from_slice(fn_args);
    args.extend_from_slice(&["--rpc-url", l1_rpc_url]);

    let raw = contracts_backend
        .cast(&args)
        .with_context(|| format!("Failed to call {} on {}", sig, contract))?;
    let raw = raw.trim();
    let first = raw.split_whitespace().next().unwrap_or("");
    if let Some(hex) = first.strip_prefix("0x") {
        return u64::from_str_radix(hex, 16)
            .with_context(|| format!("Failed to parse hex value '{}'", first));
    }
    first
        .parse::<u64>()
        .with_context(|| format!("Failed to parse decimal value '{}'", first))
}

fn read_l2_block_number_by_tag(
    contracts_backend: &EraContractsBackend,
    l2_rpc_url: &str,
    tag: &str,
) -> Result<u64> {
    let raw = contracts_backend
        .cast(&["block", tag, "--field", "number", "--rpc-url", l2_rpc_url])
        .with_context(|| format!("Failed to read '{}' block number from {}", tag, l2_rpc_url))?;
    let raw = raw.trim();
    if let Some(hex) = raw.strip_prefix("0x") {
        return u64::from_str_radix(hex, 16)
            .with_context(|| format!("Failed to parse hex block number '{}'", raw));
    }
    raw.parse::<u64>()
        .with_context(|| format!("Failed to parse decimal block number '{}'", raw))
}

// FIXME(#7, Stanislav): This test-only drain strategy relies on stopping new
// block production until `safe == finalized`, which is not what we'll do in
// production — mainnet keeps producing blocks through an upgrade and
// `safe` will never catch up to `finalized`. Follow-up should check the
// protocol version on the server and wait until all blocks with the
// previous version are finalized, regardless of whether new (next-version)
// blocks are still being produced.
fn wait_for_server_batches_to_drain_before_upgrade(
    contracts_backend: &EraContractsBackend,
    l1_rpc_url: &str,
    l2_rpc_url: &str,
    diamond_proxy_addr: &str,
    timeout: Duration,
) -> Result<()> {
    println!("Waiting for server batches to drain before chain upgrade...");
    let start = std::time::Instant::now();
    loop {
        let committed = read_u64_from_cast_call(
            contracts_backend,
            l1_rpc_url,
            diamond_proxy_addr,
            "getTotalBatchesCommitted()(uint256)",
        )?;
        let executed = read_u64_from_cast_call(
            contracts_backend,
            l1_rpc_url,
            diamond_proxy_addr,
            "getTotalBatchesExecuted()(uint256)",
        )?;
        let latest_block = read_l2_block_number_by_tag(contracts_backend, l2_rpc_url, "latest")
            .context("Failed to read latest L2 block number from running server")?;
        let safe_block = read_l2_block_number_by_tag(contracts_backend, l2_rpc_url, "safe")
            .context("Failed to read safe L2 block number from running server")?;
        let finalized_block =
            read_l2_block_number_by_tag(contracts_backend, l2_rpc_url, "finalized")
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
    contracts_backend: &EraContractsBackend,
    l1_rpc_url: &str,
    contracts: &Contracts,
    wallets: &WalletsFile,
) -> Result<()> {
    let raw = contracts_backend
        .cast(&[
            "block",
            "latest",
            "--field",
            "timestamp",
            "--rpc-url",
            l1_rpc_url,
        ])
        .context("Failed to get latest block timestamp")?;
    let current_timestamp: u64 = raw
        .trim()
        .parse()
        .context("Failed to parse latest block timestamp")?;
    let upgrade_timestamp = current_timestamp + 60;

    let target_protocol_version = read_u64_from_cast_call(
        contracts_backend,
        l1_rpc_url,
        &contracts.ecosystem_contracts.state_transition_proxy_addr,
        "protocolVersion()(uint256)",
    )
    .context("Failed to read target protocol version from CTM")?;

    println!(
        "Scheduling upgrade timestamp {} for target protocol version {}",
        upgrade_timestamp, target_protocol_version
    );

    let new_protocol_version = target_protocol_version.to_string();
    let upgrade_timestamp_str = upgrade_timestamp.to_string();

    // Call ChainAdmin.setUpgradeTimestamp directly rather than through
    // `protocol_ops chain set-upgrade-timestamp`. The v31 protocol_ops script
    // (AdminFunctions.adminScheduleUpgrade) routes the call through
    // ChainAdmin.multicall, which requires setUpgradeTimestamp to be
    // `onlySelf` — that's the v31 shape. In the v30.2 fixture state this
    // repo loads, setUpgradeTimestamp is `onlyOwner`, so the multicall path
    // reverts inside with "Ownable: caller is not the owner" (msg.sender
    // inside the inner call is the ChainAdmin itself, not its owner). The
    // owner (= ecosystem governor) can call setUpgradeTimestamp directly
    // on either version, so we do that and skip the shim entirely.
    println!("\n  Calling ChainAdmin.setUpgradeTimestamp directly as governor...");
    contracts_backend
        .cast(&[
            "send",
            &contracts.l1.chain_admin_addr,
            "setUpgradeTimestamp(uint256,uint256)",
            &new_protocol_version,
            &upgrade_timestamp_str,
            "--private-key",
            &wallets.ecosystem.governor.private_key,
            "--rpc-url",
            l1_rpc_url,
        ])
        .context("ChainAdmin.setUpgradeTimestamp failed")?;

    let scheduled_timestamp = read_u64_from_cast_call_with_args(
        contracts_backend,
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
async fn test_v30_to_v31_upgrade() -> Result<()> {
    let root = get_project_root();

    println!("=== Starting v30 to v31 upgrade test ===\n");

    let contracts_backend = create_era_backend()?;

    let preset = get_default_preset();

    let version_dir = get_default_version_dir();
    let chain_dir = get_default_chain_dir();
    let l1_state_path = integration_tests::anvil::resolve_l1_state_in_version_dir(&version_dir)?;

    println!("Starting Anvil L1 chain with v30.2 state...");
    let anvil = Anvil::spawn_with_state(&l1_state_path).await?;
    let l1_rpc_url = anvil.rpc_url();
    println!("✓ Anvil L1 is ready at {}", l1_rpc_url);

    let wallets_path = chain_dir.join("wallets.yaml");
    let contracts_path = chain_dir.join("contracts.yaml");

    println!("Loading wallets and contracts configuration...");
    let wallets = integration_tests::upgrade_config::load_wallets(&wallets_path)
        .context("Failed to load wallets.yaml")?;
    let contracts =
        Contracts::load_from_path(&contracts_path).context("Failed to load contracts.yaml")?;
    println!("✓ Wallets and contracts loaded");

    // Build config.yaml from contracts + wallets instead of keeping a hand-
    // maintained copy in tree. Same ServerConfigBuilder that generate-l1-state
    // uses for freshly generated chains.
    let config_path = write_upgrade_server_config(&chain_dir, &contracts, &wallets, 6565)?;

    println!("Starting zksync-os-server on v30.2...");
    let server = ServerBuilder::new(preset, "upgrade_v30_to_v31")
        .chain_name("default")
        .config_path(&config_path)
        .diamond_proxy_addr(&contracts.l1.diamond_proxy_addr)
        .bridgehub_addr(&contracts.ecosystem_contracts.bridgehub_proxy_addr)
        .chain_id(6565)
        .spawn(&anvil)
        .map_err(|e| anyhow::anyhow!("Failed to start server via ServerBuilder: {:?}", e))?;
    println!("✓ Server started ({})", server.container_name());

    // DEFAULT_ANVIL_PRIVATE_KEY is one of anvil's pre-funded accounts (10k ETH
    // from genesis, preserved across --load-state), so no L1 top-up is needed.
    //
    // Deposit ≥ 1 ETH so traffic txs pass `eth_estimateGas`. The zksync-os
    // RPC simulates the tx with `gas_limit = block_gas_limit (100M)` and
    // `max_fee_per_gas ≈ base_fee × gas_price_scale_factor`, then trips
    // `LackOfFundForMaxFee` if `balance < max_fee_per_gas × 100M + value`
    // before narrowing the gas limit. With 0.01 ETH the check fails at
    // base_fee ≈ 0.1 gwei. 1 ETH gives enough headroom.
    let test_address = address_from_private_key(DEFAULT_ANVIL_PRIVATE_KEY)
        .context("Failed to derive address for DEFAULT_ANVIL_PRIVATE_KEY")?;
    let balance = server
        .fund_account_via_l1_deposit(&test_address, 1.0, L1DepositBaseToken::Eth)
        .await
        .context("Failed to fund DEFAULT_ANVIL_PRIVATE_KEY on L2 via L1 bridge")?;
    anyhow::ensure!(
        balance > 0,
        "DEFAULT_ANVIL_PRIVATE_KEY L2 balance must be > 0, got {}",
        balance
    );
    println!(
        "✓ DEFAULT_ANVIL_PRIVATE_KEY funded on L2 via L1 bridge (balance {} wei)",
        balance
    );

    println!("Driving L2 traffic until 3 more batches are executed on L1...");
    server
        .wait_for_executed_batches_with_traffic()
        .with_context(|| {
            format!(
                "Pre-upgrade traffic/batch wait failed. Server logs: {}",
                server.logs_path().display()
            )
        })?;

    // Update permanent values for upgrade
    update_permanent_values(&contracts_backend, &contracts)?;

    // Run ecosystem upgrade stages (script output via stdout, no v31-upgrade-*.toml files)
    let script_output =
        run_ecosystem_upgrades(&contracts_backend, &root, l1_rpc_url, &contracts, &wallets)?;

    // Generate upgrade YAML output from in-memory script output
    generate_upgrade_yaml(&contracts_backend, &script_output)?;

    // Notify server about upcoming upgrade, then wait for commit/execute counters to converge.
    schedule_upgrade_timestamp(&contracts_backend, l1_rpc_url, &contracts, &wallets)?;
    wait_for_server_batches_to_drain_before_upgrade(
        &contracts_backend,
        l1_rpc_url,
        server.rpc_url().as_str(),
        &contracts.l1.diamond_proxy_addr,
        Duration::from_secs(120),
    )?;

    // STOP HERE — the actual v30 → v31 chain upgrade (and everything
    // downstream) is disabled until the upstream upgrade path on
    // era-contracts' `gateway-commands-in-protocol-ops` branch is ready
    // against this fixture. `AdminFunctions.s.sol::upgradeChainFromCTM`
    // currently reverts inside the DiamondProxy fallback with `"F"` when
    // applied to the committed v30.2 state (protocol version v30.0.1 per
    // `IZKChain.getProtocolVersion`), which is an upstream issue, not a
    // test-infrastructure one. When the upstream flow is ready, delete
    // this early return and re-enable the four steps below it.
    println!(
        "\n=== Stopping before chain upgrade — upstream upgrade flow is WIP. \
         Test passes up to and including schedule_upgrade_timestamp + drain. ==="
    );
    server
        .kill()
        .map_err(|e| anyhow::anyhow!("Failed to kill server after test: {:?}", e))?;
    return Ok(());

    #[allow(unreachable_code)]
    {
        println!("Keeping zksync-os-server running during chain upgrade...");

        // Run chain upgrade
        run_chain_upgrade(&contracts_backend, &root, l1_rpc_url, &contracts, &wallets)?;

        // Verify the protocol version was upgraded to v31
        verify_protocol_version(&contracts_backend, &root, l1_rpc_url)?;

        // Run final upgrade stages
        run_final_upgrade_stages(&contracts_backend, &root, l1_rpc_url, &contracts, &wallets)?;

        // Wait for 3 *new* batches produced under v31, regardless of how many were
        // produced during the pre-upgrade phase.
        println!("Driving post-upgrade traffic until 3 more batches are executed on L1...");
        server
            .wait_for_executed_batches_with_traffic()
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
}
