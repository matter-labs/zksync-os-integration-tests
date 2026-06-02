use std::time::{Duration, Instant};

use anyhow::{Context, Result};

/// Poll `url` with `eth_chainId` until it responds successfully or `timeout` elapses.
pub async fn wait_for_rpc_ready(url: &str, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1,
        "method": "eth_chainId", "params": []
    });
    loop {
        if let Ok(resp) = client.post(url).json(&body).send().await {
            if resp.status().is_success() {
                return Ok(());
            }
        }
        if Instant::now() >= deadline {
            anyhow::bail!("RPC at {url} did not become ready within {timeout:?}");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Poll L2 `eth_getBlockByNumber("finalized")` until the finalized block number
/// reaches at least `target_block`, or `timeout` elapses.
///
/// The "finalized" tag on a ZKsync OS L2 node advances when the corresponding
/// L1 batch execution transaction is confirmed on L1. Use this after submitting
/// L2 transactions to wait for full finality.
pub async fn wait_for_l2_block_finalized(
    l2_rpc: &str,
    target_block: u64,
    timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        match get_l2_finalized_block(l2_rpc).await {
            Ok(finalized) if finalized >= target_block => return Ok(()),
            _ => {}
        }
        if Instant::now() >= deadline {
            anyhow::bail!("L2 finalized block did not reach {target_block} within {timeout:?}");
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Return the current L2 finalized block number via `eth_getBlockByNumber("finalized")`.
pub async fn get_l2_finalized_block(l2_rpc: &str) -> Result<u64> {
    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1,
        "method": "eth_getBlockByNumber",
        "params": ["finalized", false]
    });
    let resp = client
        .post(l2_rpc)
        .json(&body)
        .send()
        .await
        .context("eth_getBlockByNumber(finalized)")?;
    let json: serde_json::Value = resp.json().await?;
    let hex = json["result"]["number"]
        .as_str()
        .context("finalized block has no number field")?;
    let n = u64::from_str_radix(hex.trim_start_matches("0x"), 16)
        .context("parse finalized block number")?;
    Ok(n)
}
