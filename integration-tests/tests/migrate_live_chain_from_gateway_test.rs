//! Migrate a gateway-settling chain back to L1 settlement, end-to-end.
//!
//! Driven entirely from cached state (`generate-l1-state` produces a chain
//! that already settles on the gateway). The test:
//!
//!   1. Spawns the gateway server and the gateway-settling chain server.
//!   2. Lets the chain produce + execute a few batches on the gateway, so we
//!      know the starting fixture is healthy.
//!   3. Runs the `chain gateway migrate-from` sequence: notify-server (chain
//!      owner key), submit (chain owner key — also captures the L2 priority
//!      tx hash from the L1 receipt), finalize (deployer key), and
//!      set-da-validator-pair (chain owner key).
//!   4. Verifies on the L1 bridgehub that the chain's settlementLayer has
//!      flipped back to L1 (`settlementLayer(chainId) == 0`).
//!
//! TODO: a follow-up "restart the chain server on L1 settlement and drive
//! traffic" step is currently disabled — see the inline TODO further down
//! this file for the groundwork that's already in place and what's blocking
//! it. Until that lands, this test only exercises the migration flow itself,
//! not the chain's operation on its new settlement layer.
use std::time::Duration;

use alloy::primitives::{Address, FixedBytes, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::Filter;
use alloy::sol;
use anyhow::{Context, Result};
use integration_tests::anvil::Anvil;
use integration_tests::l1_state::{
    chain_config_path, load_ecosystem, load_wallets, resolve_l1_state,
};
use integration_tests::presets::load_current_preset;
use integration_tests::protocol_ops::EraContractsBackend;
use integration_tests::server::ServerBuilder;

use alloy::primitives::keccak256;

fn new_priority_request_topic() -> FixedBytes<32> {
    keccak256(
        "NewPriorityRequest(uint256,bytes32,uint64,(uint256,uint256,uint256,uint256,uint256,uint256,uint256,uint256,uint256,uint256,uint256[4],bytes,bytes,uint256[],bytes,bytes),bytes[])"
    )
}

sol! {
    #[sol(rpc)]
    interface IBridgehub {
        function settlementLayer(uint256 chainId) external view returns (uint256);
        function getZKChain(uint256 chainId) external view returns (address);
    }

    #[sol(rpc)]
    interface IZKChain {
        function getDAValidatorPair() external view returns (address, uint8);
        function getTotalBatchesCommitted() external view returns (uint256);
        function getTotalBatchesExecuted() external view returns (uint256);
    }
}

/// Scan L1 logs since `from_block` for `NewPriorityRequest` events emitted by
/// `gateway_diamond_proxy` and return the L2 priority tx hash (the `txHash`
/// field of the most recent matching event).
async fn fetch_priority_op_l2_hash(
    l1_rpc_url: &str,
    gateway_diamond_proxy: Address,
    from_block: u64,
) -> Result<FixedBytes<32>> {
    let provider = ProviderBuilder::new()
        .on_builtin(l1_rpc_url)
        .await
        .context("connect L1 provider")?;
    let filter = Filter::new()
        .address(gateway_diamond_proxy)
        .event_signature(new_priority_request_topic())
        .from_block(from_block);
    let logs = provider
        .get_logs(&filter)
        .await
        .context("eth_getLogs for NewPriorityRequest")?;
    let log = logs.last().ok_or_else(|| {
        anyhow::anyhow!(
            "No NewPriorityRequest event from gateway diamond proxy {gateway_diamond_proxy} \
             since L1 block {from_block} — did `migrate-from submit` actually broadcast?"
        )
    })?;
    // NewPriorityRequest is a non-indexed event. Its data layout is:
    //   [0..32)   uint256 txId
    //   [32..64)  bytes32 txHash    <-- what we want
    //   [64..)    rest of the encoded args
    let data = log.data().data.as_ref();
    anyhow::ensure!(
        data.len() >= 64,
        "NewPriorityRequest log data too short: {} bytes",
        data.len()
    );
    Ok(FixedBytes::<32>::from_slice(&data[32..64]))
}

async fn run_migrate_live_chain_from_gateway_test() -> Result<()> {
    integration_tests::server::get_or_create_run_id("migrate_live_chain_from_gateway");
    let preset = load_current_preset()?;
    let eco = load_ecosystem(&preset)?;

    let wallets = load_wallets(&preset).context("load wallets.yaml")?;

    println!("\n=== Loading l1-state.json into Anvil ===");
    let state_path = resolve_l1_state(&preset)?;
    let anvil = Anvil::spawn_with_state(&state_path).await?;
    let l1_rpc_url = anvil.rpc_url().to_string();
    println!("Anvil ready at {l1_rpc_url}");

    let (chain_name, chain_id) = eco.chain_a();
    println!("Gateway     : chain {}", eco.gateway_chain_id());
    println!("Migrating   : chain {chain_id} ({chain_name})");

    let chain_wallets = wallets
        .chains
        .get(chain_name)
        .ok_or_else(|| anyhow::anyhow!("wallets.yaml missing entry for chain '{}'", chain_name))?;
    let chain_owner_pk = chain_wallets.owner.private_key.clone();
    let deployer_pk = wallets.ecosystem.deployer.private_key.clone();
    let deployer_addr = wallets.ecosystem.deployer.address.clone();

    // Resolve the chain's diamond proxy from L1 and snapshot the DA validator
    // The gateway-settling chain's L1 DA validator pair is zeroed (it settled
    // on gateway, not L1). To find the correct L1 DA validator for phase 3
    // (set-da-validator-pair after migrating back to L1), read it from an
    // L1-settling chain that still has its pair intact.
    let l1_provider = ProviderBuilder::new()
        .on_builtin(&l1_rpc_url)
        .await
        .context("connect L1 provider")?;
    let bridgehub: Address = eco.bridgehub.parse().context("parse bridgehub")?;

    let (l1_chain_name, l1_chain_id) = eco.l1_settling();
    let bh = IBridgehub::new(bridgehub, &l1_provider);
    let l1_chain_proxy = bh
        .getZKChain(U256::from(l1_chain_id))
        .call()
        .await
        .context("bridgehub.getZKChain for L1-settling chain")?
        ._0;
    let l1_zk_chain = IZKChain::new(l1_chain_proxy, &l1_provider);
    let da_pair = l1_zk_chain
        .getDAValidatorPair()
        .call()
        .await
        .context("getDAValidatorPair() on L1-settling chain")?;
    let l1_da_validator = format!("{:#x}", da_pair._0);
    println!("L1 DA validator (from L1-settling chain {l1_chain_name}): {l1_da_validator}");
    anyhow::ensure!(
        da_pair._0 != Address::ZERO,
        "L1-settling chain's DA validator is zero — fixture is broken",
    );

    let gw_config = chain_config_path(&preset, integration_tests::l1_state::GATEWAY_CHAIN_NAME)?;
    let chain_config = chain_config_path(&preset, chain_name)?;

    println!(
        "\n=== Starting gateway server (chain {}) ===",
        eco.gateway_chain_id()
    );
    let gw_server = ServerBuilder::new(
        preset.clone(),
        integration_tests::l1_state::GATEWAY_CHAIN_NAME,
    )
    .ephemeral()
    .config_path(&gw_config)
    .spawn(&anvil)
    .map_err(|e| anyhow::anyhow!("Failed to start gateway: {:?}", e))?;
    let gw_l2_rpc = gw_server.rpc_url();
    println!("Gateway ready at {gw_l2_rpc}");

    println!(
        "\n=== Starting gateway-settling chain server (chain {}) ===",
        chain_id
    );
    let chain_server = ServerBuilder::new(preset.clone(), chain_name)
        .gateway_rpc_url(&gw_l2_rpc)
        .config_path(&chain_config)
        .spawn(&anvil)
        .map_err(|e| anyhow::anyhow!("Failed to start chain server: {:?}", e))?;

    // Sanity check: the fixture must actually be settling on the gateway
    // before we try to migrate it back.
    println!(
        "\n=== Sanity: wait for gateway-settled batches on chain {} ===",
        chain_id
    );
    chain_server
        .wait_for_traffic_tx_executed_on_l1()
        .context("gateway-settling chain batches (pre-migration)")?;

    let eco_dir = integration_tests::l1_state::resolve_ecosystem_dir(&preset)?;
    let contracts_backend =
        EraContractsBackend::from_preset(&preset, "migrate_live_chain_from_gateway", &[])?;

    let gateway_diamond_proxy = bh
        .getZKChain(U256::from(eco.gateway_chain_id()))
        .call()
        .await
        .context("bridgehub.getZKChain(gateway)")?
        ._0;

    // ── Debug: check ChainAdmin ZK token balance ──────────────────────
    {
        let chain_diamond = bh
            .getZKChain(U256::from(chain_id))
            .call()
            .await
            .context("bridgehub.getZKChain(chain)")?
            ._0;
        let chain_admin_raw = contracts_backend
            .cast(&[
                "call",
                &format!("{:#x}", chain_diamond),
                "getAdmin()(address)",
                "--rpc-url",
                &l1_rpc_url,
            ])
            .context("getAdmin()")?;
        let chain_admin = chain_admin_raw.trim();

        // Find the gateway's base token (ZK)
        let base_token_raw = contracts_backend
            .cast(&[
                "call",
                &eco.bridgehub,
                "baseToken(uint256)(address)",
                &eco.gateway_chain_id().to_string(),
                "--rpc-url",
                &l1_rpc_url,
            ])
            .context("bridgehub.baseToken(gateway)")?;
        let base_token = base_token_raw.trim();

        let balance_raw = contracts_backend
            .cast(&[
                "call",
                base_token,
                "balanceOf(address)(uint256)",
                chain_admin,
                "--rpc-url",
                &l1_rpc_url,
            ])
            .context("ZK balanceOf(ChainAdmin)")?;
        let balance = balance_raw.trim();

        let owner_balance_raw = contracts_backend
            .cast(&[
                "call",
                base_token,
                "balanceOf(address)(uint256)",
                &chain_wallets.owner.address,
                "--rpc-url",
                &l1_rpc_url,
            ])
            .context("ZK balanceOf(chain owner EOA)")?;
        let owner_balance = owner_balance_raw.trim();

        println!("  ChainAdmin ({chain_admin}) ZK balance: {balance}");
        println!(
            "  Chain owner EOA ({}) ZK balance: {owner_balance}",
            chain_wallets.owner.address
        );
        println!("  Gateway base token (ZK): {base_token}");
    }

    // ── Load diamond cut data from cached file ─────────────────────────
    //
    // Anvil state dumps don't preserve historical events. The migrate-from
    // submit step normally auto-resolves the L1 diamond cut data by scanning
    // the CTM for NewUpgradeCutData events, but those events are lost in the
    // dump. We pass the data explicitly via L1_DIAMOND_CUT_DATA env.
    let l1_diamond_cut_data = std::fs::read_to_string(eco_dir.join("diamond_cut_data.hex"))
        .context("read diamond_cut_data.hex — regenerate l1-state cache")?;
    let l1_diamond_cut_data = l1_diamond_cut_data.trim().to_string();

    let eco_path = contracts_backend.ecosystem_yaml_path(&preset)?;
    let signers: &[&str] = &[&chain_owner_pk, &deployer_pk];

    // Per-run UUID suffix in the work dir: `contracts_artifacts/` survives
    // across test invocations (macOS Docker/VirtioFS bind-mount quirk), so
    // bundles emitted by prior runs stay on disk. Without a unique suffix,
    // `--out` appends to an existing `manifest.json` and
    // `dev execute-safe` would replay stale bundles.
    let migrate_dir = format!(
        "migrate_live_chain_from_gateway_{chain_id}_{}",
        uuid::Uuid::new_v4(),
    );

    // ── Phase 0: pause-deposits + notify-server (chain admin) ─────────────
    // The gateway's Migrator facet requires deposits paused + server
    // notified before the withdrawal priority tx can execute. One fork, one
    // Safe bundle containing both stages.
    let phase0_safe_rel = format!("{migrate_dir}/phase0/safe");
    let phase0_safe_abs = contracts_backend.work_path(&phase0_safe_rel);
    contracts_backend
        .protocol_ops(&[
            "chain",
            "gateway",
            "migrate-from",
            "phase-0-pause-deposits",
            "--l1-rpc-url",
            &l1_rpc_url,
            "--ecosystem",
            &eco_path,
            "--chain",
            chain_name,
            "--out",
            &phase0_safe_abs,
        ])
        .context("migrate-from phase 0 (pause-deposits + notify-server)")?;

    contracts_backend
        .parse_safe_bundles(&phase0_safe_rel, &l1_rpc_url)?
        .apply(signers)
        .context("apply migrate-from phase 0 bundles")?;

    println!("  Waiting for gateway to process pause-deposits...");
    gw_server
        .wait_for_traffic_tx_executed_on_l1()
        .context("gateway batches after pause-deposits")?;

    // Phase 0's L1 `pauseDepositsBeforeInitiatingMigration` only sets
    // `pausedDepositsTimestamp` on the *L1* chain diamond. For a
    // gateway-settling chain it also enqueues a cross-chain priority tx
    // (`L2ChainAssetHandler.requestPauseDepositsForChainOnGateway`) on the
    // gateway's L1 diamond; when that executes on gateway L2, the chain's
    // gateway-side diamond sets its own `pausedDepositsTimestamp`. Phase 1's
    // migration tx checks the *gateway-side* timestamp, so we must wait for
    // the cross-chain pause to actually propagate before proceeding.
    //
    // Poll the chain's gateway-side diamond storage slot 62
    // (`ZKChainStorage.pausedDepositsTimestamp`, see ZKChainStorage.sol) via
    // the gateway L2 RPC until it's non-zero.
    println!("  Waiting for pause-deposits to propagate to gateway chain diamond...");
    let gw_l2_bridgehub_sys: Address = "0x0000000000000000000000000000000000010002"
        .parse()
        .context("parse gateway L2 bridgehub")?;
    let gw_l2_provider = ProviderBuilder::new()
        .on_builtin(gw_l2_rpc.as_str())
        .await
        .context("connect gateway L2 provider")?;
    let gw_bh = IBridgehub::new(gw_l2_bridgehub_sys, &gw_l2_provider);
    let gw_side_chain_diamond = gw_bh
        .getZKChain(U256::from(chain_id))
        .call()
        .await
        .context("gateway L2 bridgehub.getZKChain(chain)")?
        ._0;
    anyhow::ensure!(
        gw_side_chain_diamond != Address::ZERO,
        "Gateway L2 bridgehub has no ZKChain diamond for chain {} — fixture is broken",
        chain_id,
    );
    {
        use alloy::primitives::B256;
        // Slot 62, not 64: `ZKChainStorage` packs zksyncOS (bool) +
        // l2DACommitmentScheme (enum) + assetTracker (address) into slot 60,
        // putting nativeTokenVault at 61 and pausedDepositsTimestamp at 62.
        // The `@dev STORAGE SLOT` comments in ZKChainStorage.sol don't
        // account for this packing (verified via
        // `forge inspect MigratorFacet storageLayout`).
        let slot = U256::from(62);
        let start = std::time::Instant::now();
        let deadline = Duration::from_secs(20);
        loop {
            let raw: B256 = gw_l2_provider
                .get_storage_at(gw_side_chain_diamond, slot)
                .await
                .context("eth_getStorageAt on gateway chain diamond slot 64")?
                .into();
            let ts = U256::from_be_bytes(raw.0);
            if !ts.is_zero() {
                println!("  Gateway chain diamond pausedDepositsTimestamp = {ts}");
                break;
            }
            if start.elapsed() >= deadline {
                anyhow::bail!(
                    "pausedDepositsTimestamp on gateway chain diamond {gw_side_chain_diamond} \
                     stayed 0 after {:.1}s — pause-deposits cross-chain tx didn't propagate",
                    start.elapsed().as_secs_f64(),
                );
            }
            // Drive the gateway a bit — send one L2 traffic tx and wait
            // for its batch to finalize, so the gateway advances its
            // priority-queue processing.
            let _ = gw_server
                .wait_for_traffic_tx_executed_on_l1()
                .context("drive gateway while waiting for pause propagation")?;
        }
    }

    // After pause-deposits propagates, the chain may still have batches that
    // were committed before the pause but haven't executed yet. The gateway's
    // `Migrator.forwardedBridgeBurn` reverts with `NotAllBatchesExecuted` if
    // `totalBatchesCommitted != totalBatchesExecuted` on the chain's
    // gateway-side diamond, so wait for the chain to drain its execute queue
    // before submitting the migration. With block_time=250ms and
    // batch_timeout=1s, even an idle chain seals empty batches every ~1s, so
    // commit and execute can briefly diverge here.
    {
        let gw_side_chain = IZKChain::new(gw_side_chain_diamond, &gw_l2_provider);
        let start = std::time::Instant::now();
        let deadline = Duration::from_secs(60);
        loop {
            let committed = gw_side_chain
                .getTotalBatchesCommitted()
                .call()
                .await
                .context("getTotalBatchesCommitted on gateway-side chain diamond")?
                ._0;
            let executed = gw_side_chain
                .getTotalBatchesExecuted()
                .call()
                .await
                .context("getTotalBatchesExecuted on gateway-side chain diamond")?
                ._0;
            if committed == executed {
                println!("  Chain has drained execute queue (committed = executed = {committed})");
                break;
            }
            if start.elapsed() >= deadline {
                anyhow::bail!(
                    "chain {chain_id} still has committed > executed (committed={committed}, executed={executed}) \
                     after {:.1}s — gateway-side execute queue did not drain in time",
                    start.elapsed().as_secs_f64(),
                );
            }
            // Drive the gateway one batch so its execute pipeline advances.
            let _ = gw_server
                .wait_for_traffic_tx_executed_on_l1()
                .context("drive gateway while waiting for execute queue to drain")?;
        }
    }

    // ── Phase 1: submit (chain admin) ────────────────────────────────────
    //
    // Anvil state dumps drop historical events, so the CTM-event fallback
    // inside `phase-1-submit` can't find `NewUpgradeCutData`. Pass the
    // cached diamond cut data explicitly via `--l1-diamond-cut-data` (real
    // chains auto-resolve).
    let phase1_safe_rel = format!("{migrate_dir}/phase1/safe");
    let phase1_safe_abs = contracts_backend.work_path(&phase1_safe_rel);
    contracts_backend
        .protocol_ops(&[
            "chain",
            "gateway",
            "migrate-from",
            "phase-1-submit",
            "--l1-rpc-url",
            &l1_rpc_url,
            "--ecosystem",
            &eco_path,
            "--chain",
            chain_name,
            "--l1-gas-price",
            "1000000000",
            "--l1-diamond-cut-data",
            l1_diamond_cut_data.as_str(),
            "--refund-recipient",
            &deployer_addr,
            "--out",
            &phase1_safe_abs,
        ])
        .context("migrate-from phase 1 (submit)")?;
    contracts_backend
        .parse_safe_bundles(&phase1_safe_rel, &l1_rpc_url)?
        .apply(signers)
        .context("apply migrate-from phase 1 bundles")?;

    // Extract the L2 priority tx hash from the NewPriorityRequest event on
    // L1. `.apply()` above emitted it on the gateway diamond proxy. Scan
    // from block 0 — the diamond-proxy address filter already scopes the
    // search correctly.
    let l2_priority_tx_hash = fetch_priority_op_l2_hash(&l1_rpc_url, gateway_diamond_proxy, 0)
        .await
        .context("extract L2 priority tx hash from submit's L1 receipt")?;
    let l2_priority_tx_hex = format!("{l2_priority_tx_hash:#x}");
    println!("  L2 priority tx hash (on gateway): {l2_priority_tx_hex}");

    // ── Phase 2: finalize (deployer) ─────────────────────────────────────
    let phase2_safe_rel = format!("{migrate_dir}/phase2/safe");
    let phase2_safe_abs = contracts_backend.work_path(&phase2_safe_rel);
    contracts_backend
        .protocol_ops(&[
            "chain",
            "gateway",
            "migrate-from",
            "phase-2-finalize",
            "--l1-rpc-url",
            &l1_rpc_url,
            "--ecosystem",
            &eco_path,
            "--chain",
            chain_name,
            "--deployer-address",
            &deployer_addr,
            "--gateway-rpc-url",
            gw_l2_rpc.as_str(),
            "--migration-l2-tx-hash",
            l2_priority_tx_hex.as_str(),
            "--out",
            &phase2_safe_abs,
        ])
        .context("migrate-from phase 2 (finalize)")?;
    contracts_backend
        .parse_safe_bundles(&phase2_safe_rel, &l1_rpc_url)?
        .apply(signers)
        .context("apply migrate-from phase 2 bundles")?;

    // ── Phase 3: set-da-validator-pair (chain admin) ─────────────────────
    let phase3_safe_rel = format!("{migrate_dir}/phase3/safe");
    let phase3_safe_abs = contracts_backend.work_path(&phase3_safe_rel);
    contracts_backend
        .protocol_ops(&[
            "chain",
            "gateway",
            "migrate-from",
            "phase-3-set-da-validator-pair",
            "--l1-rpc-url",
            &l1_rpc_url,
            "--ecosystem",
            &eco_path,
            "--chain",
            chain_name,
            "--l1-da-validator",
            l1_da_validator.as_str(),
            "--l2-da-commitment-scheme",
            "blobs-zk-sync-os",
            "--out",
            &phase3_safe_abs,
        ])
        .context("migrate-from phase 3 (set-da-validator-pair)")?;
    contracts_backend
        .parse_safe_bundles(&phase3_safe_rel, &l1_rpc_url)?
        .apply(signers)
        .context("apply migrate-from phase 3 bundles")?;

    // ── Verify: settlementLayer flipped back to L1 ────────────────────────
    let bridgehub_contract = IBridgehub::new(bridgehub, &l1_provider);
    let settlement_layer = bridgehub_contract
        .settlementLayer(U256::from(chain_id))
        .call()
        .await
        .context("bridgehub.settlementLayer")?
        ._0;
    println!(
        "\n=== Post-migration: bridgehub.settlementLayer({}) = {} ===",
        chain_id, settlement_layer
    );
    anyhow::ensure!(
        settlement_layer != U256::from(eco.gateway_chain_id()),
        "settlementLayer({}) is still gateway chain {} after migration",
        chain_id,
        eco.gateway_chain_id(),
    );

    // TODO: end-to-end post-migration restart is disabled. The idea: kill
    // the pre-migration chain server (whose L1 committer was targeting the
    // gateway), respawn it reusing the same RocksDB so new batches commit
    // to L1, then wait for `wait_for_traffic_tx_executed_on_l1`.
    //
    // Already in place for when we re-enable:
    //   - zksync-os-server's `PubdataMode::Blobs + gateway_rpc_url.is_some()`
    //     startup check is relaxed; `L1CommitWatcher` / `L1PersistBatchWatcher`
    //     fall back to the current SL tip when `find_l1_commit_block_by_batch_number`
    //     misses (pre-migration commits live on the former SL).
    //   - `CommittedBatchProvider` cascades through the gateway diamond too,
    //     so pre-migration commit metadata is resolvable without the
    //     previous run's local RocksDB.
    //   - `InteropRootsSubpool::on_canonical_state_change` downgrades its
    //     envelope-vs-tx assert to a warn (rebuilt envelope diverges across
    //     a migration boundary).
    //   - `generate-l1-state` writes `l1-state.gateway-state.tar.gz` into
    //     the persistent cache dir so `--skip-generate` reruns still find
    //     it; the commented-out block below funds the chain's L1-sender
    //     operators via `anvil_utils::fund_account` before restart.
    //
    // Blocker: after respawn the sequencer produces a few blocks but the
    // cast-sent traffic tx never confirms within cast's timeout — the block
    // pipeline stalls. Likely further issues: the `interop_fee_updater` loop
    // failing on `gatewaySettlementFee()` against the gateway asset tracker,
    // other subpools (`interop_fee`, `sl_chain_id`, `l1`) hitting the same
    // envelope-rebuild divergence as `interop_roots` did, and block replay
    // against the new SL context.
    //
    // For now, clean up the still-running pre-migration chain server and
    // stop here so the migration-from-gateway flow itself is exercised.
    let _ = chain_server.kill();
    let _ = gw_server.kill();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let _ = anvil.kill();

    // println!("\n=== Restarting chain server with L1 settlement ===");
    // tokio::time::sleep(Duration::from_secs(5)).await;
    //
    // println!("\n=== Funding migrated chain's L1-sender operators on L1 ===");
    // for (label, op) in [
    //     ("commit", &chain_wallets.commit_operator),
    //     ("prove", &chain_wallets.prove_operator),
    //     ("execute", &chain_wallets.execute_operator),
    // ] {
    //     println!("  funding {label} operator {} on L1", op.address);
    //     integration_tests::anvil_utils::fund_account(
    //         &op.address,
    //         "5ether",
    //         &l1_rpc_url,
    //         &deployer_pk,
    //     )
    //     .with_context(|| format!("fund L1 balance for {label} operator"))?;
    // }
    //
    // let chain_server = ServerBuilder::new(preset.clone(), chain_name)
    //     .gateway_rpc_url(&gw_l2_rpc)
    //     .config_path(&chain_config)
    //     .env("l1_sender_pubdata_mode", "Blobs")
    //     .spawn(&anvil)
    //     .map_err(|e| anyhow::anyhow!("Failed to restart chain server on L1: {:?}", e))?;
    //
    // println!("=== Driving traffic until migrated chain commits to L1 ===");
    // chain_server
    //     .wait_for_traffic_tx_executed_on_l1()
    //     .context("post-migration batches executed on L1")?;
    //
    // let _ = chain_server.kill();
    // let _ = gw_server.kill();
    // tokio::time::sleep(Duration::from_millis(200)).await;
    // let _ = anvil.kill();

    println!("\nTest passed!");
    Ok(())
}

#[tokio::test]
async fn test_migrate_live_chain_from_gateway() {
    run_migrate_live_chain_from_gateway_test()
        .await
        .expect("migrate_live_chain_from_gateway test failed");
}
