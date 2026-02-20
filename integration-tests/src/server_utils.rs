use std::process::Command;
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

pub const DEFAULT_TEST_PRIVATE_KEY: &str =
    "0x7726827caac94a7f9e1b160f7ea819f172f7b6f9d2a97f992c38edeab82d4110";

/// Poll `eth_chainId` via `cast chain-id` until the RPC endpoint is reachable.
pub fn wait_for_chain_to_be_ready(
    rpc_url: &str,
    service_name: &str,
    max_attempts: usize,
    retry_delay: Duration,
) -> Result<()> {
    let mut last_error = String::new();
    for attempt in 1..=max_attempts {
        match Command::new("cast")
            .args(["chain-id", "--rpc-url", rpc_url])
            .output()
        {
            Ok(response) => {
                if response.status.success() {
                    let chain_id = String::from_utf8_lossy(&response.stdout).trim().to_string();
                    println!(
                        "{} ready at {} (chainId: {}) on attempt {}/{}",
                        service_name, rpc_url, chain_id, attempt, max_attempts
                    );
                    return Ok(());
                }
                last_error = String::from_utf8_lossy(&response.stderr).trim().to_string();
            }
            Err(err) => {
                last_error = err.to_string();
            }
        }

        if attempt < max_attempts {
            sleep(retry_delay);
        }
    }

    anyhow::bail!(
        "{} RPC at {} did not become reachable after {} attempts. Last error: {}",
        service_name,
        rpc_url,
        max_attempts,
        last_error
    );
}

/// Send L2 transactions every 3 seconds and poll L1 until at least `min_batches`
/// are executed on the chain contract.
pub fn wait_for_executed_batches_with_traffic(
    l2_rpc_url: &str,
    l1_rpc_url: &str,
    diamond_proxy_addr: &str,
    sender_private_key: &str,
    min_batches: u64,
    timeout: Duration,
) -> Result<u64> {
    let start = Instant::now();
    let mut tx_count = 0u64;
    let mut next_progress_at = start + Duration::from_secs(5);

    loop {
        let executed = get_total_batches_executed(l1_rpc_url, diamond_proxy_addr)
            .context("Failed to read getTotalBatchesExecuted from L1")?;

        let now = Instant::now();
        if now >= next_progress_at {
            println!(
                "Progress: executed_l1_batches={}, sent_txs={}",
                executed, tx_count
            );
            next_progress_at = now + Duration::from_secs(5);
        }

        if executed >= min_batches {
            println!(
                "Reached executed L1 batches target: {} (sent {} txs)",
                executed, tx_count
            );
            return Ok(executed);
        }

        if start.elapsed() >= timeout {
            anyhow::bail!(
                "Timed out waiting for executed L1 batches. target={}, current={}, sent_txs={}",
                min_batches,
                executed,
                tx_count
            );
        }

        send_traffic_tx(l2_rpc_url, sender_private_key)
            .with_context(|| format!("Failed to send traffic tx #{}", tx_count + 1))?;
        tx_count += 1;
        sleep(Duration::from_secs(3));
    }
}

fn send_traffic_tx(l2_rpc_url: &str, sender_private_key: &str) -> Result<()> {
    let output = Command::new("cast")
        .args([
            "send",
            "0x0000000000000000000000000000000000000001",
            "--value",
            "1",
            "--private-key",
            sender_private_key,
            "--rpc-url",
            l2_rpc_url,
        ])
        .output()
        .context("Failed to execute cast send for server traffic")?;

    if !output.status.success() {
        anyhow::bail!(
            "cast send failed:\nSTDOUT:\n{}\nSTDERR:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

fn get_total_batches_executed(l1_rpc_url: &str, diamond_proxy_addr: &str) -> Result<u64> {
    let output = Command::new("cast")
        .args([
            "call",
            diamond_proxy_addr,
            "getTotalBatchesExecuted()(uint256)",
            "--rpc-url",
            l1_rpc_url,
        ])
        .output()
        .context("Failed to execute cast call for getTotalBatchesExecuted")?;

    if !output.status.success() {
        anyhow::bail!(
            "cast call failed:\nSTDOUT:\n{}\nSTDERR:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
    parse_u64_value(&raw)
        .with_context(|| format!("Unable to parse getTotalBatchesExecuted output: '{}'", raw))
}

fn parse_u64_value(raw: &str) -> Result<u64> {
    if let Some(hex) = raw.strip_prefix("0x") {
        return u64::from_str_radix(hex, 16).context("Invalid hex value");
    }
    raw.parse::<u64>().context("Invalid decimal value")
}
