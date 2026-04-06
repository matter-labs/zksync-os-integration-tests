use anyhow::{Context, Result};
use integration_tests::anvil::Anvil;
use integration_tests::anvil::DEFAULT_ANVIL_PRIVATE_KEY;
use integration_tests::anvil_utils::fund_account;
use integration_tests::l1_state::{
    chain_config_path, load_ecosystem, load_wallets, resolve_l1_state,
};
use integration_tests::presets::load_current_preset;
use integration_tests::protocol_ops::EraContractsBackend;
use integration_tests::server::ServerBuilder;
use integration_tests::server_utils::{
    address_from_private_key, fund_l2_via_l1_deposit_ex, wait_for_executed_batches_with_traffic,
};
use std::time::Duration;

async fn run_gateway_settling_test() -> Result<()> {
    let preset = load_current_preset()?;
    let contracts_backend = EraContractsBackend::from_preset(&preset, "gateway_settling", &[])?;
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

    // Load wallets
    let wallets = load_wallets(&preset)?;
    let gw_wallets = wallets
        .chains
        .get(&gw.name)
        .ok_or_else(|| anyhow::anyhow!("No wallets for gateway chain '{}'", gw.name))?;
    let chain_wallets = wallets
        .chains
        .get(&chain.name)
        .ok_or_else(|| anyhow::anyhow!("No wallets for chain '{}'", chain.name))?;

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

    // Fund all operators on L1
    println!("\n=== Funding L1 operators ===");
    for w in [gw_wallets, chain_wallets] {
        for pk in [
            &w.commit_operator.private_key,
            &w.prove_operator.private_key,
            &w.execute_operator.private_key,
        ] {
            let addr = address_from_private_key(pk)?;
            fund_account(&addr, "100ether", &l1_rpc_url, DEFAULT_ANVIL_PRIVATE_KEY)?;
        }
    }
    let test_address = address_from_private_key(DEFAULT_ANVIL_PRIVATE_KEY)?;
    fund_account(
        &test_address,
        "10ether",
        &l1_rpc_url,
        DEFAULT_ANVIL_PRIVATE_KEY,
    )?;

    // ---- Gateway server (ephemeral mode with archived RocksDB) ----
    println!("\n=== Starting gateway server (chain {}) ===", gw.chain_id);
    let gw_server = ServerBuilder::new(preset.clone(), "gateway_settling")
        .chain_name(&gw.name)
        .ephemeral()
        .config_path(&gw_config_path)
        .spawn(&anvil)
        .map_err(|e| anyhow::anyhow!("Failed to start gateway server: {:?}", e))?;
    let gw_l2_rpc = gw_server.rpc_url();
    let gw_log = gw_server.logs_path();
    println!("Gateway server ready at {gw_l2_rpc}");

    // The gateway uses ZK as its base token. Read it on-chain, then mint
    // and approve so L1→L2 deposits can pay in ZK.
    println!("\n=== Minting & approving base token for deposits ===");
    let test_address = address_from_private_key(DEFAULT_ANVIL_PRIVATE_KEY)?;
    let zk_token = contracts_backend
        .cast(&[
            "call",
            &eco.bridgehub,
            "baseToken(uint256)(address)",
            &gw.chain_id.to_string(),
            "--rpc-url",
            &l1_rpc_url,
        ])?
        .trim()
        .to_string();
    let base_token_is_eth = zk_token == "0x0000000000000000000000000000000000000001";
    println!("  Base token: {zk_token} (is_eth={base_token_is_eth})");
    if !base_token_is_eth {
        let zk_mint = "1000000000000000000000000000000000000000";
        // Fund the deployer (who has minting rights) so it can pay for gas.
        fund_account(
            &wallets.ecosystem.deployer.address,
            "10ether",
            &l1_rpc_url,
            DEFAULT_ANVIL_PRIVATE_KEY,
        )?;
        contracts_backend
            .cast(&[
                "send",
                &zk_token,
                "mint(address,uint256)",
                &test_address,
                zk_mint,
                "--private-key",
                &wallets.ecosystem.deployer.private_key,
                "--rpc-url",
                &l1_rpc_url,
            ])
            .context("mint base token to test account")?;
        // Approve bridgehub + shared bridge + NTV
        let shared_bridge = contracts_backend
            .cast(&[
                "call",
                &eco.bridgehub,
                "sharedBridge()(address)",
                "--rpc-url",
                &l1_rpc_url,
            ])?
            .trim()
            .to_string();
        let ntv = contracts_backend
            .cast(&[
                "call",
                &shared_bridge,
                "nativeTokenVault()(address)",
                "--rpc-url",
                &l1_rpc_url,
            ])?
            .trim()
            .to_string();
        for spender in [eco.bridgehub.as_str(), shared_bridge.as_str(), ntv.as_str()] {
            contracts_backend
                .cast(&[
                    "send",
                    &zk_token,
                    "approve(address,uint256)",
                    spender,
                    zk_mint,
                    "--private-key",
                    DEFAULT_ANVIL_PRIVATE_KEY,
                    "--rpc-url",
                    &l1_rpc_url,
                ])
                .context("approve base token")?;
        }
    }

    // Fund gateway L2
    println!("\n=== Funding gateway L2 ===");
    fund_l2_via_l1_deposit_ex(
        &l1_rpc_url,
        &gw_l2_rpc,
        &eco.bridgehub,
        gw.chain_id,
        &test_address,
        0.1,
        Duration::from_secs(120),
        Some(gw_log.as_path()),
        base_token_is_eth,
    )
    .context("fund gateway L2 test account")?;

    println!("\n=== Funding gateway L2 for chain operators ===");
    for addr in [
        &chain_wallets.commit_operator.address,
        &chain_wallets.prove_operator.address,
        &chain_wallets.execute_operator.address,
    ] {
        fund_l2_via_l1_deposit_ex(
            &l1_rpc_url,
            &gw_l2_rpc,
            &eco.bridgehub,
            gw.chain_id,
            addr,
            5.0,
            Duration::from_secs(120),
            Some(gw_log.as_path()),
            base_token_is_eth,
        )
        .context("fund gateway L2 for chain operator")?;
    }

    println!("\n=== Waiting for gateway batches ===");
    wait_for_executed_batches_with_traffic(
        &gw_l2_rpc,
        &l1_rpc_url,
        &gw.diamond_proxy,
        DEFAULT_ANVIL_PRIVATE_KEY,
        3,
        Duration::from_secs(180),
    )
    .context("gateway batches")?;

    // ---- Gateway-settling chain server (fresh, no ephemeral state) ----
    println!(
        "\n=== Starting gateway-settling chain {} ===",
        chain.chain_id
    );
    let chain_server = ServerBuilder::new(preset, "gateway_settling")
        .chain_name(&chain.name)
        .config_path(&chain_config)
        .gateway_rpc_url(&gw_l2_rpc)
        .spawn(&anvil)
        .map_err(|e| anyhow::anyhow!("Failed to start chain server: {:?}", e))?;
    let chain_l2_rpc = chain_server.rpc_url();
    println!("Chain server ready at {chain_l2_rpc}");

    // Wait for the chain to produce blocks
    println!("\n=== Waiting for chain to produce blocks ===");
    let start = std::time::Instant::now();
    let min_blocks = 5u64;
    loop {
        if start.elapsed() > Duration::from_secs(120) {
            anyhow::bail!("Timed out waiting for chain to produce blocks");
        }
        if let Ok(raw) = contracts_backend.cast(&["block-number", "--rpc-url", &chain_l2_rpc]) {
            if let Ok(n) = raw.trim().parse::<u64>() {
                if n >= min_blocks {
                    println!("  Chain {} reached block {}", chain.chain_id, n);
                    break;
                }
            }
        }
        std::thread::sleep(Duration::from_secs(2));
    }

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
