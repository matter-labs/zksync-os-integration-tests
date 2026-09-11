use std::path::Path;
use std::process::Command;

use alloy::primitives::{Address, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::sol;
use anyhow::{ensure, Context, Result};
use serde_json::Value;
use zk_deployer::anvil::{deploy_builder, spawn_from_file};
use zk_deployer::deployed::DeployedEcosystem;

const CHAIN_ID: u64 = 6565;

sol! {
    #[sol(rpc)]
    interface IChainVerifier {
        function getVerifier() external view returns (address);
        function disabledProofSystems() external view returns (uint8);
    }

    #[sol(rpc)]
    interface IMultiProofVerifier {
        function ZISK_RANGE_VERIFIER() external view returns (address);
        function getProofMode(uint8 disabledProofSystems) external view returns (uint256);
    }

    #[sol(rpc)]
    interface ITestnetVerifier {
        function INNER_VERIFIER() external view returns (address);
    }

    #[sol(rpc)]
    interface IZiskVerifier {
        function PLONK_VERIFIER() external view returns (address);
        function verify(uint256[] publicInputs, uint256[] proof) external view returns (bool);
    }
}

fn run(workdir: &Path, cache: &Path, args: &[&str]) -> Result<()> {
    eprintln!("[multiprover] zk-deployer {}", args.join(" "));
    let output = Command::new(env!("CARGO_BIN_EXE_zk-deployer"))
        .args(args)
        .current_dir(workdir)
        .env("ZISK_BACKEND_CACHE", cache)
        .output()
        .with_context(|| format!("run zk-deployer {args:?}"))?;
    ensure!(
        output.status.success(),
        "zk-deployer {args:?} failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn fresh_multiprover_deployment_wires_external_backend() -> Result<()> {
    // Always start from an empty deployment and backend cache: a restored L1
    // fixture would hide regressions in preparation, broadcasting, or wiring.
    let workdir = tempfile::tempdir()?;
    let cache = workdir.path().join("backend-cache");
    std::fs::write(
        workdir.path().join("intent.yaml"),
        format!(
            "schema_version: 1\nmulti_proof_verifier: true\nchains:\n  - chain_id: {CHAIN_ID}\n    da_mode: rollup\n"
        ),
    )?;

    run(workdir.path(), &cache, &["build-contracts", "--with-zisk"])?;
    run(workdir.path(), &cache, &["bootstrap", "--broadcast"])?;
    run(workdir.path(), &cache, &["apply", "--broadcast"])?;
    run(workdir.path(), &cache, &["server-config"])?;

    let state: Value = serde_json::from_slice(&std::fs::read(workdir.path().join("state.json"))?)?;
    let backend: Address = serde_json::from_value(
        state["steps"]["zisk.plonk_verifier.deploy"]["verifier_address"].clone(),
    )?;
    let deployed = DeployedEcosystem::load(workdir.path())?;
    let chain = deployed.chain(CHAIN_ID)?;
    let anvil = spawn_from_file(deploy_builder(), &workdir.path().join("l1-state.json")).await?;
    let provider = ProviderBuilder::new().connect_http(anvil.endpoint().parse()?);

    let diamond = IChainVerifier::new(chain.diamond_proxy, &provider);
    let outer = ITestnetVerifier::new(
        diamond
            .getVerifier()
            .call()
            .await
            .context("chain verifier")?,
        &provider,
    );
    let verifier = IMultiProofVerifier::new(
        outer
            .INNER_VERIFIER()
            .call()
            .await
            .context("multiproof testnet wrapper")?,
        &provider,
    );
    let range_wrapper_address = verifier
        .ZISK_RANGE_VERIFIER()
        .call()
        .await
        .context("ZiSK range wrapper")?;
    let range_wrapper = ITestnetVerifier::new(range_wrapper_address, &provider);
    let range_address = range_wrapper
        .INNER_VERIFIER()
        .call()
        .await
        .context("ZiSK testnet wrapper")?;
    ensure!(!provider.get_code_at(range_address).await?.is_empty());
    let range = IZiskVerifier::new(range_address, &provider);
    assert_eq!(range.PLONK_VERIFIER().call().await?, backend);
    let disabled = diamond.disabledProofSystems().call().await?;
    assert_eq!(disabled, 0);
    assert_eq!(
        verifier.getProofMode(disabled).call().await?,
        alloy::primitives::U256::from(5)
    );

    let contracts = protocol_ops::common::paths::contracts_root();
    let output = Command::new("node")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tools/zisk-backend/zisk-backend.js"))
        .arg("prepare")
        .arg(&contracts)
        .env("ZISK_BACKEND_CACHE", &cache)
        .output()?;
    ensure!(
        output.status.success(),
        "backend cache validation failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let artifact_path = String::from_utf8(output.stdout)?;
    let artifact_path = Path::new(artifact_path.trim());
    ensure!(artifact_path.starts_with(cache.canonicalize()?));
    ensure!(!artifact_path.starts_with(contracts));
    let artifact: Value = serde_json::from_slice(&std::fs::read(artifact_path)?)?;
    let runtime = artifact["deployedBytecode"]["object"]
        .as_str()
        .context("backend runtime bytecode")?;
    let runtime = hex::decode(runtime.strip_prefix("0x").unwrap_or(runtime))?;
    ensure!(!runtime.is_empty());
    assert_eq!(
        provider.get_code_at(backend).await?.as_ref(),
        runtime.as_slice()
    );

    // Independently generated GPU proof: check both a valid range and a
    // changed commitment against the wrapper wired by this deployment.
    let vector: Value = serde_json::from_str(include_str!("data/zisk-120-range.json"))?;
    let mut public_inputs: Vec<U256> = serde_json::from_value(vector["public_inputs"].clone())?;
    let proof_bytes = hex::decode(
        vector["proof"]
            .as_str()
            .context("proof fixture")?
            .trim_start_matches("0x"),
    )?;
    let (proof_words, remainder) = proof_bytes.as_chunks::<32>();
    ensure!(
        remainder.is_empty(),
        "proof fixture must contain whole words"
    );
    let proof: Vec<U256> = proof_words
        .iter()
        .map(|word| U256::from_be_slice(word))
        .collect();
    let range = IZiskVerifier::new(range_wrapper_address, &provider);
    assert!(
        range
            .verify(public_inputs.clone(), proof.clone())
            .call()
            .await?
    );
    public_inputs[0] ^= U256::from(1);
    assert!(!range.verify(public_inputs, proof).call().await?);

    // Reuse the backend on an existing L1, then resume bootstrap. This must
    // persist the prerequisite just like the automatic deployment path.
    let reuse = tempfile::tempdir()?;
    let unused_cache = reuse.path().join("unused-backend-cache");
    std::fs::write(
        reuse.path().join("intent.yaml"),
        format!(
            "schema_version: 1\nmulti_proof_verifier: true\nl1_rpc_url: '{}'\nzisk_plonk_verifier_addr: '{backend:#x}'\nchains:\n  - chain_id: {CHAIN_ID}\n    da_mode: rollup\n",
            anvil.endpoint()
        ),
    )?;
    run(reuse.path(), &unused_cache, &["bootstrap", "--broadcast"])?;
    run(reuse.path(), &unused_cache, &["bootstrap", "--broadcast"])?;
    ensure!(
        !unused_cache.exists(),
        "reusing a backend must not prepare another one"
    );
    let reused_state: Value =
        serde_json::from_slice(&std::fs::read(reuse.path().join("state.json"))?)?;
    let reused_backend: Address = serde_json::from_value(
        reused_state["steps"]["zisk.plonk_verifier.deploy"]["verifier_address"].clone(),
    )?;
    assert_eq!(reused_backend, backend);

    let config: serde_yaml::Value =
        serde_yaml::from_slice(&std::fs::read(workdir.path().join("server.yaml"))?)?;
    assert_eq!(config["genesis"]["chain_id"].as_u64(), Some(CHAIN_ID));
    Ok(())
}
