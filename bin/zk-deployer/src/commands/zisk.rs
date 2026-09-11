use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use alloy::network::{EthereumWallet, TransactionBuilder};
use alloy::primitives::{Address, Bytes};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use anyhow::{Context, Result};
use protocol_ops::common::PrivateKey;

pub fn prepare_plonk_verifier(contracts_root: &Path) -> Result<PathBuf> {
    // Embed only our preparation scripts and dependency pins so installed
    // binaries can prepare the backend without the deployer source checkout.
    let helper = tempfile::tempdir()?;
    for (name, contents) in [
        (
            "zisk-backend.js",
            include_str!("../../tools/zisk-backend/zisk-backend.js"),
        ),
        (
            "render_plonk_verifier.js",
            include_str!("../../tools/zisk-backend/render_plonk_verifier.js"),
        ),
        (
            "package.json",
            include_str!("../../tools/zisk-backend/package.json"),
        ),
        (
            "package-lock.json",
            include_str!("../../tools/zisk-backend/package-lock.json"),
        ),
    ] {
        std::fs::write(helper.path().join(name), contents)?;
    }
    let output = Command::new("node")
        .arg(helper.path().join("zisk-backend.js"))
        .arg("prepare")
        .arg(contracts_root)
        .stderr(Stdio::inherit())
        .output()
        .context("run ZiSK backend preparation (requires Node.js, npm, and pinned Foundry)")?;
    anyhow::ensure!(output.status.success(), "ZiSK backend preparation failed");
    let artifact = PathBuf::from(String::from_utf8(output.stdout)?.trim());
    anyhow::ensure!(
        artifact.is_absolute() && artifact.is_file(),
        "ZiSK backend preparation returned no artifact"
    );
    Ok(artifact)
}

pub async fn deploy_plonk_verifier(
    l1_rpc_url: &str,
    private_key: &PrivateKey,
    contracts_root: &Path,
) -> Result<Address> {
    let artifact_path = prepare_plonk_verifier(contracts_root)?;
    let bytecode = load_zisk_plonk_bytecode(&artifact_path)?;
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

fn load_zisk_plonk_bytecode(artifact_path: &Path) -> Result<Vec<u8>> {
    let content = std::fs::read_to_string(artifact_path)
        .with_context(|| format!("read {}", artifact_path.display()))?;
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
    #[ignore = "requires PROTOCOL_CONTRACTS_ROOT, Node.js, npm, and pinned Foundry"]
    fn prepares_backend_from_embedded_helper() {
        let root = PathBuf::from(std::env::var("PROTOCOL_CONTRACTS_ROOT").unwrap())
            .canonicalize()
            .unwrap();
        let artifact = prepare_plonk_verifier(&root).unwrap();

        assert!(!artifact.starts_with(root));
        assert!(!load_zisk_plonk_bytecode(&artifact).unwrap().is_empty());
        assert_eq!(
            prepare_plonk_verifier(&protocol_ops::common::paths::contracts_root()).unwrap(),
            artifact
        );
    }

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
            load_zisk_plonk_bytecode(&artifact_dir.join("ZiskSnarkPlonkVerifier.json")).unwrap(),
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

        let error = load_zisk_plonk_bytecode(&artifact_dir.join("ZiskSnarkPlonkVerifier.json"))
            .unwrap_err();
        assert!(error.to_string().contains("no bytecode"));
    }
}
