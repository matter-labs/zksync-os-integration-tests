use anyhow::Result;
use rstest::rstest;
use tests::fixtures::ecosystem;
use tests::Ecosystem;

/// Verify the full commit → prove → execute → finalize pipeline.
#[rstest]
#[tokio::test(flavor = "multi_thread")]
async fn chain_executes_a_batch(#[future] ecosystem: Ecosystem) -> Result<()> {
    let eco = ecosystem.await;
    let hash = eco.chain().ping().await?;
    eco.chain().wait_for_tx_finalized(hash).await?;
    Ok(())
}

/// Two independent chains settle on the same L1.
#[rstest]
#[tokio::test(flavor = "multi_thread")]
async fn two_chains_settle_on_l1(
    #[future]
    #[with(vec![6565, 6566])]
    ecosystem: Ecosystem,
) -> Result<()> {
    let eco = ecosystem.await;

    // Trigger a batch on every chain, then wait for each ping to finalize.
    // (Waiting per-tx, not per-"next batch": chain B's ping may already be
    // finalized by the time we're done waiting on chain A.)
    let mut pings = Vec::new();
    for chain in eco.chains() {
        pings.push(chain.ping().await?);
    }
    for (chain, hash) in eco.chains().zip(pings) {
        chain.wait_for_tx_finalized(hash).await?;
    }
    Ok(())
}
