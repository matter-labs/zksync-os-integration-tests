//! Reproduction test for a server bug: assertion failure in
//! `L1Subpool::on_canonical_state_change` (`lib/mempool/src/subpools/l1.rs:83`).
//!
//! The server crashes when processing L1 priority transactions because two
//! transactions with the same nonce but different `max_fee_per_gas` / `to_mint`
//! are compared with a strict equality assertion.
//!
//! This reproduces with both the combined `ecosystem init` and the split
//! `hub init` + `ctm init` + `chain init` flows.
//!
//! This test is expected to FAIL (server crash) until the bug is fixed.
//! Once fixed, remove `#[should_panic]` and verify the server stays running.

use anyhow::{Context, Result};
use integration_tests::anvil::Anvil;
use integration_tests::anvil::DEFAULT_ANVIL_PRIVATE_KEY;
use integration_tests::anvil_utils::fund_account;
use integration_tests::l1_state::WalletsFile;
use integration_tests::presets::load_current_preset;
use integration_tests::protocol_ops::EraContractsBackend;
use integration_tests::server_config::ServerConfigBuilder;
use std::fs;
use std::time::Duration;

const CHAIN_ID: u64 = 900;
const CHAIN_NAME: &str = "server_priority_bug";

fn extract_json(obj: &serde_json::Value, path: &str) -> Result<String> {
    let mut v = obj;
    for key in path.split('.') {
        v = v
            .get(key)
            .ok_or_else(|| anyhow::anyhow!("Missing key {:?} in path {:?}", key, path))?;
    }
    v.as_str()
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("Key {:?} is not a string", path))
}

