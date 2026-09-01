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
use alloy::primitives::{Address, U256};
use anyhow::{Context, Result};
use protocol_ops::common::abi::ZkChainAbi;

use tests::eth::{call, provider};
use tests::upgrade_v31_to_v33::fixture::{self, DEPLOYER_KEY};
use tests::upgrade_v31_to_v33::{protocol, runbook};

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

    let upgrade_block = runbook::run_upgrade(&eco).await?;

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
