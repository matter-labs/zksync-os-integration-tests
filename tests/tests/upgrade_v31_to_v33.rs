/// End-to-end v31→v33 protocol upgrade on an L1-settling ZKsync OS chain.
///
/// Starts the v31.0 frozen fixture (anvil state + in-process server), drives
/// the full upgrade through protocol-ops, and verifies the upgraded chain
/// still processes deposits. The fixture is restored from a committed snapshot
/// via [`fixture::start`]; the upgrade steps live in [`protocol`].
///
/// The target version comes from the pinned era-contracts revision's genesis
/// config, not from this test — see [`protocol`]. On
/// `release/v0.33.0-atomic-interop` that is v33, which is why the only
/// hard-coded version numbers here are the assertions.
use std::time::Duration;

use alloy::primitives::{Address, U256};
use anyhow::{Context, Result};
use protocol_ops::commands::ecosystem::upgrade::{
    run_upgrade_governance, run_upgrade_prepare_all, UpgradeGovernanceArgs, UpgradePrepareAllArgs,
};
use protocol_ops::common::abi::ZkChainAbi;
use protocol_ops::common::forge::scripts::{
    CORE_UPGRADE_V31_SCRIPT_PATH, CTM_UPGRADE_V31_SCRIPT_PATH, UPGRADE_V31_CORE_OUTPUT_PATH,
};

use tests::eth::{call, provider};
use tests::upgrade_v31_to_v33::fixture::{
    self, CHAIN_ADMIN_OWNER_KEY, DEPLOYER_ADDR, DEPLOYER_KEY, GOVERNOR_KEY,
};
use tests::upgrade_v31_to_v33::protocol;

/// The upgrade waits on the chain executing its pre-upgrade priority ops; on a
/// cold fixture that is a full commit/prove/execute round per batch.
const PRIORITY_QUEUE_DRAIN_TIMEOUT: Duration = Duration::from_secs(300);

/// `PubdataContent.FULL_PUBDATA` — the first variant of the enum the v33
/// `Getters` facet exposes, and the value an upgraded rollup keeps.
const FULL_PUBDATA: u8 = 0;

#[tokio::test(flavor = "multi_thread")]
async fn test_v31_to_v33_upgrade() -> Result<()> {
    // The upgrade runbook runs forge scripts — compiled contracts are required.
    tests::fixtures::ensure_contracts_built().await;

    let eco = fixture::start().await.context("start v31.0 fixture")?;
    let chain = eco.chain();
    let l1_rpc = chain.l1_rpc_url();
    let bridgehub = chain.bridgehub_addr();
    let chain_id = chain.chain_id();

    let ctm = protocol_ops::common::l1_contracts::resolve_ctm_proxy(l1_rpc, bridgehub, chain_id)
        .await
        .context("resolve CTM")?;

    // ── ecosystem upgrade-prepare (deployer) ─────────────────────────────────
    //
    // The v31 CTM exposes the getters protocol-ops resolves the bytecodes
    // supplier and rollup DA manager from, so unlike the v30 fixture no
    // pre-v31 override addresses are needed.
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
        upgrade_input_path: fixture::V31_UPGRADE_INPUT_PATH.to_string(),
        core_output_path: UPGRADE_V31_CORE_OUTPUT_PATH.to_string(),
        core_script_path: CORE_UPGRADE_V31_SCRIPT_PATH.to_string(),
        ctm_script_path: CTM_UPGRADE_V31_SCRIPT_PATH.to_string(),
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
    protocol::record_priority_op_lower_bound(l1_rpc, bridgehub, chain_id, ctm, DEPLOYER_KEY)
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
    protocol::assert_protocol_version(l1_rpc, bridgehub, chain_id, 33)
        .await
        .context("protocol version assertion")?;

    // The v33 diamond answers `getPubdataContent()`, whose value the Executor
    // folds into every batch's chain-config hash and the server discovers from
    // L1. An upgraded rollup must read FULL_PUBDATA — the storage slot the
    // upgrade leaves untouched — or the batches below would not verify.
    let diamond = protocol_ops::common::l1_contracts::resolve_zk_chain(l1_rpc, bridgehub, chain_id)
        .await
        .context("resolve diamond")?;
    let pubdata_content = call(
        &provider(l1_rpc).await?,
        diamond,
        ZkChainAbi::getPubdataContentCall {},
    )
    .await
    .context("getPubdataContent")?;
    anyhow::ensure!(
        pubdata_content == FULL_PUBDATA,
        "expected FULL_PUBDATA after the upgrade, got {pubdata_content}"
    );

    // The L1 upgrade unblocks the gatekeeper; wait for the v33 upgrade batch
    // to commit/prove/execute.
    chain
        .wait_for_block_finalized(upgrade_block)
        .await
        .context("wait for upgrade batch to finalize")?;

    // ── Post-upgrade traffic: L1→L2 deposit must work end-to-end ─────────────
    let recipient: Address = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8".parse()?;
    zk_deployer::l1_l2_deposit::deposit_base_token(
        l1_rpc,
        bridgehub,
        chain_id,
        recipient,
        U256::from(1_000_000_000_000_000_000u128), // 1 ETH
        1_000_000_000,
        DEPLOYER_KEY,
        None, // ETH-based chain
    )
    .await
    .context("post-upgrade L1→L2 deposit")?;
    zk_deployer::l1_l2_deposit::wait_for_l2_balance(chain.l2_rpc_url(), recipient, 120)
        .await
        .context("wait for post-upgrade deposit on L2")?;

    // Wait for the deposit's block to finalize. Snapshot latest_block after the
    // balance appears — the deposit is in a block <= this number.
    let deposit_block = chain
        .latest_block()
        .await
        .context("get latest block after deposit")?;
    chain
        .wait_for_block_finalized(deposit_block)
        .await
        .context("wait for post-upgrade batch to finalize")?;

    Ok(())
}
