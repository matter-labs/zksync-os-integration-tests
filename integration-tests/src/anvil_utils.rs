use anyhow::{Context, Result};
use std::process::Command;

/// Default private key for the rich account used to fund impersonated accounts.
pub const RICH_ACCOUNT_PRIVATE_KEY: &str =
    "0x7726827caac94a7f9e1b160f7ea819f172f7b6f9d2a97f992c38edeab82d4110";

/// Enable Anvil account impersonation
pub fn impersonate_account(address: &str, l1_rpc_url: &str) -> Result<()> {
    Command::new("cast")
        .args([
            "rpc",
            "anvil_impersonateAccount",
            address,
            "--rpc-url",
            l1_rpc_url,
        ])
        .output()
        .context("Failed to enable impersonation")?;
    Ok(())
}

/// Stop Anvil account impersonation
pub fn stop_impersonating_account(address: &str, l1_rpc_url: &str) {
    let _ = Command::new("cast")
        .args([
            "rpc",
            "anvil_stopImpersonatingAccount",
            address,
            "--rpc-url",
            l1_rpc_url,
        ])
        .output();
}

/// Fund an account with ETH using the rich account private key
pub fn fund_account(
    address: &str,
    amount: &str,
    l1_rpc_url: &str,
    private_key: &str,
) -> Result<()> {
    Command::new("cast")
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

    Command::new("cast")
        .args(&cmd_args)
        .output()
        .with_context(|| context_msg.to_string())?;
    Ok(())
}

/// Impersonate account, call contract, then stop impersonating
pub fn call_as_impersonated(
    contract: &str,
    function_sig: &str,
    args: &[&str],
    from: &str,
    context_msg: &str,
    l1_rpc_url: &str,
) -> Result<()> {
    impersonate_account(from, l1_rpc_url)?;
    let result = call_contract_from(contract, function_sig, args, from, context_msg, l1_rpc_url);
    stop_impersonating_account(from, l1_rpc_url);
    result
}

/// Make a read-only contract call and return the output
pub fn call_contract_view(
    contract: &str,
    function_sig: &str,
    context_msg: &str,
    l1_rpc_url: &str,
) -> Result<std::process::Output> {
    Command::new("cast")
        .args([
            "call",
            contract,
            function_sig,
            "--rpc-url",
            l1_rpc_url,
        ])
        .output()
        .with_context(|| context_msg.to_string())
}
