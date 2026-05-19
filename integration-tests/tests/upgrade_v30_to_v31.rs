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
use std::fs;
use std::path::{Path, PathBuf};

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

#[derive(serde::Deserialize)]
struct UpgradePrepareToml {
    core: UpgradePrepareCore,
}

#[derive(serde::Deserialize)]
struct UpgradePrepareCore {
    upgrade_addresses: CoreUpgradeAddresses,
}

#[derive(serde::Deserialize)]
struct CoreUpgradeAddresses {
    native_token_vault_addr: String,
    bridgehub: BridgehubUpgradeAddresses,
}

#[derive(serde::Deserialize)]
struct BridgehubUpgradeAddresses {
    chain_asset_handler_proxy_addr: String,
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

async fn transfer_ownable_to_governance(
    contracts_backend: &EraContractsBackend,
    label: &str,
    contract_address: &str,
    ecosystem_governance: &str,
    l1_rpc_url: &str,
) -> Result<()> {
    println!("    {} ({})", label, contract_address);

    let current_owner = contracts_backend
        .cast(&[
            "call",
            contract_address,
            "owner()(address)",
            "--rpc-url",
            l1_rpc_url,
        ])
        .with_context(|| format!("read {label} owner"))?;
    let current_owner = current_owner.trim().to_string();
    println!("      Current owner: {}", current_owner);

    if current_owner.eq_ignore_ascii_case(ecosystem_governance) {
        println!("      Already owned by ecosystem governance, skipping");
        return Ok(());
    }

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
        .with_context(|| format!("transfer {label} ownership"))?;

    stop_impersonating_account(&current_owner, l1_rpc_url).await;

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
        .with_context(|| format!("accept {label} ownership"))?;

    stop_impersonating_account(ecosystem_governance, l1_rpc_url).await;

    println!("      Ownership transferred to ecosystem governance");
    Ok(())
}

async fn transfer_prepared_core_contracts_to_governance(
    contracts_backend: &EraContractsBackend,
    l1_rpc_url: &str,
    contracts: &Contracts,
    ecosystem_toml_path: &Path,
) -> Result<()> {
    println!("\n  Transferring prepared core contracts to ecosystem governance...");

    let ecosystem_toml = fs::read_to_string(ecosystem_toml_path)
        .with_context(|| format!("read {}", ecosystem_toml_path.display()))?;
    let prepared: UpgradePrepareToml =
        toml::from_str(&ecosystem_toml).context("parse upgrade-prepare ecosystem.toml")?;
    let ecosystem_governance = contracts.ecosystem_contracts.governance.as_str();

    // TODO(protocol-ops): v30 fixtures need these anvil-only handoffs because
    // the freshly prepared contracts are deployer-owned while governance
    // stage0/stage1 calls execute from the Governance contract.
    transfer_ownable_to_governance(
        contracts_backend,
        "ChainAssetHandler",
        &prepared
            .core
            .upgrade_addresses
            .bridgehub
            .chain_asset_handler_proxy_addr,
        ecosystem_governance,
        l1_rpc_url,
    )
    .await?;
    transfer_ownable_to_governance(
        contracts_backend,
        "NativeTokenVault",
        &prepared.core.upgrade_addresses.native_token_vault_addr,
        ecosystem_governance,
        l1_rpc_url,
    )
    .await?;

    Ok(())
}

/// Run ecosystem upgrade stages via direct protocol-ops invocations.
///
/// Phase 1: `ecosystem upgrade-prepare-all` (deployer) deploys core + CTM
/// contracts and emits the merged ecosystem TOML consumed by governance.
/// Phase 2: anvil-only fixture ownership handoffs needed before governance.
/// Phase 3: transfer legacy Governance.owner() to the persisted governor key.
/// Phase 4: `ecosystem upgrade-governance` (governor) — runs governance
///          stages 0+1+2 on a single anvil fork, emits one Safe bundle.
async fn run_ecosystem_upgrades(
    contracts_backend: &EraContractsBackend,
    l1_rpc_url: &str,
    contracts: &Contracts,
    wallets: &WalletsFile,
) -> Result<()> {
    println!("\n=== Running Ecosystem Upgrades (direct protocol-ops) ===");

    fund_governance_accounts(l1_rpc_url, wallets)?;

    let deployer_key = wallets.ecosystem.deployer.private_key.as_str();
    let governor_key = wallets.ecosystem.governor.private_key.as_str();
    let deployer_addr = wallets.ecosystem.deployer.address.as_str();
    let bridgehub = contracts.ecosystem_contracts.bridgehub_proxy_addr.as_str();
    let ctm_proxy = contracts
        .ecosystem_contracts
        .state_transition_proxy_addr
        .as_str();

    // Per-run UUID suffix in the work dir: `contracts_artifacts/` survives
    // across test invocations, so without a unique suffix protocol-ops would
    // append to an existing manifest and `dev execute-safe` would replay
    // stale bundles.
    let run_tag = uuid::Uuid::new_v4();

    // ── Phase 1: upgrade-prepare-all (deployer) ───────────────────────────
    //
    // TODO(v30-removal): the three pre-v31 override flags below are only
    // needed because the v30 CTMs in this fixture don't expose
    // L1_BYTECODES_SUPPLIER(), isZKsyncOS(), or getRollupDAManager().
    // On v31+ ecosystems protocol-ops auto-resolves them from L1.
    let prepare_dir = format!("upgrade_prepare_{run_tag}");
    let governance_toml_rel = format!("{prepare_dir}/ecosystem.toml");
    let governance_toml_abs = contracts_backend.work_path(&governance_toml_rel);
    let prepare_out_abs = contracts_backend.work_path(&prepare_dir);

    println!("\n  Running ecosystem upgrade-prepare-all ...");
    contracts_backend
        .protocol_ops(&[
            "ecosystem",
            "upgrade-prepare-all",
            "--l1-rpc-url",
            l1_rpc_url,
            "--bridgehub",
            bridgehub,
            "--deployer-address",
            deployer_addr,
            "--ctm-proxy",
            ctm_proxy,
            "--bytecodes-supplier-address",
            contracts
                .ecosystem_contracts
                .l1_bytecodes_supplier_addr
                .as_str(),
            "--rollup-da-manager-address",
            contracts.ecosystem_contracts.l1_rollup_da_manager.as_str(),
            "--is-zk-sync-os",
            "true",
            "--out",
            &prepare_out_abs,
        ])
        .context("ecosystem upgrade-prepare-all failed")?;

    println!("  Applying prepare Safe bundles (deployer) ...");
    contracts_backend
        .parse_safe_bundles(&prepare_dir, l1_rpc_url)?
        .apply(&[deployer_key, governor_key])
        .context("apply ecosystem-upgrade-prepare-all Safe bundles")?;

    let governance_toml_host = contracts_backend.work_dir().join(&governance_toml_rel);
    anyhow::ensure!(
        governance_toml_host.exists(),
        "ecosystem.toml not found at {}",
        governance_toml_host.display(),
    );

    // ── Phase 2: prepared-contract ownership handoffs (test fixture hack) ─
    transfer_prepared_core_contracts_to_governance(
        contracts_backend,
        l1_rpc_url,
        contracts,
        &governance_toml_host,
    )
    .await?;

    // ── Phase 3: legacy governance ownership transfer (test fixture hack) ─
    //
    // The v30.2 fixture's original Governance.owner() private key was not
    // persisted. Move ownership to the governor key before the governance
    // replay so `dev execute-safe` can apply the generated bundle honestly.
    transfer_governance_ownership_to_governor(contracts_backend, l1_rpc_url, contracts, wallets)
        .await?;

    // ── Phase 4: governance stages 0+1+2 on one fork (governor) ──────────
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
            "--bridgehub",
            bridgehub,
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

    Ok(())
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

/// Reset `dir` so a fresh prepare run can't stumble into stale `.safe.json`
/// files from earlier invocations. Deletes the directory entirely if it exists
/// and recreates it empty.
fn reset_safe_bundle_dir(dir: &Path) -> Result<()> {
    if dir.exists() {
        fs::remove_dir_all(dir).with_context(|| format!("remove_dir_all {}", dir.display()))?;
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
    chain_id: u64,
) -> Result<()> {
    println!("\n  Preparing chain upgrade Safe bundle (protocol_ops --simulate)...");
    let governor_key = wallets.ecosystem.governor.private_key.as_str();
    let access_control_restriction = contracts.l1.access_control_restriction_addr.as_str();
    let bridgehub = contracts.ecosystem_contracts.bridgehub_proxy_addr.as_str();
    let chain_id = chain_id.to_string();

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
            "--bridgehub",
            bridgehub,
            "--chain-id",
            &chain_id,
            "--access-control-restriction",
            access_control_restriction,
        ])
        .context("chain upgrade --simulate failed")?;

    println!("  Applying chain upgrade Safe bundle(s)...");
    contracts_backend
        .parse_safe_bundles(out_rel, l1_rpc_url)?
        .apply(&[governor_key])
        .context("apply chain upgrade Safe bundles")?;

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

    let local_binary = contracts_backend.tool_binary("upgrade-readiness-checker")?;
    let cmd_name = local_binary
        .as_ref()
        .map(|b| b.to_string_lossy().to_string())
        .unwrap_or_else(|| "upgrade-readiness-checker".to_string());

    let chain_id_str = chain_id.to_string();
    let minor_str = target_minor.to_string();
    let patch_str = target_patch.to_string();

    contracts_backend
        .run(
            &[
                &cmd_name,
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
            ],
            None,
        )
        .context("upgrade-readiness-checker failed")?;

    println!("  ✓ Server has produced a receipt for the L2 upgrade tx");
    Ok(())
}

/// Run v31 ecosystem stage3: register bridged tokens in the NTV and migrate
/// per-chain token balances from NTV into the L1AssetTracker. Newer
/// protocol-ops prepares this as a Safe bundle and it must run before the
/// per-chain upgrade.
fn run_stage3_token_migration(
    contracts_backend: &EraContractsBackend,
    l1_rpc_url: &str,
    contracts: &Contracts,
    wallets: &WalletsFile,
) -> Result<()> {
    println!("\n  Running ecosystem upgrade stage3 (token migration)...");
    let deployer_key = wallets.ecosystem.deployer.private_key.as_str();
    let deployer_addr = wallets.ecosystem.deployer.address.as_str();
    let bridgehub = contracts.ecosystem_contracts.bridgehub_proxy_addr.as_str();

    // Stage3 reads `l1-contracts/script-config/v31-bridged-tokens.toml` listing
    // legacy bridged tokens to register in the NTV. A fresh test chain has none,
    // but the file must exist or `vm.readFile` reverts. Write an empty one.
    // `write_repo_file` works in both Local (host fs) and Docker (writes via
    // the mounted work dir + `mv` inside the container) modes.
    contracts_backend
        .write_repo_file(
            "l1-contracts/script-config/v31-bridged-tokens.toml",
            "[tokens]\nbridged_tokens = []\n",
        )
        .context("write placeholder v31-bridged-tokens.toml")?;

    let out_rel = "stage3";
    let out_host = contracts_backend.work_dir().join(out_rel);
    reset_safe_bundle_dir(&out_host)?;
    let out_arg = contracts_backend.work_path(out_rel);

    contracts_backend
        .protocol_ops(&[
            "ecosystem",
            "stage3",
            "--l1-rpc-url",
            l1_rpc_url,
            "--bridgehub",
            bridgehub,
            "--sender",
            deployer_addr,
            "--out",
            &out_arg,
        ])
        .context("ecosystem stage3 failed")?;

    println!("  Applying stage3 Safe bundle(s)...");
    contracts_backend
        .parse_safe_bundles(out_rel, l1_rpc_url)?
        .apply(&[deployer_key])
        .context("apply ecosystem stage3 Safe bundles")?;
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
    chain_id: u64,
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
    let current_chain_protocol_version = read_u64_from_cast_call(
        contracts_backend,
        l1_rpc_url,
        &contracts.l1.diamond_proxy_addr,
        "getProtocolVersion()(uint256)",
    )
    .context("Failed to read current protocol version from chain diamond")?;
    let server_notifier = contracts_backend
        .cast(&[
            "call",
            &contracts.ecosystem_contracts.state_transition_proxy_addr,
            "serverNotifierAddress()(address)",
            "--rpc-url",
            l1_rpc_url,
        ])
        .context("Failed to read ServerNotifier address from CTM")?;
    let server_notifier = server_notifier.trim().to_string();

    println!(
        "Scheduling upgrade timestamp {} for current protocol version {} (target {})",
        upgrade_timestamp, current_chain_protocol_version, target_protocol_version
    );

    let new_protocol_version = target_protocol_version.to_string();
    let upgrade_timestamp_str = upgrade_timestamp.to_string();
    let bridgehub = contracts.ecosystem_contracts.bridgehub_proxy_addr.as_str();
    let access_control_restriction = contracts.l1.access_control_restriction_addr.as_str();
    let chain_id_str = chain_id.to_string();

    let out_rel = "set_upgrade_timestamp";
    let out_host = contracts_backend.work_dir().join(out_rel);
    reset_safe_bundle_dir(&out_host)?;
    let out_arg = contracts_backend.work_path(out_rel);

    println!("\n  Preparing ChainAdmin/ServerNotifier upgrade timestamp Safe bundle...");
    contracts_backend
        .protocol_ops(&[
            "chain",
            "set-upgrade-timestamp",
            "--l1-rpc-url",
            l1_rpc_url,
            "--bridgehub",
            bridgehub,
            "--chain-id",
            &chain_id_str,
            "--new-protocol-version",
            &new_protocol_version,
            "--upgrade-timestamp",
            &upgrade_timestamp_str,
            "--access-control-restriction",
            access_control_restriction,
            "--out",
            &out_arg,
        ])
        .context("chain set-upgrade-timestamp failed")?;

    println!("  Applying set-upgrade-timestamp Safe bundle(s)...");
    contracts_backend
        .parse_safe_bundles(out_rel, l1_rpc_url)?
        .apply(&[wallets.ecosystem.governor.private_key.as_str()])
        .context("apply set-upgrade-timestamp Safe bundles")?;

    let scheduled_timestamp = read_u64_from_cast_call_with_args(
        contracts_backend,
        l1_rpc_url,
        &server_notifier,
        "protocolVersionToUpgradeTimestamp(uint256,uint256)(uint256)",
        &[
            &chain_id.to_string(),
            &current_chain_protocol_version.to_string(),
        ],
    )
    .context("Failed to read ServerNotifier protocolVersionToUpgradeTimestamp after scheduling")?;
    anyhow::ensure!(
        scheduled_timestamp == upgrade_timestamp,
        "Upgrade timestamp mismatch after scheduling: expected {}, got {}",
        upgrade_timestamp,
        scheduled_timestamp
    );
    println!(
        "✓ Upgrade timestamp confirmed on-chain: protocolVersionToUpgradeTimestamp({}, {}) = {}",
        chain_id, current_chain_protocol_version, scheduled_timestamp
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

    // Run ecosystem upgrade stages via direct protocol-ops.
    run_ecosystem_upgrades(&contracts_backend, l1_rpc_url, &contracts, &wallets).await?;

    // Notify server about the upcoming upgrade. The `wait_for_server_to_process_upgrade`
    // call below (via upgrade-readiness-checker) enforces the stronger invariant we
    // actually need before `run_chain_upgrade` bumps L1's `protocolVersion()`: the L2
    // upgrade tx has a receipt and every pre-upgrade batch is finalized on the
    // settlement layer.
    schedule_upgrade_timestamp(&contracts_backend, l1_rpc_url, &contracts, &wallets, 6565)?;

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

    // Stage3 registers legacy tokens before the per-chain upgrade so token
    // withdrawals unblock as soon as the chain diamond upgrade lands.
    run_stage3_token_migration(&contracts_backend, l1_rpc_url, &contracts, &wallets)?;

    // Run chain upgrade.
    run_chain_upgrade(&contracts_backend, l1_rpc_url, &contracts, &wallets, 6565)?;

    // Verify the protocol version was upgraded to v31
    verify_protocol_version(&contracts_backend, l1_rpc_url, &contracts)?;

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
