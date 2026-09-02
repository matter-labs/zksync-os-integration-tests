//! Moving an upgraded validium off no-DA.
//!
//! A no-DA validium publishes nothing to L1, so the interop commitment tree leaves its
//! `L2InteropCommitmentTree` records are unreachable from L1 and the chain cannot take part in
//! atomic interop. v33 is the first version where a validium-priced chain may publish through
//! blobs, which is what this moves it onto. (Such a chain can stay on no-DA past v33 — it only
//! has to commit `LOGS_ONLY` rather than the version's `FULL_PUBDATA` default — but then its
//! interop data still never reaches L1.) The move has two halves, in this order:
//!
//!   1. the server starts posting through blobs ([`prepare_server_for_blobs`]), which it does
//!      only from v33 onward — `PubdataMode::adapt_for_protocol_version` keeps sealing pre-v33
//!      batches with no DA, so this is safe to do while the chain is still on v31;
//!   2. the L1 side — the DA validator pair and `PubdataContent::LogsOnly` — which protocol-ops
//!      puts in the same `ChainAdmin.multicall` as the diamond cut.
//!
//! In that order there is no window on either side: the server is already configured when the cut
//! lands, and the cut, the pair and the content take effect in one transaction.

use anyhow::{Context, Result};

use crate::ecosystem::Ecosystem;
use crate::eth::{call, provider};
use alloy::primitives::Address;
use protocol_ops::common::abi::ZkChainAbi;

/// Restart `chain_id`'s server with `pubdata_mode: Blobs` — the config change an operator makes
/// before the chain reaches v33. Until then the server keeps committing the empty no-DA scheme.
///
/// Settles one batch afterwards to prove exactly that: the restarted server still commits, proves
/// and executes on a chain that is on the old protocol version and validium-priced on L1, with the
/// new setting already in its config.
pub async fn prepare_server_for_blobs(eco: &mut Ecosystem, chain_id: u64) -> Result<()> {
    eco.restart_chain_with_config(chain_id, "l1_sender:\n  pubdata_mode: Blobs\n")
        .await
        .with_context(|| format!("restart chain {chain_id}'s server in Blobs pubdata mode"))?;

    let chain = eco
        .chains()
        .find(|c| c.chain_id() == chain_id)
        .with_context(|| format!("chain {chain_id} is not part of this ecosystem"))?;
    let hash = chain
        .ping()
        .await
        .with_context(|| format!("send an L2 transaction on chain {chain_id} after the restart"))?;
    chain
        .wait_for_tx_finalized(hash)
        .await
        .with_context(|| format!("settle a batch on chain {chain_id} after the restart"))?;
    Ok(())
}

/// The blobs DA validator to move onto: the one the ecosystem's rollup chain commits with. The
/// upgrade leaves a chain's DA validator pair alone, so this is the same address before and after
/// the cut — and by construction one the settlement layer accepts.
pub async fn blobs_da_validator(
    l1_rpc: &str,
    bridgehub: Address,
    eco: &Ecosystem,
) -> Result<Address> {
    let rollup = eco
        .chains()
        .find(|c| c.chain_id() == super::fixture::ROLLUP_CHAIN_ID)
        .context("the fixture's rollup chain")?;
    let diamond =
        protocol_ops::common::l1_contracts::resolve_zk_chain(l1_rpc, bridgehub, rollup.chain_id())
            .await
            .context("resolve the rollup's diamond")?;
    let provider = provider(l1_rpc).await?;
    let pair = call(&provider, diamond, ZkChainAbi::getDAValidatorPairCall {}).await?;
    Ok(pair._0)
}
