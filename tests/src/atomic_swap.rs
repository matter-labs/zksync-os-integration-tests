//! End-to-end atomic-interop swap between two L1-settling chains (no gateway in the path), driven
//! natively in Rust.
//!
//! [`run`] is the scenario; the tests supply the two chains and differ only in how those chains
//! came to exist (a fresh ecosystem vs one that has been through a protocol upgrade). It drives
//! the full bundle-model atomic swap entirely in Rust (no TS driver):
//!
//!   1. deploy + register a TestnetERC20 on each chain (mint to the user, approve the NTV),
//!   2. register the two chains with each other for interop (permissionless `registerChain`),
//!   3. atomic-send both legs (burn + IMT insert), with the send-time low-nullifier index supplied
//!      by the server's Rust IMT engine (`zks_getImtLowNullifierIndex`),
//!   4. wait for each send batch to execute on L1 and fetch the COMPLETE per-leg inclusion proof
//!      (`zks_getImtInclusionProof`): the IMT membership half against the batch-end commitment-tree
//!      root plus the settlement half authenticating that root as a chain-batch-root leaf against the
//!      imported interop root — no separate L2->L1 message proof,
//!   5. call `InteropHandler.executeAtomicBundle` per leg and assert both mints land and both source
//!      legs stay Committed / both bundles report FullyExecuted.
//!
//! Requirements:
//! - `PROTOCOL_CONTRACTS_ROOT` must point at the era-contracts atomic-interop checkout (atomic
//!   genesis contracts + relaxed gateway-mode guards), and `out/TestnetERC20Token.sol/...` must be
//!   built there (the token creation bytecode is read from it).
//! - The zksync-os-server build must serve the chain-batch-root leaf proof model (dynamic-height
//!   IMT + settlement-anchored `zks_getImt*` RPCs) and the timestamped interop-root import
//!   (`(blockNumber, root, timestamp)` tuples, struct-wrapped execute wire data).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use alloy::eips::BlockNumberOrTag;
use alloy::network::{EthereumWallet, TransactionBuilder};
use alloy::primitives::{address, keccak256, Address, Bytes, FixedBytes, B256, U256};
use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol;
use alloy::sol_types::{SolCall, SolEvent, SolValue};
use anyhow::{ensure, Context, Result};
use serde::Deserialize;
use tokio::time::sleep;

use crate::chain::Chain;

// ── Canonical L2 built-in addresses (mirror contracts/common/l2-helpers/L2ContractAddresses.sol) ──
const INTEROP_CENTER: Address = address!("000000000000000000000000000000000001000d");
const INTEROP_HANDLER: Address = address!("000000000000000000000000000000000001000e");
const ATOMIC_FLOW_MANAGER: Address = address!("0000000000000000000000000000000000010014");
const NATIVE_TOKEN_VAULT: Address = address!("0000000000000000000000000000000000010004");
const L2_BRIDGEHUB: Address = address!("0000000000000000000000000000000000010002");
const ASSET_ROUTER: Address = address!("0000000000000000000000000000000000010003");
const INTEROP_ROOT_STORAGE: Address = address!("0000000000000000000000000000000000010008");
#[allow(dead_code)]
const COMMITMENT_TREE: Address = address!("0000000000000000000000000000000000010012");

/// Standard Anvil account #0 — funded on the harness L1; `registerChain` is permissionless.
const ANVIL_KEY0: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

const TOKEN_DECIMALS: u8 = 18;
/// Buffer added to the current L1 timestamp to form the flow deadline. The deadline is now a
/// settlement-layer TIMESTAMP (each leg's batch must settle on L1 before it); 24h is far beyond the
/// few seconds a batch takes to settle on the harness Anvil.
const DEADLINE_BUFFER_SECS: u64 = 24 * 3600;
/// Gas limit for an atomic `sendBundle`. A chain's *first* IMT insert costs about 3.2M — it grows
/// the tree rather than filling it — while later ones settle around 0.6M, so the limit is set from
/// the first-insert case with room to spare. It was 3M once, which the first insert overran: an
/// out-of-gas revert carries no reason and reads like a protocol failure.
const ATOMIC_SEND_GAS: u64 = 6_000_000;
const TX_GAS: u64 = 5_000_000;

/// `ATOMIC_FLOW_PREIMAGE_VERSION` (IAtomicInterop.sol) — the only accepted preimage version.
const ATOMIC_FLOW_PREIMAGE_VERSION: FixedBytes<1> = FixedBytes([0x01]);
/// `LegState.Committed` (IAtomicInterop.sol).
const LEG_COMMITTED: u8 = 1;
/// `BundleStatus.FullyExecuted` (common/Messaging.sol).
const BUNDLE_FULLY_EXECUTED: u8 = 2;

