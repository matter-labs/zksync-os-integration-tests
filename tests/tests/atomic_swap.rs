//! The atomic-interop swap, run on the two ecosystems that can host it: a freshly deployed one and
//! one whose chain has been through the v31 -> v33 protocol upgrade.
//!
//! The scenario itself lives in `tests::atomic_swap`; these are only its entry points.

use anyhow::{Context, Result};
use rstest::rstest;

use tests::fixtures::{ecosystem, ChainDef, ValidiumDa};
use tests::upgrade_v31_to_v33::{fixture, runbook};
use tests::Ecosystem;

/// Fresh ecosystem: chain 6565 is a rollup, chain 6566 a validium publishing its logs-only
/// pubdata through blobs — the atomic-interop participant configuration — so the swap is
/// exercised across heterogeneous pubdata shapes.
#[rstest]
#[tokio::test(flavor = "multi_thread")]
async fn atomic_swap_l1_settled(
    #[future]
    #[with(vec![
        ChainDef::rollup(6565),
        ChainDef::validium(6566, ValidiumDa::Blobs)
    ])]
    ecosystem: Ecosystem,
) -> Result<()> {
    let eco = ecosystem.await;
    let chains: Vec<_> = eco.chains().collect();
    tests::atomic_swap::run(chains[0], chains[1]).await
}

/// Upgraded ecosystem: the frozen v31 rollup and validium are both taken to v33, the validium is
/// moved onto blobs so its interop (IMT) leaves reach L1, and then the same swap runs between them.
#[tokio::test(flavor = "multi_thread")]
async fn atomic_swap_after_v31_to_v33_upgrade() -> Result<()> {
    // Both the upgrade runbook and the DA switch run forge scripts.
    tests::fixtures::ensure_contracts_built().await;

    let mut eco = fixture::start().await.context("start the v31.1 fixture")?;

    // The runbook takes both chains to v33 and moves the validium off no-DA, which is what makes
    // its interop (IMT) leaves reachable from L1 and the swap below possible.
    let upgrade_blocks = runbook::run_upgrade(&mut eco).await?;

    for (chain_id, upgrade_block) in upgrade_blocks {
        eco.chains()
            .find(|c| c.chain_id() == chain_id)
            .expect("upgraded chain")
            .wait_for_block_finalized(upgrade_block)
            .await
            .with_context(|| format!("finalize chain {chain_id}'s upgrade batch"))?;
    }

    let rollup = eco
        .chains()
        .find(|c| c.chain_id() == fixture::ROLLUP_CHAIN_ID)
        .expect("rollup chain");
    let validium = eco
        .chains()
        .find(|c| c.chain_id() == fixture::VALIDIUM_CHAIN_ID)
        .expect("validium chain");
    tests::atomic_swap::run(rollup, validium).await
}
