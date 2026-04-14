use anyhow::{Context, Result};
use integration_tests::anvil::Anvil;
use integration_tests::l1_state::{
    chain_config_path, load_ecosystem, load_wallets, resolve_l1_state,
};
use integration_tests::presets::load_current_preset;
use integration_tests::server::ServerBuilder;

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

    let _wallets = load_wallets(&preset)?;

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

    println!(
        "\n=== Starting server for L1-settling chain {} ===",
        chain.chain_id
    );
    // `generate-l1-state` pre-queued an L1→L2 deposit for
    // DEFAULT_ANVIL_PRIVATE_KEY; the server processes it as it spins up,
    // so we do not need a test-side `fund_account_via_l1_deposit` here.
    let server = ServerBuilder::new(preset, &chain.name)
        .spawn(&anvil)
        .map_err(|e| anyhow::anyhow!("Failed to start server: {:?}", e))?;

    let l2_rpc_url = server.rpc_url();
    println!("Server ready at {l2_rpc_url}");

    println!("\n=== Waiting for executed batches ===");
    let executed = server
        .wait_for_executed_batches_with_traffic()
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
