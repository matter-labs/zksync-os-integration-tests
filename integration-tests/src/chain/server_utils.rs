use std::fs;
use std::path::Path;
use std::process::Command;
use std::thread::sleep;
use std::time::Duration;

use std::borrow::Cow;

use anyhow::{Context, Result};

const ANSI_RESET: &str = "\x1b[0m";
const ANSI_PURPLE: &str = "\x1b[35m";
const ANSI_RED: &str = "\x1b[31m";
const ANSI_YELLOW: &str = "\x1b[33m";
const ANSI_BLUE: &str = "\x1b[34m";

/// Poll `eth_chainId` via `cast chain-id` until the RPC endpoint is reachable.
pub fn wait_for_chain_to_be_ready(
    rpc_url: &str,
    service_name: &str,
    max_attempts: usize,
    retry_delay: Duration,
    server_logs_path: Option<&Path>,
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

    if rpc_failure_looks_like_server_down(&last_error) {
        if let Some(log_path) = server_logs_path {
            if let Err(err) = print_stacktrace_context(log_path, 100) {
                eprintln!(
                    "Failed to extract stacktrace context from server logs '{}': {}",
                    log_path.display(),
                    err
                );
            }
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

fn rpc_failure_looks_like_server_down(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    [
        "connection refused",
        "failed to connect",
        "connection reset",
        "timed out",
        "connection closed",
        "transport error",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn print_stacktrace_context(log_path: &Path, context_lines_before: usize) -> Result<()> {
    let raw_logs = fs::read_to_string(log_path)
        .with_context(|| format!("Failed to read server log file '{}'", log_path.display()))?;

    let sanitized_logs = strip_ansi_escape_sequences(&raw_logs);
    if sanitized_logs != raw_logs {
        fs::write(log_path, &sanitized_logs).with_context(|| {
            format!(
                "Failed to write ANSI-cleaned server log file '{}'",
                log_path.display()
            )
        })?;
    }

    let lines: Vec<&str> = sanitized_logs.lines().collect();
    if lines.is_empty() {
        return Ok(());
    }

    let stacktrace_start = lines.iter().rposition(|line| {
        let lower = line.to_ascii_lowercase();
        lower.contains("stack backtrace:")
            || lower.contains("stacktrace:")
            || lower.contains("stack trace:")
    });

    let start = stacktrace_start
        .map(|idx| idx.saturating_sub(context_lines_before))
        .unwrap_or_else(|| lines.len().saturating_sub(200));
    let stacktrace_start_in_excerpt = stacktrace_start.filter(|idx| *idx >= start);

    eprintln!(
        "\n{ANSI_PURPLE}===== Server log excerpt: {} (from line {}) ====={ANSI_RESET}",
        log_path.display(),
        start + 1
    );
    for (offset, line) in lines[start..].iter().enumerate() {
        let absolute_idx = start + offset;
        let formatted_line: Cow<'_, str> = if stacktrace_start_in_excerpt
            .map(|stack_idx| absolute_idx >= stack_idx)
            .unwrap_or(false)
        {
            Cow::Owned(format!("{ANSI_RED}{line}{ANSI_RESET}"))
        } else {
            colorize_log_level(line)
        };
        eprintln!("{formatted_line}");
    }
    eprintln!("{ANSI_PURPLE}===== End server log excerpt ====={ANSI_RESET}\n");

    Ok(())
}

fn colorize_log_level(line: &str) -> Cow<'_, str> {
    if line.contains(" ERROR ") {
        return Cow::Owned(format!("{ANSI_RED}{line}{ANSI_RESET}"));
    }
    if line.contains(" WARN ") {
        return Cow::Owned(format!("{ANSI_YELLOW}{line}{ANSI_RESET}"));
    }
    if line.contains(" DEBUG ") {
        return Cow::Owned(format!("{ANSI_BLUE}{line}{ANSI_RESET}"));
    }
    Cow::Borrowed(line)
}

pub fn strip_ansi_escape_codes_in_file(log_path: &Path) -> Result<()> {
    let raw = fs::read_to_string(log_path)
        .with_context(|| format!("Failed to read log file '{}'", log_path.display()))?;
    let sanitized = strip_ansi_escape_sequences(&raw);
    if sanitized != raw {
        fs::write(log_path, sanitized)
            .with_context(|| format!("Failed to write log file '{}'", log_path.display()))?;
    }
    Ok(())
}

pub fn strip_ansi_escape_sequences(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut it = input.chars().peekable();

    while let Some(ch) = it.next() {
        if ch != '\u{1b}' {
            out.push(ch);
            continue;
        }

        match it.peek().copied() {
            Some('[') => {
                it.next(); // consume '['
                           // Consume CSI sequence until a final byte (usually an ASCII letter).
                for c in it.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            Some(']') => {
                it.next(); // consume ']'
                           // Consume OSC sequence until BEL or ST (ESC \)
                loop {
                    match it.next() {
                        Some('\u{0007}') | None => break,
                        Some('\u{1b}') => {
                            if matches!(it.peek(), Some('\\')) {
                                it.next();
                                break;
                            }
                        }
                        Some(_) => {}
                    }
                }
            }
            _ => {
                // Unknown escape sequence. Drop ESC itself.
            }
        }
    }

    out
}

/// Send a tiny self-driven L2 transaction (1 wei to `0x...01`) to nudge the
/// server's batch builder. Internal implementation for
/// [`crate::server::Server::send_traffic_tx`].
pub(crate) fn send_traffic_tx(l2_rpc_url: &str, sender_private_key: &str) -> Result<()> {
    send_traffic_tx_returning_hash(l2_rpc_url, sender_private_key).map(|_| ())
}

/// Same as [`send_traffic_tx`] but returns the tx hash. Used by callers that
/// need to track the L2 → L1 lifecycle for a *specific* tx (e.g. waiting for
/// its containing batch to be executed on L1) instead of just nudging the
/// server.
pub(crate) fn send_traffic_tx_returning_hash(
    l2_rpc_url: &str,
    sender_private_key: &str,
) -> Result<String> {
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
            "--json",
        ])
        .output()
        .context("Failed to execute cast send for server traffic")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        if rpc_failure_looks_like_server_down(&stderr) {
            if let Some(log_path) = find_latest_server_log_path().ok().flatten() {
                let _ = print_stacktrace_context(&log_path, 100);
            }
        }
        anyhow::bail!(
            "cast send failed:\nSTDOUT:\n{}\nSTDERR:\n{}",
            String::from_utf8_lossy(&output.stdout),
            stderr
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let receipt: serde_json::Value = serde_json::from_str(stdout.trim())
        .with_context(|| format!("parse `cast send --json` output: {stdout}"))?;
    let hash = receipt
        .get("transactionHash")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("`cast send --json` output missing transactionHash"))?
        .to_string();
    Ok(hash)
}

/// Poll an L2 RPC for `tx_hash`'s receipt and return its `blockNumber`.
pub(crate) fn wait_for_l2_tx_block_number(
    l2_rpc_url: &str,
    tx_hash: &str,
    timeout: std::time::Duration,
) -> Result<u64> {
    use std::time::Instant;
    let start = Instant::now();
    loop {
        let output = Command::new("cast")
            .args(["receipt", tx_hash, "--rpc-url", l2_rpc_url, "--json"])
            .output()
            .context("spawn `cast receipt`")?;
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let parsed: serde_json::Value = serde_json::from_str(stdout.trim())
                .with_context(|| format!("parse `cast receipt --json`: {stdout}"))?;
            if let Some(block_str) = parsed.get("blockNumber").and_then(|v| v.as_str()) {
                if !block_str.is_empty() && block_str != "null" {
                    let trimmed = block_str.trim_start_matches("0x");
                    return u64::from_str_radix(trimmed, 16)
                        .with_context(|| format!("parse blockNumber {block_str:?}"));
                }
            }
        }
        if start.elapsed() >= timeout {
            anyhow::bail!(
                "L2 tx {tx_hash} receipt not available within {:.1}s",
                timeout.as_secs_f64(),
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// Read the current `finalized` block number from an L2 RPC.
///
/// On a zksync-os L2 RPC, `finalized` tracks the highest L2 block whose
/// containing batch has been executed on the settlement layer. So this
/// advances only as the commit → prove → execute pipeline finishes.
pub(crate) fn get_l2_finalized_block_number(l2_rpc_url: &str) -> Result<u64> {
    let output = Command::new("cast")
        .args([
            "block",
            "finalized",
            "--field",
            "number",
            "--rpc-url",
            l2_rpc_url,
        ])
        .output()
        .context("spawn `cast block finalized`")?;
    if !output.status.success() {
        anyhow::bail!(
            "cast block finalized failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let s = stdout.trim();
    if let Some(hex) = s.strip_prefix("0x") {
        u64::from_str_radix(hex, 16).with_context(|| format!("parse finalized block hex {s:?}"))
    } else {
        s.parse::<u64>()
            .with_context(|| format!("parse finalized block dec {s:?}"))
    }
}

fn find_latest_server_log_path() -> Result<Option<std::path::PathBuf>> {
    // Scan the per-preset logs root (`test-run-logs/<preset>/`). If
    // `PRESET_NAME` isn't set or the dir doesn't exist yet, there's nothing to
    // scan — return `None` so callers fall through cleanly.
    let logs_root = match crate::server::preset_logs_root() {
        Ok(p) if p.exists() => p,
        _ => return Ok(None),
    };

    let mut best: Option<(u32, std::path::PathBuf)> = None;
    for entry in fs::read_dir(&logs_root)
        .with_context(|| format!("Failed to read '{}'", logs_root.display()))?
    {
        let entry = entry?;
        let run_dir = entry.path();
        if !run_dir.is_dir() {
            continue;
        }
        let dir_name = run_dir.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if dir_name == "previous_runs" {
            continue;
        }

        for log_entry in fs::read_dir(&run_dir)
            .with_context(|| format!("Failed to read run dir '{}'", run_dir.display()))?
        {
            let log_entry = log_entry?;
            let path = log_entry.path();
            let log_name = match path.file_name().and_then(|v| v.to_str()) {
                Some(v) => v,
                None => continue,
            };
            if !log_name.starts_with("server_run") || !log_name.ends_with(".json") {
                continue;
            }
            let run_part = log_name
                .strip_prefix("server_run")
                .and_then(|v| v.split_once('_').map(|(idx, _)| idx));
            let run_idx = match run_part.and_then(|v| v.parse::<u32>().ok()) {
                Some(v) => v,
                None => continue,
            };
            match &best {
                Some((best_idx, _)) if *best_idx >= run_idx => {}
                _ => best = Some((run_idx, path)),
            }
        }
    }

    Ok(best.map(|(_, path)| path))
}

pub(crate) fn get_total_batches_executed(
    l1_rpc_url: &str,
    diamond_proxy_addr: &str,
) -> Result<u64> {
    cast_u64_call(
        l1_rpc_url,
        diamond_proxy_addr,
        "getTotalBatchesExecuted()(uint256)",
    )
    .context("Failed to read getTotalBatchesExecuted")
}

fn get_total_batches_committed(l1_rpc_url: &str, diamond_proxy_addr: &str) -> Result<u64> {
    cast_u64_call(
        l1_rpc_url,
        diamond_proxy_addr,
        "getTotalBatchesCommitted()(uint256)",
    )
    .context("Failed to read getTotalBatchesCommitted")
}

fn cast_u64_call(rpc_url: &str, target: &str, sig: &str) -> Result<u64> {
    cast_u64_call_at(rpc_url, target, sig, None)
}

fn cast_u64_call_at(
    rpc_url: &str,
    target: &str,
    sig: &str,
    block_tag: Option<&str>,
) -> Result<u64> {
    let mut args: Vec<&str> = vec!["call", target, sig, "--rpc-url", rpc_url];
    if let Some(tag) = block_tag {
        args.extend_from_slice(&["--block", tag]);
    }
    let output = Command::new("cast")
        .args(&args)
        .output()
        .with_context(|| format!("spawn `cast call {target} {sig}`"))?;
    if !output.status.success() {
        anyhow::bail!(
            "cast call {sig} failed:\nSTDOUT:\n{}\nSTDERR:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
    parse_u64_value(&raw).with_context(|| format!("parse `{sig}` output '{raw}'"))
}

/// Poll the chain's diamond proxy on its current settlement layer until
/// `totalBatchesCommitted == totalBatchesExecuted` and that state remains
/// stable for `stable_for`.
///
/// The stability window guards against racing ahead of a transient
/// equality — e.g. right after a `notifyServerMigrationToGateway` event,
/// the server still has one final batch (containing the `SetSLChainId`
/// system tx) to seal. Naïve `committed == executed` can pass at a lull
/// before that batch hits L1; the caller would then attempt a
/// migrate-to-gateway submit that reverts `NotAllBatchesExecuted()`.
///
/// Used by flows that require a quiescent chain — e.g. chain upgrades or
/// migrate-to-gateway submit, both of which revert if the commit/execute
/// pipeline still has in-flight batches.
pub fn wait_for_committed_eq_executed(
    settlement_rpc_url: &str,
    diamond_proxy_addr: &str,
    stable_for: Duration,
    timeout: Duration,
) -> Result<u64> {
    let start = std::time::Instant::now();
    let mut last_log = start;
    let mut stable_since: Option<std::time::Instant> = None;
    loop {
        let committed = get_total_batches_committed(settlement_rpc_url, diamond_proxy_addr)?;
        let executed = get_total_batches_executed(settlement_rpc_url, diamond_proxy_addr)?;
        if committed == executed {
            let since = *stable_since.get_or_insert_with(std::time::Instant::now);
            if since.elapsed() >= stable_for {
                println!(
                    "  batches drained: committed == executed == {} on {} (stable for {:?})",
                    committed, diamond_proxy_addr, stable_for
                );
                return Ok(committed);
            }
        } else {
            stable_since = None;
        }
        if last_log.elapsed() >= Duration::from_secs(5) {
            println!(
                "  waiting for drain on {}: committed={} executed={}",
                diamond_proxy_addr, committed, executed
            );
            last_log = std::time::Instant::now();
        }
        if start.elapsed() >= timeout {
            anyhow::bail!(
                "Timed out waiting for {} to drain: committed={} executed={}",
                diamond_proxy_addr,
                committed,
                executed,
            );
        }
        sleep(Duration::from_millis(500));
    }
}

fn parse_u64_value(raw: &str) -> Result<u64> {
    if let Some(hex) = raw.strip_prefix("0x") {
        return u64::from_str_radix(hex, 16).context("Invalid hex value");
    }
    raw.parse::<u64>().context("Invalid decimal value")
}

/// Derive Ethereum address from a private key.
pub fn address_from_private_key(private_key: &str) -> Result<String> {
    use alloy::signers::local::PrivateKeySigner;
    let key = private_key.strip_prefix("0x").unwrap_or(private_key);
    let signer: PrivateKeySigner = key.parse().context("Invalid private key")?;
    Ok(format!("{:?}", signer.address()))
}

pub(crate) fn print_deposit_failure_server_logs(server_logs_path: Option<&Path>) {
    if let Some(log_path) = server_logs_path {
        if let Err(err) = print_stacktrace_context(log_path, 100) {
            eprintln!(
                "Failed to extract stacktrace context from server logs '{}': {}",
                log_path.display(),
                err
            );
        }
        return;
    }
    if let Ok(Some(log_path)) = find_latest_server_log_path() {
        let _ = print_stacktrace_context(&log_path, 100);
    }
}
