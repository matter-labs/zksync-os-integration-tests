use anyhow::{Context, Result};
use integration_tests::anvil::Anvil;
use integration_tests::l1_state::{chain_config_path, load_ecosystem, resolve_l1_state};
use integration_tests::presets::load_current_preset;
use integration_tests::server::ServerBuilder;

async fn run_l1_settling_test() -> Result<()> {
    integration_tests::server::get_or_create_run_id("l1_settling");
    let preset = load_current_preset()?;
    let eco = load_ecosystem(&preset)?;

    println!("\n=== Loading l1-state.json into Anvil ===");
    let state_path = resolve_l1_state(&preset)?;
    let anvil = Anvil::spawn_with_state(&state_path).await?;
    let l1_rpc_url = anvil.rpc_url().to_string();
    println!("Anvil ready at {l1_rpc_url}");

    let (chain_name, chain_id) = eco.l1_settling();
    println!("Testing L1-settling chain {chain_id} ({chain_name})");

    let config_path = chain_config_path(&preset, chain_name)?;
    anyhow::ensure!(
        config_path.exists(),
        "Chain config not found: {}",
        config_path.display()
    );

    println!(
        "\n=== Starting server for L1-settling chain {} ===",
        chain_id
    );
    // `generate-l1-state` pre-queued an L1→L2 deposit for
    // DEFAULT_ANVIL_PRIVATE_KEY; the server processes it as it spins up,
    // so we do not need a test-side `fund_account_via_l1_deposit` here.
    let server = ServerBuilder::new(preset, chain_name)
        .spawn(&anvil)
        .map_err(|e| anyhow::anyhow!("Failed to start server: {:?}", e))?;

    let l2_rpc_url = server.rpc_url();
    println!("Server ready at {l2_rpc_url}");

    println!("\n=== Waiting for executed batches ===");
    let executed = server
        .wait_for_traffic_tx_executed_on_l1()
        .context("wait for executed batches")?;

    println!(
        "\n=== L1-settling chain {} reached {} executed batches ===",
        chain_id, executed
    );

    let _ = server.kill();
    let _ = anvil.kill();
    println!("\nTest passed!");
    Ok(())
}

#[tokio::test]
async fn test_l1_settling_chain() {
    run_l1_settling_test()
        .await
        .expect("l1_settling_test failed");
}