sol! {
    // ── ERC-7786 attribute encoders (selector + args via abi_encode) ──
    interface IERC7786Attributes {
        function indirectCall(uint256 callValue);
        function atomicBundle(AtomicFlowPreimage flowPreimage, uint256 lowNullifierIndex);
    }

    // Mirrors atomic-interop/IAtomicInterop.sol: the flowId preimage. `version` must equal
    // ATOMIC_FLOW_PREIMAGE_VERSION (0x01); `legBundleHashes` strictly ascending;
    // `legSourceChainIds` positional (aligned 1:1 with the sorted hashes).
    struct AtomicFlowPreimage {
        bytes1 version;
        uint64 deadline;
        uint256 settlementLayerChainId;
        bytes32[] legBundleHashes;
        uint256[] legSourceChainIds;
    }

    // ── Bundle / proof structs (mirror common/Messaging.sol + atomic-interop/IAtomicInterop.sol) ──
    struct InteropCallStarter { bytes to; bytes data; bytes[] callAttributes; }
    struct InteropCall {
        bytes1 version;
        bool shadowAccount;
        address to;
        address from;
        uint256 value;
        bytes data;
    }
    struct BundleAttributes { bytes executionAddress; bytes unbundlerAddress; bool useFixedFee; bytes32 salt; }
    struct InteropBundle {
        bytes1 version;
        uint256 sourceChainId;
        uint256 destinationChainId;
        bytes32 destinationBaseTokenAssetId;
        bytes32 interopBundleSalt;
        InteropCall[] calls;
        BundleAttributes bundleAttributes;
    }
    struct IMTLeaf { uint256 value; uint256 nextIndex; uint256 nextValue; }
    // The server serves this COMPLETE (IMT half + settlement half); `settlementProof`
    // authenticates `chainImtRoot` as a chain-batch-root leaf against the imported interop root,
    // replacing the old L2->L1 message proof.
    struct ImtProof {
        uint256 sourceChainId;
        uint256 batchNumber;
        bytes32 chainImtRoot;
        // Timeout-branch selector (begin vs end IMT root); ignored by the finality path this test
        // exercises.
        bool provesAgainstBeginRoot;
        bytes32[] settlementProof;
        IMTLeaf leaf;
        uint256 imtLeafIndex;
        bytes32[] imtProof;
    }
    struct AtomicFlow {
        bytes32 flowId;
        AtomicFlowPreimage preimage;
    }
    struct AtomicFinalityProof {
        AtomicFlow flow;
        ImtProof[] proofs;
    }

    #[sol(rpc)]
    interface IInteropCenter {
        function sendBundle(
            bytes destinationChainId,
            InteropCallStarter[] callStarters,
            bytes[] bundleAttributes
        ) external payable returns (bytes32 bundleHash);
        function previewBundleHash(
            bytes destinationChainId,
            InteropCallStarter[] callStarters,
            bytes[] bundleAttributes
        ) external;
        error InteropPreviewHash(bytes32 bundleHash);
        function interopProtocolFee() external view returns (uint256);
        event InteropBundleSent(bytes32 l2l1MsgHash, bytes32 interopBundleHash, InteropBundle interopBundle);
    }
    #[sol(rpc)]
    interface IInteropHandler {
        function executeAtomicBundle(bytes bundle, AtomicFinalityProof finality) external;
        function bundleStatus(bytes32 bundleHash) external view returns (uint8);
    }
    #[sol(rpc)]
    interface IAtomicFlowManager {
        function legState(bytes32 flowId, bytes32 bundleHash) external view returns (uint8);
    }
    #[sol(rpc)]
    interface IL2NativeTokenVault {
        function registerToken(address token) external;
        function assetId(address token) external view returns (bytes32);
        function tokenAddress(bytes32 assetId) external view returns (address);
    }
    #[sol(rpc)]
    interface IL2Bridgehub {
        function baseTokenAssetId(uint256 chainId) external view returns (bytes32);
    }
    #[sol(rpc)]
    interface IL1Bridgehub {
        function chainRegistrationSender() external view returns (address);
    }
    #[sol(rpc)]
    interface IChainRegistrationSender {
        function registerChain(uint256 chainToBeRegistered, uint256 chainRegisteredOn) external;
    }
    // `interopRoots` returns the stored `(root, timestamp)` tuple (`StoredInteropRoot`).
    #[sol(rpc)]
    interface IL2InteropRootStorage {
        function interopRoots(uint256 chainId, uint256 blockOrBatchNumber) external view returns (bytes32 root, uint256 timestamp);
    }
    #[sol(rpc)]
    interface ITestnetERC20 {
        function mint(address to, uint256 amount) external;
        function approve(address spender, uint256 amount) external returns (bool);
        function balanceOf(address account) external view returns (uint256);
    }
}

