//! Fund L1 addresses (batch operators, bundle signers) so they can pay gas to
//! broadcast.

use alloy::network::{EthereumWallet, TransactionBuilder};
use alloy::primitives::{Address, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use anyhow::{Context, Result};

use protocol_ops::common::logger;

/// 1 gwei gas price
const FUND_GAS_PRICE_WEI: u128 = 1_000_000_000;
/// Top a target up to this balance when it holds less than [`FUND_MIN_WEI`].
const FUND_TARGET_WEI: u128 = 100 * 1_000_000_000_000_000_000; // 100 ETH
/// Skip topping up a target already holding at least this much (idempotent).
const FUND_MIN_WEI: u128 = 10 * 1_000_000_000_000_000_000; // 10 ETH

/// Top `target` up to [`FUND_TARGET_WEI`] via a value transfer from `funder_key`,
/// but only when it currently holds less than [`FUND_MIN_WEI`] (idempotent, so
/// safe to call on resume / in the re-fund loop). `funder_key` must itself be
/// funded on the target chain.
pub async fn fund(l1_rpc_url: &str, funder_key: &str, target: Address) -> Result<()> {
    let signer: PrivateKeySigner = funder_key.parse().context("invalid funder private key")?;
    let funder = signer.address();
    // Self-funding is a no-op: the funder is by definition already funded.
    if funder == target {
        return Ok(());
    }

    let provider = ProviderBuilder::new()
        .wallet(EthereumWallet::from(signer))
        .connect_http(l1_rpc_url.parse()?);

    let balance = provider
        .get_balance(target)
        .await
        .with_context(|| format!("get_balance({target:#x})"))?;
    if balance >= U256::from(FUND_MIN_WEI) {
        return Ok(());
    }

    let tx = TransactionRequest::default()
        .with_to(target)
        .with_value(U256::from(FUND_TARGET_WEI) - balance)
        .with_gas_price(FUND_GAS_PRICE_WEI);

    let pending = provider
        .send_transaction(tx)
        .await
        .with_context(|| format!("funding transfer to {target:#x} from {funder:#x}"))?;
    let receipt = pending
        .get_receipt()
        .await
        .with_context(|| format!("await funding-transfer receipt for {target:#x}"))?;
    anyhow::ensure!(receipt.status(), "funding transfer to {target:#x} reverted");
    logger::info(format!("  funded {target:#x} via transfer"));
    Ok(())
}
