//! L2→L1 ETH withdrawal: initiate on L2, finalize on L1.
//!
//! Mirrors the canonical zksync-ethers / zksync-os-server withdrawal flow:
//!   1. call `L2BaseToken.withdraw{value}` on L2 — burns L2 ETH and emits the
//!      L2→L1 message that the L1Messenger records,
//!   2. wait for the batch to be executed on L1 (the proof only exists once the
//!      batch root is on L1),
//!   3. fetch the message's merkle proof via `zks_getL2ToL1LogProof`,
//!   4. call `L1AssetRouter.finalizeWithdrawal` on L1 with the proof.
//!
//! The L1 interfaces (`L1AssetRouter`, `Bridgehub`) come from
//! `protocol_ops::common::abi`. The two L2 *system-contract* interfaces below
//! (`L2BaseToken`, `L1Messenger`) have no L1 artifact, so they are declared
//! here at their well-known fixed addresses.

use std::str::FromStr;
use std::time::{Duration, Instant};

use alloy::network::{EthereumWallet, TransactionBuilder};
use alloy::primitives::{address, Address, Bytes, TxHash, B256, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::{Index, TransactionRequest};
use alloy::signers::local::PrivateKeySigner;
use alloy::sol;
use alloy::sol_types::SolCall;
use anyhow::{Context, Result};
use serde::Deserialize;

use protocol_ops::common::abi::{BridgehubAbi, IL1AssetRouterAbi};

// The modern withdrawal entrypoint is `L1Nullifier.finalizeDeposit`, which
// takes the L2 sender explicitly. (The asset router's `finalizeWithdrawal`
// forwards to the nullifier's *legacy* path, which reverts with
// `LegacyBridgeNotSet` on chains that never had a legacy L2 bridge.) Neither
// `IL1Nullifier` nor its `FinalizeL1DepositParams` has an L1 artifact exposed
// through `protocol_ops`, so the minimal shape is declared here.
sol! {
    interface IL1Nullifier {
        struct FinalizeL1DepositParams {
            uint256 chainId;
            uint256 l2BatchNumber;
            uint256 l2MessageIndex;
            address l2Sender;
            uint16 l2TxNumberInBatch;
            bytes message;
            bytes32[] merkleProof;
        }
        function finalizeDeposit(FinalizeL1DepositParams calldata _finalizeWithdrawalParams) external;
    }
}

/// L2 base-token system contract. On an ETH-based chain `withdraw` burns L2 ETH
/// and emits the L2→L1 message that unlocks the funds on L1.
pub const L2_BASE_TOKEN_ADDRESS: Address = address!("000000000000000000000000000000000000800a");
/// L2 messenger system contract. Emits `L1MessageSent` carrying the exact
/// message bytes that `finalizeWithdrawal` re-verifies against the batch root.
pub const L1_MESSENGER_ADDRESS: Address = address!("0000000000000000000000000000000000008008");

sol! {
    interface IBaseToken {
        function withdraw(address _l1Receiver) external payable;
    }
    interface IL1Messenger {
        event L1MessageSent(address indexed _sender, bytes32 indexed _hash, bytes _message);
    }
}

/// `zks_getL2ToL1LogProof` result. A subset of the server's `L2ToL1LogProof`
/// (extra fields like `gatewayBlockNumber` are ignored).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct L2ToL1LogProof {
    /// L1 batch number that contains the withdrawal log.
    batch_number: u64,
    /// Merkle path proving the log's inclusion.
    proof: Vec<B256>,
    /// Index of the leaf in the tree — the `_l2MessageIndex` finalize expects.
    id: u32,
}

/// Initiate an ETH withdrawal from L2 to `l1_receiver`, signed by
/// `withdrawer_key`. Returns the (mined, non-reverted) L2 transaction hash.
pub async fn withdraw_eth(
    l2_rpc_url: &str,
    withdrawer_key: &str,
    l1_receiver: Address,
    amount_wei: U256,
) -> Result<TxHash> {
    let signer: PrivateKeySigner = withdrawer_key.parse().context("parse withdrawer key")?;
    let provider = ProviderBuilder::new()
        .wallet(EthereumWallet::from(signer))
        .connect(l2_rpc_url)
        .await
        .context("connect to L2 with wallet")?;

    let calldata = IBaseToken::withdrawCall {
        _l1Receiver: l1_receiver,
    }
    .abi_encode();
    let tx = TransactionRequest::default()
        .with_to(L2_BASE_TOKEN_ADDRESS)
        .with_input(Bytes::from(calldata))
        .with_value(amount_wei);

    let receipt = provider
        .send_transaction(tx)
        .await
        .context("send L2 withdraw")?
        .get_receipt()
        .await
        .context("await L2 withdraw receipt")?;
    anyhow::ensure!(
        receipt.status(),
        "L2 withdraw tx {:#x} reverted",
        receipt.transaction_hash
    );
    Ok(receipt.transaction_hash)
}

