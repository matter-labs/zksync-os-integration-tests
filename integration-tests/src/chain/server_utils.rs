use std::fs;
use std::path::Path;
use std::process::Command;
use std::thread::sleep;
use std::time::{Duration, Instant};

use std::borrow::Cow;

use anyhow::{Context, Result};

use crate::utils::find_project_root;

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
    Ok(())
}

fn find_latest_server_log_path() -> Result<Option<std::path::PathBuf>> {
    let project_root = find_project_root()?;
    let logs_root = project_root.join("test-run-logs");
    if !logs_root.exists() {
        return Ok(None);
    }

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

/// Derive Ethereum address from a private key.
pub fn address_from_private_key(private_key: &str) -> Result<String> {
    use alloy::signers::local::PrivateKeySigner;
    let key = private_key.strip_prefix("0x").unwrap_or(private_key);
    let signer: PrivateKeySigner = key.parse().context("Invalid private key")?;
    Ok(format!("{:?}", signer.address()))
}

fn cast_balance_transient(stderr: &str) -> bool {
    stderr.contains("Connection refused")
        || stderr.contains("tcp connect error")
        || stderr.contains("client error (Connect)")
        || stderr.contains("operation timed out")
        || stderr.contains("timed out")
}

/// `Ok(None)` = RPC unreachable / transient; `Ok(Some(wei))` = balance (may be 0).
fn poll_l2_balance_once(address: &str, l2_rpc_url: &str) -> Result<Option<u128>> {
    let output = Command::new("cast")
        .args(["balance", address, "--rpc-url", l2_rpc_url])
        .output()
        .context("Failed to run cast balance")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if cast_balance_transient(&stderr) {
            return Ok(None);
        }
        anyhow::bail!("cast balance failed:\nSTDERR:\n{}", stderr);
    }
    let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let balance = if let Some(hex) = raw.strip_prefix("0x") {
        u128::from_str_radix(hex, 16).context("Invalid hex balance")?
    } else {
        raw.parse::<u128>().context("Invalid decimal balance")?
    };
    Ok(Some(balance))
}

/// Submit a Bridgehub L1→L2 deposit and poll L2 until `l2_recipient`'s
/// balance strictly increases. Internal implementation for
/// [`crate::server::Server::fund_account_via_l1_deposit`].
///
/// When `base_token_is_eth = false`, the caller must have pre-approved the
/// base token to the bridgehub.
///
/// When `server_logs_path` is set (or discoverable under `test-run-logs/`),
/// RPC failures and balance poll timeouts print a server log excerpt so
/// crashes match `upgrade-tests` / traffic diagnostics.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn fund_l2_via_l1_deposit_ex(
    l1_rpc_url: &str,
    l2_rpc_url: &str,
    bridgehub_addr: &str,
    chain_id: u64,
    l2_recipient: &str,
    amount_ether: f64,
    balance_poll_timeout: Duration,
    server_logs_path: Option<&Path>,
    base_token_is_eth: bool,
) -> Result<u128> {
    // Snapshot the balance before submitting the deposit so we can detect
    // the credit even if the recipient was already funded. A transient RPC
    // failure is treated as "zero" — the strict-increase check below still
    // gives us a meaningful signal in that case.
    let balance_before = poll_l2_balance_once(l2_recipient, l2_rpc_url)
        .ok()
        .flatten()
        .unwrap_or(0);

    if let Err(err) = crate::l1_l2_deposit::submit_l1_to_l2_deposit_ex(
        l1_rpc_url,
        bridgehub_addr,
        chain_id,
        crate::anvil::DEFAULT_ANVIL_PRIVATE_KEY,
        amount_ether,
        Some(l2_recipient),
        base_token_is_eth,
    )
    .await
    {
        print_deposit_failure_server_logs(server_logs_path);
        return Err(err).context("Bridgehub L1→L2 deposit");
    }
    let deadline = Instant::now() + balance_poll_timeout;
    while Instant::now() < deadline {
        match poll_l2_balance_once(l2_recipient, l2_rpc_url) {
            Ok(Some(balance)) if balance > balance_before => return Ok(balance),
            Ok(_) => sleep(Duration::from_secs(2)),
            Err(err) => {
                let msg = format!("{err:#}");
                if rpc_failure_looks_like_server_down(&msg) {
                    print_deposit_failure_server_logs(server_logs_path);
                }
                return Err(err);
            }
        }
    }
    print_deposit_failure_server_logs(server_logs_path);
    let logs_hint = server_logs_path
        .map(|p| format!(" Server logs: {}", p.display()))
        .unwrap_or_default();
    anyhow::bail!(
        "L2 balance for {} did not grow above pre-deposit {} within {:?}.{}",
        l2_recipient,
        balance_before,
        balance_poll_timeout,
        logs_hint
    )
}

fn print_deposit_failure_server_logs(server_logs_path: Option<&Path>) {
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
