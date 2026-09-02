//! What the committed v31.1 fixture is supposed to be.
//!
//! The snapshot is a binary blob produced by a toolchain that is no longer in this workspace (see
//! `tests/README.md`), so this is how it is checked: not by re-deriving the bytes, which is not
//! reproducible, but by asserting the properties every test built on it relies on. Run it after
//! regenerating the fixture — if it passes, the new snapshot is a drop-in replacement.

use anyhow::{Context, Result};
use protocol_ops::common::abi::{BridgehubAbi, ZkChainAbi};

use tests::eth::{call, provider};
use tests::upgrade_v31_to_v33::fixture::{
    self, ROLLUP_CHAIN_ADMIN_OWNER_KEY, ROLLUP_CHAIN_ID, VALIDIUM_CHAIN_ADMIN_OWNER_KEY,
    VALIDIUM_CHAIN_ID,
};

/// Packed `v31.1.0`: minor 31 in bits 32..64, patch 1.
const EXPECTED_PROTOCOL_VERSION: u64 = (31 << 32) | 1;

/// `L2DACommitmentScheme` (`system-contracts/contracts/Constants.sol`).
const EMPTY_NO_DA: u8 = 1;
const BLOBS_ZKSYNC_OS: u8 = 4;

/// `PubdataPricingMode` (`l1-contracts/contracts/common/Config.sol`).
const PRICING_ROLLUP: u8 = 0;
const PRICING_VALIDIUM: u8 = 1;

#[tokio::test(flavor = "multi_thread")]
async fn fixture_is_a_v31_rollup_and_validium() -> Result<()> {
    let eco = fixture::start().await.context("start the v31.1 fixture")?;
    let l1_rpc = eco.chain().l1_rpc_url();
    let bridgehub = eco.chain().bridgehub_addr();
    let l1 = provider(l1_rpc).await?;

    // Exactly the two chains, and both servers came up and settled a batch (`fixture::start`
    // waits for that, so reaching this line is the assertion).
    let chain_ids = call(&l1, bridgehub, BridgehubAbi::getAllZKChainChainIDsCall {}).await?;
    let mut ids: Vec<u64> = chain_ids.iter().map(|id| id.to::<u64>()).collect();
    ids.sort_unstable();
    anyhow::ensure!(
        ids == vec![ROLLUP_CHAIN_ID, VALIDIUM_CHAIN_ID],
        "expected chains [{ROLLUP_CHAIN_ID}, {VALIDIUM_CHAIN_ID}] on the bridgehub, got {ids:?}"
    );

    for (chain_id, pricing, scheme, admin_key) in [
        (
            ROLLUP_CHAIN_ID,
            PRICING_ROLLUP,
            BLOBS_ZKSYNC_OS,
            ROLLUP_CHAIN_ADMIN_OWNER_KEY,
        ),
        (
            VALIDIUM_CHAIN_ID,
            PRICING_VALIDIUM,
            EMPTY_NO_DA,
            VALIDIUM_CHAIN_ADMIN_OWNER_KEY,
        ),
    ] {
        let diamond =
            protocol_ops::common::l1_contracts::resolve_zk_chain(l1_rpc, bridgehub, chain_id)
                .await
                .with_context(|| format!("resolve chain {chain_id}"))?;

        let version = call(&l1, diamond, ZkChainAbi::getProtocolVersionCall {}).await?;
        anyhow::ensure!(
            version.to::<u64>() == EXPECTED_PROTOCOL_VERSION,
            "chain {chain_id} must be v31.1 before the upgrade, got packed {version}"
        );

        let pricing_mode = call(&l1, diamond, ZkChainAbi::getPubdataPricingModeCall {}).await?;
        anyhow::ensure!(
            pricing_mode == pricing,
            "chain {chain_id} pricing mode: expected {pricing}, got {pricing_mode}"
        );

        let (_validator, da_scheme) = {
            let pair = call(&l1, diamond, ZkChainAbi::getDAValidatorPairCall {}).await?;
            (pair._0, pair._1)
        };
        anyhow::ensure!(
            da_scheme == scheme,
            "chain {chain_id} DA commitment scheme: expected {scheme}, got {da_scheme}"
        );

        // Every ChainAdmin-targeted bundle is signed with the key the fixture module names, so a
        // regenerated snapshot with different wallets has to update those constants.
        let chain_admin = call(&l1, diamond, ZkChainAbi::getAdminCall {}).await?;
        let owner = call(&l1, chain_admin, ChainAdminOwner::ownerCall {}).await?;
        let expected: alloy::signers::local::PrivateKeySigner =
            admin_key.parse().context("parse chain admin key")?;
        anyhow::ensure!(
            owner == alloy::signers::Signer::address(&expected),
            "chain {chain_id} ChainAdmin owner {owner:#x} is not the key the fixture module names"
        );
    }

    Ok(())
}

alloy::sol! {
    #[sol(rpc)]
    interface ChainAdminOwner {
        function owner() external view returns (address);
    }
}