/// Per-chain context: a wallet-bound L2 provider, the chain id, and its freshly-deployed test token.
struct ChainCtx {
    chain_id: u64,
    provider: DynProvider,
    token: Address,
}

// ── RPC response shapes (zks_* extensions) ──
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RpcImtLeaf {
    value: U256,
    next_index: U256,
    next_value: U256,
}

/// The complete `zks_getImtInclusionProof` response: IMT half plus the settlement half
/// (`settlement_proof` / `settlement_block_number`) that authenticates `chain_imt_root` against the
/// imported interop root. Mirrors the server's `ImtProof` (lib/rpc_api/src/types.rs).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RpcImtProof {
    batch_number: u64,
    settlement_block_number: Option<u64>,
    chain_imt_root: B256,
    settlement_proof: Vec<B256>,
    leaf: RpcImtLeaf,
    imt_leaf_index: u64,
    imt_proof: Vec<B256>,
}

/// `keccak256(abi.encode(uint256 chainId, address NTV, address token))` — mirrors `DataEncoding.encodeNTVAssetId`.
fn ntv_asset_id(chain_id: u64, token: Address) -> B256 {
    keccak256((U256::from(chain_id), NATIVE_TOKEN_VAULT, token).abi_encode_params())
}

/// ERC-7930 EVM chain reference without an address component.
fn encode_evm_chain(chain_id: u64) -> Bytes {
    let be = chain_id.to_be_bytes();
    let first = be.iter().position(|&b| b != 0).unwrap_or(be.len() - 1);
    let chain_ref = &be[first..];
    let mut out = vec![0x00, 0x01, 0x00, 0x00, chain_ref.len() as u8];
    out.extend_from_slice(chain_ref);
    out.push(0x00);
    Bytes::from(out)
}

/// ERC-7930 EVM address without a chain reference.
fn encode_evm_address(addr: Address) -> Bytes {
    let mut out = vec![0x00, 0x01, 0x00, 0x00, 0x00, 0x14];
    out.extend_from_slice(addr.as_slice());
    Bytes::from(out)
}

/// `secondBridgeData` for an ERC20 transfer via the L2 asset router: `0x01 ++ abi.encode(bytes32 assetId, bytes burnData)`
/// where `burnData = abi.encode(uint256 amount, address receiver, address(0))`.
fn token_transfer_data(asset_id: B256, amount: U256, recipient: Address) -> Bytes {
    let burn_data = (amount, recipient, Address::ZERO).abi_encode_params();
    let mut out = vec![0x01u8];
    out.extend_from_slice(&(asset_id, Bytes::from(burn_data)).abi_encode_params());
    Bytes::from(out)
}

/// Indirect-call ERC-7786 attribute with zero call value.
fn indirect_call_attr() -> Bytes {
    Bytes::from(
        IERC7786Attributes::indirectCallCall {
            callValue: U256::ZERO,
        }
        .abi_encode(),
    )
}

/// `atomicBundle` ERC-7786 attribute carrying the out-of-band atomic params (the flowId preimage
/// plus the IMT low-nullifier index). Deliberately NOT part of the bundle hash (see InteropCenter).
fn atomic_bundle_attr(preimage: AtomicFlowPreimage, low_nullifier_index: U256) -> Bytes {
    Bytes::from(
        IERC7786Attributes::atomicBundleCall {
            flowPreimage: preimage,
            lowNullifierIndex: low_nullifier_index,
        }
        .abi_encode(),
    )
}

/// The bridge call-starter that burns `amount` of `source`'s token and mints it to `recipient` on the destination.
fn bridge_call_starter(source: &ChainCtx, amount: U256, recipient: Address) -> InteropCallStarter {
    InteropCallStarter {
        to: encode_evm_address(ASSET_ROUTER),
        data: token_transfer_data(
            ntv_asset_id(source.chain_id, source.token),
            amount,
            recipient,
        ),
        callAttributes: vec![indirect_call_attr()],
    }
}

/// `flowId = keccak256(abi.encode(bytes32[] legBundleHashes, uint256[] legSourceChainIds, uint64 deadline, uint256 settlementLayerChainId))`.
/// `legBundleHashes` is strictly ascending; `legSourceChainIds` is POSITIONAL (aligned 1:1 with the
/// hashes, may repeat, need not be sorted).
/// `flowId = keccak256(abi.encode(preimage))` (AtomicFlowManager._validateAndComputeFlowId).
fn compute_flow_id(preimage: &AtomicFlowPreimage) -> B256 {
    keccak256(preimage.abi_encode())
}

