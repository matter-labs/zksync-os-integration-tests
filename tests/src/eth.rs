//! Thin typed L1 RPC helpers.
//!
//! Contract ABI definitions live in the version-pinned `protocol_ops` crate — add new
//! interfaces there, not here.

use alloy::network::{EthereumWallet, TransactionBuilder};
use alloy::primitives::Address;
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol_types::SolCall;
use anyhow::{Context, Result};

/// Connect a plain (unsigned) provider.
pub async fn provider(rpc_url: &str) -> Result<impl Provider> {
    ProviderBuilder::new()
        .connect(rpc_url)
        .await
        .context("connect")
}

/// Typed `eth_call`: encode `call_data`, call `to`, decode the return value.
pub async fn call<C: SolCall>(
    provider: &impl Provider,
    to: Address,
    call_data: C,
) -> Result<C::Return> {
    let raw = provider
        .call(
            TransactionRequest::default()
                .with_to(to)
                .with_input(call_data.abi_encode()),
        )
        .await
        .with_context(|| format!("eth_call {} on {to:#x}", C::SIGNATURE))?;
    C::abi_decode_returns(&raw)
        .with_context(|| format!("decode {} return from {to:#x}", C::SIGNATURE))
}

/// Send a typed call signed by `key` and wait for the receipt.
pub async fn send_as_signer<C: SolCall>(
    rpc_url: &str,
    key: &str,
    to: Address,
    call_data: C,
) -> Result<()> {
    let signer: PrivateKeySigner = key.parse().context("parse private key")?;
    let provider = ProviderBuilder::new()
        .wallet(EthereumWallet::from(signer))
        .connect(rpc_url)
        .await
        .context("connect with wallet")?;
    let tx = TransactionRequest::default()
        .with_to(to)
        .with_input(call_data.abi_encode())
        .with_gas_price(1_000_000_000u128)
        .with_gas_limit(1_000_000u64);
    let receipt = provider
        .send_transaction(tx)
        .await
        .with_context(|| format!("send {} to {to:#x}", C::SIGNATURE))?
        .get_receipt()
        .await
        .context("get_receipt")?;
    anyhow::ensure!(
        receipt.status(),
        "{} tx {:#x} reverted",
        C::SIGNATURE,
        receipt.transaction_hash
    );
    Ok(())
}
