use std::path::PathBuf;

use alloy::signers::local::PrivateKeySigner;
use anyhow::Result;
use clap::Parser;
use sha2::{Digest as _, Sha256};

#[derive(Parser, Debug)]
pub struct WalletsGenerateArgs {
    /// Comma-separated chain IDs (e.g. "505,6565").
    #[arg(long, value_delimiter = ',')]
    pub chains: Vec<u64>,

    /// Seed prefix for ecosystem-level keys.
    #[arg(long, default_value = "ecosystem")]
    pub ecosystem_seed: String,

    /// Output file path.
    #[arg(long, default_value = "wallets.yaml")]
    pub output: PathBuf,
}

pub async fn run(args: WalletsGenerateArgs) -> Result<()> {
    if args.chains.is_empty() {
        anyhow::bail!("at least one chain ID is required via --chains");
    }

    let yaml = generate_wallets_yaml(&args.chains, &args.ecosystem_seed)?;

    if let Some(parent) = args.output.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::write(&args.output, &yaml)?;
    println!("wallets written to: {}", args.output.display());
    Ok(())
}

pub fn generate_wallets_yaml(chains: &[u64], ecosystem_seed: &str) -> Result<String> {
    let mut yaml = String::new();

    yaml.push_str("ecosystem:\n");
    for role in ["owner", "governor", "token_multiplier_setter"] {
        let seed = format!("{role}|{ecosystem_seed}");
        let (address, private_key) = wallet_from_seed(&seed)?;
        push_full_wallet(&mut yaml, "  ", role, &address, &private_key);
    }

    for chain_id in chains {
        yaml.push_str(&format!("{chain_id}:\n"));

        for role in ["owner", "fee_account"] {
            let seed = format!("{role}|{chain_id}");
            let (address, private_key) = wallet_from_seed(&seed)?;
            push_full_wallet(&mut yaml, "  ", role, &address, &private_key);
        }

        for role in [
            "operator_commit_sk",
            "operator_prove_sk",
            "operator_execute_sk",
        ] {
            let seed = format!("{role}|{chain_id}");
            let (address, private_key) = wallet_from_seed(&seed)?;
            push_full_wallet(&mut yaml, "  ", role, &address, &private_key);
        }
    }

    Ok(yaml)
}

fn private_key_from_seed(seed: &str) -> [u8; 32] {
    Sha256::digest(seed.as_bytes()).into()
}

fn wallet_from_seed(seed: &str) -> Result<(String, String)> {
    let sk_bytes = private_key_from_seed(seed);
    let signer = PrivateKeySigner::from_slice(&sk_bytes)
        .map_err(|e| anyhow::anyhow!("invalid private key for seed '{seed}': {e}"))?;
    let address = format!("{:#x}", signer.address());
    let private_key = format!("0x{}", hex::encode(sk_bytes));
    Ok((address, private_key))
}

fn push_full_wallet(yaml: &mut String, indent: &str, role: &str, address: &str, private_key: &str) {
    yaml.push_str(&format!(
        "{indent}{role}:\n{indent}  address: '{address}'\n{indent}  private_key: '{private_key}'\n"
    ));
}
