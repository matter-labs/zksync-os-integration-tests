use std::borrow::Cow;
use std::time::{Duration, Instant};

use alloy::eips::BlockNumberOrTag;
use alloy::primitives::U64;
use alloy::providers::{Provider, ProviderBuilder};
use anyhow::{Context, Result};

/// Poll `url` with `eth_chainId` until it responds successfully or `timeout` elapses.
pub async fn wait_for_rpc_ready(url: &str, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let provider = ProviderBuilder::new().connect(url).await?;
    loop {
        if provider
            .raw_request::<_, U64>(Cow::Borrowed("eth_chainId"), ())
            .await
            .is_ok()
        {
            return Ok(());
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
/// Prints progress every 10 s so long-running waits are observable.
pub async fn wait_for_l2_block_finalized(
    l2_rpc: &str,
    target_block: u64,
    timeout: Duration,
) -> Result<()> {
    let provider = ProviderBuilder::new()
        .connect(l2_rpc)
        .await
        .context("connect to L2")?;
    let start = Instant::now();
    let deadline = start + timeout;
    let mut last_log = Instant::now();

    loop {
        if Instant::now() >= deadline {
            let suffix = match get_finalized_block(&provider).await {
                Ok(current) => format!("current={current}"),
                Err(e) => format!("last RPC error: {e}"),
            };
            anyhow::bail!(
                "L2 finalized block did not reach {target_block} within {timeout:?} ({suffix})"
            );
        }
        match get_finalized_block(&provider).await {
            Ok(finalized) => {
                if finalized >= target_block {
                    return Ok(());
                }
                if last_log.elapsed() >= Duration::from_secs(10) {
                    eprintln!(
                        "[wait_for_l2_block_finalized] finalized={finalized} target={target_block} elapsed={:.0}s",
                        start.elapsed().as_secs_f64()
                    );
                    last_log = Instant::now();
                }
            }
            Err(e) => {
                if last_log.elapsed() >= Duration::from_secs(10) {
                    eprintln!(
                        "[wait_for_l2_block_finalized] RPC error: {e} (elapsed={:.0}s)",
                        start.elapsed().as_secs_f64()
                    );
                    last_log = Instant::now();
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Poll L2 `eth_getBlockByNumber("latest")` until the latest block number
/// reaches at least `target_block`, or `timeout` elapses.
///
/// Prints progress every 10 s so long-running waits are observable.
pub async fn wait_for_l2_block_produced(
    l2_rpc: &str,
    target_block: u64,
    timeout: Duration,
) -> Result<()> {
    let provider = ProviderBuilder::new()
        .connect(l2_rpc)
        .await
        .context("connect to L2")?;
    let start = Instant::now();
    let deadline = start + timeout;
    let mut last_log = Instant::now();

    loop {
        if Instant::now() >= deadline {
            let suffix = match provider
                .get_block_by_number(BlockNumberOrTag::Latest)
                .await
                .context("eth_getBlockByNumber(latest)")
                .and_then(|b| b.context("latest block not found"))
            {
                Ok(block) => format!("current={}", block.header.number),
                Err(e) => format!("last RPC error: {e}"),
            };
            anyhow::bail!(
                "L2 latest block did not reach {target_block} within {timeout:?} ({suffix})"
            );
        }
        match provider
            .get_block_by_number(BlockNumberOrTag::Latest)
            .await
            .context("eth_getBlockByNumber(latest)")
            .and_then(|b| b.context("latest block not found"))
        {
            Ok(block) => {
                if block.header.number >= target_block {
                    return Ok(());
                }
                if last_log.elapsed() >= Duration::from_secs(10) {
                    eprintln!(
                        "[wait_for_l2_block_produced] latest={} target={target_block} elapsed={:.0}s",
                        block.header.number,
                        start.elapsed().as_secs_f64()
                    );
                    last_log = Instant::now();
                }
            }
            Err(e) => {
                if last_log.elapsed() >= Duration::from_secs(10) {
                    eprintln!(
                        "[wait_for_l2_block_produced] RPC error: {e} (elapsed={:.0}s)",
                        start.elapsed().as_secs_f64()
                    );
                    last_log = Instant::now();
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn get_finalized_block(provider: &impl Provider) -> Result<u64> {
    let block = provider
        .get_block_by_number(BlockNumberOrTag::Finalized)
        .await
        .context("eth_getBlockByNumber(finalized)")?
        .context("finalized block not found")?;
    Ok(block.header.number)
}
