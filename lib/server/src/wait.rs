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
pub async fn wait_for_l2_block_finalized(
    l2_rpc: &str,
    target_block: u64,
    timeout: Duration,
) -> Result<()> {
    let provider = ProviderBuilder::new()
        .connect(l2_rpc)
        .await
        .context("connect to L2")?;
    poll_block_until(
        &provider,
        BlockNumberOrTag::Finalized,
        "finalized",
        target_block,
        timeout,
    )
    .await
}

/// Poll L2 `eth_getBlockByNumber("latest")` until the latest block number
/// reaches at least `target_block`, or `timeout` elapses.
pub async fn wait_for_l2_block_produced(
    l2_rpc: &str,
    target_block: u64,
    timeout: Duration,
) -> Result<()> {
    let provider = ProviderBuilder::new()
        .connect(l2_rpc)
        .await
        .context("connect to L2")?;
    poll_block_until(
        &provider,
        BlockNumberOrTag::Latest,
        "latest",
        target_block,
        timeout,
    )
    .await
}

async fn poll_block_until(
    provider: &impl Provider,
    tag: BlockNumberOrTag,
    label: &str,
    target: u64,
    timeout: Duration,
) -> Result<()> {
    let start = Instant::now();
    let deadline = start + timeout;
    let mut last_log = Instant::now();

    loop {
        if Instant::now() >= deadline {
            let suffix = match get_block_number(provider, tag).await {
                Ok(current) => format!("current={current}"),
                Err(e) => format!("last RPC error: {e}"),
            };
            anyhow::bail!("L2 {label} block did not reach {target} within {timeout:?} ({suffix})");
        }
        match get_block_number(provider, tag).await {
            Ok(current) => {
                if current >= target {
                    return Ok(());
                }
                if last_log.elapsed() >= Duration::from_secs(10) {
                    tracing::info!(
                        "{label} current={current} target={target} elapsed={:.0}s",
                        start.elapsed().as_secs_f64()
                    );
                    last_log = Instant::now();
                }
            }
            Err(e) => {
                if last_log.elapsed() >= Duration::from_secs(10) {
                    tracing::warn!(
                        "{label} RPC error: {e} elapsed={:.0}s",
                        start.elapsed().as_secs_f64()
                    );
                    last_log = Instant::now();
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn get_block_number(provider: &impl Provider, tag: BlockNumberOrTag) -> Result<u64> {
    let block = provider
        .get_block_by_number(tag)
        .await
        .with_context(|| format!("eth_getBlockByNumber({tag:?})"))?
        .with_context(|| format!("{tag:?} block not found"))?;
    Ok(block.header.number)
}
