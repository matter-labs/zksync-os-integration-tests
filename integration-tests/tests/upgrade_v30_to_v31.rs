use anyhow::{Context, Result};
use integration_tests::anvil::Anvil;
use integration_tests::anvil::DEFAULT_ANVIL_PRIVATE_KEY;
use integration_tests::anvil_utils::{
    fund_account, impersonate_account, stop_impersonating_account, RICH_ACCOUNT_PRIVATE_KEY,
};
use integration_tests::l1_state::EcosystemConfig;
use integration_tests::protocol_ops::EraContractsBackend;
use integration_tests::server::{L1DepositBaseToken, ServerBuilder};
use integration_tests::server_utils::address_from_private_key;
use integration_tests::upgrade_config::{Contracts, WalletsFile};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Script output from `protocol_ops ecosystem upgrade-prepare`, read from
/// the per-command metadata block in `manifest.json`.
#[derive(Clone)]
struct UpgradePrepareOutput {
    core: serde_json::Value,
}

fn parse_upgrade_prepare_manifest(manifest_path: &Path) -> Result<UpgradePrepareOutput> {
    let content = fs::read_to_string(manifest_path).with_context(|| {
        format!("Failed to read manifest: {}", manifest_path.display())
    })?;
    let root: serde_json::Value = serde_json::from_str(&content)
        .context("Failed to parse upgrade-prepare manifest.json")?;
    let output = root
        .get("metadata")
        .and_then(|m| m.as_array())
        .and_then(|arr| arr.first())
        .and_then(|entry| entry.get("output"))
        .ok_or_else(|| anyhow::anyhow!("Missing metadata[0].output in manifest"))?;
    let core = output
        .get("core")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("Missing output.core in manifest metadata"))?;
    Ok(UpgradePrepareOutput { core })
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

