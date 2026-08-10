use std::path::{Path, PathBuf};

use alloy::dyn_abi::DynSolValue;
use alloy::network::{EthereumWallet, TransactionBuilder};
use alloy::primitives::{keccak256, Address, Bytes, B256, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use anyhow::{Context, Result};
use clap::Parser;
use protocol_ops::common::abi::{
    BridgehubAbi, IL1AssetRouterAbi, IL1NativeTokenVaultAbi, TestnetERC20TokenAbi,
};
use protocol_ops::common::PrivateKey;

const CREATE2_FACTORY: &str = "0x4e59b44847b379578588920cA78FbF26c0B4956C";

#[derive(Parser, Debug)]
pub struct TokenDeployArgs {
    /// L1 RPC URL
    #[arg(long)]
    pub l1_rpc_url: String,

    /// Private key of the deployer (pays gas, minted first)
    #[arg(long)]
    pub private_key: PrivateKey,

    /// Path to l1-contracts/out directory containing Forge artifacts
    #[arg(long)]
    pub l1_contracts_out: PathBuf,

    /// Bridgehub proxy address (needed for NTV registration)
    #[arg(long)]
    pub bridgehub: Address,

    /// Token symbol (e.g. ZK)
    #[arg(long)]
    pub symbol: String,

    /// Token name (e.g. "ZKSync Token")
    #[arg(long)]
    pub name: String,

    /// Additional addresses to mint to (comma-separated)
    #[arg(long, value_delimiter = ',')]
    pub mint_to: Vec<Address>,

    /// Mint amount per address (in token units, default: 10^27)
    #[arg(long)]
    pub mint_amount: Option<U256>,

    /// CREATE2 salt (32 bytes hex, default: 0x000...001)
    #[arg(long)]
    pub salt: Option<B256>,
}

pub async fn run(args: TokenDeployArgs) -> Result<()> {
    let token_address = deploy(args).await?;
    println!("token_address: {token_address:#x}");
    Ok(())
}

pub async fn deploy(args: TokenDeployArgs) -> Result<Address> {
    let salt = args.salt.unwrap_or_else(|| {
        let mut s = B256::ZERO;
        s.0[31] = 1;
        s
    });
    let mint_amount = args
        .mint_amount
        .unwrap_or_else(|| U256::from(10u64).pow(U256::from(27u64)));

    let signer: PrivateKeySigner = args
        .private_key
        .expose()
        .parse()
        .context("invalid private key")?;
    let deployer_address = signer.address();
    let wallet = EthereumWallet::from(signer);
    let provider = ProviderBuilder::new()
        .wallet(wallet)
        .connect_http(args.l1_rpc_url.parse()?);

    let token_address = deploy_token(
        &provider,
        &args.name,
        &args.symbol,
        salt,
        &args.l1_contracts_out,
    )
    .await?;
    println!("token deployed: {token_address:#x}");

    let token = TestnetERC20TokenAbi::new(token_address, provider.clone());
    let mut mint_targets = vec![deployer_address];
    mint_targets.extend_from_slice(&args.mint_to);
    for addr in &mint_targets {
        token
            .mint(*addr, mint_amount)
            .send()
            .await
            .with_context(|| format!("mint to {addr:#x}"))?
            .get_receipt()
            .await?;
        println!("  minted {mint_amount} to {addr:#x}");
    }

    let bridgehub = BridgehubAbi::new(args.bridgehub, provider.clone());
    let asset_router_addr = bridgehub
        .sharedBridge()
        .call()
        .await
        .context("bridgehub.sharedBridge()")?;
    let asset_router = IL1AssetRouterAbi::new(asset_router_addr, provider.clone());
    let ntv_addr = asset_router
        .nativeTokenVault()
        .call()
        .await
        .context("assetRouter.nativeTokenVault()")?;
    let ntv = IL1NativeTokenVaultAbi::new(ntv_addr, provider.clone());
    let existing_asset_id = ntv
        .assetId(token_address)
        .call()
        .await
        .context("ntv.assetId()")?;
    if existing_asset_id == B256::ZERO {
        ntv.registerToken(token_address)
            .send()
            .await
            .context("ntv.registerToken()")?
            .get_receipt()
            .await?;
        println!("  registered on NTV ({ntv_addr:#x})");
    } else {
        println!("  already registered on NTV ({ntv_addr:#x})");
    }

    Ok(token_address)
}

async fn deploy_token(
    provider: &impl Provider,
    name: &str,
    symbol: &str,
    salt: B256,
    l1_contracts_out: &Path,
) -> Result<Address> {
    let bytecode = load_token_bytecode(l1_contracts_out)?;

    let constructor_args = DynSolValue::Tuple(vec![
        DynSolValue::String(name.to_string()),
        DynSolValue::String(symbol.to_string()),
        DynSolValue::Uint(U256::from(18u8), 8),
    ])
    .abi_encode_params();

    let mut init_code = bytecode.clone();
    init_code.extend_from_slice(&constructor_args);

    let mut data = Vec::with_capacity(32 + init_code.len());
    data.extend_from_slice(salt.as_slice());
    data.extend_from_slice(&init_code);

    let factory: Address = CREATE2_FACTORY.parse()?;

    let init_code_hash = keccak256(&init_code);
    let mut preimage = [0u8; 1 + 20 + 32 + 32];
    preimage[0] = 0xff;
    preimage[1..21].copy_from_slice(factory.as_slice());
    preimage[21..53].copy_from_slice(salt.as_slice());
    preimage[53..85].copy_from_slice(init_code_hash.as_slice());
    let hash = keccak256(preimage);
    let token_address = Address::from_slice(&hash[12..]);

    let existing_code = provider
        .get_code_at(token_address)
        .await
        .context("eth_getCode")?;
    if !existing_code.is_empty() {
        println!("token already deployed: {token_address:#x}");
        return Ok(token_address);
    }

    let tx = TransactionRequest::default()
        .with_to(factory)
        .with_input(Bytes::from(data));
    provider
        .send_transaction(tx)
        .await
        .context("CREATE2 deploy")?
        .get_receipt()
        .await?;

    Ok(token_address)
}

fn load_token_bytecode(l1_contracts_out: &Path) -> Result<Vec<u8>> {
    let artifact_path = l1_contracts_out.join("TestnetERC20Token.sol/TestnetERC20Token.json");
    let content = std::fs::read_to_string(&artifact_path)
        .with_context(|| format!("read {}", artifact_path.display()))?;
    let artifact: serde_json::Value = serde_json::from_str(&content)?;
    let bytecode = artifact["bytecode"]["object"]
        .as_str()
        .or_else(|| artifact["bytecode"].as_str())
        .filter(|&b| b != "0x")
        .ok_or_else(|| anyhow::anyhow!("no bytecode in TestnetERC20Token artifact"))?;
    let hex = bytecode.strip_prefix("0x").unwrap_or(bytecode);
    hex::decode(hex).context("decode TestnetERC20Token bytecode")
}
