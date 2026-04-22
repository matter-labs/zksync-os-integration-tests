use anyhow::{Context, Result};
use integration_tests::anvil::Anvil;
use integration_tests::l1_state::{chain_config_path, load_ecosystem, resolve_l1_state};
use integration_tests::presets::load_current_preset;
use integration_tests::server::ServerBuilder;

async fn run_gateway_settling_test() -> Result<()> {
    integration_tests::server::get_or_create_run_id("gateway_settling");
    let preset = load_current_preset()?;
    let eco = load_ecosystem(&preset)?;

    println!("\n=== Loading l1-state.json into Anvil ===");
    let state_path = resolve_l1_state(&preset)?;
    let anvil = Anvil::spawn_with_state(&state_path).await?;
    let l1_rpc_url = anvil.rpc_url().to_string();
    println!("Anvil ready at {l1_rpc_url}");

    let (chain_name, chain_id) = eco.chain_a();

    println!("Gateway chain {}", eco.gateway_chain_id());
    println!("Gateway-settling chain {chain_id} ({chain_name})");

    // Resolve config paths from state directory
    let gw_config_path =
        chain_config_path(&preset, integration_tests::l1_state::GATEWAY_CHAIN_NAME)?;
    let chain_config = chain_config_path(&preset, chain_name)?;
    anyhow::ensure!(
        gw_config_path.exists(),
        "Gateway config not found: {}",
        gw_config_path.display()
    );
    anyhow::ensure!(
        chain_config.exists(),
        "Chain config not found: {}",
        chain_config.display()
    );

    // ---- Gateway server (ephemeral mode with archived RocksDB) ----
    println!(
        "\n=== Starting gateway server (chain {}) ===",
        eco.gateway_chain_id()
    );
    let gw_server = ServerBuilder::new(
        preset.clone(),
        integration_tests::l1_state::GATEWAY_CHAIN_NAME,
    )
    .ephemeral()
    .config_path(&gw_config_path)
    .spawn(&anvil)
    .map_err(|e| anyhow::anyhow!("Failed to start gateway server: {:?}", e))?;
    let gw_l2_rpc = gw_server.rpc_url();
    println!("Gateway server ready at {gw_l2_rpc}");

    // ---- Gateway-settling chain server (fresh, no ephemeral state) ----
    println!("\n=== Starting gateway-settling chain {} ===", chain_id);
    let chain_server = ServerBuilder::new(preset, chain_name)
        .gateway_rpc_url(&gw_l2_rpc)
        .spawn(&anvil)
        .map_err(|e| anyhow::anyhow!("Failed to start chain server: {:?}", e))?;
    let chain_l2_rpc = chain_server.rpc_url();
    println!("Chain server ready at {chain_l2_rpc}");

    // Verify the chain produces and settles batches end-to-end.
    println!("\n=== Waiting for executed batches on gateway-settling chain ===");
    chain_server
        .wait_for_traffic_tx_executed_on_l1()
        .context("gateway-settling chain batches")?;

    let _ = chain_server.kill();
    let _ = gw_server.kill();
    let _ = anvil.kill();

    println!("\nTest passed!");
    Ok(())
}

#[tokio::test]
async fn test_gateway_settling_chain() {
    run_gateway_settling_test()
        .await
        .expect("gateway_settling_test failed");
}
