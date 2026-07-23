/// End-to-end v30→v31 protocol upgrade on an L1-settling ZKsync OS chain.
///
/// Starts the v30.2 frozen fixture (anvil state + in-process server), drives
/// the full upgrade through protocol-ops, and verifies the upgraded chain
/// still processes deposits. The fixture is restored from a committed snapshot
/// via [`fixture::start`]; the upgrade steps live in [`protocol`].
use std::time::Duration;

use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::signers::local::PrivateKeySigner;
use anyhow::{Context, Result};
use protocol_ops::commands::ecosystem::upgrade::{
    run_upgrade_governance, run_upgrade_prepare_all, UpgradeGovernanceArgs, UpgradePrepareAllArgs,
};
use protocol_ops::common::forge::scripts::{
    CORE_UPGRADE_V31_SCRIPT_PATH, CTM_UPGRADE_V31_SCRIPT_PATH, UPGRADE_V31_CORE_OUTPUT_PATH,
};

use tests::upgrade_v30_to_v31::fixture::{
    self, DEPLOYER_ADDR, DEPLOYER_KEY, GOVERNOR_KEY, V30_BYTECODES_SUPPLIER, V30_ROLLUP_DA_MANAGER,
};
use tests::upgrade_v30_to_v31::protocol;

#[tokio::test(flavor = "multi_thread")]
async fn test_v30_to_v31_upgrade() -> Result<()> {
    // The upgrade runbook runs forge scripts — compiled contracts are required.
    tests::fixtures::ensure_contracts_built().await;

    let eco = fixture::start().await.context("start v30.2 fixture")?;
    let chain = eco.chain();
    let l1_rpc = chain.l1_rpc_url();
    let bridgehub = chain.bridgehub_addr();
    let chain_id = chain.chain_id();

    // ── Initiate a withdrawal on the still-v30 chain ─────────────────────────
    //
    // PR #2237 keeps withdrawals live across the v31 ecosystem upgrade: once
    // `initializeL1V31Upgrade` stamps the placeholder marker on every chain,
    // `L1AssetTracker._getWithdrawalChain` used to revert until each chain ran
    // its own diamond upgrade. We reproduce that exact window: initiate the
    // withdrawal now (a strictly pre-v31 batch), then finalize it on L1 after
    // the ecosystem upgrade but before this chain's diamond upgrade.
    //
    // The v30.2 fixture has no pre-funded test wallets, so fund an L2 account
    // via an L1→L2 deposit first, then withdraw part of it to a fresh L1
    // address whose balance delta cleanly measures the finalized amount.
    let withdrawer_key = zk_deployer::l1_l2_deposit::DEFAULT_L2_RICH_KEYS[2];
    let withdrawer: Address = withdrawer_key.parse::<PrivateKeySigner>()?.address();
    let withdraw_l1_receiver: Address = "0x000000000000000000000000000000000000bEEF".parse()?;
    let withdraw_amount = U256::from(100_000_000_000_000_000u128); // 0.1 ETH

    zk_deployer::l1_l2_deposit::deposit_eth(
        l1_rpc,
        bridgehub,
        chain_id,
        withdrawer,
        U256::from(1_000_000_000_000_000_000u128), // 1 ETH
        zk_deployer::l1_l2_deposit::DEFAULT_L1_TO_L2_GAS_PRICE,
        DEPLOYER_KEY,
    )
    .await
    .context("pre-upgrade deposit to fund the withdrawer")?;
    zk_deployer::l1_l2_deposit::wait_for_l2_balance(chain.l2_rpc_url(), withdrawer, 120)
        .await
        .context("wait for withdrawer funding on L2")?;

    let withdraw_tx = zk_deployer::l2_l1_withdraw::withdraw_eth(
        chain.l2_rpc_url(),
        withdrawer_key,
        withdraw_l1_receiver,
        withdraw_amount,
    )
    .await
    .context("initiate L2→L1 withdrawal")?;
    // Finalizing needs the withdrawal's batch executed on L1 (that's when the
    // log proof becomes available); waiting here also keeps the batch pre-v31.
    chain
        .wait_for_tx_finalized(withdraw_tx)
        .await
        .context("finalize withdrawal batch on L1")?;

    // ── ecosystem upgrade-prepare (deployer) ─────────────────────────────────
    //
    // The three pre-v31 override addresses are needed because the v30 CTM
    // doesn't expose the getters protocol-ops would auto-resolve them from.
    let prepare_dir = eco.workdir().join("upgrade_prepare");
    let governance_toml = eco.workdir().join("ecosystem.toml");
    let ctm = protocol_ops::common::l1_contracts::resolve_ctm_proxy(l1_rpc, bridgehub, chain_id)
        .await
        .context("resolve CTM")?;
    run_upgrade_prepare_all(UpgradePrepareAllArgs {
        shared: protocol::shared_args(l1_rpc, &prepare_dir),
        topology: protocol::ecosystem_args(bridgehub),
        deployer_address: Some(DEPLOYER_ADDR.parse().unwrap()),
        ctm_proxies: vec![ctm],
        ctm_config: None,
        bytecodes_supplier_address: Some(V30_BYTECODES_SUPPLIER.parse().unwrap()),
        rollup_da_manager_address: Some(V30_ROLLUP_DA_MANAGER.parse().unwrap()),
        is_zk_sync_os: Some(true),
        create2_factory_salt: None,
        upgrade_input_path: fixture::V30_UPGRADE_INPUT_PATH.to_string(),
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

    // ── Schedule the upgrade; server produces the L2 upgrade block ───────────
    //
    // Capture the latest L2 block BEFORE scheduling: the server's upgrade-tx
    // watcher polls every 100 ms, so the upgrade block is often sealed before
    // schedule_upgrade_timestamp() even returns. Capturing afterwards would
    // make us wait for a block that no traffic will ever produce.
    let pre_upgrade_latest = chain.latest_block().await.unwrap_or(0);
    protocol::schedule_upgrade_timestamp(l1_rpc, eco.workdir(), GOVERNOR_KEY, bridgehub, chain_id)
        .await
        .context("schedule upgrade timestamp")?;

    // The upgrade_gatekeeper blocks the v31 batch until the L1 chain upgrade
    // below; meanwhile the server seals the L2 upgrade block.
    let upgrade_block = pre_upgrade_latest + 1;
    chain
        .wait_for_block_produced(upgrade_block)
        .await
        .context("wait for upgrade block")?;

    // Before bumping L1's protocolVersion, make sure every pre-upgrade (v30)
    // batch is finalized on L1 — otherwise the upgrade_gatekeeper sees
    // contract(v31) > batch(v30) and hard-fails.
    chain
        .wait_for_block_finalized(pre_upgrade_latest)
        .await
        .context("wait for pre-upgrade batches to finalize")?;

    // ── Stage3 token migration + L1 chain upgrade ────────────────────────────
    protocol::run_stage3(l1_rpc, eco.workdir(), bridgehub, chain_id, DEPLOYER_KEY)
        .await
        .context("stage3 token migration")?;

    // ── Finalize the withdrawal inside the placeholder window ────────────────
    //
    // The ecosystem upgrade (governance) has stamped the v31 placeholder marker
    // and stage3 has registered ETH in the L1AssetTracker, but this chain's
    // diamond upgrade has not run yet — so the marker is still the placeholder.
    // This is exactly the case PR #2237 fixes: pre-PR, finalizeWithdrawal would
    // revert in `_getWithdrawalChain`; post-PR it attributes the withdrawal to
    // the chain and succeeds.
    protocol::assert_in_v31_placeholder_window(l1_rpc, bridgehub, chain_id)
        .await
        .context("confirm chain holds the v31 placeholder marker before finalize")?;
    let l1_provider = chain.l1_provider().await?;
    let receiver_before = l1_provider.get_balance(withdraw_l1_receiver).await?;
    zk_deployer::l2_l1_withdraw::finalize_withdrawal(
        l1_rpc,
        chain.l2_rpc_url(),
        bridgehub,
        chain_id,
        withdraw_tx,
        DEPLOYER_KEY,
        Duration::from_secs(120),
    )
    .await
    .context("finalize withdrawal during the v31 placeholder window")?;
    let receiver_after = l1_provider.get_balance(withdraw_l1_receiver).await?;
    anyhow::ensure!(
        receiver_after - receiver_before == withdraw_amount,
        "withdrawal did not credit the L1 receiver: before={receiver_before}, after={receiver_after}"
    );

    protocol::run_chain_upgrade(l1_rpc, eco.workdir(), GOVERNOR_KEY, bridgehub, chain_id)
        .await
        .context("chain upgrade")?;
    protocol::assert_protocol_version(l1_rpc, bridgehub, chain_id, 31)
        .await
        .context("protocol version assertion")?;

    // ── ZKOS base-token supply backfill (post-upgrade requirement) ───────────
    protocol::set_zkos_pre_v31_total_supply(l1_rpc, bridgehub, chain_id, GOVERNOR_KEY)
        .await
        .context("set ZKOS pre-v31 total supply")?;

    // The L1 upgrade unblocks the gatekeeper; wait for the v31 upgrade batch
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
        None, // ETH base token
    )
    .await
    .context("post-upgrade L1→L2 deposit")?;
    zk_deployer::l1_l2_deposit::wait_for_l2_balance(chain.l2_rpc_url(), recipient, 120)
        .await
        .context("wait for post-upgrade deposit on L2")?;

    // Wait for the deposit's block to finalize. Snapshot latest_block after the
    // balance appears — the deposit is in a block <= this number. This avoids the
    // wait_for_batch() race where the batch may have already finalized before the
    // snapshot is taken, leaving wait_for_batch() waiting for a block that never comes.
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
