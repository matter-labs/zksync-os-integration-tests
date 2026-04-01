//! L1→L2 deposit through Bridgehub `requestL2TransactionDirect`.
//! Forked from `zksync-os-server/tools/generate-deposit` — integration-tests does not build or invoke
//! any `zksync-os-server` tooling except the server binary.

use std::str::FromStr;

use alloy::network::{EthereumWallet, TxSigner};
use alloy::primitives::{Address, U256};
use alloy::providers::utils::Eip1559Estimation;
use alloy::providers::{Provider, ProviderBuilder};
use alloy::signers::local::LocalSigner;
use anyhow::{Context, Result};

/// Must match `zksync_os_types::REQUIRED_L1_TO_L2_GAS_PER_PUBDATA_BYTE` / OS protocol.
pub const REQUIRED_L1_TO_L2_GAS_PER_PUBDATA_BYTE: u64 = 800;

const L2_DEPOSIT_GAS_LIMIT: u64 = 500_000;

alloy::sol! {
    #[allow(missing_docs)]
    struct L2CanonicalTransaction {
        uint256 txType;
        uint256 from;
        uint256 to;
        uint256 gasLimit;
        uint256 gasPerPubdataByteLimit;
        uint256 maxFeePerGas;
        uint256 maxPriorityFeePerGas;
        uint256 paymaster;
        uint256 nonce;
        uint256 value;
        uint256[4] reserved;
        bytes data;
        bytes signature;
        uint256[] factoryDeps;
        bytes paymasterInput;
        bytes reservedDynamic;
    }

    interface IMailbox {
        event NewPriorityRequest(
            uint256 txId,
            bytes32 txHash,
            uint64 expirationTimestamp,
            L2CanonicalTransaction transaction,
            bytes[] factoryDeps
        );
    }

    #[sol(rpc)]
    interface IBridgehub {
        struct L2TransactionRequestDirect {
            uint256 chainId;
            uint256 mintValue;
            address l2Contract;
            uint256 l2Value;
            bytes l2Calldata;
            uint256 l2GasLimit;
            uint256 l2GasPerPubdataByteLimit;
            bytes[] factoryDeps;
            address refundRecipient;
        }

        function requestL2TransactionDirect(
            L2TransactionRequestDirect calldata _request
        ) external payable returns (bytes32 canonicalTxHash);

        function l2TransactionBaseCost(
            uint256 _chainId,
            uint256 _gasPrice,
            uint256 _l2GasLimit,
            uint256 _l2GasPerPubdataByteLimit
        ) external view returns (uint256);
    }
}

/// Submit an L1→L2 deposit (same flow as `zksync_os_generate_deposit` binary).
///
/// Runs the async Bridgehub calls on a fresh runtime in a dedicated thread so this stays correct
/// when called from `#[tokio::test]` (current-thread) as well as from plain sync code.
pub fn submit_l1_to_l2_deposit_via_bridgehub(
    l1_rpc_url: &str,
    bridgehub_addr: &str,
    chain_id: u64,
    private_key: &str,
    amount_ether: f64,
) -> Result<()> {
    let l1_rpc_url = l1_rpc_url.to_string();
    let bridgehub_addr = bridgehub_addr.to_string();
    let private_key = private_key.to_string();
    match std::thread::Builder::new()
        .name("l1-l2-deposit".into())
        .spawn(move || {
            tokio::runtime::Runtime::new()
                .context("tokio::runtime::Runtime::new")?
                .block_on(submit_l1_to_l2_deposit_via_bridgehub_inner(
                    l1_rpc_url.as_str(),
                    bridgehub_addr.as_str(),
                    chain_id,
                    private_key.as_str(),
                    amount_ether,
                ))
        })
        .map_err(|e| anyhow::anyhow!("spawn l1-l2-deposit thread: {e}"))?
        .join()
    {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(e),
        Err(_) => anyhow::bail!("l1-l2-deposit thread panicked"),
    }
}