/// Finalize on L1 a withdrawal previously initiated on L2 by [`withdraw_eth`].
///
/// The withdrawal's batch must already be executed on L1 — otherwise the node
/// has no proof to serve yet; this polls `zks_getL2ToL1LogProof` for up to
/// `proof_timeout` to absorb the commit→prove→execute lag. Returns the L1
/// finalize transaction hash.
pub async fn finalize_withdrawal(
    l1_rpc_url: &str,
    l2_rpc_url: &str,
    bridgehub: Address,
    chain_id: u64,
    l2_withdraw_tx: TxHash,
    finalizer_key: &str,
    proof_timeout: Duration,
) -> Result<TxHash> {
    let l2_provider = ProviderBuilder::new()
        .connect(l2_rpc_url)
        .await
        .context("connect to L2")?;

    // The typed receipt gives us the L1Messenger `L1MessageSent` event (the
    // exact message bytes) and the in-batch tx index.
    let receipt = l2_provider
        .get_transaction_receipt(l2_withdraw_tx)
        .await
        .context("eth_getTransactionReceipt")?
        .with_context(|| format!("withdrawal tx {l2_withdraw_tx:#x} not found on L2"))?;
    let message = receipt
        .logs()
        .iter()
        .filter(|log| log.address() == L1_MESSENGER_ADDRESS)
        .find_map(|log| log.log_decode::<IL1Messenger::L1MessageSent>().ok())
        .context("no L1MessageSent event in withdrawal receipt")?
        .inner
        .data
        ._message;
    let l2_tx_number_in_batch: u16 = receipt
        .transaction_index
        .context("withdrawal receipt has no transaction index")?
        .try_into()
        .context("transaction index exceeds u16")?;

    // The proof is keyed by the message's position in the receipt's
    // `l2ToL1Logs` — a zksync-os-specific field absent from the standard
    // receipt type, so it is read from the raw JSON. That same log's `key`
    // holds the (left-padded) address of the L2 contract that sent the message,
    // which `finalizeDeposit` needs as `l2Sender`.
    let raw_receipt: serde_json::Value = l2_provider
        .client()
        .request("eth_getTransactionReceipt", (l2_withdraw_tx,))
        .await
        .context("raw eth_getTransactionReceipt")?;
    let (l2_to_l1_log_index, l2_to_l1_log) = raw_receipt["l2ToL1Logs"]
        .as_array()
        .context("receipt has no l2ToL1Logs array")?
        .iter()
        .enumerate()
        .find(|(_, log)| {
            log["sender"]
                .as_str()
                .and_then(|s| Address::from_str(s).ok())
                == Some(L1_MESSENGER_ADDRESS)
        })
        .context("no L1Messenger entry in l2ToL1Logs")?;
    let l2_sender = {
        let key: B256 = l2_to_l1_log["key"]
            .as_str()
            .context("l2ToL1Logs entry has no key")?
            .parse()
            .context("parse l2ToL1Logs key")?;
        Address::from_slice(&key[12..])
    };

    let proof = wait_for_log_proof(
        &l2_provider,
        l2_withdraw_tx,
        l2_to_l1_log_index,
        proof_timeout,
    )
    .await?;

    // Resolve the L1Nullifier (bridgehub → assetRouter → L1_NULLIFIER) and
    // finalize there directly.
    let l1_plain = ProviderBuilder::new()
        .connect(l1_rpc_url)
        .await
        .context("connect to L1")?;
    let asset_router = {
        let raw = l1_plain
            .call(
                TransactionRequest::default()
                    .with_to(bridgehub)
                    .with_input(BridgehubAbi::assetRouterCall {}.abi_encode()),
            )
            .await
            .context("bridgehub.assetRouter()")?;
        BridgehubAbi::assetRouterCall::abi_decode_returns(&raw).context("decode assetRouter")?
    };
    let nullifier = {
        let raw = l1_plain
            .call(
                TransactionRequest::default()
                    .with_to(asset_router)
                    .with_input(IL1AssetRouterAbi::L1_NULLIFIERCall {}.abi_encode()),
            )
            .await
            .context("assetRouter.L1_NULLIFIER()")?;
        IL1AssetRouterAbi::L1_NULLIFIERCall::abi_decode_returns(&raw)
            .context("decode L1_NULLIFIER")?
    };

    let signer: PrivateKeySigner = finalizer_key.parse().context("parse finalizer key")?;
    let l1_signed = ProviderBuilder::new()
        .wallet(EthereumWallet::from(signer))
        .connect(l1_rpc_url)
        .await
        .context("connect to L1 with wallet")?;
    let calldata = IL1Nullifier::finalizeDepositCall {
        _finalizeWithdrawalParams: IL1Nullifier::FinalizeL1DepositParams {
            chainId: U256::from(chain_id),
            l2BatchNumber: U256::from(proof.batch_number),
            l2MessageIndex: U256::from(proof.id),
            l2Sender: l2_sender,
            l2TxNumberInBatch: l2_tx_number_in_batch,
            message,
            merkleProof: proof.proof,
        },
    }
    .abi_encode();
    let receipt = l1_signed
        .send_transaction(
            TransactionRequest::default()
                .with_to(nullifier)
                .with_input(Bytes::from(calldata)),
        )
        .await
        .context("send finalizeDeposit")?
        .get_receipt()
        .await
        .context("await finalizeDeposit receipt")?;
    anyhow::ensure!(
        receipt.status(),
        "finalizeDeposit tx {:#x} reverted",
        receipt.transaction_hash
    );
    Ok(receipt.transaction_hash)
}

/// Poll `zks_getL2ToL1LogProof` until the node has a proof (it returns `null`
/// until the batch is executed on L1), or `timeout` elapses.
async fn wait_for_log_proof(
    l2_provider: &impl Provider,
    l2_withdraw_tx: TxHash,
    l2_to_l1_log_index: usize,
    timeout: Duration,
) -> Result<L2ToL1LogProof> {
    let started = Instant::now();
    loop {
        let proof: Option<L2ToL1LogProof> = l2_provider
            .client()
            .request(
                "zks_getL2ToL1LogProof",
                (l2_withdraw_tx, Index::from(l2_to_l1_log_index)),
            )
            .await
            .context("zks_getL2ToL1LogProof")?;
        if let Some(proof) = proof {
            return Ok(proof);
        }
        anyhow::ensure!(
            started.elapsed() < timeout,
            "node did not provide a withdrawal proof for tx {l2_withdraw_tx:#x} within {timeout:?}"
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}