/// Write a minimal `ecosystem.yaml` synthesized from `contracts.yaml` so
/// that protocol-ops commands (which take `--ecosystem <path>`) can run
/// against the v30.2 fixture. Placed next to `wallets.yaml` so downstream
/// helpers that expect both alongside can find them.
fn write_synthetic_ecosystem_yaml(
    contracts: &Contracts,
    chain_id: u64,
    deployer_addr: &str,
    out_path: &Path,
) -> Result<()> {
    let eco = EcosystemConfig {
        bridgehub: contracts.ecosystem_contracts.bridgehub_proxy_addr.clone(),
        deployer: Some(deployer_addr.to_string()),
        chains: {
            let mut chains = std::collections::BTreeMap::new();
            chains.insert(integration_tests::l1_state::GATEWAY_CHAIN_NAME.to_string(), 0);
            chains.insert("default".to_string(), chain_id);
            chains
        },
    };
    let yaml = serde_yaml::to_string(&eco).context("serialize synthetic ecosystem.yaml")?;
    fs::write(out_path, yaml)
        .with_context(|| format!("write synthetic ecosystem.yaml to {}", out_path.display()))?;
    println!("  Synthetic ecosystem.yaml -> {}", out_path.display());
    Ok(())
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

/// Fund the governor and deployer wallets with ETH so they can pay for
/// upgrade transactions on Anvil.
fn fund_governance_accounts(l1_rpc_url: &str, wallets: &WalletsFile) -> Result<()> {
    for (label, addr) in [
        ("governor", wallets.ecosystem.governor.address.as_str()),
        ("deployer", wallets.ecosystem.deployer.address.as_str()),
    ] {
        fund_account(addr, "10ether", l1_rpc_url, RICH_ACCOUNT_PRIVATE_KEY)
            .with_context(|| format!("fund {label} ({addr})"))?;
    }
    Ok(())
}

/// Transfer ownership of the ecosystem governance timelock to the governor wallet
/// This is needed because the forge scripts broadcast from the governance owner,
/// but we only have the governor wallet's private key
async fn transfer_governance_ownership_to_governor(
    contracts_backend: &EraContractsBackend,
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
    impersonate_account(&current_owner, l1_rpc_url).await?;
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

    stop_impersonating_account(&current_owner, l1_rpc_url).await;

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
async fn transfer_contract_ownership(
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
    impersonate_account(&current_owner, l1_rpc_url).await?;
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

    stop_impersonating_account(&current_owner, l1_rpc_url).await;

    // Accept ownership as ecosystem governance
    impersonate_account(ecosystem_governance, l1_rpc_url).await?;
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

    stop_impersonating_account(ecosystem_governance, l1_rpc_url).await;

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
async fn transfer_new_contracts_ownership(
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
    )
    .await?;

    let native_token_vault = extract_json_value(core, "upgrade_addresses.native_token_vault_addr")?;
    transfer_contract_ownership(
        contracts_backend,
        "NativeTokenVault",
        &native_token_vault,
        ecosystem_governance,
        l1_rpc_url,
    )
    .await?;

    println!("  ✓ All ownership transfers complete");
    Ok(())
}

/// Run ecosystem upgrade stages via direct protocol-ops invocations.
///
/// Phase 1: `ecosystem upgrade-prepare` (deployer) — deploys new contracts.
/// Phase 2: ownership transfers (test-specific impersonation hacks).
/// Phase 3: `ecosystem upgrade-governance` (governor) — runs governance
///          stages 0+1+2 on a single anvil fork, emits one Safe bundle.
///
/// Returns the parsed upgrade-prepare output (for use by downstream code).
async fn run_ecosystem_upgrades(
    contracts_backend: &EraContractsBackend,
    l1_rpc_url: &str,
    contracts: &Contracts,
    wallets: &WalletsFile,
) -> Result<UpgradePrepareOutput> {
    println!("\n=== Running Ecosystem Upgrades (direct protocol-ops) ===");

    fund_governance_accounts(l1_rpc_url, wallets)?;

    let eco_path_str = contracts_backend.work_path("ecosystem.yaml");
    let deployer_key = wallets.ecosystem.deployer.private_key.as_str();
    let governor_key = wallets.ecosystem.governor.private_key.as_str();
    let deployer_addr = wallets.ecosystem.deployer.address.as_str();

    // Per-run UUID suffix in the work dir: `contracts_artifacts/` survives
    // across test invocations, so without a unique suffix the prepare stage
    // would append to an existing manifest and `dev execute-safe` would
    // replay stale bundles.
    let run_tag = uuid::Uuid::new_v4();

    // ── Phase 1: upgrade-prepare (deployer) ───────────────────────────────
    //
    // TODO(v30-removal): the three pre-v31 override flags below are only
    // needed because the v30 CTMs in this fixture don't expose
    // L1_BYTECODES_SUPPLIER(), isZKsyncOS(), or getRollupDAManager().
    // On v31+ ecosystems protocol-ops auto-resolves them from L1.
    let prepare_dir = format!("upgrade_prepare_{run_tag}");
    let governance_toml_rel = format!("{prepare_dir}/governance.toml");
    let manifest_rel = format!("{prepare_dir}/manifest.json");
    let governance_toml_abs = contracts_backend.work_path(&governance_toml_rel);
    let prepare_out_abs = contracts_backend.work_path(&prepare_dir);

    println!("\n  Running ecosystem upgrade-prepare ...");
    contracts_backend
        .protocol_ops(&[
            "ecosystem",
            "upgrade-prepare",
            "--l1-rpc-url",
            l1_rpc_url,
            "--ecosystem",
            &eco_path_str,
            "--deployer-address",
            deployer_addr,
            "--bytecodes-supplier-address",
            contracts.ecosystem_contracts.l1_bytecodes_supplier_addr.as_str(),
            "--rollup-da-manager-address",
            contracts.ecosystem_contracts.l1_rollup_da_manager.as_str(),
            "--is-zk-sync-os",
            "true",
            "--governance-toml-out",
            &governance_toml_abs,
            "--out",
            &prepare_out_abs,
        ])
        .context("ecosystem upgrade-prepare failed")?;

    println!("  Applying prepare Safe bundles (deployer) ...");
    contracts_backend
        .parse_safe_bundles(&prepare_dir, l1_rpc_url)?
        .apply(&[deployer_key])
        .context("apply ecosystem-upgrade-prepare Safe bundles")?;

    // Read deployed-address metadata from the manifest's `metadata[0].output`
    // (consumed by the ownership-transfer hacks below).
    let manifest_host = contracts_backend.work_dir().join(&manifest_rel);
    let script_output = parse_upgrade_prepare_manifest(&manifest_host)
        .context("Failed to read upgrade-prepare metadata from manifest.json")?;

    std::thread::sleep(Duration::from_secs(1));

    // ── Phase 2: ownership transfers (test-specific hacks) ───────────────
    transfer_new_contracts_ownership(
        contracts_backend,
        l1_rpc_url,
        contracts,
        &script_output.core,
    )
    .await?;
    transfer_governance_ownership_to_governor(contracts_backend, l1_rpc_url, contracts, wallets)
        .await?;

    let governance_toml_host = contracts_backend.work_dir().join(&governance_toml_rel);
    anyhow::ensure!(
        governance_toml_host.exists(),
        "governance.toml not found at {} — did prepare stage write --governance-toml-out?",
        governance_toml_host.display(),
    );

    // ── Phase 3: governance stages 0+1 on one fork (governor) ────────────
    //
    // Direct `ecosystem upgrade-governance` — one protocol-ops invocation
    // runs stages 0+1+2 against a single anvil fork, emitting one Safe
    // bundle containing all three governance calls.
    let governance_dir = format!("upgrade_governance_{run_tag}");
    let governance_out_abs = contracts_backend.work_path(&governance_dir);

    println!("\n  Running ecosystem upgrade-governance (stages 0+1+2) ...");
    contracts_backend
        .protocol_ops(&[
            "ecosystem",
            "upgrade-governance",
            "--l1-rpc-url",
            l1_rpc_url,
            "--ecosystem",
            &eco_path_str,
            "--governance-toml",
            &governance_toml_abs,
            "--out",
            &governance_out_abs,
        ])
        .context("ecosystem upgrade-governance failed")?;

    println!("  Applying governance Safe bundles (governor) ...");
    contracts_backend
        .parse_safe_bundles(&governance_dir, l1_rpc_url)?
        .apply(&[governor_key])
        .context("apply ecosystem-upgrade-governance Safe bundles")?;

    println!("  Ecosystem upgrade stages 0-1-2 completed");

    Ok(script_output)
}

/// Verify that the chain's protocol version matches the expected v31 version
fn verify_protocol_version(
    contracts_backend: &EraContractsBackend,
    l1_rpc_url: &str,
    contracts: &Contracts,
) -> Result<()> {
    println!("\n  Verifying protocol version...");

    let diamond_proxy = &contracts.l1.diamond_proxy_addr;
    println!("    Diamond proxy: {}", diamond_proxy);

    // Get current protocol version from chain
    let version_str = contracts_backend
        .cast(&[
            "call",
            diamond_proxy,
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

/// Return the one `.safe.json` file protocol-ops emitted into `dir`. Bails if
/// the count isn't exactly one — that would mean the protocol-ops command grew
/// to emit multiple bundles and the caller needs updating to iterate them.
fn single_safe_bundle(dir: &Path) -> Result<PathBuf> {
    let mut matches: Vec<PathBuf> = fs::read_dir(dir)
        .with_context(|| format!("read_dir {}", dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|s| s.to_str())
                .map(|s| s.ends_with(".safe.json"))
                .unwrap_or(false)
        })
        .collect();
    matches.sort();
    anyhow::ensure!(
        matches.len() == 1,
        "expected exactly one .safe.json in {}, found {}: {:?}",
        dir.display(),
        matches.len(),
        matches
    );
    Ok(matches.into_iter().next().unwrap())
}

/// Reset `dir` so a fresh prepare run can't stumble into stale `.safe.json`
/// files from earlier invocations (which would make `single_safe_bundle`
/// fail). Deletes the directory entirely if it exists and recreates it empty.
fn reset_safe_bundle_dir(dir: &Path) -> Result<()> {
    if dir.exists() {
        fs::remove_dir_all(dir)
            .with_context(|| format!("remove_dir_all {}", dir.display()))?;
    }
    fs::create_dir_all(dir).with_context(|| format!("mkdir -p {}", dir.display()))?;
    Ok(())
}

/// Run chain upgrade via the prepare + execute-safe pattern.
///
/// 1. `chain upgrade --simulate --out <dir>` runs
///    `AdminFunctions.s.sol::upgradeChainFromCTM(...)` against a forked anvil,
///    emitting a Safe bundle with the intended `ChainAdmin.multicall` tx.
/// 2. `dev execute-safe --safe-file <bundle>` replays the bundle against the
///    live anvil under the chain admin / access-control default-admin key.
fn run_chain_upgrade(
    contracts_backend: &EraContractsBackend,
    l1_rpc_url: &str,
    contracts: &Contracts,
    wallets: &WalletsFile,
    chain_name: &str,
) -> Result<()> {
    let _ = contracts; // kept for signature parity with other upgrade helpers
    println!("\n  Preparing chain upgrade Safe bundle (protocol_ops --simulate)...");
    let governor_key = wallets.ecosystem.governor.private_key.as_str();
    let access_control_restriction = contracts.l1.access_control_restriction_addr.as_str();
    let eco_path_str = contracts_backend.work_path("ecosystem.yaml");

    let out_rel = "chain_upgrade";
    let out_host = contracts_backend.work_dir().join(out_rel);
    reset_safe_bundle_dir(&out_host)?;
    let out_arg = contracts_backend.work_path(out_rel);

    contracts_backend
        .protocol_ops(&[
            "chain",
            "upgrade",
            "--l1-rpc-url",
            l1_rpc_url,
            "--out",
            &out_arg,
            "--ecosystem",
            &eco_path_str,
            "--chain",
            chain_name,
            "--access-control-restriction",
            access_control_restriction,
        ])
        .context("chain upgrade --simulate failed")?;

    let bundle_host = single_safe_bundle(&out_host)?;
    let bundle_name = bundle_host
        .file_name()
        .and_then(|n| n.to_str())
        .context("safe bundle filename not valid UTF-8")?;
    let bundle_arg = contracts_backend.work_path(&format!("{out_rel}/{bundle_name}"));
    println!("  chain upgrade Safe bundle: {}", bundle_host.display());

    println!("  Applying chain upgrade bundle via `dev execute-safe`...");
    contracts_backend
        .protocol_ops(&[
            "dev",
            "execute-safe",
            "--safe-file",
            &bundle_arg,
            "--l1-rpc-url",
            l1_rpc_url,
            "--private-key",
            governor_key,
        ])
        .context("dev execute-safe for chain upgrade failed")?;

    Ok(())
}

/// Wait for the zksync-os-server to process the scheduled L2 upgrade tx by
/// shelling out to the `upgrade-readiness-checker` tool under
/// `era-contracts/tools/upgrade-readiness-checker`. The tool polls the chain's
/// L2 RPC for the canonical upgrade-tx receipt; returning 0 means the server
/// has queued + executed the upgrade, so its next batches will be at the new
/// protocol version. Having that anchor lets us call `run_chain_upgrade`
/// (which bumps L1 `protocolVersion()` to v31) without racing the batcher
/// into the `contract > batch` panic in `upgrade_gatekeeper`.
fn wait_for_server_to_process_upgrade(
    contracts_backend: &EraContractsBackend,
    l1_rpc_url: &str,
    l2_rpc_url: &str,
    contracts: &Contracts,
    chain_id: u64,
    target_minor: u32,
    target_patch: u32,
) -> Result<()> {
    println!("\n  Waiting for server to process L2 upgrade tx (upgrade-readiness-checker)...");

    let era_path = contracts_backend
        .era_path()
        .ok_or_else(|| anyhow::anyhow!("upgrade-readiness-checker requires a local era-contracts checkout"))?;
    let tool_dir = era_path.join("tools").join("upgrade-readiness-checker");
    let manifest = tool_dir.join("Cargo.toml");
    anyhow::ensure!(
        manifest.exists(),
        "upgrade-readiness-checker not found at {}",
        manifest.display()
    );

    let chain_id_str = chain_id.to_string();
    let minor_str = target_minor.to_string();
    let patch_str = target_patch.to_string();

    let mut child = std::process::Command::new("cargo")
        .args([
            "run",
            "--release",
            "--manifest-path",
            manifest.to_str().unwrap(),
            "--",
            "--l2-rpc-url",
            l2_rpc_url,
            "--chain-id",
            &chain_id_str,
            "--settlement-rpc-url",
            l1_rpc_url,
            "--bridgehub-address",
            &contracts.ecosystem_contracts.bridgehub_proxy_addr,
            "--target-minor-version",
            &minor_str,
            "--target-patch-version",
            &patch_str,
            "--zksync-os",
        ])
        .spawn()
        .context("failed to spawn upgrade-readiness-checker")?;

    // The tool retries forever on RPC errors. Cap the wall-clock here so a
    // dead L2 server (connection refused) doesn't hang the test.
    let timeout = Duration::from_secs(30);
    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        match child.try_wait().context("wait on upgrade-readiness-checker")? {
            Some(status) => break status,
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!(
                    "upgrade-readiness-checker did not finish within {:?}",
                    timeout
                );
            }
            None => std::thread::sleep(Duration::from_millis(250)),
        }
    };

    anyhow::ensure!(
        status.success(),
        "upgrade-readiness-checker exited with status {status}"
    );
    println!("  ✓ Server has produced a receipt for the L2 upgrade tx");
    Ok(())
}

/// Run v31 ecosystem upgrade Stage 3: register bridged tokens in the NTV and
/// migrate per-chain token balances from NTV into the L1AssetTracker. This is
/// post-governance bookkeeping and can be broadcast by any funded key.
fn run_stage3_token_migration(
    contracts_backend: &EraContractsBackend,
    l1_rpc_url: &str,
    contracts: &Contracts,
    wallets: &WalletsFile,
) -> Result<()> {
    println!("\n  Running ecosystem upgrade stage3 (token migration)...");
    let deployer_key = wallets.ecosystem.deployer.private_key.as_str();
    let bridgehub = contracts.ecosystem_contracts.bridgehub_proxy_addr.as_str();

    // Stage3 reads `l1-contracts/script-config/v31-bridged-tokens.toml` listing
    // legacy bridged tokens to register in the NTV. A fresh test chain has none,
    // but the file must exist or `vm.readFile` reverts. Create an empty one.
    if let Some(era_path) = contracts_backend.era_path() {
        let bridged_tokens_toml = era_path
            .join("l1-contracts/script-config/v31-bridged-tokens.toml");
        if !bridged_tokens_toml.exists() {
            fs::write(&bridged_tokens_toml, "[tokens]\nbridged_tokens = []\n")
                .with_context(|| {
                    format!(
                        "Failed to write placeholder bridged-tokens toml at {}",
                        bridged_tokens_toml.display()
                    )
                })?;
        }
    }

    contracts_backend
        .forge_script(
            &[
                "deploy-scripts/upgrade/v31/EcosystemUpgrade_v31.s.sol:EcosystemUpgrade_v31",
                "--sig",
                "stage3(address)",
                bridgehub,
                "--rpc-url",
                l1_rpc_url,
                "--broadcast",
                "--private-key",
                deployer_key,
                "--legacy",
                "--slow",
            ],
            &[],
        )
        .context("stage3 token migration failed")?;
    Ok(())
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
    // Instant upgrade: use the current block timestamp directly so the
    // server's `L1UpgradeTxWatcher` stops blocking immediately. The test
    // then waits via the `upgrade-readiness-checker` tool for the server to
    // actually process the L2 upgrade tx before we apply the L1 diamond cut.
    let upgrade_timestamp = current_timestamp;

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
    integration_tests::server::get_or_create_run_id("upgrade_v30_to_v31");

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
    let server = ServerBuilder::new(preset.clone(), "default")
        .config_path(&config_path)
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
    server
        .fund_account_via_l1_deposit(&test_address, 1.0, L1DepositBaseToken::Eth)
        .await
        .context("Failed to fund DEFAULT_ANVIL_PRIVATE_KEY on L2 via L1 bridge")?;
    println!("✓ DEFAULT_ANVIL_PRIVATE_KEY funded on L2 via L1 bridge");

    server
        .wait_for_traffic_tx_executed_on_l1()
        .with_context(|| {
            format!(
                "Pre-upgrade traffic/batch wait failed. Server logs: {}",
                server.logs_path().display()
            )
        })?;

    // Synthesize a minimal ecosystem.yaml from contracts.yaml for
    // protocol-ops (which takes --ecosystem <path>). Write it into the
    // backend's work_dir so it's visible inside the Docker container
    // (only work_dir is mounted).
    let eco_yaml_path = contracts_backend.work_dir().join("ecosystem.yaml");
    write_synthetic_ecosystem_yaml(
        &contracts,
        6565,
        &wallets.ecosystem.deployer.address,
        &eco_yaml_path,
    )?;

    // Run ecosystem upgrade stages via direct protocol-ops.
    let _script_output =
        run_ecosystem_upgrades(&contracts_backend, l1_rpc_url, &contracts, &wallets).await?;

    // Notify server about the upcoming upgrade. The `wait_for_server_to_process_upgrade`
    // call below (via upgrade-readiness-checker) enforces the stronger invariant we
    // actually need before `run_chain_upgrade` bumps L1's `protocolVersion()`: the L2
    // upgrade tx has a receipt and every pre-upgrade batch is finalized on the
    // settlement layer.
    schedule_upgrade_timestamp(&contracts_backend, l1_rpc_url, &contracts, &wallets)?;

    // The server's `L1UpgradeTxWatcher` waits until the scheduled timestamp
    // (instant, set above) before it queues the L2 upgrade tx into the
    // mempool. Block here until the server has a receipt for that tx —
    // otherwise `run_chain_upgrade` below would bump L1's `protocolVersion()`
    // while the sequencer is still producing v30 batches, tripping the
    // `upgrade_gatekeeper`'s `contract > batch` panic.
    let packed = read_u64_from_cast_call(
        &contracts_backend,
        l1_rpc_url,
        &contracts.ecosystem_contracts.state_transition_proxy_addr,
        "protocolVersion()(uint256)",
    )?;
    let target_minor = ((packed >> 32) & 0xFFFF_FFFF) as u32;
    let target_patch = (packed & 0xFFFF_FFFF) as u32;
    // URLs handed to the readiness-checker must match its network context.
    // Dispatch on `preset.era_contracts`'s shape (mirrors the existing
    // `Anvil::rpc_url_for` / `Server::rpc_url_for` convention): DockerTag
    // implies a dockerised test setup where native child processes reach
    // host-published ports via `host.docker.internal`.
    let l1_url_for_checker = anvil.rpc_url_for(&preset.era_contracts);
    let l2_url_for_checker = server.rpc_url_for(&preset.era_contracts);
    wait_for_server_to_process_upgrade(
        &contracts_backend,
        &l1_url_for_checker,
        &l2_url_for_checker,
        &contracts,
        6565,
        target_minor,
        target_patch,
    )?;

    println!("Keeping zksync-os-server running during chain upgrade...");

    // Run chain upgrade
    run_chain_upgrade(&contracts_backend, l1_rpc_url, &contracts, &wallets, "default")?;

    // Verify the protocol version was upgraded to v31
    verify_protocol_version(&contracts_backend, l1_rpc_url, &contracts)?;

    // Stage 3: NTV → AssetTracker token migration. Broadcasts via deployer key
    // (any funded account works).
    run_stage3_token_migration(&contracts_backend, l1_rpc_url, &contracts, &wallets)?;

    // Wait for new batches produced under v31.
    server
        .wait_for_traffic_tx_executed_on_l1()
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