/// `commitValue = keccak256(abi.encode(bytes4 ATOMIC_COMMIT_LEAF_TAG, bytes32 flowId, bytes32 bundleHash))` as uint256.
fn commit_value(flow_id: B256, bundle_hash: B256) -> U256 {
    let tag = FixedBytes::<4>::from_slice(&keccak256(b"AtomicInterop.commit.v1")[..4]);
    U256::from_be_bytes(keccak256((tag, flow_id, bundle_hash).abi_encode_params()).0)
}

/// Build a wallet-bound L2 provider for `signer`.
async fn l2_provider(rpc: &str, signer: PrivateKeySigner) -> Result<DynProvider> {
    Ok(ProviderBuilder::new()
        .wallet(EthereumWallet::from(signer))
        .connect(rpc)
        .await
        .context("connect L2 with wallet")?
        .erased())
}

/// Deploy a fresh TestnetERC20Token (creation bytecode from the era-contracts forge `out/`), returning its address.
async fn deploy_token(provider: &DynProvider, era_root: &Path) -> Result<Address> {
    let artifact_path =
        era_root.join("l1-contracts/out/TestnetERC20Token.sol/TestnetERC20Token.json");
    let json: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&artifact_path)
            .with_context(|| format!("read token artifact {}", artifact_path.display()))?,
    )?;
    let bytecode_hex = json["bytecode"]["object"]
        .as_str()
        .context("token artifact missing bytecode.object")?;
    let mut init =
        hex::decode(bytecode_hex.trim_start_matches("0x")).context("decode token bytecode")?;
    init.extend_from_slice(
        &(
            "AtomicTest".to_string(),
            "ATT".to_string(),
            U256::from(TOKEN_DECIMALS),
        )
            .abi_encode_params(),
    );
    let tx = TransactionRequest::default()
        .with_deploy_code(init)
        .with_gas_limit(TX_GAS);
    let receipt = provider.send_transaction(tx).await?.get_receipt().await?;
    ensure!(receipt.status(), "token deploy reverted");
    receipt
        .contract_address
        .context("no contract address in deploy receipt")
}

/// Deploy + mint + register-with-NTV + approve a test token for `signer` on the chain at `rpc`.
async fn setup_token(
    rpc: &str,
    chain_id: u64,
    signer: PrivateKeySigner,
    mint: U256,
) -> Result<ChainCtx> {
    let provider = l2_provider(rpc, signer.clone()).await?;
    let user = signer.address();
    let token = deploy_token(&provider, &era_root()).await?;

    let erc20 = ITestnetERC20::new(token, &provider);
    let r = erc20
        .mint(user, mint)
        .gas(TX_GAS)
        .send()
        .await?
        .get_receipt()
        .await?;
    ensure!(r.status(), "mint reverted");

    let ntv = IL2NativeTokenVault::new(NATIVE_TOKEN_VAULT, &provider);
    if ntv.assetId(token).call().await? == B256::ZERO {
        let r = ntv
            .registerToken(token)
            .gas(TX_GAS)
            .send()
            .await?
            .get_receipt()
            .await?;
        ensure!(r.status(), "registerToken reverted");
    }
    let r = erc20
        .approve(NATIVE_TOKEN_VAULT, mint)
        .gas(TX_GAS)
        .send()
        .await?
        .get_receipt()
        .await?;
    ensure!(r.status(), "approve reverted");
    println!("[atomic-swap] chain {chain_id}: token {token} deployed/registered/approved");
    Ok(ChainCtx {
        chain_id,
        provider,
        token,
    })
}

