//! Moving an upgraded validium off no-DA.
//!
//! A no-DA validium publishes nothing to L1, so the interop commitment tree leaves its
//! `L2InteropCommitmentTree` records are unreachable from L1 and the chain cannot take part in
//! atomic interop. v33 is the first version where a validium-priced chain may publish through
//! blobs (`ProtocolSemanticVersion::MIN_VERSION_WITH_VALIDIUM_DA`), so the move is part of taking
//! such a chain to v33 — in two halves, in this order:
//!
//!   1. [`prepare_server_for_blobs`], before anything happens on L1. The server reads its pubdata
//!      mode once at startup, and `PubdataMode::adapt_for_protocol_version` keeps sealing pre-v33
//!      batches with no DA regardless of it — so the restart is inert until the chain reaches v33,
//!      and doing it first means the server is already correct when it does.
//!   2. [`to_logs_only_blobs`], right after the diamond cut: the DA validator pair and
//!      `PubdataContent::LOGS_ONLY`.
//!
//! Between the two the chain's commits revert with `MismatchL2DACommitmentScheme` — the server
//! sends the blobs scheme while L1 still stores the no-DA one — and resume once the pair is set.
//! That is the harmless failure of the pair; the other order commits v33 batches that no prover
//! can settle. Closing the window entirely takes putting both calls in the cut's own
//! `ChainAdmin.multicall`, which needs the protocol-ops change in
//! matter-labs/era-contracts#2455.

use std::time::{Duration, Instant};

use alloy::primitives::{Address, Bytes, U256};
use alloy::sol_types::SolCall;
use anyhow::{Context, Result};
use protocol_ops::common::abi::{IChainAdminAbi, ZkChainAbi};
use protocol_ops::types::L2DACommitmentScheme;

use super::fixture::{CHAIN_SIGNING_KEYS, VALIDIUM_CHAIN_ADMIN_OWNER_KEY};
use super::protocol;
use crate::ecosystem::Ecosystem;
use crate::eth::{call, provider, send_as_signer};

/// `PubdataContent.LOGS_ONLY` (era-contracts `Constants.sol`).
const LOGS_ONLY: u8 = 1;

/// How long the chain gets to prove out the batches committed before the switch.
const QUIESCE_TIMEOUT: Duration = Duration::from_secs(300);

/// Restart `chain_id`'s server with `pubdata_mode: Blobs` — the config change an operator makes
/// before the chain reaches v33, since the server only reads the setting at startup.
pub async fn prepare_server_for_blobs(eco: &mut Ecosystem, chain_id: u64) -> Result<()> {
    eco.restart_chain_with_config(chain_id, "l1_sender:\n  pubdata_mode: Blobs\n")
        .await
        .with_context(|| format!("restart chain {chain_id}'s server in Blobs pubdata mode"))
}

/// Move `chain_id`'s L1 configuration from no-DA to logs-only pubdata posted through blobs.
pub async fn to_logs_only_blobs(eco: &Ecosystem, chain_id: u64) -> Result<()> {
    let chain = eco
        .chains()
        .find(|c| c.chain_id() == chain_id)
        .with_context(|| format!("chain {chain_id} is not part of this ecosystem"))?;
    let l1_rpc = chain.l1_rpc_url().to_string();
    let bridgehub = chain.bridgehub_addr();
    let workdir = eco.workdir().to_path_buf();

    let blobs_validator = blobs_da_validator(&l1_rpc, bridgehub, eco).await?;

    protocol::set_da_validator_pair(
        &l1_rpc,
        &workdir,
        bridgehub,
        chain_id,
        blobs_validator,
        // The scheme the rollup's blobs validator accepts on ZKsync OS. It has to match what the
        // server sends, or every commit reverts with `MismatchL2DACommitmentScheme`.
        L2DACommitmentScheme::BlobsZKSyncOS,
        CHAIN_SIGNING_KEYS,
    )
    .await
    .context("set the blobs DA validator pair")?;

    wait_for_no_unverified_batches(&l1_rpc, bridgehub, chain_id, QUIESCE_TIMEOUT)
        .await
        .context("wait for the in-flight batches to prove out")?;

    set_pubdata_content(&l1_rpc, bridgehub, chain_id, LOGS_ONLY)
        .await
        .context("set LOGS_ONLY pubdata content")
}

/// The blobs DA validator to move onto: the one the ecosystem's rollup chain already commits with,
/// so the switch cannot land on a validator the settlement layer does not accept.
async fn blobs_da_validator(l1_rpc: &str, bridgehub: Address, eco: &Ecosystem) -> Result<Address> {
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

/// `Admin.setPubdataContent` through `ChainAdmin.multicall`, signed by the ChainAdmin's owner —
/// the path every other `onlyAdmin` setter takes.
///
/// TODO(protocol-ops): replace with a `chain set-pubdata-content` command; `AdminFunctions.s.sol`
/// has an entry point for the DA pair but not yet for the content.
async fn set_pubdata_content(
    l1_rpc: &str,
    bridgehub: Address,
    chain_id: u64,
    content: u8,
) -> Result<()> {
    let diamond = protocol_ops::common::l1_contracts::resolve_zk_chain(l1_rpc, bridgehub, chain_id)
        .await
        .context("resolve diamond")?;
    let provider = provider(l1_rpc).await?;
    let chain_admin = call(&provider, diamond, ZkChainAbi::getAdminCall {}).await?;

    send_as_signer(
        l1_rpc,
        VALIDIUM_CHAIN_ADMIN_OWNER_KEY,
        chain_admin,
        IChainAdminAbi::multicallCall {
            _calls: vec![IChainAdminAbi::Call {
                target: diamond,
                value: U256::ZERO,
                data: Bytes::from(
                    ZkChainAbi::setPubdataContentCall {
                        _pubdataContent: content,
                    }
                    .abi_encode(),
                ),
            }],
            _requireSuccess: true,
        },
    )
    .await
    .context("ChainAdmin.multicall(setPubdataContent)")
}

/// `setPubdataContent` reverts while committed-but-unverified batches exist: those ran under the
/// old chain config and would become unprovable.
async fn wait_for_no_unverified_batches(
    l1_rpc: &str,
    bridgehub: Address,
    chain_id: u64,
    timeout: Duration,
) -> Result<()> {
    let diamond = protocol_ops::common::l1_contracts::resolve_zk_chain(l1_rpc, bridgehub, chain_id)
        .await
        .context("resolve diamond")?;
    let provider = provider(l1_rpc).await?;

    let deadline = Instant::now() + timeout;
    loop {
        let committed = call(
            &provider,
            diamond,
            ZkChainAbi::getTotalBatchesCommittedCall {},
        )
        .await?;
        let verified = call(
            &provider,
            diamond,
            ZkChainAbi::getTotalBatchesVerifiedCall {},
        )
        .await?;
        if committed == verified {
            return Ok(());
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "chain {chain_id} still has unverified batches (committed {committed}, verified {verified})"
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}