async fn run_test() -> Result<()> {
    let preset = load_current_preset()?;
    let work_name = format!(
        "server_priority_bug_{}",
        uuid::Uuid::new_v4().to_string().get(..8).unwrap_or("run")
    );
    let backend = EraContractsBackend::from_preset(&preset, &work_name, &[])?;

    // Generate wallets.yaml and load keys from it
    let wallets_out = backend.work_path("wallets.yaml");
    backend.run(
        &["wallets-gen", "--chains", CHAIN_NAME, "--output", &wallets_out],
        None,
    )?;
    let wallets: WalletsFile = serde_yaml::from_str(
        &fs::read_to_string(backend.work_dir().join("wallets.yaml"))?,
    )?;
    let deployer_pk = &wallets.ecosystem.deployer.private_key;
    let deployer_addr = &wallets.ecosystem.deployer.address;
    let eco_owner = wallets.ecosystem.owner.as_ref().context("missing ecosystem.owner")?;
    let eco_owner_pk = &eco_owner.private_key;
    let eco_owner_addr = &eco_owner.address;
    let chain_w = wallets.chains.get(CHAIN_NAME).context("missing chain wallets")?;
    let chain_owner = chain_w.owner.as_ref().context("missing chain owner")?;
    let chain_owner_pk = &chain_owner.private_key;
    let chain_owner_addr = &chain_owner.address;
    let commit_pk = &chain_w.commit_operator.private_key;
    let commit_addr = &chain_w.commit_operator.address;
    let prove_pk = &chain_w.prove_operator.private_key;
    let prove_addr = &chain_w.prove_operator.address;
    let execute_pk = &chain_w.execute_operator.private_key;
    let execute_addr = &chain_w.execute_operator.address;

    let anvil = Anvil::spawn_fresh().await?;
    let l1_rpc = anvil.rpc_url().to_string();

    // Fund accounts
    for addr in [deployer_addr.as_str(), eco_owner_addr.as_str(), chain_owner_addr.as_str()] {
        fund_account(addr, "4000ether", &l1_rpc, DEFAULT_ANVIL_PRIVATE_KEY)?;
    }

    // ── hub init ──
    println!("=== hub init ===");
    let hub_out = backend.work_path("hub.init.json");
    backend.protocol_ops(&[
        "hub", "init",
        "--owner", &eco_owner_addr,
        "--private-key", &deployer_pk,
        "--owner-pk", &eco_owner_pk,
        "--l1-rpc-url", &l1_rpc,
        "--out", &hub_out, "-v",
    ])?;
    let hub_json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(backend.work_dir().join("hub.init.json"))?)?;
    let hub = hub_json.get("output").context("no output")?;
    let bridgehub = extract_json(hub, "deployed_addresses.bridgehub.bridgehub_proxy_addr")?;
    let create2 = extract_json(hub, "contracts.create2_factory_addr")?;

    // ── ctm init ──
    println!("=== ctm init ===");
    let ctm_out = backend.work_path("ctm.init.json");
    backend.protocol_ops(&[
        "ctm", "init",
        "--bridgehub", &bridgehub,
        "--vm-type", "zksyncos",
        "--private-key", &deployer_pk,
        "--bridgehub-owner-pk", &eco_owner_pk,
        "--bridgehub-admin-pk", &eco_owner_pk,
        "--create2-factory-addr", &create2,
        "--l1-rpc-url", &l1_rpc,
        "--out", &ctm_out, "-v",
    ])?;
    let ctm_json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(backend.work_dir().join("ctm.init.json"))?)?;
    let ctm = ctm_json.get("output").context("no output")?;
    let ctm_proxy = extract_json(ctm, "deployed_addresses.state_transition.state_transition_proxy_addr")?;
    let da_validator = extract_json(ctm, "deployed_addresses.blobs_zksync_os_l1_da_validator_addr")?;
    let bytecodes = extract_json(ctm, "deployed_addresses.state_transition.bytecodes_supplier_addr")?;

    // ── chain init ──
    println!("=== chain init ({CHAIN_ID}) ===");
    let chain_out = backend.work_path("chain.init.json");
    backend.protocol_ops(&[
        "chain", "init",
        "--ctm-proxy", &ctm_proxy,
        "--l1-da-validator", &da_validator,
        "--chain-id", &CHAIN_ID.to_string(),
        "--owner", &chain_owner_addr,
        "--commit-operator", &commit_addr,
        "--prove-operator", &prove_addr,
        "--execute-operator", &execute_addr,
        "--vm-type", "zksyncos",
        "--private-key", &deployer_pk,
        "--owner-pk", &chain_owner_pk,
        "--bridgehub-admin-pk", &eco_owner_pk,
        "--create2-factory-addr", &create2,
        "--l1-rpc-url", &l1_rpc,
        "--out", &chain_out, "-v",
    ])?;

    for addr in [&deployer_addr, &commit_addr, &prove_addr, &execute_addr] {
        fund_account(addr, "100ether", &l1_rpc, DEFAULT_ANVIL_PRIVATE_KEY)?;
    }

    // ── genesis ──
    let genesis_path = backend.work_path("genesis.json");
    let workdir = backend.repo_path("tools/zksync-os-genesis-gen");
    backend.run(
        &["zksync-os-genesis-gen", "--output-file", &genesis_path],
        Some(&workdir),
    )?;
    let genesis_host = backend.work_dir().join("genesis.json");

    // ── config ──
    let config_path = backend.work_dir().join("config.yaml");
    fs::write(
        &config_path,
        ServerConfigBuilder::new(
            &bridgehub, &bytecodes, &genesis_host, CHAIN_ID,
            commit_pk.as_str(), prove_pk.as_str(), execute_pk.as_str(),
        )
        .build(),
    )?;

    // ── start server — expected to crash ──
    println!("=== Starting server (expecting assertion failure in L1Subpool) ===");
    let result = integration_tests::server::ServerBuilder::new(preset, "server_priority_bug")
        .chain_name("server_priority_bug")
        .config_path(&config_path)
        .spawn(&anvil);

    match result {
        Ok(server) => {
            std::thread::sleep(Duration::from_secs(5));
            let running = server.is_running().unwrap_or(false);
            let _ = server.kill();
            anvil.kill()?;
            if running {
                anyhow::bail!(
                    "BUG FIXED: server stayed running. Remove #[should_panic] from this test."
                );
            }
            anyhow::bail!("Server crashed (priority tx assertion failure)");
        }
        Err(e) => {
            anvil.kill()?;
            anyhow::bail!("Server failed to start: {e:?}");
        }
    }
}

#[tokio::test]
#[should_panic(expected = "server_priority_bug")]
async fn test_server_priority_tx_assertion_bug() {
    run_test()
        .await
        .expect("server_priority_bug test failed");
}
