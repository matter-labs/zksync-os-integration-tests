//! The v31 -> v33 upgrade, as one call.
//!
//! Both the upgrade test and the post-upgrade atomic-swap test need the same runbook; it lives
//! here so neither owns it. The individual steps are in [`super::protocol`].

use std::time::Duration;

use anyhow::{Context, Result};
use protocol_ops::commands::ecosystem::upgrade::{
    run_upgrade_governance, run_upgrade_prepare_all, UpgradeGovernanceArgs, UpgradePrepareAllArgs,
};
use protocol_ops::common::forge::scripts::{
    CORE_UPGRADE_V33_SCRIPT_PATH, CTM_UPGRADE_V33_SCRIPT_PATH, UPGRADE_V33_CORE_OUTPUT_PATH,
};

use super::fixture::{self, CHAIN_ADMIN_OWNER_KEY, DEPLOYER_ADDR, DEPLOYER_KEY, GOVERNOR_KEY};
use super::protocol;
use crate::ecosystem::Ecosystem;

/// The upgrade waits on the chain executing its pre-upgrade priority ops; on a cold fixture that
/// is a full commit/prove/execute round per batch.
const PRIORITY_QUEUE_DRAIN_TIMEOUT: Duration = Duration::from_secs(300);

/// Drive the whole v31 -> v33 upgrade on `eco`'s chain: ecosystem prepare + governance, the
/// priority-op bound this release requires, the scheduled upgrade timestamp the server reacts to,
/// and the chain's diamond cut. Returns the L2 block the upgrade transaction was sealed in, so
/// callers can wait for it to finalize.
pub async fn run_upgrade(eco: &Ecosystem) -> Result<u64> {
    let chain = eco.chain();
    let l1_rpc = chain.l1_rpc_url();
    let bridgehub = chain.bridgehub_addr();
    let chain_id = chain.chain_id();

    let ctm = protocol_ops::common::l1_contracts::resolve_ctm_proxy(l1_rpc, bridgehub, chain_id)
        .await
        .context("resolve CTM")?;

    // ── ecosystem upgrade-prepare (deployer) ─────────────────────────────────
    //
    // Driven through the v33 scripts (`deploy-scripts/upgrade/v33/`), which extend the
    // Default* bases directly: the v31 flow's stage-2 legacy-Gateway decommission and
    // stage-3 token migration are one-time v30->v31 work and must not be replayed here.
    // The v31 CTM exposes the getters protocol-ops resolves the bytecodes supplier and
    // rollup DA manager from, so no pre-v31 override addresses are needed.
    let prepare_dir = eco.workdir().join("upgrade_prepare");
    let governance_toml = eco.workdir().join("ecosystem.toml");
    run_upgrade_prepare_all(UpgradePrepareAllArgs {
        shared: protocol::shared_args(l1_rpc, &prepare_dir),
        topology: protocol::ecosystem_args(bridgehub),
        deployer_address: Some(DEPLOYER_ADDR.parse().unwrap()),
        ctm_proxies: vec![ctm],
        ctm_config: None,
        bytecodes_supplier_address: None,
        rollup_da_manager_address: None,
        is_zk_sync_os: Some(true),
        create2_factory_salt: None,
        upgrade_input_path: fixture::UPGRADE_INPUT_PATH.to_string(),
        core_output_path: UPGRADE_V33_CORE_OUTPUT_PATH.to_string(),
        core_script_path: CORE_UPGRADE_V33_SCRIPT_PATH.to_string(),
        ctm_script_path: CTM_UPGRADE_V33_SCRIPT_PATH.to_string(),
    })
    .await
    .context("upgrade-prepare")?;
    protocol::apply(&prepare_dir, &[DEPLOYER_KEY, GOVERNOR_KEY], l1_rpc)
        .await
        .context("apply upgrade-prepare")?;

    // ── ecosystem upgrade-governance (stages 0+1+2, governor) ────────────────
    let gov_dir = eco.workdir().join("upgrade_governance");
    run_upgrade_governance(UpgradeGovernanceArgs {
        shared: protocol::shared_args(l1_rpc, &gov_dir),
        topology: protocol::ecosystem_args(bridgehub),
        governance_toml: vec![governance_toml],
    })
    .await
    .context("upgrade-governance")?;
    protocol::apply(&gov_dir, &[GOVERNOR_KEY], l1_rpc)
        .await
        .context("apply upgrade-governance")?;

    // ── Prerequisite: pin the priority-op bound, then let it drain ───────────
    //
    // Recorded before the upgrade is scheduled so it lands in its own
    // transaction, as V32UpgradeZKsyncOS requires.
    protocol::record_priority_op_lower_bound(
        l1_rpc,
        eco.workdir(),
        bridgehub,
        chain_id,
        ctm,
        DEPLOYER_KEY,
    )
    .await
    .context("record priority-op lower bound")?;
    protocol::wait_for_priority_ops_processed(
        l1_rpc,
        bridgehub,
        chain_id,
        ctm,
        PRIORITY_QUEUE_DRAIN_TIMEOUT,
    )
    .await
    .context("wait for priority ops below the bound to be processed")?;

    // ── Schedule the upgrade; server produces the L2 upgrade block ───────────
    //
    // Capture the latest L2 block BEFORE scheduling: the server's upgrade-tx
    // watcher polls every 100 ms, so the upgrade block is often sealed before
    // schedule_upgrade_timestamp() even returns. Capturing afterwards would
    // make us wait for a block that no traffic will ever produce.
    let pre_upgrade_latest = chain.latest_block().await.unwrap_or(0);
    protocol::schedule_upgrade_timestamp(
        l1_rpc,
        eco.workdir(),
        &[GOVERNOR_KEY, CHAIN_ADMIN_OWNER_KEY],
        bridgehub,
        chain_id,
    )
    .await
    .context("schedule upgrade timestamp")?;

    // The upgrade_gatekeeper blocks the v33 batch until the L1 chain upgrade
    // below; meanwhile the server seals the L2 upgrade block.
    let upgrade_block = pre_upgrade_latest + 1;
    chain
        .wait_for_block_produced(upgrade_block)
        .await
        .context("wait for upgrade block")?;

    // Before bumping L1's protocolVersion, make sure every pre-upgrade (v31)
    // batch is finalized on L1 — otherwise the upgrade_gatekeeper sees
    // contract(v33) > batch(v31) and hard-fails.
    chain
        .wait_for_block_finalized(pre_upgrade_latest)
        .await
        .context("wait for pre-upgrade batches to finalize")?;

    // ── L1 chain upgrade ─────────────────────────────────────────────────────
    protocol::run_chain_upgrade(
        l1_rpc,
        eco.workdir(),
        &[GOVERNOR_KEY, CHAIN_ADMIN_OWNER_KEY],
        bridgehub,
        chain_id,
    )
    .await
    .context("chain upgrade")?;

    Ok(upgrade_block)
}
