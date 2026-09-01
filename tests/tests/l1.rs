use alloy::network::TransactionBuilder;
use alloy::primitives::U256;
use alloy::rpc::types::TransactionRequest;
use anyhow::Result;
use rstest::rstest;
use tests::fixtures::{ecosystem, ChainDef};
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
    #[with(vec![ChainDef::rollup(6565), ChainDef::rollup(6566)])]
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

/// A logs-only validium commits/proves/executes on L1 alongside a rollup: it registers with
/// the same blobs DA validator and rollup pricing mode, and differs only in the pubdata
/// content its diamond was initialized with.
#[rstest]
#[tokio::test(flavor = "multi_thread")]
async fn logs_only_validium_settles_on_l1(
    #[future]
    #[with(vec![
        ChainDef::logs_only_validium(6565),
        ChainDef::rollup(6566),
    ])]
    ecosystem: Ecosystem,
) -> Result<()> {
    let eco = ecosystem.await;

    let mut pings = Vec::new();
    for chain in eco.chains() {
        // Explicit gas limit instead of ping()'s eth_estimateGas: on a calldata-priced
        // chain (pubdata ~17 gwei vs ~0.1 gwei basefee) estimation-sized transfers are
        // currently rejected by the pool as "intrinsic gas too low" — a server-side
        // estimation/validation mismatch, orthogonal to the DA settlement this test covers.
        let self_addr = chain.wallet(0).address();
        let tx = TransactionRequest::default()
            .with_to(self_addr)
            .with_value(U256::from(1u64))
            .with_gas_limit(1_000_000);
        pings.push(chain.send_tx(tx).await?);
    }
    for (chain, hash) in eco.chains().zip(pings) {
        chain.wait_for_tx_finalized(hash).await?;
    }
    Ok(())
}
