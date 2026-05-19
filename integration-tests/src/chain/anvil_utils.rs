use anyhow::{Context, Result};
use std::process::Command;

pub const EIP1967_PROXY_ADMIN_SLOT: &str =
    "0xb53127684a568b3173ae13b9f8a6016e243e63b6e8ee1178d6a717850b5d6103";

/// Default private key for the rich account used to fund impersonated accounts.
pub const RICH_ACCOUNT_PRIVATE_KEY: &str =
    "0x7726827caac94a7f9e1b160f7ea819f172f7b6f9d2a97f992c38edeab82d4110";

/// Send a JSON-RPC request and fail if the response carries an `error` field.
async fn rpc_call(
    l1_rpc_url: &str,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value> {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    });
    let resp = reqwest::Client::new()
        .post(l1_rpc_url)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("POST {method} to {l1_rpc_url}"))?;
    let json: serde_json::Value = resp
        .json()
        .await
        .with_context(|| format!("{method} response was not JSON"))?;
    if let Some(err) = json.get("error") {
        anyhow::bail!("{method} RPC error: {err}");
    }
    Ok(json)
}

/// Enable Anvil account impersonation
pub async fn impersonate_account(address: &str, l1_rpc_url: &str) -> Result<()> {
    rpc_call(
        l1_rpc_url,
        "anvil_impersonateAccount",
        serde_json::json!([address]),
    )
    .await
    .with_context(|| format!("impersonate_account({address})"))?;
    Ok(())
}

/// Stop Anvil account impersonation
pub async fn stop_impersonating_account(address: &str, l1_rpc_url: &str) {
    let _ = rpc_call(
        l1_rpc_url,
        "anvil_stopImpersonatingAccount",
        serde_json::json!([address]),
    )
    .await;
}

/// Fund an account with ETH using the rich account private key
pub fn fund_account(
    address: &str,
    amount: &str,
    l1_rpc_url: &str,
    private_key: &str,
) -> Result<()> {
    let output = Command::new("cast")
        .args([
            "send",
            address,
            "--value",
            amount,
            "--private-key",
            private_key,
            "--rpc-url",
            l1_rpc_url,
            "--gas-price",
            "100gwei",
        ])
        .output()
        .context("Failed to fund account")?;
    anyhow::ensure!(
        output.status.success(),
        "fund_account failed for {address} (amount={amount}): {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

/// Set an account's ETH balance via `anvil_setBalance`.
///
/// Direct state mutation — no tx, no gas, no nonce. Safe to call concurrently
/// for different addresses.
pub async fn anvil_set_balance(address: &str, wei: u128, l1_rpc_url: &str) -> Result<()> {
    let value = format!("0x{wei:x}");
    rpc_call(
        l1_rpc_url,
        "anvil_setBalance",
        serde_json::json!([address, value]),
    )
    .await
    .with_context(|| format!("anvil_set_balance({address}, wei={wei})"))?;
    Ok(())
}

/// Call a contract function from an impersonated account
pub fn call_contract_from(
    contract: &str,
    function_sig: &str,
    args: &[&str],
    from: &str,
    context_msg: &str,
    l1_rpc_url: &str,
) -> Result<()> {
    let mut cmd_args = vec!["send", contract, function_sig];
    cmd_args.extend_from_slice(args);
    cmd_args.extend_from_slice(&["--from", from, "--rpc-url", l1_rpc_url, "--unlocked"]);

    let output = Command::new("cast")
        .args(&cmd_args)
        .output()
        .with_context(|| context_msg.to_string())?;
    anyhow::ensure!(
        output.status.success(),
        "{context_msg}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

/// Impersonate account, call contract, then stop impersonating
pub async fn call_as_impersonated(
    contract: &str,
    function_sig: &str,
    args: &[&str],
    from: &str,
    context_msg: &str,
    l1_rpc_url: &str,
) -> Result<()> {
    impersonate_account(from, l1_rpc_url).await?;
    let result = call_contract_from(contract, function_sig, args, from, context_msg, l1_rpc_url);
    stop_impersonating_account(from, l1_rpc_url).await;
    result
}
