//! L2 -> L1 base-token withdrawal: the send on L2, and the finalization on L1.
//!
//! A withdrawal in this release is an ordinary NON-atomic interop bundle addressed to the L1 chain
//! id — `InteropCenter.sendBundle` with a single indirect call to the L2 asset router, which
//! resolves to a `finalizeDeposit` call targeting the L1 asset router. It travels as an L2 -> L1
//! message and is finalized by `L1InteropHandler.executeBundle`. Neither of the older paths exists
//! any more: `L2BaseToken` has no `withdraw` entry point, and `L1Nullifier` no legacy
//! withdrawal-message path.
//!
//! The bundle encoders live in [`crate::atomic_swap`], which sends the same shape of bundle to an
//! L2 destination; only the destination chain, the value handling and the proof differ here.
//!
//! The base token is the interesting asset for the v33 upgrade: it is native to L1, so its
//! finalization is what `L1NativeTokenVault.bridgedOut` governs (see
//! [`crate::upgrade_v31_to_v33::protocol`]).

use std::time::{Duration, Instant};

use alloy::network::EthereumWallet;
use alloy::primitives::{keccak256, Address, Bytes, B256, U256};
use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use alloy::rpc::types::Index;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol;
use alloy::sol_types::{SolCall, SolEvent, SolValue};
use anyhow::{ensure, Context, Result};
use serde::Deserialize;

use protocol_ops::common::abi::{IL1AssetRouterAbi, IL2NativeTokenVaultAbi};

use crate::atomic_swap::{
    encode_evm_address, encode_evm_chain, indirect_call_attr, token_transfer_data,
    IERC7786Attributes, IInteropCenter, InteropCallStarter, ASSET_ROUTER, INTEROP_CENTER,
    NATIVE_TOKEN_VAULT,
};
use crate::chain::Chain;
use crate::eth::{call, provider};

/// `BundleStatus.FullyExecuted` (common/Messaging.sol).
pub const BUNDLE_FULLY_EXECUTED: u8 = 2;

/// `BUNDLE_IDENTIFIER` (common/Messaging.sol) — the prefix the InteropCenter publishes a bundle
/// message under, and the byte `L1InteropHandler` rebuilds the proven message data with.
const BUNDLE_IDENTIFIER: u8 = 0x01;

/// Gas for the withdrawal `sendBundle`. An L1-destined bundle does no IMT insert (that is the
/// atomic path), so it is far cheaper than an atomic send; this is the same ceiling the rest of the
/// suite gives an L2 transaction.
const WITHDRAW_GAS: u64 = 5_000_000;

sol! {
    struct L2Message {
        uint16 txNumberInBatch;
        address sender;
        bytes data;
    }
    struct MessageInclusionProof {
        uint256 chainId;
        uint256 l1BatchNumber;
        uint256 l2MessageIndex;
        L2Message message;
        bytes32[] proof;
    }

    #[sol(rpc)]
    interface IL1InteropHandler {
        function executeBundle(bytes bundle, MessageInclusionProof proof) external;
        function bundleStatus(bytes32 bundleHash) external view returns (uint8);
    }

    /// `l1InteropHandler` is a plain public variable on the L1 asset router rather than part of
    /// `IL1AssetRouter`, so protocol-ops' artifact-generated ABI does not carry its getter.
    #[sol(rpc)]
    interface IL1AssetRouterInteropHandler {
        function l1InteropHandler() external view returns (address);
    }

    /// `L1NativeTokenVault._handleBridgeFromChain` — what an inbound amount exceeding the vault's
    /// outstanding `bridgedOut` for an L1-native asset reverts with.
    error InsufficientChainBalance(uint256 chainId, bytes32 assetId, uint256 amount);
}

/// A withdrawal that has been sent on L2 and not yet finalized on L1.
#[derive(Debug, Clone)]
pub struct SentWithdrawal {
    /// The chain the withdrawal was sent from — the `sourceChainId` its finalization is proven
    /// under, and the chain id the vault reports in `InsufficientChainBalance`.
    pub chain_id: u64,
    pub asset_id: B256,
    pub amount: U256,
    pub l1_receiver: Address,
    pub bundle_hash: B256,
    /// `abi.encode(InteropBundle)` — what `executeBundle` takes and hashes.
    pub bundle_data: Bytes,
    pub l2_tx_hash: B256,
}

