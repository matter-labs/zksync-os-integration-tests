use std::path::Path;

use alloy::network::{EthereumWallet, TransactionBuilder};
use alloy::primitives::{Address, Bytes};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use anyhow::{Context, Result};
use protocol_ops::common::PrivateKey;

pub async fn deploy_plonk_verifier(
    l1_rpc_url: &str,
    private_key: &PrivateKey,
    l1_contracts_out: &Path,
) -> Result<Address> {
    let bytecode = load_zisk_plonk_bytecode(l1_contracts_out)?;
    let signer: PrivateKeySigner = private_key
        .expose()
        .parse()
        .context("invalid private key")?;
    let wallet = EthereumWallet::from(signer);
    let provider = ProviderBuilder::new()
        .wallet(wallet)
        .connect_http(l1_rpc_url.parse()?);

    let transaction = TransactionRequest::default().with_deploy_code(Bytes::from(bytecode));
    let receipt = provider
        .send_transaction(transaction)
        .await
        .context("deploy ZiskSnarkPlonkVerifier")?
        .get_receipt()
        .await
        .context("wait for ZiskSnarkPlonkVerifier deployment")?;
    let address = receipt
        .contract_address
        .context("ZiskSnarkPlonkVerifier deployment receipt has no contract address")?;
    let code = provider
        .get_code_at(address)
        .await
        .context("read deployed ZiskSnarkPlonkVerifier code")?;
    anyhow::ensure!(
        !code.is_empty(),
        "ZiskSnarkPlonkVerifier deployment produced no code at {address:#x}"
    );

    Ok(address)
}

fn load_zisk_plonk_bytecode(l1_contracts_out: &Path) -> Result<Vec<u8>> {
    let artifact_path =
        l1_contracts_out.join("ZiskSnarkPlonkVerifier.sol/ZiskSnarkPlonkVerifier.json");
    let content = std::fs::read_to_string(&artifact_path).with_context(|| {
        format!(
            "read {} — run `zk-deployer build-contracts --with-zisk` first",
            artifact_path.display()
        )
    })?;
    let artifact: serde_json::Value = serde_json::from_str(&content)?;
    let bytecode = artifact["bytecode"]["object"]
        .as_str()
        .or_else(|| artifact["bytecode"].as_str())
        .filter(|bytecode| *bytecode != "0x")
        .ok_or_else(|| anyhow::anyhow!("no bytecode in ZiskSnarkPlonkVerifier artifact"))?;
    let bytecode = bytecode.strip_prefix("0x").unwrap_or(bytecode);
    hex::decode(bytecode).context("decode ZiskSnarkPlonkVerifier bytecode")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_generated_plonk_bytecode_from_foundry_artifact() {
        let dir = tempfile::tempdir().unwrap();
        let artifact_dir = dir.path().join("ZiskSnarkPlonkVerifier.sol");
        std::fs::create_dir_all(&artifact_dir).unwrap();
        std::fs::write(
            artifact_dir.join("ZiskSnarkPlonkVerifier.json"),
            r#"{"bytecode":{"object":"0x60016000"}}"#,
        )
        .unwrap();

        assert_eq!(
            load_zisk_plonk_bytecode(dir.path()).unwrap(),
            hex::decode("60016000").unwrap()
        );
    }

    #[test]
    fn rejects_artifact_without_deployable_bytecode() {
        let dir = tempfile::tempdir().unwrap();
        let artifact_dir = dir.path().join("ZiskSnarkPlonkVerifier.sol");
        std::fs::create_dir_all(&artifact_dir).unwrap();
        std::fs::write(
            artifact_dir.join("ZiskSnarkPlonkVerifier.json"),
            r#"{"bytecode":{"object":"0x"}}"#,
        )
        .unwrap();

        let error = load_zisk_plonk_bytecode(dir.path()).unwrap_err();
        assert!(error.to_string().contains("no bytecode"));
    }
}
