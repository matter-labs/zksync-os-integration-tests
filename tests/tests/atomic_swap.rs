//! The atomic-interop swap, run on the two ecosystems that can host it: a freshly deployed one and
//! one whose chain has been through the v31 -> v33 protocol upgrade.
//!
//! The scenario itself lives in `tests::atomic_swap`; these are only its entry points.

use anyhow::{Context, Result};
use rstest::rstest;

use tests::fixtures::{ecosystem, ChainDef};
use tests::upgrade_v31_to_v33::{fixture, runbook, second_chain};
use tests::Ecosystem;

/// The chain id the post-upgrade test creates alongside the fixture's chain 506.
const SECOND_CHAIN_ID: u64 = 6567;

/// Fresh ecosystem: chain 6565 is a rollup, chain 6566 a logs-only validium, so the swap is
/// exercised across heterogeneous pubdata shapes.
#[rstest]
#[tokio::test(flavor = "multi_thread")]
async fn atomic_swap_l1_settled(
    #[future]
    #[with(vec![
        ChainDef::rollup(6565),
        ChainDef::logs_only_validium(6566)
    ])]
    ecosystem: Ecosystem,
) -> Result<()> {
    let eco = ecosystem.await;
    let chains: Vec<_> = eco.chains().collect();
    tests::atomic_swap::run(chains[0], chains[1]).await
}

/// Upgraded ecosystem: the frozen v31 chain is taken to v33 and then swaps with a chain created on
/// the upgraded ecosystem — the same scenario, on chains that got there the other way.
///
/// Ignored: creating the second chain fails in `RegisterZKChain.s.sol` with
/// `NoLogsFound(NewChainCreationParams)`. The script reads the CTM's
/// `newChainCreationParamsBlock(protocolVersion)` and re-reads the params out of that block's logs,
/// but an upgrade *copies* the recorded block from the old protocol version
/// (`ChainTypeManagerBase:434`), so on a chain restored from a state snapshot it points into
/// history the snapshot does not carry — a state dump has state, not receipts. Chain creation on an
/// upgraded ecosystem therefore needs either the params re-emitted after the upgrade (a governance
/// `setChainCreationParams`) or a two-chain fixture that never has to create one.
#[ignore = "creating a chain on a snapshot-restored upgraded ecosystem hits NoLogsFound(NewChainCreationParams)"]
#[tokio::test(flavor = "multi_thread")]
async fn atomic_swap_after_v31_to_v33_upgrade() -> Result<()> {
    // Both the upgrade runbook and chain creation run forge scripts.
    tests::fixtures::ensure_contracts_built().await;

    let mut eco = fixture::start().await.context("start v31.0 fixture")?;
    let upgrade_block = runbook::run_upgrade(&eco).await?;
    eco.chain()
        .wait_for_block_finalized(upgrade_block)
        .await
        .context("wait for the upgrade batch to finalize")?;

    second_chain::create_and_start(&mut eco, SECOND_CHAIN_ID)
        .await
        .context("create the second chain on the upgraded ecosystem")?;

    let chains: Vec<_> = eco.chains().collect();
    tests::atomic_swap::run(chains[0], chains[1]).await
}
