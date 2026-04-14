use alloy::network::EthereumWallet;
use alloy::primitives::{address, Address, Bytes, FixedBytes, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::signers::local::LocalSigner;
use alloy::sol;
use anyhow::{Context, Result};
use integration_tests::anvil::Anvil;
use integration_tests::l1_state::{chain_config_path, load_ecosystem, resolve_l1_state};
use integration_tests::presets::load_current_preset;
use std::str::FromStr;
use std::time::Duration;

use integration_tests::anvil::DEFAULT_ANVIL_PRIVATE_KEY;

// ---------------------------------------------------------------------------
// System contract addresses
// ---------------------------------------------------------------------------

const L1_MESSENGER_ADDRESS: Address = address!("0000000000000000000000000000000000008008");
const L2_MESSAGE_VERIFICATION_ADDRESS: Address =
    address!("0000000000000000000000000000000000010009");
const L2_INTEROP_ROOT_STORAGE_ADDRESS: Address =
    address!("0000000000000000000000000000000000010008");

// ---------------------------------------------------------------------------
// Contract interfaces
// ---------------------------------------------------------------------------

sol! {
    #[sol(rpc)]
    contract IL1Messenger {
        function sendToL1(bytes calldata _message) external returns (bytes32);
    }

    #[sol(rpc)]
    contract IMessageVerification {
        struct L2Message {
            uint16 txNumberInBatch;
            address sender;
            bytes data;
        }

        function proveL2MessageInclusionShared(
            uint256 _chainId,
            uint256 _blockOrBatchNumber,
            uint256 _index,
            L2Message calldata _message,
            bytes32[] calldata _proof
        ) external view returns (bool);
    }

    #[sol(rpc)]
    contract IL2InteropRootStorage {
        function interopRoots(uint256 chainId, uint256 batchNumber) external view returns (bytes32);
    }
}

// ---------------------------------------------------------------------------
// ZKsync-specific RPC types (subset of zksync_os_rpc_api::types)
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct L2ToL1LogProof {
    batch_number: u64,
    proof: Vec<FixedBytes<32>>,
    id: u32,
    #[allow(dead_code)]
    root: FixedBytes<32>,
    gateway_block_number: Option<u64>,
}

/// Call `zks_getL2ToL1LogProof` with MessageRoot target variant.
async fn get_message_root_proof(
    rpc_url: &str,
    tx_hash: FixedBytes<32>,
) -> Result<Option<L2ToL1LogProof>> {
    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "zks_getL2ToL1LogProof",
        "params": [tx_hash, 0, "messageRoot"]
    });
    let resp = client
        .post(rpc_url)
        .json(&body)
        .send()
        .await
        .context("zks_getL2ToL1LogProof request")?;
    let json: serde_json::Value = resp.json().await?;
    if let Some(err) = json.get("error") {
        anyhow::bail!("RPC error: {}", err);
    }
    let result = json
        .get("result")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    if result.is_null() {
        return Ok(None);
    }
    Ok(Some(serde_json::from_value(result)?))
}

