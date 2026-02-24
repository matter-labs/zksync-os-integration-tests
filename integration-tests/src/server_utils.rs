use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
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

pub const DEFAULT_TEST_PRIVATE_KEY: &str =
    "0x7726827caac94a7f9e1b160f7ea819f172f7b6f9d2a97f992c38edeab82d4110";

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
    let raw_logs = fs::read_to_string(log_path).with_context(|| {
        format!("Failed to read server log file '{}'", log_path.display())
    })?;

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
                while let Some(c) = it.next() {
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
    let logs_root = project_root.join("integration-tests/logs");
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

/// Derive Ethereum address from a private key via `cast wallet address`.
pub fn address_from_private_key(private_key: &str) -> Result<String> {
    let output = Command::new("cast")
        .args(["wallet", "address", "--private-key", private_key])
        .output()
        .context("Failed to run cast wallet address")?;
    if !output.status.success() {
        anyhow::bail!(
            "cast wallet address failed:\nSTDERR:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Return L2 native balance in wei via `cast balance`.
pub fn get_l2_balance(address: &str, l2_rpc_url: &str) -> Result<u128> {
    let output = Command::new("cast")
        .args(["balance", address, "--rpc-url", l2_rpc_url])
        .output()
        .context("Failed to run cast balance")?;
    if !output.status.success() {
        anyhow::bail!(
            "cast balance failed:\nSTDERR:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if let Some(hex) = raw.strip_prefix("0x") {
        return u128::from_str_radix(hex, 16).context("Invalid hex balance");
    }
    raw.parse::<u128>().context("Invalid decimal balance")
}

fn read_toolchain_from_dir(dir: &Path) -> Option<String> {
    let toml_path = dir.join("rust-toolchain.toml");
    if toml_path.exists() {
        let content = fs::read_to_string(&toml_path).ok()?;
        for line in content.lines() {
            let line = line.trim();
            if line.starts_with("channel") {
                let rest = line
                    .strip_prefix("channel")?
                    .trim()
                    .trim_start_matches('=')
                    .trim();
                let channel = rest.trim_matches('"').trim_matches('\'').trim();
                if !channel.is_empty() {
                    return Some(channel.to_string());
                }
            }
        }
    }
    let legacy_path = dir.join("rust-toolchain");
    if legacy_path.exists() {
        let content = fs::read_to_string(&legacy_path).ok()?;
        return Some(content.trim().to_string());
    }
    None
}

/// Build and run the generate-deposit tool from zksync-os-server to submit an L1->L2 deposit,
/// then poll L2 until `test_address` has balance > 0. Caller must fund `test_address` on L1 first.
pub fn fund_l2_via_l1_deposit(
    server_root: &Path,
    l1_rpc_url: &str,
    l2_rpc_url: &str,
    bridgehub_addr: &str,
    chain_id: u64,
    test_private_key: &str,
    amount_ether: f64,
    balance_poll_timeout: Duration,
) -> Result<u128> {
    let test_address = address_from_private_key(test_private_key)?;
    // Build generate-deposit (same pattern as server build: use repo toolchain).
    let mut build_cmd = Command::new("cargo");
    build_cmd
        .arg("build")
        .arg("--release")
        .arg("-p")
        .arg("zksync_os_generate_deposit")
        .current_dir(server_root)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    if let Some(toolchain) = read_toolchain_from_dir(server_root) {
        build_cmd.env("RUSTUP_TOOLCHAIN", &toolchain);
    }
    let status = build_cmd
        .status()
        .context("Failed to run cargo build for generate-deposit")?;
    if !status.success() {
        anyhow::bail!("cargo build -p zksync_os_generate_deposit failed in {}", server_root.display());
    }
    let bin = server_root.join("target/release/zksync_os_generate_deposit");
    if !bin.exists() {
        anyhow::bail!("generate-deposit binary not found at {}", bin.display());
    }
    let output = Command::new(&bin)
        .args([
            "--bridgehub",
            bridgehub_addr,
            "--chain-id",
            &chain_id.to_string(),
            "--l1-rpc-url",
            l1_rpc_url,
            "--private-key",
            test_private_key,
            "--amount",
            &amount_ether.to_string(),
        ])
        .output()
        .context("Failed to run generate-deposit")?;
    if !output.status.success() {
        anyhow::bail!(
            "generate-deposit failed:\nSTDOUT:\n{}\nSTDERR:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let deadline = Instant::now() + balance_poll_timeout;
    while Instant::now() < deadline {
        let balance = get_l2_balance(&test_address, l2_rpc_url)?;
        if balance > 0 {
            return Ok(balance);
        }
        sleep(Duration::from_secs(2));
    }
    anyhow::bail!(
        "L2 balance for {} did not become > 0 within {:?}",
        test_address,
        balance_poll_timeout
    )
}