/// `zks_getL2ToL1LogProof` result (the fields the finalization needs).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RpcLogProof {
    /// L1 batch number holding the log.
    batch_number: u64,
    /// Merkle path, in the form the contracts consume (the first word is metadata).
    proof: Vec<B256>,
    /// Index of the log's leaf in the batch's L2 -> L1 log tree — the `l2MessageIndex` the
    /// inclusion proof carries.
    id: u32,
}

/// Withdraw `amount` of `chain`'s base token to `l1_receiver` on L1, signed by `wallet`.
///
/// Returns once the L2 transaction is mined; the withdrawal is finalizable on L1 only after its
/// batch executes there (see [`inclusion_proof`]).
pub async fn send_base_token(
    chain: &Chain,
    wallet: &PrivateKeySigner,
    amount: U256,
    l1_receiver: Address,
) -> Result<SentWithdrawal> {
    let sender = wallet.address();
    let l2: DynProvider = ProviderBuilder::new()
        .wallet(EthereumWallet::from(wallet.clone()))
        .connect(chain.l2_rpc_url())
        .await
        .context("connect to L2 with wallet")?
        .erased();
    let l1_chain_id = provider(chain.l1_rpc_url())
        .await?
        .get_chain_id()
        .await
        .context("L1 chain id")?;

    let asset_id = call(
        &l2,
        NATIVE_TOKEN_VAULT,
        IL2NativeTokenVaultAbi::BASE_TOKEN_ASSET_IDCall {},
    )
    .await
    .context("L2 NTV base-token asset id")?;

    // `isInteropBundleSaltUsed[sender][salt]` is one-shot and the default salt is `bytes32(0)`, so
    // only the first bundle a sender ever sends can omit one. The sender's nonce is unique per
    // send, which makes the salt unique without any state of our own.
    let nonce = l2
        .get_transaction_count(sender)
        .await
        .context("L2 nonce of the withdrawer")?;
    let salt = keccak256((sender, U256::from(nonce)).abi_encode_params());
    let salt_attr = Bytes::from(IERC7786Attributes::interopBundleSaltCall { salt }.abi_encode());

    let starter = InteropCallStarter {
        to: encode_evm_address(ASSET_ROUTER),
        data: token_transfer_data(asset_id, amount, l1_receiver),
        // The base-token burn takes the withdrawn amount as source-side value; the NTV requires it
        // to equal `msg.value`, which is why the send below carries exactly `amount` (an L1
        // destination pays no interop protocol fee).
        callAttributes: vec![indirect_call_attr(amount)],
    };

    let ic = IInteropCenter::new(INTEROP_CENTER, &l2);
    let send = ic
        .sendBundle(
            encode_evm_chain(l1_chain_id),
            vec![starter],
            vec![salt_attr],
        )
        .value(amount)
        .gas(WITHDRAW_GAS);
    let receipt = send.clone().send().await?.get_receipt().await?;
    if !receipt.status() {
        // A mined-but-reverted transaction carries no reason. Replay it as a call against the block
        // it landed in to get one, instead of reporting only that it failed.
        let reason = send
            .block(receipt.block_number.unwrap_or_default().into())
            .call()
            .await
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_else(|| {
                format!(
                    "no revert reason, and the same call succeeds against that state — so it ran \
                     out of the {WITHDRAW_GAS} gas it was given (used {})",
                    receipt.gas_used
                )
            });
        anyhow::bail!(
            "withdrawal sendBundle reverted on chain {} (tx {:#x}): {reason}",
            chain.chain_id(),
            receipt.transaction_hash
        );
    }

    let (bundle_hash, bundle_data) = receipt
        .inner
        .logs()
        .iter()
        .filter(|log| log.address() == INTEROP_CENTER)
        .find_map(|log| IInteropCenter::InteropBundleSent::decode_log(&log.inner).ok())
        .map(|decoded| {
            (
                decoded.data.interopBundleHash,
                Bytes::from(decoded.data.interopBundle.abi_encode()),
            )
        })
        .context("InteropBundleSent event not found in the withdrawal receipt")?;

    Ok(SentWithdrawal {
        chain_id: chain.chain_id(),
        asset_id,
        amount,
        l1_receiver,
        bundle_hash,
        bundle_data,
        l2_tx_hash: receipt.transaction_hash,
    })
}

