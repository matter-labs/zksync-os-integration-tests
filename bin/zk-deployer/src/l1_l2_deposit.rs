use alloy::network::{EthereumWallet, TransactionBuilder};
use alloy::primitives::{Address, Bytes, TxHash, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol_types::SolCall;
use anyhow::{Context, Result};

use protocol_ops::common::abi::BridgehubAbi;

const L2_DEPOSIT_GAS_LIMIT: u64 = 500_000;
const REQUIRED_L1_TO_L2_GAS_PER_PUBDATA_BYTE: u64 = 800;

/// Submit an L1→L2 deposit via `Bridgehub.requestL2TransactionDirect()` to
/// fund `recipient` with `amount_wei` of ETH on the gateway L2.
///
/// Returns the L1 transaction hash.  The deposit is queued as a priority
/// transaction on the gateway; call `wait_for_gateway_balance` afterwards to
/// block until the gateway has processed it.
pub async fn fund_on_gateway(
    l1_rpc_url: &str,
    bridgehub_addr: Address,
    gateway_chain_id: u64,
    recipient: Address,
    amount_wei: U256,
    gas_price_wei: u64,
    deployer_sk: &str,
) -> Result<TxHash> {
    let pk_str = deployer_sk.strip_prefix("0x").unwrap_or(deployer_sk);
    let pk_bytes = hex::decode(pk_str).context("invalid deployer private key hex")?;
    let signer = PrivateKeySigner::from_slice(&pk_bytes).context("invalid deployer private key")?;
    let from = signer.address();
    let wallet = EthereumWallet::from(signer);
    let provider = ProviderBuilder::new()
        .wallet(wallet)
        .connect_http(l1_rpc_url.parse()?);

    // Query l2TransactionBaseCost to compute the required msg.value.
    // Use the sol!-generated call struct to avoid hand-rolling ABI encoding.
    let base_cost = {
        let calldata = BridgehubAbi::l2TransactionBaseCostCall {
            _chainId: U256::from(gateway_chain_id),
            _gasPrice: U256::from(gas_price_wei),
            _l2GasLimit: U256::from(L2_DEPOSIT_GAS_LIMIT),
            _l2GasPerPubdataByteLimit: U256::from(REQUIRED_L1_TO_L2_GAS_PER_PUBDATA_BYTE),
        }
        .abi_encode();

        let req = TransactionRequest::default()
            .with_to(bridgehub_addr)
            .with_input(alloy::primitives::Bytes::from(calldata));
        let result = provider
            .call(req)
            .await
            .context("l2TransactionBaseCost() call")?;
        anyhow::ensure!(
            result.len() >= 32,
            "l2TransactionBaseCost() returned < 32 bytes"
        );
        U256::from_be_slice(&result[..32])
    };

    let mint_value = amount_wei + base_cost;

    // Encode requestL2TransactionDirect(L2TransactionRequestDirect) using the
    // sol!-generated call struct.  This avoids the dyn-abi double-wrapping bug
    // where wrapping struct_fields in an outer Tuple before calling
    // `.abi_encode()` produced two leading offsets instead of one.
    let calldata2 = BridgehubAbi::requestL2TransactionDirectCall {
        _request: BridgehubAbi::L2TransactionRequestDirect {
            chainId: U256::from(gateway_chain_id),
            mintValue: mint_value,
            l2Contract: recipient,
            l2Value: U256::ZERO,
            l2Calldata: Bytes::new(),
            l2GasLimit: U256::from(L2_DEPOSIT_GAS_LIMIT),
            l2GasPerPubdataByteLimit: U256::from(REQUIRED_L1_TO_L2_GAS_PER_PUBDATA_BYTE),
            factoryDeps: vec![],
            refundRecipient: recipient,
        },
    }
    .abi_encode();

    let tx = TransactionRequest::default()
        .with_from(from)
        .with_to(bridgehub_addr)
        .with_input(alloy::primitives::Bytes::from(calldata2))
        .with_value(mint_value)
        .with_gas_price(gas_price_wei as u128);

    let pending = provider
        .send_transaction(tx)
        .await
        .context("send deposit")?;
    let l1_tx_hash = *pending.tx_hash();
    pending.get_receipt().await.context("await receipt")?;
    Ok(l1_tx_hash)
}

/// Poll the gateway L2 until `address` has a non-zero ETH balance.
///
/// Times out after `timeout_secs` seconds with an error.
pub async fn wait_for_gateway_balance(
    gateway_rpc_url: &str,
    address: Address,
    timeout_secs: u64,
) -> Result<()> {
    let provider: alloy::providers::RootProvider<alloy::network::Ethereum> =
        ProviderBuilder::default().connect_http(gateway_rpc_url.parse()?);
    let start = std::time::Instant::now();
    let timeout = std::time::Duration::from_secs(timeout_secs);

    loop {
        if start.elapsed() > timeout {
            anyhow::bail!(
                "Timed out after {}s waiting for {address:#x} to receive ETH on gateway",
                timeout_secs,
            );
        }

        match provider.get_balance(address).await {
            Ok(balance) if balance > U256::ZERO => return Ok(()),
            Ok(_) => {}
            Err(_) => {
                // Gateway may not be fully up yet; treat transient errors as
                // "not ready" and keep polling.
            }
        }

        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
}
