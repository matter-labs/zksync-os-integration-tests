/// End-to-end v31→v33 protocol upgrade on L1-settling ZKsync OS chains.
///
/// Starts the frozen v31.1 fixture — a rollup and a validium, each with its own in-process
/// server, on one anvil L1 — drives the full upgrade through protocol-ops, and verifies both
/// upgraded chains still process deposits. The fixture is restored from a committed snapshot via
/// [`fixture::start`]; the upgrade steps live in [`protocol`].
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

/// The semver the upgrade lands on, as `(major, minor, patch)`. It comes from the pinned
/// era-contracts revision's genesis config, so it moves with the pin.
const UPGRADED_VERSION: (u32, u32, u32) = (0, 33, 0);

/// `PubdataContent.FULL_PUBDATA` — the first variant of the enum the v33
/// `Getters` facet exposes, and the value an upgraded rollup keeps.
const FULL_PUBDATA: u8 = 0;
/// `PubdataContent.LOGS_ONLY` — what a validium reads once the upgrade moves it onto blobs.
const LOGS_ONLY: u8 = 1;

#[tokio::test(flavor = "multi_thread")]
async fn test_v31_to_v33_upgrade() -> Result<()> {
    // The upgrade runbook runs forge scripts — compiled contracts are required.
    tests::fixtures::ensure_contracts_built().await;

    let mut eco = fixture::start().await.context("start the v31.1 fixture")?;
    let l1_rpc = eco.chain().l1_rpc_url().to_string();
    let l1_rpc = l1_rpc.as_str();
    let bridgehub = eco.chain().bridgehub_addr();

    let upgrade_blocks = runbook::run_upgrade(&mut eco).await?;

    for (chain_id, upgrade_block) in &upgrade_blocks {
        let chain_id = *chain_id;
        protocol::assert_protocol_version(l1_rpc, bridgehub, chain_id, UPGRADED_VERSION)
            .await
            .with_context(|| format!("protocol version of chain {chain_id}"))?;

        // The v33 diamond answers `getPubdataContent()`, whose value the Executor folds into
        // every batch's chain-config hash and the server discovers from L1. The rollup keeps the
        // FULL_PUBDATA the upgrade leaves untouched; the validium is LOGS_ONLY, because moving it
        // onto blobs is part of taking it to v33, so that its interop data reaches L1.
        let diamond =
            protocol_ops::common::l1_contracts::resolve_zk_chain(l1_rpc, bridgehub, chain_id)
                .await
                .context("resolve diamond")?;
        let pubdata_content = call(
            &provider(l1_rpc).await?,
            diamond,
            ZkChainAbi::getPubdataContentCall {},
        )
        .await
        .context("getPubdataContent")?;
        let expected = if chain_id == fixture::VALIDIUM_CHAIN_ID {
            LOGS_ONLY
        } else {
            FULL_PUBDATA
        };
        anyhow::ensure!(
            pubdata_content == expected,
            "expected pubdata content {expected} on chain {chain_id} after the upgrade, \
             got {pubdata_content}"
        );

        // The L1 upgrade unblocks the gatekeeper; wait for the v33 upgrade batch to
        // commit/prove/execute.
        eco.chains()
            .find(|c| c.chain_id() == chain_id)
            .expect("upgraded chain")
            .wait_for_block_finalized(*upgrade_block)
            .await
            .with_context(|| format!("finalize chain {chain_id}'s upgrade batch"))?;
    }

    // ── Post-upgrade traffic: an L1→L2 deposit must work on both chains ──────
    //
    // The recipient is an address nothing has ever funded, so its balance appearing on L2 is the
    // deposit and nothing else. A rich wallet would already hold the base token the fixture's
    // `apply` deposited into it, and the check would pass without the chain doing anything.
    const DEPOSIT_WEI: u128 = 1_000_000_000_000_000_000; // 1 ETH
    for chain in eco.chains() {
        let chain_id = chain.chain_id();
        let recipient = fresh_recipient(chain_id);
        zk_deployer::l1_l2_deposit::deposit_base_token(
            l1_rpc,
            bridgehub,
            chain_id,
            recipient,
            U256::from(DEPOSIT_WEI),
            1_000_000_000,
            DEPLOYER_KEY,
            None, // ETH-based chain
        )
        .await
        .with_context(|| format!("post-upgrade deposit to chain {chain_id}"))?;
        zk_deployer::l1_l2_deposit::wait_for_l2_balance(chain.l2_rpc_url(), recipient, 120)
            .await
            .with_context(|| format!("deposit lands on chain {chain_id}"))?;

        // At least the deposited value, not exactly it: the deposit names the recipient as its
        // refund recipient too, so whatever of the base cost the L2 transaction does not burn is
        // credited on top.
        let balance = chain
            .balance(recipient)
            .await
            .with_context(|| format!("balance of {recipient} on chain {chain_id}"))?;
        anyhow::ensure!(
            balance >= U256::from(DEPOSIT_WEI),
            "chain {chain_id} credited {recipient} with {balance}, less than the {DEPOSIT_WEI} \
             deposited"
        );

        // Wait for the deposit's block to finalize. Snapshot latest_block after the balance
        // appears — the deposit is in a block <= this number.
        let deposit_block = chain
            .latest_block()
            .await
            .context("latest block after the deposit")?;
        chain
            .wait_for_block_finalized(deposit_block)
            .await
            .with_context(|| format!("finalize chain {chain_id}'s post-upgrade batch"))?;
    }

    Ok(())
}

/// An address no wallet list contains and no fixture funds, carrying the chain id in its low bytes
/// so a failure says which chain it belongs to.
fn fresh_recipient(chain_id: u64) -> Address {
    let mut bytes = [0u8; 20];
    bytes[..4].copy_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
    bytes[12..].copy_from_slice(&chain_id.to_be_bytes());
    Address::from(bytes)
}
