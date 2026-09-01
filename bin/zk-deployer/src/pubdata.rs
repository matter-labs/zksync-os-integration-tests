//! Post-init pubdata-content configuration for validium chains.

use alloy::network::{EthereumWallet, TransactionBuilder};
use alloy::primitives::{Address, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol_types::SolCall;
use anyhow::{Context, Result};

use protocol_ops::common::abi::{IChainAdminAbi, ZkChainAbi};

/// `PubdataContent.LOGS_ONLY` (era-contracts `Constants.sol`).
const LOGS_ONLY: u8 = 1;

/// Set the diamond's pubdata content to `LOGS_ONLY` via `ChainAdmin.multicall`
/// (the diamond's admin), signed by the ChainAdmin owner.
///
/// This has to run as a follow-up L1 transaction: the register/init forge
/// scripts leave the diamond at the `FULL_PUBDATA` default and protocol-ops
/// has no command wrapping `setPubdataContent`. It must land before the chain
/// commits its first batch — the server reads `getPubdataContent()` once at
/// startup, and the Executor pins the value into every batch proof's
/// chain-config hash.
pub async fn set_logs_only_pubdata_content(
    l1_rpc_url: &str,
    owner: &PrivateKeySigner,
    chain_admin: Address,
    diamond: Address,
) -> Result<()> {
    let provider = ProviderBuilder::new()
        .wallet(EthereumWallet::from(owner.clone()))
        .connect(l1_rpc_url)
        .await
        .context("connect with ChainAdmin owner wallet")?;

    let inner = ZkChainAbi::setPubdataContentCall {
        _pubdataContent: LOGS_ONLY,
    }
    .abi_encode();
    let call = IChainAdminAbi::multicallCall {
        _calls: vec![IChainAdminAbi::Call {
            target: diamond,
            value: U256::ZERO,
            data: inner.into(),
        }],
        _requireSuccess: true,
    };
    let receipt = provider
        .send_transaction(
            TransactionRequest::default()
                .with_to(chain_admin)
                .with_input(call.abi_encode()),
        )
        .await
        .context("send ChainAdmin.multicall(setPubdataContent)")?
        .get_receipt()
        .await
        .context("await setPubdataContent receipt")?;
    anyhow::ensure!(
        receipt.status(),
        "ChainAdmin.multicall(setPubdataContent) reverted (tx {:#x})",
        receipt.transaction_hash
    );

    let raw = provider
        .call(
            TransactionRequest::default()
                .with_to(diamond)
                .with_input(ZkChainAbi::getPubdataContentCall {}.abi_encode()),
        )
        .await
        .context("eth_call getPubdataContent")?;
    let content = ZkChainAbi::getPubdataContentCall::abi_decode_returns(&raw)
        .context("decode getPubdataContent return")?;
    anyhow::ensure!(
        content == LOGS_ONLY,
        "getPubdataContent() returned {content}, expected LOGS_ONLY ({LOGS_ONLY})"
    );
    Ok(())
}