/// Register the two chains with each other for interop (sets `baseTokenAssetId` cross-chain), polling until set.
async fn register_chains_for_interop(
    l1_rpc: &str,
    bridgehub: Address,
    a: &ChainCtx,
    b: &ChainCtx,
) -> Result<()> {
    let l1_signer: PrivateKeySigner = ANVIL_KEY0.parse()?;
    let l1 = ProviderBuilder::new()
        .wallet(EthereumWallet::from(l1_signer))
        .connect(l1_rpc)
        .await?
        .erased();
    let sender_addr = IL1Bridgehub::new(bridgehub, &l1)
        .chainRegistrationSender()
        .call()
        .await?;
    let sender = IChainRegistrationSender::new(sender_addr, &l1);
    println!("[atomic-swap] chainRegistrationSender = {sender_addr}");

    // A must learn B (registerChain(B, A)) and vice versa.
    for (to_register, registered_on) in [(b, a), (a, b)] {
        let l2bh = IL2Bridgehub::new(L2_BRIDGEHUB, &registered_on.provider);
        if l2bh
            .baseTokenAssetId(U256::from(to_register.chain_id))
            .call()
            .await?
            != B256::ZERO
        {
            continue;
        }
        let r = sender
            .registerChain(
                U256::from(to_register.chain_id),
                U256::from(registered_on.chain_id),
            )
            .gas(TX_GAS)
            .send()
            .await?
            .get_receipt()
            .await?;
        ensure!(r.status(), "registerChain reverted");
        // Poll until the L2 service tx lands and sets the base-token asset id.
        let start = Instant::now();
        loop {
            if l2bh
                .baseTokenAssetId(U256::from(to_register.chain_id))
                .call()
                .await?
                != B256::ZERO
            {
                break;
            }
            ensure!(
                start.elapsed() < Duration::from_secs(120),
                "chain {} never learned chain {}'s base token",
                registered_on.chain_id,
                to_register.chain_id
            );
            sleep(Duration::from_secs(1)).await;
        }
        println!(
            "[atomic-swap] chain {} now knows {}",
            registered_on.chain_id, to_register.chain_id
        );
    }
    Ok(())
}

/// Predict a leg's bundleHash via the `previewBundleHash` quoter: it runs `sendBundle`'s exact
/// stateful assembly and ALWAYS reverts with `InteropPreviewHash(bundleHash)` (rolling back the
/// indirect legs' value burn), so the hash comes out of the revert data of a static call. The
/// prediction needs no msg.value and no atomic attribute — the real attribute cannot be built yet
/// anyway, since its preimage contains this bundle's own hash.
async fn predict_bundle_hash(
    source: &ChainCtx,
    dest: &ChainCtx,
    amount: U256,
    recipient: Address,
) -> Result<B256> {
    let ic = IInteropCenter::new(INTEROP_CENTER, &source.provider);
    let err = ic
        .previewBundleHash(
            encode_evm_chain(dest.chain_id),
            vec![bridge_call_starter(source, amount, recipient)],
            vec![],
        )
        .gas(ATOMIC_SEND_GAS)
        .call()
        .await
        .err()
        .context("previewBundleHash returned instead of reverting")?;
    match err.as_decoded_error::<IInteropCenter::InteropPreviewHash>() {
        Some(preview) => Ok(preview.bundleHash),
        None => Err(anyhow::anyhow!(
            "unexpected revert from previewBundleHash: {err:#}"
        )),
    }
}

/// Atomic-send one leg (burn + IMT insert). Returns `(bundleData, bundleHash, txHash, sendBlock)`.
#[allow(clippy::too_many_arguments)]
async fn send_atomic_leg(
    source: &ChainCtx,
    dest: &ChainCtx,
    amount: U256,
    recipient: Address,
    flow_id: B256,
    preimage: &AtomicFlowPreimage,
    predicted_hash: B256,
    fee: U256,
) -> Result<(Bytes, B256, B256, u64)> {
    let value = commit_value(flow_id, predicted_hash);
    // Low-nullifier (predecessor) index from the server's Rust IMT engine against the pre-insert tree.
    let block = source.provider.get_block_number().await?;
    let low_null: Option<u64> = source
        .provider
        .raw_request("zks_getImtLowNullifierIndex".into(), (value, block))
        .await
        .context("zks_getImtLowNullifierIndex")?;
    let low_null = low_null.context("no low-nullifier leaf for commit value")?;

    let ic = IInteropCenter::new(INTEROP_CENTER, &source.provider);
    let send = ic
        .sendBundle(
            encode_evm_chain(dest.chain_id),
            vec![bridge_call_starter(source, amount, recipient)],
            vec![atomic_bundle_attr(preimage.clone(), U256::from(low_null))],
        )
        .value(fee)
        .gas(ATOMIC_SEND_GAS);
    let receipt = send.clone().send().await?.get_receipt().await?;
    if !receipt.status() {
        // A mined-but-reverted transaction carries no reason. Replay it as a call against the
        // block it landed in to get one, instead of reporting only that it failed.
        let reason = send
            .block(receipt.block_number.unwrap_or_default().into())
            .call()
            .await
            .err()
            .map(|e| format!("{e:#}"))
            // The same call succeeding against the same state means the transaction did not fail
            // on its inputs — running out of gas is what is left.
            .unwrap_or_else(|| {
                format!(
                    "no revert reason, and the same call succeeds against that state — so it ran \
                     out of the {ATOMIC_SEND_GAS} gas it was given (used {})",
                    receipt.gas_used
                )
            });
        anyhow::bail!(
            "atomic sendBundle reverted on chain {} (tx {:#x}): {reason}",
            source.chain_id,
            receipt.transaction_hash
        );
    }
    let tx_hash = receipt.transaction_hash;

    // Extract the InteropBundleSent event: bundleHash + the bundle struct (re-encoded as bundleData).
    let mut found: Option<(B256, Bytes)> = None;
    for log in receipt.inner.logs() {
        if log.address() != INTEROP_CENTER {
            continue;
        }
        if let Ok(decoded) = IInteropCenter::InteropBundleSent::decode_log(&log.inner) {
            let data = decoded.data;
            found = Some((
                data.interopBundleHash,
                Bytes::from(data.interopBundle.abi_encode()),
            ));
            break;
        }
    }
    let (bundle_hash, bundle_data) = found.context("InteropBundleSent event not found")?;
    ensure!(
        bundle_hash == predicted_hash,
        "predicted bundleHash {predicted_hash} != emitted {bundle_hash}"
    );
    let send_block = source
        .provider
        .get_transaction_receipt(tx_hash)
        .await?
        .context("send receipt")?
        .block_number
        .context("send block number")?;
    Ok((bundle_data, bundle_hash, tx_hash, send_block))
}

