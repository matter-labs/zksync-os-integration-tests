use anyhow::{Context, Result};
use integration_tests::anvil::Anvil;
use integration_tests::l1_state::{
    chain_config_path, load_ecosystem, load_wallets, resolve_l1_state,
};
use integration_tests::presets::load_current_preset;
use integration_tests::server::ServerBuilder;

async fn run_gateway_settling_test() -> Result<()> {
    let preset = load_current_preset()?;
    let eco = load_ecosystem(&preset)?;
    anyhow::ensure!(
        !eco.gateway_settling_chains.is_empty(),
        "No gateway-settling chains in ecosystem"
    );
    let gw = &eco.gateway;
    let chain = &eco.gateway_settling_chains[0];

    println!(
        "Gateway chain {} (diamond_proxy={})",
        gw.chain_id, gw.diamond_proxy
    );
    println!(
        "Gateway-settling chain {} (diamond_proxy={})",
        chain.chain_id, chain.diamond_proxy
    );

    // Load wallets (used by server configs for operator keys)
    let _wallets = load_wallets(&preset)?;

    println!("\n=== Loading l1-state.json into Anvil ===");
    let state_path = resolve_l1_state(&preset, &eco)?;
    let anvil = Anvil::spawn_with_state(&state_path).await?;
    let l1_rpc_url = anvil.rpc_url().to_string();
    println!("Anvil ready at {l1_rpc_url}");

    // Resolve config paths from state directory
    let gw_config_path = chain_config_path(&preset, &gw.name)?;
    let chain_config = chain_config_path(&preset, &chain.name)?;
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
    println!("\n=== Starting gateway server (chain {}) ===", gw.chain_id);
    let gw_server = ServerBuilder::new(preset.clone(), "gateway_settling")
        .chain_name(&gw.name)
        .ephemeral()
        .config_path(&gw_config_path)
        .spawn(&anvil)
        .map_err(|e| anyhow::anyhow!("Failed to start gateway server: {:?}", e))?;
    let gw_l2_rpc = gw_server.rpc_url();
    println!("Gateway server ready at {gw_l2_rpc}");

    // ---- Gateway-settling chain server (fresh, no ephemeral state) ----
    println!(
        "\n=== Starting gateway-settling chain {} ===",
        chain.chain_id
    );
    let chain_server = ServerBuilder::new(preset, "gateway_settling")
        .chain_name(&chain.name)
        .config_path(&chain_config)
        .gateway_rpc_url(&gw_l2_rpc)
        .diamond_proxy_addr(&chain.diamond_proxy)
        .spawn(&anvil)
        .map_err(|e| anyhow::anyhow!("Failed to start chain server: {:?}", e))?;
    let chain_l2_rpc = chain_server.rpc_url();
    println!("Chain server ready at {chain_l2_rpc}");

    // Verify the chain produces and settles batches end-to-end.
    println!("\n=== Waiting for executed batches on gateway-settling chain ===");
    chain_server
        .wait_for_executed_batches_with_traffic()
        .context("gateway-settling chain batches")?;

    chain_server
        .kill()
        .map_err(|e| anyhow::anyhow!("kill chain server: {:?}", e))?;
    gw_server
        .kill()
        .map_err(|e| anyhow::anyhow!("kill gateway server: {:?}", e))?;
    anvil.kill()?;

    println!("\nTest passed!");
    Ok(())
}

#[tokio::test]
async fn test_gateway_settling_chain() {
    run_gateway_settling_test()
        .await
        .expect("gateway_settling_test failed");
}
