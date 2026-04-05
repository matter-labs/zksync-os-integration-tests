use anyhow::{Context, Result};
use integration_tests::anvil::Anvil;
use integration_tests::anvil::DEFAULT_ANVIL_PRIVATE_KEY;
use integration_tests::anvil_utils::fund_account;
use integration_tests::l1_state::{
    chain_config_path, load_ecosystem, load_wallets, resolve_l1_state,
};
use integration_tests::presets::load_current_preset;
use integration_tests::server::ServerBuilder;
use integration_tests::server_utils::{
    address_from_private_key, fund_l2_via_l1_deposit, wait_for_executed_batches_with_traffic,
};
use std::time::Duration;

async fn run_l1_settling_test() -> Result<()> {
    let preset = load_current_preset()?;
    let eco = load_ecosystem(&preset)?;

    anyhow::ensure!(
        !eco.l1_settling_chains.is_empty(),
        "No L1-settling chains in ecosystem"
    );
    let chain = &eco.l1_settling_chains[0];
    println!(
        "Testing L1-settling chain {} (diamond_proxy={})",
        chain.chain_id, chain.diamond_proxy
    );

    let wallets = load_wallets(&preset)?;
    let chain_wallets = wallets
        .chains
        .get(&chain.name)
        .ok_or_else(|| anyhow::anyhow!("No wallets for chain '{}'", chain.name))?;

    println!("\n=== Loading l1-state.json into Anvil ===");
    let state_path = resolve_l1_state(&preset, &eco)?;
    let anvil = Anvil::spawn_with_state(&state_path).await?;
    let l1_rpc_url = anvil.rpc_url().to_string();
    println!("Anvil ready at {l1_rpc_url}");

    let config_path = chain_config_path(&preset, &chain.name)?;
    anyhow::ensure!(
        config_path.exists(),
        "Chain config not found: {}",
        config_path.display()
    );

    println!("\n=== Funding L1 operators ===");
    for pk in [
        &chain_wallets.commit_operator.private_key,
        &chain_wallets.prove_operator.private_key,
        &chain_wallets.execute_operator.private_key,
    ] {
        let addr = address_from_private_key(pk)?;
        fund_account(&addr, "100ether", &l1_rpc_url, DEFAULT_ANVIL_PRIVATE_KEY)
            .with_context(|| format!("fund operator {addr}"))?;
    }
    let test_address = address_from_private_key(DEFAULT_ANVIL_PRIVATE_KEY)?;
    fund_account(
        &test_address,
        "10ether",
        &l1_rpc_url,
        DEFAULT_ANVIL_PRIVATE_KEY,
    )?;

    println!(
        "\n=== Starting server for L1-settling chain {} ===",
        chain.chain_id
    );
    let server = ServerBuilder::new(preset, "l1_settling")
        .config_path(&config_path)
        .spawn(&anvil)
        .map_err(|e| anyhow::anyhow!("Failed to start server: {:?}", e))?;

    let l2_rpc_url = server.rpc_url();
    let server_logs = server.logs_path();
    println!("Server ready at {l2_rpc_url}");

    println!("\n=== Funding L2 via deposit ===");
    let test_address = address_from_private_key(DEFAULT_ANVIL_PRIVATE_KEY)?;
    fund_l2_via_l1_deposit(
        &l1_rpc_url,
        &l2_rpc_url,
        &eco.bridgehub,
        chain.chain_id,
        &test_address,
        0.1,
        Duration::from_secs(120),
        Some(server_logs.as_path()),
    )
    .context("fund L2 via deposit")?;

    println!("\n=== Waiting for executed batches ===");
    let executed = wait_for_executed_batches_with_traffic(
        &l2_rpc_url,
        &l1_rpc_url,
        &chain.diamond_proxy,
        DEFAULT_ANVIL_PRIVATE_KEY,
        3,
        Duration::from_secs(180),
    )
    .context("wait for executed batches")?;

    println!(
        "\n=== L1-settling chain {} reached {} executed batches ===",
        chain.chain_id, executed
    );

    server
        .kill()
        .map_err(|e| anyhow::anyhow!("kill server: {:?}", e))?;
    anvil.kill()?;
    println!("\nTest passed!");
    Ok(())
}

#[tokio::test]
async fn test_l1_settling_chain() {
    run_l1_settling_test()
        .await
        .expect("l1_settling_test failed");
}
