//! End-to-end atomic-interop swap between two L1-settling chains (no gateway in the path), driven
//! natively in Rust.
//!
//! The `ecosystem` fixture with `#[with(vec![6565, 6566])]` brings up two L1-settling ZKsync OS
//! chains on one Anvil L1, each with its own in-process server. This test then drives the full
//! bundle-model atomic swap between them entirely in Rust (no TS driver):
//!
//!   1. deploy + register a TestnetERC20 on each chain (mint to the user, approve the NTV),
//!   2. register the two chains with each other for interop (permissionless `registerChain`),
//!   3. atomic-send both legs (burn + IMT insert), with the send-time low-nullifier index supplied
//!      by the server's Rust IMT engine (`zks_getImtLowNullifierIndex`),
//!   4. wait for each leg's commitment-tree root to settle on L1 and fetch the real message proof
//!      (`zks_getL2ToL1LogProof`, the L1 aggregation-hop proof) plus the IMT inclusion proof
//!      (`zks_getImtInclusionProof`, built + self-verified by the Rust engine),
//!   5. call `InteropHandler.executeAtomicBundle` per leg and assert both mints land and both source
//!      legs stay Committed / both bundles report FullyExecuted.
//!
//! Requirements:
//! - `PROTOCOL_CONTRACTS_ROOT` must point at the era-contracts `atomic-imt-interop` checkout (atomic
//!   genesis contracts + relaxed gateway-mode guards), and `out/TestnetERC20Token.sol/...` must be
//!   built there (the token creation bytecode is read from it).
//! - The zksync-os-server local build must be the `kl/l1-settled-interop-proof` branch (L1
//!   aggregation-hop proof + the `zks_getImt*` RPCs).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use alloy::network::{EthereumWallet, TransactionBuilder};
use alloy::primitives::{address, keccak256, Address, Bytes, FixedBytes, B256, U256};
use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol;
use alloy::sol_types::{SolCall, SolEvent, SolValue};
use anyhow::{ensure, Context, Result};
use rstest::rstest;
use serde::Deserialize;
use tokio::time::sleep;

use tests::fixtures::ecosystem;
use tests::Ecosystem;

// ── Canonical L2 built-in addresses (mirror contracts/common/l2-helpers/L2ContractAddresses.sol) ──
const INTEROP_CENTER: Address = address!("000000000000000000000000000000000001000d");
const INTEROP_HANDLER: Address = address!("000000000000000000000000000000000001000e");
const ATOMIC_FLOW_MANAGER: Address = address!("0000000000000000000000000000000000010014");
const NATIVE_TOKEN_VAULT: Address = address!("0000000000000000000000000000000000010004");
const COMMITMENT_TREE: Address = address!("0000000000000000000000000000000000010012");
const L2_BRIDGEHUB: Address = address!("0000000000000000000000000000000000010002");
const ASSET_ROUTER: Address = address!("0000000000000000000000000000000000010003");
const INTEROP_ROOT_STORAGE: Address = address!("0000000000000000000000000000000000010008");

/// Standard Anvil account #0 — funded on the harness L1; `registerChain` is permissionless.
const ANVIL_KEY0: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

const TOKEN_DECIMALS: u8 = 18;
/// The flow deadline, as an L1 settlement-layer **timestamp** (unix seconds). The atomic-interop
/// l1-timestamp feature binds each settled batch's `block.timestamp` into its leaf and rejects a leg
/// whose batch settled after the deadline (`ProofDeadlineExceeded`), so this must sit comfortably
/// above the batches' real settlement timestamps — far in the future here (year ~2286).
const DEADLINE: u64 = 10_000_000_000;
const ATOMIC_SEND_GAS: u64 = 3_000_000;
const TX_GAS: u64 = 5_000_000;

/// `LegState.Committed` (IAtomicInterop.sol).
const LEG_COMMITTED: u8 = 1;
/// `BundleStatus.FullyExecuted` (common/Messaging.sol).
const BUNDLE_FULLY_EXECUTED: u8 = 2;