/// Poll until the log proof is available (block finalized + proof generated).
async fn wait_for_message_proof(
    rpc_url: &str,
    tx_hash: FixedBytes<32>,
    timeout: Duration,
) -> Result<L2ToL1LogProof> {
    let start = tokio::time::Instant::now();
    loop {
        if start.elapsed() > timeout {
            anyhow::bail!("Timed out waiting for message proof");
        }
        if let Some(proof) = get_message_root_proof(rpc_url, tx_hash).await? {
            return Ok(proof);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Poll `L2InteropRootStorage.interopRoots(chainId, batchNumber)` on a chain
/// until the root is non-zero.
async fn wait_for_interop_root(
    rpc_url: &str,
    gw_chain_id: u64,
    gw_batch_number: u64,
    timeout: Duration,
) -> Result<FixedBytes<32>> {
    let provider = ProviderBuilder::new()
        .on_builtin(rpc_url)
        .await
        .context("connect to L2 for interop root polling")?;
    let root_storage = IL2InteropRootStorage::new(L2_INTEROP_ROOT_STORAGE_ADDRESS, &provider);

    let start = tokio::time::Instant::now();
    loop {
        if start.elapsed() > timeout {
            anyhow::bail!(
                "Timed out waiting for interop root (gwChain={}, gwBatch={})",
                gw_chain_id,
                gw_batch_number
            );
        }
        let root = root_storage
            .interopRoots(U256::from(gw_chain_id), U256::from(gw_batch_number))
            .call()
            .await
            .context("interopRoots call")?
            ._0;
        if !root.is_zero() {
            println!(
                "  Interop root found: gwChain={}, gwBatch={}, root={}",
                gw_chain_id, gw_batch_number, root
            );
            return Ok(root);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

// ---------------------------------------------------------------------------
// Traffic helper
// ---------------------------------------------------------------------------

/// Send a trivial L2 transaction to generate traffic.
/// Uses `cast` directly instead of EraBackend because this runs in a spawned
/// task and always targets a local RPC (no Docker remapping needed).
fn send_l2_traffic(l2_rpc_url: &str, private_key: &str) -> Result<()> {
    let output = std::process::Command::new("cast")
        .args([
            "send",
            "0x0000000000000000000000000000000000000001",
            "--value",
            "1",
            "--private-key",
            private_key,
            "--rpc-url",
            l2_rpc_url,
        ])
        .output()
        .context("cast send for traffic")?;
    if !output.status.success() {
        anyhow::bail!(
            "traffic tx failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Test
// ---------------------------------------------------------------------------

async fn run_interop_message_test() -> Result<()> {
    let preset = load_current_preset()?;
    let eco = load_ecosystem(&preset)?;
    anyhow::ensure!(
        eco.gateway_settling_chains.len() >= 2,
        "Interop test needs at least 2 gateway-settling chains, got {}",
        eco.gateway_settling_chains.len()
    );
    let gw = &eco.gateway;
    let chain_a = &eco.gateway_settling_chains[0];
    let chain_b = &eco.gateway_settling_chains[1];

    println!("Gateway:  chain {} ({})", gw.chain_id, gw.diamond_proxy);
    println!(
        "Chain A:  chain {} ({})",
        chain_a.chain_id, chain_a.diamond_proxy
    );
    println!(
        "Chain B:  chain {} ({})",
        chain_b.chain_id, chain_b.diamond_proxy
    );

    // Resolve config paths
    let gw_config = chain_config_path(&preset, &gw.name)?;
    let chain_a_config = chain_config_path(&preset, &chain_a.name)?;
    let chain_b_config = chain_config_path(&preset, &chain_b.name)?;
    for (name, path) in [
        ("Gateway", &gw_config),
        ("Chain A", &chain_a_config),
        ("Chain B", &chain_b_config),
    ] {
        anyhow::ensure!(
            path.exists(),
            "{} config not found: {}",
            name,
            path.display()
        );
    }

    println!("\n=== Loading l1-state.json into Anvil ===");
    let state_path = resolve_l1_state(&preset, &eco)?;
    let anvil = Anvil::spawn_with_state(&state_path).await?;
    let l1_rpc_url = anvil.rpc_url().to_string();
    println!("Anvil ready at {l1_rpc_url}");

    // ---- Start gateway ----
    println!("\n=== Starting gateway server (chain {}) ===", gw.chain_id);
    let gw_server = integration_tests::server::ServerBuilder::new(preset.clone(), &gw.name)
        .ephemeral()
        .config_path(&gw_config)
        .spawn(&anvil)
        .map_err(|e| anyhow::anyhow!("Failed to start gateway: {:?}", e))?;
    let gw_l2_rpc = gw_server.rpc_url();
    println!("Gateway ready at {gw_l2_rpc}");

    // ---- Start chain A (fresh, gateway_rpc_url set via env var) ----
    println!("\n=== Starting chain A (chain {}) ===", chain_a.chain_id);
    let chain_a_server =
        integration_tests::server::ServerBuilder::new(preset.clone(), &chain_a.name)
            .gateway_rpc_url(&gw_l2_rpc)
            .spawn(&anvil)
            .map_err(|e| anyhow::anyhow!("Failed to start chain A server: {:?}", e))?;
    let chain_a_l2_rpc = chain_a_server.rpc_url();
    println!("Chain A ready at {chain_a_l2_rpc}");

    // ---- Start chain B (fresh, gateway_rpc_url set via env var) ----
    println!("\n=== Starting chain B (chain {}) ===", chain_b.chain_id);
    let chain_b_server = integration_tests::server::ServerBuilder::new(preset, &chain_b.name)
        .gateway_rpc_url(&gw_l2_rpc)
        .spawn(&anvil)
        .map_err(|e| anyhow::anyhow!("Failed to start chain B server: {:?}", e))?;
    let chain_b_l2_rpc = chain_b_server.rpc_url();
    println!("Chain B ready at {chain_b_l2_rpc}");

    // ---- Send L2→L1 message on chain A ----
    let test_address =
        integration_tests::server_utils::address_from_private_key(DEFAULT_ANVIL_PRIVATE_KEY)?;
    println!("\n=== Sending L2→L1 message on chain A ===");
    let wallet = EthereumWallet::new(
        LocalSigner::from_str(DEFAULT_ANVIL_PRIVATE_KEY).context("parse test private key")?,
    );
    let chain_a_provider = ProviderBuilder::new()
        .with_recommended_fillers()
        .wallet(wallet)
        .on_builtin(&chain_a_l2_rpc)
        .await
        .context("connect chain A provider")?;

    let messenger = IL1Messenger::new(L1_MESSENGER_ADDRESS, &chain_a_provider);
    let message_data = Bytes::from(b"hello interop".to_vec());

    let receipt = tokio::time::timeout(Duration::from_secs(120), async {
        messenger
            .sendToL1(message_data.clone())
            .send()
            .await
            .context("send L2→L1 message")?
            .get_receipt()
            .await
            .context("get L2→L1 message receipt")
    })
    .await
    .context("L2→L1 message timed out after 120s")??;

    anyhow::ensure!(receipt.status(), "L2→L1 message transaction reverted");
    let block_number = receipt.block_number.context("missing block_number")?;
    let tx_hash = receipt.transaction_hash;
    let tx_index = receipt.transaction_index.context("missing tx_index")?;
    println!(
        "  Message sent: tx={}, block={}, txIndex={}",
        tx_hash, block_number, tx_index
    );

    // ---- Get message proof (MessageRoot variant) ----
    // Drive L2 traffic on chain A so batches are produced and settled on the gateway.
    println!("\n=== Waiting for message proof (MessageRoot) — sending traffic ===");
    let chain_a_rpc_for_traffic = chain_a_l2_rpc.clone();
    let traffic_handle = tokio::spawn(async move {
        loop {
            let _ = send_l2_traffic(&chain_a_rpc_for_traffic, DEFAULT_ANVIL_PRIVATE_KEY);
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    });
    let log_proof =
        wait_for_message_proof(&chain_a_l2_rpc, tx_hash, Duration::from_secs(300)).await?;
    traffic_handle.abort();
    let gw_block_number = log_proof
        .gateway_block_number
        .context("MessageRoot proof must have gateway_block_number")?;
    println!(
        "  Proof: batch={}, id={}, gwBlock={}, proof_len={}",
        log_proof.batch_number,
        log_proof.id,
        gw_block_number,
        log_proof.proof.len()
    );

    // ---- Wait for interop root on chain B ----
    println!("\n=== Waiting for interop root on chain B ===");
    wait_for_interop_root(
        &chain_b_l2_rpc,
        gw.chain_id,
        gw_block_number,
        Duration::from_secs(300),
    )
    .await?;

    // ---- Verify message inclusion on chain B ----
    println!("\n=== Verifying message inclusion on chain B ===");
    let chain_b_provider = ProviderBuilder::new()
        .on_builtin(&chain_b_l2_rpc)
        .await
        .context("connect chain B provider")?;

    let verifier = IMessageVerification::new(L2_MESSAGE_VERIFICATION_ADDRESS, &chain_b_provider);

    let chain_a_id = chain_a_provider.get_chain_id().await?;
    let included = verifier
        .proveL2MessageInclusionShared(
            U256::from(chain_a_id),
            U256::from(log_proof.batch_number),
            U256::from(log_proof.id),
            IMessageVerification::L2Message {
                txNumberInBatch: tx_index as u16,
                sender: Address::from_str(&test_address)?,
                data: message_data,
            },
            log_proof.proof,
        )
        .call()
        .await
        .context("proveL2MessageInclusionShared call")?
        ._0;

    anyhow::ensure!(included, "Message was NOT included in the interop proof");
    println!("\n=== Message verified on chain B ===");

    // ---- Verify chain B produces executed batches ----
    println!("\n=== Waiting for chain B progress ===");
    chain_b_server
        .wait_for_executed_batches_with_traffic()
        .context("chain B progress")?;

    // ---- Cleanup ----
    let _ = chain_b_server.kill();
    let _ = chain_a_server.kill();
    let _ = gw_server.kill();
    anvil.kill()?;

    println!("\nTest passed!");
    Ok(())
}

#[tokio::test]
async fn test_interop_message_verification() {
    run_interop_message_test()
        .await
        .expect("interop message test failed");
}
