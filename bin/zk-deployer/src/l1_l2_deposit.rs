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

/// Default L1 gas price (wei) used when pricing L1→L2 deposits on local Anvil.
pub const DEFAULT_L1_TO_L2_GAS_PRICE: u64 = 1_000_000_000;

/// Default amount of ETH deposited to each well-known dev wallet on L2 when a
/// chain is brought up against local Anvil. Generous enough for test traffic
/// without exhausting the deployer's (finite) L1 balance across many chains.
pub const DEFAULT_L2_FUND_AMOUNT_ETH: u64 = 100;

/// Well-known dev wallet private keys funded on L2 by default for local/Anvil
/// deployments, so a freshly-deployed chain is immediately ready to operate.
///
/// Slot 0 is the ZKsync-era rich account (`0x36615Cf…`); slots 1–9 are the
/// standard Anvil mnemonic accounts #1–#9 — i.e. the same accounts Anvil
/// pre-funds on L1, now rich on L2 as well. These keys are public test
/// fixtures, not secrets. Account #0 (`0xf39F…`) is intentionally omitted: it
/// is the L1 deployer, not an L2 end-user wallet.
pub const DEFAULT_L2_RICH_KEYS: [&str; 10] = [
    // ZKsync-era rich account — 0x36615Cf349d7F6344891B1e7CA7C72883F5dc049
    "0x7726827caac94a7f9e1b160f7ea819f172f7b6f9d2a97f992c38edeab82d4110",
    // Anvil #1 — 0x70997970C51812dc3A010C7d01b50e0d17dc79C8
    "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d",
    // Anvil #2 — 0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC
    "0x5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a",
    // Anvil #3 — 0x90F79bf6EB2c4f870365E785982E1f101E93b906
    "0x7c852118294e51e653712a81e05800f419141751be58f605c371e15141b007a6",
    // Anvil #4 — 0x15d34AAf54267DB7D7c367839AAf71A00a2C6A65
    "0x47e179ec197488593b187f80a00eb0da91f1b9d0b13f8733639f19c30a34926a",
    // Anvil #5 — 0x9965507D1a55bcC2695C58ba16FB37d819B0A4dc
    "0x8b3a350cf5c34c9194ca85829a2df0ec3153be0318b5e2d3348e872092edffba",
    // Anvil #6 — 0x976EA74026E726554dB657fA54763abd0C3a0aa9
    "0x92db14e403b83dfe3df233f83dfa3a0d7096f21ca9b0d6d6b8d88b2b4ec1564e",
    // Anvil #7 — 0x14dC79964da2C08b23698B3D3cc7Ca32193d9955
    "0x4bbbf85ce3377467afe5d46f804f221813b2bb87f24d81f60f1fcdbf7cbf4356",
    // Anvil #8 — 0x23618e81E3f5cdF7f54C3d65f7FBc0aBf5B21E8f
    "0xdbda1821b80551c9d65939329250298aa3472ba22feea921c0cf5d620ea67b97",
    // Anvil #9 — 0xa0Ee7A142d267C1f36714E4a8F75612F20a79720
    "0x2a871d0798f97d79848a013d4936a73bf4cc922c825d33c1cf7073dff6d409c6",
];

/// Submit an L1→L2 deposit via `Bridgehub.requestL2TransactionDirect()` to
/// fund `recipient` with `amount_wei` of ETH on the L2 chain `chain_id`.
///
/// Returns the L1 transaction hash.  The deposit is queued as a priority
/// transaction on the chain; call `wait_for_l2_balance` afterwards to
/// block until the chain has processed it.
pub async fn deposit_eth(
    l1_rpc_url: &str,
    bridgehub_addr: Address,
    chain_id: u64,
    recipient: Address,
    amount_wei: U256,
    gas_price_wei: u64,
    deployer_sk: &str,
) -> Result<TxHash> {
    let signer: PrivateKeySigner = deployer_sk
        .parse()
        .context("invalid deployer private key")?;
    let from = signer.address();
    let wallet = EthereumWallet::from(signer);
    let provider = ProviderBuilder::new()
        .wallet(wallet)
        .connect_http(l1_rpc_url.parse()?);

    // Query l2TransactionBaseCost to compute the required msg.value.
    // Use the sol!-generated call struct to avoid hand-rolling ABI encoding.
    let base_cost = {
        let calldata = BridgehubAbi::l2TransactionBaseCostCall {
            _chainId: U256::from(chain_id),
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
            chainId: U256::from(chain_id),
            mintValue: mint_value,
            l2Contract: recipient,
            l2Value: amount_wei,
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
    let receipt = pending.get_receipt().await.context("await receipt")?;
    anyhow::ensure!(
        receipt.status(),
        "deposit transaction reverted (tx {l1_tx_hash:#x})"
    );
    Ok(l1_tx_hash)
}

/// Fund every [`DEFAULT_L2_RICH_KEYS`] wallet with [`DEFAULT_L2_FUND_AMOUNT_ETH`]
/// ETH on `chain_id` via L1→L2 deposits, so the chain is ready to operate.
///
/// Each deposit is queued as a priority transaction and is mined into the
/// chain's first batch once a server starts processing its priority queue —
/// no L2 server needs to be running when this is called. Intended for
/// local/Anvil deployments only (on a real L1 the deployer would pay real ETH).
pub async fn fund_default_l2_wallets(
    l1_rpc_url: &str,
    bridgehub_addr: Address,
    chain_id: u64,
    deployer_sk: &str,
) -> Result<()> {
    let amount = U256::from(DEFAULT_L2_FUND_AMOUNT_ETH) * U256::from(1_000_000_000_000_000_000u128);

    // Parse recipients up front so key-parse errors surface before any RPC work.
    let recipients: Vec<Address> = DEFAULT_L2_RICH_KEYS
        .iter()
        .map(|k| {
            k.parse::<PrivateKeySigner>()
                .context("invalid default dev wallet key")
                .map(|s| s.address())
        })
        .collect::<Result<_>>()?;

    // Send sequentially: all deposits share the same deployer signer, so
    // concurrent sends race on the nonce and produce "replacement transaction
    // underpriced" rejections.
    for recipient in recipients {
        deposit_eth(
            l1_rpc_url,
            bridgehub_addr,
            chain_id,
            recipient,
            amount,
            DEFAULT_L1_TO_L2_GAS_PRICE,
            deployer_sk,
        )
        .await
        .with_context(|| format!("fund {recipient:#x} on chain {chain_id}"))?;
    }
    Ok(())
}

/// Poll the L2 until `address` has a non-zero ETH balance.
///
/// Times out after `timeout_secs` seconds with an error.
pub async fn wait_for_l2_balance(
    l2_rpc_url: &str,
    address: Address,
    timeout_secs: u64,
) -> Result<()> {
    let provider: alloy::providers::RootProvider<alloy::network::Ethereum> =
        ProviderBuilder::default().connect_http(l2_rpc_url.parse()?);
    let start = std::time::Instant::now();
    let timeout = std::time::Duration::from_secs(timeout_secs);

    loop {
        if start.elapsed() > timeout {
            anyhow::bail!(
                "Timed out after {}s waiting for {address:#x} to receive ETH on L2",
                timeout_secs,
            );
        }

        match provider.get_balance(address).await {
            Ok(balance) if balance > U256::ZERO => return Ok(()),
            Ok(_) => {}
            Err(_) => {
                // The server may not be fully up yet; treat transient errors
                // as "not ready" and keep polling.
            }
        }

        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
}