/// Poll `zks_getImtInclusionProof(value, send_block)` until the batch containing the send executes on
/// L1 and the complete proof is available. Returns the on-chain `ImtProof` plus the settlement-layer
/// block its interop-root anchor resolved at (needed to await interop-root import on the executors).
///
/// The server errors with `BatchNotAvailableYet` ("...has not been finalized...") until the batch
/// executes; a `null` result means the commit value is genuinely absent from the batch (fail fast).
async fn wait_for_inclusion_proof(
    source: &ChainCtx,
    value: U256,
    send_block: u64,
) -> Result<(ImtProof, Option<u64>)> {
    let start = Instant::now();
    loop {
        let res: Result<Option<RpcImtProof>, _> = source
            .provider
            .raw_request("zks_getImtInclusionProof".into(), (value, send_block))
            .await;
        match res {
            Ok(Some(imt)) => {
                let proof = ImtProof {
                    sourceChainId: U256::from(source.chain_id),
                    batchNumber: U256::from(imt.batch_number),
                    chainImtRoot: imt.chain_imt_root,
                    provesAgainstBeginRoot: false,
                    settlementProof: imt.settlement_proof,
                    leaf: IMTLeaf {
                        value: imt.leaf.value,
                        nextIndex: imt.leaf.next_index,
                        nextValue: imt.leaf.next_value,
                    },
                    imtLeafIndex: U256::from(imt.imt_leaf_index),
                    imtProof: imt.imt_proof,
                };
                return Ok((proof, imt.settlement_block_number));
            }
            Ok(None) => {
                anyhow::bail!("commit value {value} not present in IMT (server returned null)")
            }
            Err(e) => {
                let msg = e.to_string();
                // Retry only while the batch has not settled/executed on L1 yet.
                if !msg.contains("not been finalized") && !msg.contains("not available") {
                    return Err(anyhow::Error::from(e).context("zks_getImtInclusionProof"));
                }
                ensure!(
                    start.elapsed() < Duration::from_secs(300),
                    "timed out waiting for IMT inclusion proof of {value}"
                );
                sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

/// Poll a chain's L2InteropRootStorage until it imports the interop root for `(l1_chain_id, sl_block)`.
async fn wait_for_interop_root(
    provider: &DynProvider,
    l1_chain_id: u64,
    sl_block: u64,
) -> Result<()> {
    let storage = IL2InteropRootStorage::new(INTEROP_ROOT_STORAGE, provider);
    let start = Instant::now();
    loop {
        if storage
            .interopRoots(U256::from(l1_chain_id), U256::from(sl_block))
            .call()
            .await?
            .root
            != B256::ZERO
        {
            return Ok(());
        }
        ensure!(
            start.elapsed() < Duration::from_secs(180),
            "chain never imported interop root (L1 {l1_chain_id}, block {sl_block})"
        );
        sleep(Duration::from_secs(1)).await;
    }
}

/// era-contracts root the harness built forge artifacts in. Uses the same resolver the deployer uses
/// (`PROTOCOL_CONTRACTS_ROOT` if set, else the cargo git checkout) so it works in CI without the env
/// var — the `ecosystem` fixture runs `build-contracts` against this same root, so `out/` exists here.
fn era_root() -> PathBuf {
    protocol_ops::common::paths::contracts_root()
}

/// Run the swap between two interop-capable chains of one ecosystem.
///
/// Both chains must have the atomic-interop built-ins and a funded wallet #0; everything else the
/// scenario needs (tokens, interop registration) it sets up itself.
pub async fn run(ca: &Chain, cb: &Chain) -> Result<()> {
    // The rich wallet (#0) is funded on both chains; use it as the depositor/recipient.
    let signer = ca.wallet(0).clone();
    let user = signer.address();

    let a_amount = U256::from(10u64).pow(U256::from(TOKEN_DECIMALS)) * U256::from(10u64);
    let b_amount = U256::from(10u64).pow(U256::from(TOKEN_DECIMALS)) * U256::from(7u64);
    let mint = U256::from(10u64).pow(U256::from(TOKEN_DECIMALS)) * U256::from(1_000_000u64);

    println!(
        "[atomic-swap] setting up tokens on chains {} / {}",
        ca.chain_id(),
        cb.chain_id()
    );
    let a = setup_token(ca.l2_rpc_url(), ca.chain_id(), signer.clone(), mint).await?;
    let b = setup_token(cb.l2_rpc_url(), cb.chain_id(), signer.clone(), mint).await?;

    let fee = IInteropCenter::new(INTEROP_CENTER, &a.provider)
        .interopProtocolFee()
        .call()
        .await?;
    // The fee is passed as `msg.value` on every `sendBundle` below, so a non-zero one would have
    // to be funded per leg. Nothing sets it on a freshly deployed chain, and the upgrade does not
    // introduce one either — assert that rather than silently paying whatever it returns.
    ensure!(
        fee.is_zero(),
        "expected a zero interop protocol fee, got {fee}"
    );

    println!("[atomic-swap] registering chains for interop...");
    register_chains_for_interop(ca.l1_rpc_url(), ca.bridgehub_addr(), &a, &b).await?;

    // ── L1 (settlement layer) context: chain id + deadline (an L1 timestamp) ──
    // These chains settle directly on L1 (no gateway), so the settlement layer is L1 itself.
    let l1 = ProviderBuilder::new()
        .connect(ca.l1_rpc_url())
        .await?
        .erased();
    let l1_chain_id = l1.get_chain_id().await?;
    let l1_now = l1
        .get_block_by_number(BlockNumberOrTag::Latest)
        .await?
        .context("L1 latest block")?
        .header
        .timestamp;
    let deadline = l1_now + DEADLINE_BUFFER_SECS;

    // ── Predict bundle hashes -> flowId ──
    let h_ab = predict_bundle_hash(&a, &b, a_amount, user).await?;
    let h_ba = predict_bundle_hash(&b, &a, b_amount, user).await?;
    // legBundleHashes ascending; legSourceChainIds POSITIONAL (aligned 1:1 with the sorted hashes).
    let mut legs = [
        (h_ab, U256::from(a.chain_id)),
        (h_ba, U256::from(b.chain_id)),
    ];
    legs.sort_by_key(|x| x.0);
    let leg_hashes_asc: Vec<B256> = legs.iter().map(|(h, _)| *h).collect();
    let leg_source_chain_ids: Vec<U256> = legs.iter().map(|(_, c)| *c).collect();
    let preimage = AtomicFlowPreimage {
        version: ATOMIC_FLOW_PREIMAGE_VERSION,
        deadline,
        settlementLayerChainId: U256::from(l1_chain_id),
        legBundleHashes: leg_hashes_asc.clone(),
        legSourceChainIds: leg_source_chain_ids.clone(),
    };
    let flow_id = compute_flow_id(&preimage);
    println!("[atomic-swap] flowId={flow_id} deadline={deadline} slChainId={l1_chain_id}");

    // ── PHASE 1: atomic send both legs ──
    let a_token = ITestnetERC20::new(a.token, &a.provider);
    let b_token = ITestnetERC20::new(b.token, &b.provider);
    let a_before = a_token.balanceOf(user).call().await?;
    let b_before = b_token.balanceOf(user).call().await?;

    let (ab_data, _ab_hash, _ab_tx, ab_block) =
        send_atomic_leg(&a, &b, a_amount, user, flow_id, &preimage, h_ab, fee).await?;
    let (ba_data, _ba_hash, _ba_tx, ba_block) =
        send_atomic_leg(&b, &a, b_amount, user, flow_id, &preimage, h_ba, fee).await?;

    let mgr_a = IAtomicFlowManager::new(ATOMIC_FLOW_MANAGER, &a.provider);
    let mgr_b = IAtomicFlowManager::new(ATOMIC_FLOW_MANAGER, &b.provider);
    ensure!(
        mgr_a.legState(flow_id, h_ab).call().await? == LEG_COMMITTED,
        "AB committed on A"
    );
    ensure!(
        mgr_b.legState(flow_id, h_ba).call().await? == LEG_COMMITTED,
        "BA committed on B"
    );
    ensure!(
        a_token.balanceOf(user).call().await? == a_before - a_amount,
        "A burned aAmount"
    );
    ensure!(
        b_token.balanceOf(user).call().await? == b_before - b_amount,
        "B burned bAmount"
    );
    println!("[atomic-swap] PHASE 1 ok: both legs committed (burn + IMT insert)");

    // ── PHASE 2: wait for L1 settlement, fetch the complete IMT inclusion proofs ──
    // Each proof arrives whole from `zks_getImtInclusionProof` (IMT half + settlement half); the
    // server blocks it until the send batch executes on L1, so this doubles as the settlement wait.
    println!("[atomic-swap] waiting for send batches to execute on L1 + fetching IMT proofs...");
    let ab_value = commit_value(flow_id, h_ab);
    let ba_value = commit_value(flow_id, h_ba);
    let (ab_proof, ab_sl_block) = wait_for_inclusion_proof(&a, ab_value, ab_block).await?;
    let (ba_proof, ba_sl_block) = wait_for_inclusion_proof(&b, ba_value, ba_block).await?;
    println!(
        "[atomic-swap] AB proof: batch={} slBlock={:?}; BA proof: batch={} slBlock={:?}",
        ab_proof.batchNumber, ab_sl_block, ba_proof.batchNumber, ba_sl_block
    );

    // Proofs ordered to match legBundleHashes ascending.
    let proofs_asc = if h_ab < h_ba {
        vec![ab_proof, ba_proof]
    } else {
        vec![ba_proof, ab_proof]
    };
    let finality = AtomicFinalityProof {
        flow: AtomicFlow {
            flowId: flow_id,
            preimage: preimage.clone(),
        },
        proofs: proofs_asc,
    };

    // Both executeAtomicBundle calls verify every leg, so each executing chain must have imported the
    // L1 interop root at each leg's settlement block.
    let sl_blocks: Vec<u64> = [ab_sl_block, ba_sl_block].into_iter().flatten().collect();
    println!("[atomic-swap] waiting for interop roots (L1 {l1_chain_id}) at blocks {sl_blocks:?} on both chains...");
    for ctx in [&a, &b] {
        for &sl in &sl_blocks {
            wait_for_interop_root(&ctx.provider, l1_chain_id, sl).await?;
        }
    }
    println!("[atomic-swap] interop roots imported on both chains");

    // ── PHASE 3: executeAtomicBundle on each destination ──
    println!("[atomic-swap] executing AB on B and BA on A...");
    let handler_b = IInteropHandler::new(INTEROP_HANDLER, &b.provider);
    let handler_a = IInteropHandler::new(INTEROP_HANDLER, &a.provider);
    let r = handler_b
        .executeAtomicBundle(ab_data, finality.clone())
        .gas(TX_GAS)
        .send()
        .await?
        .get_receipt()
        .await?;
    ensure!(r.status(), "executeAtomicBundle AB on B succeeds");
    let r = handler_a
        .executeAtomicBundle(ba_data, finality)
        .gas(TX_GAS)
        .send()
        .await?
        .get_receipt()
        .await?;
    ensure!(r.status(), "executeAtomicBundle BA on A succeeds");
    ensure!(
        handler_b.bundleStatus(h_ab).call().await? == BUNDLE_FULLY_EXECUTED,
        "AB bundle FullyExecuted on B"
    );
    ensure!(
        handler_a.bundleStatus(h_ba).call().await? == BUNDLE_FULLY_EXECUTED,
        "BA bundle FullyExecuted on A"
    );

    // ── Destination mint assertions ──
    let ntv_b = IL2NativeTokenVault::new(NATIVE_TOKEN_VAULT, &b.provider);
    let shim_a_on_b = ntv_b
        .tokenAddress(ntv_asset_id(a.chain_id, a.token))
        .call()
        .await?;
    ensure!(shim_a_on_b != Address::ZERO, "shim for A's token on B");
    ensure!(
        ITestnetERC20::new(shim_a_on_b, &b.provider)
            .balanceOf(user)
            .call()
            .await?
            == a_amount,
        "recipient on B got aAmount"
    );
    let ntv_a = IL2NativeTokenVault::new(NATIVE_TOKEN_VAULT, &a.provider);
    let shim_b_on_a = ntv_a
        .tokenAddress(ntv_asset_id(b.chain_id, b.token))
        .call()
        .await?;
    ensure!(shim_b_on_a != Address::ZERO, "shim for B's token on A");
    ensure!(
        ITestnetERC20::new(shim_b_on_a, &a.provider)
            .balanceOf(user)
            .call()
            .await?
            == b_amount,
        "recipient on A got bAmount"
    );

    println!("[atomic-swap] SUCCESS: atomic swap completed end-to-end on two L1-settling chains");
    Ok(())
}