/// The L2 -> L1 message-inclusion proof for `withdrawal`, polled until the node can serve it.
///
/// The proof only exists once the batch holding the send has executed on L1, which the node
/// reports as a `null` result until then.
pub async fn inclusion_proof(
    chain: &Chain,
    withdrawal: &SentWithdrawal,
    timeout: Duration,
) -> Result<MessageInclusionProof> {
    let l2 = provider(chain.l2_rpc_url()).await?;

    // The proof is keyed by the message's position in the receipt's `l2ToL1Logs` — a ZKsync-OS
    // field the standard receipt type has no room for, so it is read from the raw JSON. Each entry
    // is emitted by the L1Messenger hook with `key` holding the (left-padded) address of the
    // contract that sent the message, which is how the bundle's own log is identified.
    let raw: serde_json::Value = l2
        .client()
        .request("eth_getTransactionReceipt", (withdrawal.l2_tx_hash,))
        .await
        .context("raw eth_getTransactionReceipt")?;
    let logs = raw["l2ToL1Logs"]
        .as_array()
        .context("receipt has no l2ToL1Logs array")?;
    let (index, log) = logs
        .iter()
        .enumerate()
        .find(|(_, log)| {
            log["key"]
                .as_str()
                .and_then(|k| k.parse::<B256>().ok())
                .is_some_and(|k| Address::from_slice(&k[12..]) == INTEROP_CENTER)
        })
        .context("no InteropCenter entry in the withdrawal's l2ToL1Logs")?;
    // ZKsync OS packs the transaction's index WITHIN ITS BLOCK into the log
    // (`L2ToL1Log.tx_number_in_block = transaction_index`), and the contracts read that field as
    // `L2Message.txNumberInBatch`.
    let tx_number_in_batch: u16 = {
        let raw = log["transactionIndex"]
            .as_str()
            .context("l2ToL1Logs entry has no transactionIndex")?;
        u64::from_str_radix(raw.trim_start_matches("0x"), 16)
            .with_context(|| format!("parse transactionIndex {raw}"))?
            .try_into()
            .context("transaction index exceeds u16")?
    };

    let started = Instant::now();
    let proof = loop {
        let proof: Option<RpcLogProof> = l2
            .client()
            .request(
                "zks_getL2ToL1LogProof",
                (withdrawal.l2_tx_hash, Index::from(index)),
            )
            .await
            .context("zks_getL2ToL1LogProof")?;
        if let Some(proof) = proof {
            break proof;
        }
        ensure!(
            started.elapsed() < timeout,
            "no withdrawal proof for tx {:#x} on chain {} within {timeout:?}",
            withdrawal.l2_tx_hash,
            withdrawal.chain_id
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    };

    let mut data = vec![BUNDLE_IDENTIFIER];
    data.extend_from_slice(&withdrawal.bundle_data);
    Ok(MessageInclusionProof {
        chainId: U256::from(withdrawal.chain_id),
        l1BatchNumber: U256::from(proof.batch_number),
        l2MessageIndex: U256::from(proof.id),
        message: L2Message {
            txNumberInBatch: tx_number_in_batch,
            // `L1InteropHandler` requires the proven message's sender to be the canonical L2
            // InteropCenter, and rebuilds `data` from the bundle it is executing.
            sender: INTEROP_CENTER,
            data: Bytes::from(data),
        },
        proof: proof.proof,
    })
}

/// The L1 interop handler — where a withdrawal is finalized, wired into the L1 asset router by the
/// upgrade's stage 1.
pub async fn l1_interop_handler(l1_rpc: &str, bridgehub: Address) -> Result<Address> {
    let l1 = provider(l1_rpc).await?;
    let asset_router = call(
        &l1,
        bridgehub,
        protocol_ops::common::abi::BridgehubAbi::assetRouterCall {},
    )
    .await
    .context("bridgehub.assetRouter()")?;
    let handler = call(
        &l1,
        asset_router,
        IL1AssetRouterInteropHandler::l1InteropHandlerCall {},
    )
    .await
    .context("assetRouter.l1InteropHandler()")?;
    ensure!(
        handler != Address::ZERO,
        "the L1 asset router has no interop handler wired"
    );
    Ok(handler)
}

/// The L1 ETH asset id — `bridgedOut`'s key for the base token of an ETH-based chain.
pub async fn l1_eth_asset_id(l1_rpc: &str, bridgehub: Address) -> Result<B256> {
    let l1 = provider(l1_rpc).await?;
    let asset_router = call(
        &l1,
        bridgehub,
        protocol_ops::common::abi::BridgehubAbi::assetRouterCall {},
    )
    .await
    .context("bridgehub.assetRouter()")?;
    call(
        &l1,
        asset_router,
        IL1AssetRouterAbi::ETH_TOKEN_ASSET_IDCall {},
    )
    .await
    .context("assetRouter.ETH_TOKEN_ASSET_ID()")
}

/// Finalize `withdrawal` on L1 and assert the bundle ends up fully executed.
pub async fn finalize(
    l1_rpc: &str,
    handler: Address,
    withdrawal: &SentWithdrawal,
    proof: &MessageInclusionProof,
    signer_key: &str,
) -> Result<()> {
    let signer: PrivateKeySigner = signer_key.parse().context("parse finalizer key")?;
    let l1 = ProviderBuilder::new()
        .wallet(EthereumWallet::from(signer))
        .connect(l1_rpc)
        .await
        .context("connect to L1 with wallet")?
        .erased();
    let contract = IL1InteropHandler::new(handler, &l1);
    let receipt = contract
        .executeBundle(withdrawal.bundle_data.clone(), proof.clone())
        .send()
        .await
        .context("send executeBundle")?
        .get_receipt()
        .await
        .context("await executeBundle receipt")?;
    ensure!(
        receipt.status(),
        "executeBundle tx {:#x} reverted",
        receipt.transaction_hash
    );
    let status = contract
        .bundleStatus(withdrawal.bundle_hash)
        .call()
        .await
        .context("bundleStatus")?;
    ensure!(
        status == BUNDLE_FULLY_EXECUTED,
        "bundle {} finalized with status {status}, expected FullyExecuted",
        withdrawal.bundle_hash
    );
    Ok(())
}

/// Replay the finalization as an `eth_call` and return the `InsufficientChainBalance` it reverts
/// with. Fails if the call would succeed, or reverts with anything else.
///
/// The proof gate runs before the bundle's calls, so reaching this error at all also proves the
/// inclusion proof itself verified — the withdrawal is blocked on vault accounting and nothing
/// else. A call rather than a transaction, because a mined-but-reverted L1 transaction carries no
/// reason.
pub async fn finalize_expecting_insufficient_balance(
    l1_rpc: &str,
    handler: Address,
    withdrawal: &SentWithdrawal,
    proof: &MessageInclusionProof,
) -> Result<InsufficientChainBalance> {
    let l1 = provider(l1_rpc).await?;
    let err = IL1InteropHandler::new(handler, &l1)
        .executeBundle(withdrawal.bundle_data.clone(), proof.clone())
        .call()
        .await
        .err()
        .context("executeBundle succeeded, expected it to revert")?;
    err.as_decoded_error::<InsufficientChainBalance>()
        .with_context(|| format!("unexpected revert from executeBundle: {err:#}"))
}

/// L1 ETH balance of `addr`.
pub async fn l1_balance(l1_rpc: &str, addr: Address) -> Result<U256> {
    provider(l1_rpc)
        .await?
        .get_balance(addr)
        .await
        .context("eth_getBalance on L1")
}