fn deposit_eip1559_estimator(base_fee_per_gas: u128, _rewards: &[Vec<u128>]) -> Eip1559Estimation {
    Eip1559Estimation {
        max_fee_per_gas: base_fee_per_gas * 3 / 2,
        max_priority_fee_per_gas: 0,
    }
}

async fn submit_l1_to_l2_deposit_via_bridgehub_inner(
    l1_rpc_url: &str,
    bridgehub_addr: &str,
    chain_id: u64,
    private_key: &str,
    amount_ether: f64,
) -> Result<()> {
    let bridgehub_address: Address = bridgehub_addr
        .parse()
        .with_context(|| format!("invalid bridgehub address {bridgehub_addr}"))?;
    let amount = U256::from((amount_ether * 1e18) as u128);
    let l1_wallet = EthereumWallet::new(
        LocalSigner::from_str(private_key).context("invalid private key for deposit")?,
    );
    let l1_provider = ProviderBuilder::new()
        .with_recommended_fillers()
        .wallet(l1_wallet.clone())
        .on_builtin(l1_rpc_url)
        .await
        .with_context(|| format!("connect L1 JSON-RPC at {l1_rpc_url}"))?;

    let bridgehub = IBridgehub::new(bridgehub_address, l1_provider.clone());
    let max_priority_fee_per_gas = l1_provider
        .get_max_priority_fee_per_gas()
        .await
        .map_err(|e| anyhow::anyhow!("eth_maxPriorityFeePerGas: {e}"))?;
    let base_l1_fees_data = l1_provider
        .estimate_eip1559_fees(Some(deposit_eip1559_estimator))
        .await
        .map_err(|e| anyhow::anyhow!("estimate_eip1559_fees: {e}"))?;
    let max_fee_per_gas = base_l1_fees_data.max_fee_per_gas + max_priority_fee_per_gas;
    let tx_base_cost = bridgehub
        .l2TransactionBaseCost(
            U256::from(chain_id),
            U256::from(max_fee_per_gas + max_priority_fee_per_gas),
            U256::from(L2_DEPOSIT_GAS_LIMIT),
            U256::from(REQUIRED_L1_TO_L2_GAS_PER_PUBDATA_BYTE),
        )
        .call()
        .await
        .map_err(|e| anyhow::anyhow!("Bridgehub.l2TransactionBaseCost: {e}"))?
        ._0;

    let sender = l1_wallet.default_signer().address();
    let request = IBridgehub::L2TransactionRequestDirect {
        chainId: U256::from(chain_id),
        mintValue: amount + tx_base_cost,
        l2Contract: sender,
        l2Value: amount,
        l2Calldata: vec![].into(),
        l2GasLimit: U256::from(L2_DEPOSIT_GAS_LIMIT),
        l2GasPerPubdataByteLimit: U256::from(REQUIRED_L1_TO_L2_GAS_PER_PUBDATA_BYTE),
        factoryDeps: vec![],
        refundRecipient: sender,
    };

    let l1_deposit_request = bridgehub
        .requestL2TransactionDirect(request)
        .value(amount + tx_base_cost)
        .max_fee_per_gas(max_fee_per_gas)
        .max_priority_fee_per_gas(max_priority_fee_per_gas)
        .into_transaction_request();

    let l1_deposit_receipt = l1_provider
        .send_transaction(l1_deposit_request)
        .await
        .map_err(|e| anyhow::anyhow!("send L1 Bridgehub deposit tx: {e}"))?
        .get_receipt()
        .await
        .map_err(|e| anyhow::anyhow!("get L1 deposit receipt: {e}"))?;

    anyhow::ensure!(
        l1_deposit_receipt.status(),
        "L1 deposit transaction reverted"
    );

    let l1_to_l2_tx_log = l1_deposit_receipt
        .inner
        .logs()
        .iter()
        .filter_map(|log| log.log_decode::<IMailbox::NewPriorityRequest>().ok())
        .next()
        .context("no L1→L2 NewPriorityRequest log from deposit tx")?;

    println!(
        "Successfully submitted L1→L2 deposit (L2 priority tx hash {})",
        l1_to_l2_tx_log.inner.txHash
    );
    Ok(())
}