sol! {
    // ── ERC-7786 attribute encoders (selector + args via abi_encode) ──
    interface IERC7786Attributes {
        function indirectCall(uint256 callValue);
        function atomicBundle(bytes32 flowId, uint64 deadline, uint256 lowNullifierIndex);
        function interopBundleSalt(bytes32 salt);
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
    struct ImtInclusionProof {
        uint256 sourceChainId;
        uint256 batchNumber;
        bytes32 chainImtRoot;
        uint16 messageTxNumberInBatch;
        uint256 messageIndex;
        bytes32[] messageProof;
        IMTLeaf leaf;
        uint256 imtLeafIndex;
        bytes32[] imtProof;
    }
    struct AtomicFlow {
        bytes32 flowId;
        uint64 deadline;
        uint256 settlementLayerChainId;
        bytes32[] legBundleHashes;
        uint256[] legSourceChainIds;
    }
    struct AtomicFinalityProof {
        AtomicFlow flow;
        ImtInclusionProof[] proofs;
    }

    #[sol(rpc)]
    interface IInteropCenter {
        function sendBundle(
            bytes destinationChainId,
            InteropCallStarter[] callStarters,
            bytes[] bundleAttributes
        ) external payable returns (bytes32 bundleHash);
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
    #[sol(rpc)]
    interface IL2InteropRootStorage {
        function interopRoots(uint256 chainId, uint256 blockOrBatchNumber) external view returns (bytes32);
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
struct RawLogProof {
    batch_number: Option<u64>,
    id: u64,
    proof: Vec<B256>,
    gateway_block_number: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RpcImtLeaf {
    value: U256,
    next_index: U256,
    next_value: U256,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RpcImtProof {
    chain_imt_root: B256,
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

/// `atomicBundle` ERC-7786 attribute carrying the out-of-band atomic params.
fn atomic_bundle_attr(flow_id: B256, deadline: u64, low_nullifier_index: U256) -> Bytes {
    Bytes::from(
        IERC7786Attributes::atomicBundleCall {
            flowId: flow_id,
            deadline,
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

/// `flowId = keccak256(abi.encode(bytes32[] legBundleHashes, uint256[] legSourceChainIds, uint64 deadline, uint256 settlementLayerChainId))` (both arrays ascending).
fn compute_flow_id(
    leg_hashes_asc: &[B256],
    chain_ids_asc: &[U256],
    deadline: u64,
    settlement_layer_chain_id: U256,
) -> B256 {
    keccak256(
        (
            leg_hashes_asc.to_vec(),
            chain_ids_asc.to_vec(),
            deadline,
            settlement_layer_chain_id,
        )
            .abi_encode_params(),
    )
}

/// `interopBundleSalt` ERC-7786 attribute. The InteropCenter folds `keccak256(msg.sender, salt)` into the
/// bundle hash and enforces each `(sender, salt)` pair is used at most once, so every leg needs a distinct salt.
fn interop_bundle_salt_attr(salt: B256) -> Bytes {
    Bytes::from(IERC7786Attributes::interopBundleSaltCall { salt }.abi_encode())
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

/// Predict a leg's bundleHash via an atomic `sendBundle` callStatic (bundleHash is independent of the atomic params).
async fn predict_bundle_hash(
    source: &ChainCtx,
    dest: &ChainCtx,
    amount: U256,
    recipient: Address,
    fee: U256,
    salt: B256,
) -> Result<B256> {
    let ic = IInteropCenter::new(INTEROP_CENTER, &source.provider);
    ic.sendBundle(
        encode_evm_chain(dest.chain_id),
        vec![bridge_call_starter(source, amount, recipient)],
        // Same salt as `send_atomic_leg` so the predicted hash matches the emitted one.
        vec![
            atomic_bundle_attr(B256::ZERO, DEADLINE, U256::ZERO),
            interop_bundle_salt_attr(salt),
        ],
    )
    .value(fee)
    .gas(ATOMIC_SEND_GAS)
    .call()
    .await
    .context("predict sendBundle callStatic")
}

/// Atomic-send one leg (burn + IMT insert). Returns `(bundleData, bundleHash, txHash, sendBlock)`.
#[allow(clippy::too_many_arguments)]
async fn send_atomic_leg(
    source: &ChainCtx,
    dest: &ChainCtx,
    amount: U256,
    recipient: Address,
    flow_id: B256,
    predicted_hash: B256,
    fee: U256,
    salt: B256,
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
    let receipt = ic
        .sendBundle(
            encode_evm_chain(dest.chain_id),
            vec![bridge_call_starter(source, amount, recipient)],
            vec![
                atomic_bundle_attr(flow_id, DEADLINE, U256::from(low_null)),
                interop_bundle_salt_attr(salt),
            ],
        )
        .value(fee)
        .gas(ATOMIC_SEND_GAS)
        .send()
        .await?
        .get_receipt()
        .await?;
    ensure!(receipt.status(), "atomic sendBundle reverted");
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

/// Index of the commitment-tree (0x10012) L2->L1 message within the tx (mirrors the TS scan; 0 fallback).
async fn commitment_tree_message_index(provider: &DynProvider, tx_hash: B256) -> Result<u64> {
    let receipt: serde_json::Value = provider
        .raw_request("eth_getTransactionReceipt".into(), (tx_hash,))
        .await
        .context("eth_getTransactionReceipt (raw)")?;
    let tree = format!("{:#x}", COMMITMENT_TREE);
    if let Some(logs) = receipt["l2ToL1Logs"].as_array() {
        for (idx, l) in logs.iter().enumerate() {
            if l["sender"].as_str().unwrap_or("").to_lowercase() == tree {
                return Ok(idx as u64);
            }
        }
    }
    Ok(0)
}

/// Poll `zks_getL2ToL1LogProof` (messageRoot target) until the commitment-tree publish in `tx_hash` settles.
async fn wait_for_message_proof(
    provider: &DynProvider,
    tx_hash: B256,
    msg_index: u64,
) -> Result<RawLogProof> {
    let start = Instant::now();
    loop {
        let res: Option<RawLogProof> = provider
            .raw_request(
                "zks_getL2ToL1LogProof".into(),
                (tx_hash, msg_index, "messageRoot"),
            )
            .await
            .context("zks_getL2ToL1LogProof")?;
        if let Some(p) = res {
            return Ok(p);
        }
        ensure!(
            start.elapsed() < Duration::from_secs(300),
            "timed out waiting for message proof of {tx_hash}"
        );
        sleep(Duration::from_secs(1)).await;
    }
}

/// Build a leg's `ImtInclusionProof`: IMT half from the Rust engine RPC, message half from the real proof.
async fn build_inclusion_proof(
    source: &ChainCtx,
    value: U256,
    send_block: u64,
    raw: &RawLogProof,
) -> Result<ImtInclusionProof> {
    let imt: Option<RpcImtProof> = source
        .provider
        .raw_request("zks_getImtInclusionProof".into(), (value, send_block))
        .await
        .context("zks_getImtInclusionProof")?;
    let imt = imt.context("commit value not present in IMT (server returned null)")?;
    Ok(ImtInclusionProof {
        sourceChainId: U256::from(source.chain_id),
        batchNumber: U256::from(raw.batch_number.unwrap_or(0)),
        chainImtRoot: imt.chain_imt_root,
        messageTxNumberInBatch: 0,
        messageIndex: U256::from(raw.id),
        messageProof: raw.proof.clone(),
        leaf: IMTLeaf {
            value: imt.leaf.value,
            nextIndex: imt.leaf.next_index,
            nextValue: imt.leaf.next_value,
        },
        imtLeafIndex: U256::from(imt.imt_leaf_index),
        imtProof: imt.imt_proof,
    })
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

#[rstest]
#[tokio::test(flavor = "multi_thread")]
async fn atomic_swap_l1_settled(
    #[future]
    #[with(vec![6565, 6566])]
    ecosystem: Ecosystem,
) -> Result<()> {
    let eco = ecosystem.await;
    let chains: Vec<_> = eco.chains().collect();
    let (ca, cb) = (chains[0], chains[1]);
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

    println!("[atomic-swap] registering chains for interop...");
    register_chains_for_interop(ca.l1_rpc_url(), ca.bridgehub_addr(), &a, &b).await?;

    // ── Predict bundle hashes -> flowId ──
    // For L1-settling chains the atomic flow's settlement layer is L1 itself; its chain id is now part
    // of the flowId preimage, so resolve it before computing the flowId.
    let l1 = ProviderBuilder::new()
        .connect(ca.l1_rpc_url())
        .await?
        .erased();
    let l1_chain_id = l1.get_chain_id().await?;
    // Each leg needs a distinct interop-bundle salt: the InteropCenter folds `keccak256(sender, salt)`
    // into the bundle hash and rejects a reused (sender, salt) pair. Legs settle on different chains, so
    // fixed per-leg salts are unique enough for the fresh test chains.
    let salt_ab = keccak256(b"atomic-swap-leg-ab");
    let salt_ba = keccak256(b"atomic-swap-leg-ba");
    let h_ab = predict_bundle_hash(&a, &b, a_amount, user, fee, salt_ab).await?;
    let h_ba = predict_bundle_hash(&b, &a, b_amount, user, fee, salt_ba).await?;
    let mut leg_hashes_asc = [h_ab, h_ba];
    leg_hashes_asc.sort();
    let mut chain_ids_asc = [U256::from(a.chain_id), U256::from(b.chain_id)];
    chain_ids_asc.sort();
    let flow_id = compute_flow_id(
        &leg_hashes_asc,
        &chain_ids_asc,
        DEADLINE,
        U256::from(l1_chain_id),
    );
    println!(
        "[atomic-swap] flowId={flow_id} deadline={DEADLINE} settlementLayer(L1)={l1_chain_id}"
    );

    // ── PHASE 1: atomic send both legs ──
    let a_token = ITestnetERC20::new(a.token, &a.provider);
    let b_token = ITestnetERC20::new(b.token, &b.provider);
    let a_before = a_token.balanceOf(user).call().await?;
    let b_before = b_token.balanceOf(user).call().await?;

    let (ab_data, _ab_hash, ab_tx, ab_block) =
        send_atomic_leg(&a, &b, a_amount, user, flow_id, h_ab, fee, salt_ab).await?;
    let (ba_data, _ba_hash, ba_tx, ba_block) =
        send_atomic_leg(&b, &a, b_amount, user, flow_id, h_ba, fee, salt_ba).await?;

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

    // ── PHASE 2: wait for L1 settlement, fetch real proofs, build inclusion proofs ──
    let ab_msg_idx = commitment_tree_message_index(&a.provider, ab_tx).await?;
    let ba_msg_idx = commitment_tree_message_index(&b.provider, ba_tx).await?;
    println!("[atomic-swap] waiting for commitment-tree roots to settle on L1...");
    let ab_raw = wait_for_message_proof(&a.provider, ab_tx, ab_msg_idx).await?;
    let ba_raw = wait_for_message_proof(&b.provider, ba_tx, ba_msg_idx).await?;
    println!(
        "[atomic-swap] AB proof: batch={:?} slBlock={:?}; BA proof: batch={:?} slBlock={:?}",
        ab_raw.batch_number,
        ab_raw.gateway_block_number,
        ba_raw.batch_number,
        ba_raw.gateway_block_number
    );

    let ab_value = commit_value(flow_id, h_ab);
    let ba_value = commit_value(flow_id, h_ba);
    let ab_proof = build_inclusion_proof(&a, ab_value, ab_block, &ab_raw).await?;
    let ba_proof = build_inclusion_proof(&b, ba_value, ba_block, &ba_raw).await?;
    // Proofs ordered to match legBundleHashes ascending.
    let proofs_asc = if h_ab < h_ba {
        vec![ab_proof, ba_proof]
    } else {
        vec![ba_proof, ab_proof]
    };
    let finality = AtomicFinalityProof {
        flow: AtomicFlow {
            flowId: flow_id,
            deadline: DEADLINE,
            settlementLayerChainId: U256::from(l1_chain_id),
            legBundleHashes: leg_hashes_asc.to_vec(),
            legSourceChainIds: chain_ids_asc.to_vec(),
        },
        proofs: proofs_asc,
    };

    // Both executeAtomicBundle calls verify every leg, so each executing chain must have imported the
    // L1 interop root at each leg's settlement block. (`l1`/`l1_chain_id` resolved above.)
    let sl_blocks: Vec<u64> = [ab_raw.gateway_block_number, ba_raw.gateway_block_number]
        .into_iter()
        .flatten()
        .collect();
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
