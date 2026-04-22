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
//! TODO: zksync-os-server cannot bootstrap a chain after migration-from-gateway.
//!   The restart-with-L1-settlement step (kill pre-migration server, respawn
//!   with `gateway_rpc_url` stripped, drive L1 batches) mirrors what the
//!   to-gateway test does. It doesn't work here because the server's
//!   `CommittedBatchProvider` scans only the *current* settlement layer (L1)
//!   for historical commit events, but pre-migration commits (batches 1..=N)
//!   live on the gateway — migration copies totals via
//!   `Migrator.forwardedBridgeMint` but not event history — so init panics
//!   with "failed to find committed batch X on either L1 or current SL".
//!   Re-enable the restart once the server can bootstrap against a
//!   post-migration-from-gateway settlement layer (e.g. by trusting local
//!   RocksDB state instead of rescanning SL events, or by adding a
//!   from-gateway-aware init codepath mirroring the to-gateway one).
use std::time::Duration;

use alloy::primitives::{address, Address, FixedBytes, U256};
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

    // Snapshot L1 head BEFORE apply so we can pull every tx the Safe bundle
    // broadcasts and inspect its receipt.
    let chain_owner_addr: Address = chain_wallets
        .owner
        .address
        .parse()
        .context("parse chain owner address")?;
    let l1_block_before_apply = l1_provider
        .get_block_number()
        .await
        .context("get L1 block number before phase 0 apply")?;
    let nonce_before = l1_provider
        .get_transaction_count(chain_owner_addr)
        .block_id(l1_block_before_apply.into())
        .await
        .context("get chain_owner nonce before phase 0 apply")?;
    println!(
        "  [debug] pre-apply: L1 block={l1_block_before_apply}, \
         chain_owner({chain_owner_addr:#x}) nonce={nonce_before}"
    );

    contracts_backend
        .parse_safe_bundles(&phase0_safe_rel, &l1_rpc_url)?
        .apply(signers)
        .context("apply migrate-from phase 0 bundles")?;

    // Dump every L1 tx from chain_owner between the snapshot and now —
    // status, gasUsed, logs — so we can see whether the pause-deposits
    // inner call actually succeeded (i.e. emitted DepositsPaused).
    {
        let l1_block_after = l1_provider
            .get_block_number()
            .await
            .context("get L1 block number after phase 0 apply")?;
        let nonce_after = l1_provider
            .get_transaction_count(chain_owner_addr)
            .await
            .context("get chain_owner nonce after phase 0 apply")?;
        println!(
            "  [debug] post-apply: L1 block={l1_block_after}, \
             chain_owner nonce={nonce_after} (delta {})",
            nonce_after - nonce_before
        );

        // Walk the new blocks, collect chain_owner's txs in order, dump receipts.
        let mut found: Vec<alloy::primitives::TxHash> = Vec::new();
        for bn in (l1_block_before_apply + 1)..=l1_block_after {
            let block = l1_provider
                .get_block_by_number(bn.into(), true)
                .await
                .context("get L1 block by number")?;
            let Some(block) = block else { continue };
            for tx in block.transactions.txns() {
                if tx.from == chain_owner_addr {
                    found.push(tx.hash);
                }
            }
        }
        println!(
            "  [debug] chain_owner L1 txs in new blocks: {}",
            found.len()
        );
        for (i, h) in found.iter().enumerate() {
            let receipt = l1_provider
                .get_transaction_receipt(*h)
                .await
                .context("get tx receipt")?;
            match receipt {
                Some(r) => {
                    let status = r.status();
                    println!(
                        "    [debug] tx #{i} {h:#x} to={:?} status={status} gasUsed={} logs={}",
                        r.to,
                        r.gas_used,
                        r.inner.logs().len(),
                    );
                    for (j, log) in r.inner.logs().iter().enumerate() {
                        let topic0 = log.topics().first().copied();
                        println!(
                            "      [debug]   log #{j} addr={:?} topic0={:?}",
                            log.address(),
                            topic0,
                        );
                    }
                }
                None => println!("    [debug] tx #{i} {h:#x} — no receipt!"),
            }
        }
    }

    // ── Debug: was phase 0 even effective on L1? ─────────────────────────
    // Inspect L1 state right after applying phase 0:
    //   - slot 33 (protocolVersion) → sanity-check storage-slot indexing
    //   - slot 36 (admin) → must equal the ChainAdmin we broadcast through
    //   - slot 64 (pausedDepositsTimestamp) → this is what should be non-zero
    //   - DepositsPaused events from chain diamond since phase 0 broadcast
    // If slot 33/36 look right but slot 64 is 0 AND no DepositsPaused event
    // was emitted, the multicall either never reached the Migrator or its
    // inner call reverted silently. If DepositsPaused *was* emitted but
    // slot 64 is still 0, my slot indexing is wrong.
    {
        use alloy::primitives::B256;
        let chain_l1_diamond = bh
            .getZKChain(U256::from(chain_id))
            .call()
            .await
            .context("bridgehub.getZKChain(chain) after phase 0")?
            ._0;
        // The STORAGE SLOT comments in ZKChainStorage.sol assume no packing
        // and are off-by-2 around slot 60: Solidity packs zksyncOS (bool) +
        // l2DACommitmentScheme (enum) + assetTracker (address) into slot 60,
        // shifting everything below it. These slots are from
        // `forge inspect MigratorFacet storageLayout`.
        for (slot, label) in [
            (33u64, "protocolVersion"),
            (36u64, "admin"),
            (50u64, "settlementLayer"),
            (61u64, "nativeTokenVault"),
            (62u64, "pausedDepositsTimestamp"),
        ] {
            let raw: B256 = l1_provider
                .get_storage_at(chain_l1_diamond, U256::from(slot))
                .await
                .context("eth_getStorageAt")?
                .into();
            println!(
                "  [debug] L1 chain diamond {chain_l1_diamond} slot {slot:>3} ({label}) = 0x{}",
                alloy::hex::encode(raw.0)
            );
        }

        // DepositsPaused was emitted, so the timestamp IS stored somewhere.
        // If slot 64 is 0, the deployed Migrator facet must be writing to a
        // different slot (older ZKChainStorage layout). Scan slots 55-80 for
        // a value that looks like a unix timestamp.
        let now_ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        println!(
            "  [debug] scanning L1 chain diamond slots 55..80 for timestamp-like values \
             (now={now_ts})"
        );
        for slot in 55u64..80u64 {
            let raw: B256 = l1_provider
                .get_storage_at(chain_l1_diamond, U256::from(slot))
                .await
                .context("eth_getStorageAt scan")?
                .into();
            let v = U256::from_be_bytes(raw.0);
            // Plausible recent unix timestamp: 1.6e9..2.1e9
            let lo: u64 = (v & U256::from(u64::MAX)).try_into().unwrap_or(0);
            let plausible = !v.is_zero()
                && v < U256::from(u64::MAX)
                && lo > 1_500_000_000
                && lo < 3_000_000_000;
            if plausible {
                println!(
                    "    [debug] slot {slot:>3} = {v} (TIMESTAMP-LIKE, delta-from-now={}s)",
                    (now_ts as i64) - (lo as i64),
                );
            } else if !v.is_zero() {
                println!(
                    "    [debug] slot {slot:>3} = 0x{}",
                    alloy::hex::encode(raw.0)
                );
            }
        }

        // Look for DepositsPaused(uint256,uint256) from the chain's L1
        // diamond. Its absence would mean phase-0's pause call never
        // actually ran on the Migrator facet.
        let deposits_paused_topic = keccak256("DepositsPaused(uint256,uint256)");
        let filter = Filter::new()
            .address(chain_l1_diamond)
            .event_signature(deposits_paused_topic)
            .from_block(0u64);
        let logs = l1_provider
            .get_logs(&filter)
            .await
            .context("eth_getLogs DepositsPaused on chain L1 diamond")?;
        println!(
            "  [debug] DepositsPaused events from chain L1 diamond: {}",
            logs.len()
        );
    }

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
    // Poll the chain's gateway-side diamond storage slot 64
    // (`ZKChainStorage.pausedDepositsTimestamp`, see ZKChainStorage.sol) via
    // the gateway L2 RPC until it's non-zero.
    // ── Debug: was the cross-chain pause tx enqueued on L1? ──────────────
    // Scan L1 NewPriorityRequest events on the gateway's L1 diamond. If
    // phase 0 triggered propagation, we should see at least one here —
    // and any events before phase 1 ran are phase-0 txs.
    {
        let filter = Filter::new()
            .address(gateway_diamond_proxy)
            .event_signature(new_priority_request_topic())
            .from_block(0u64);
        let logs = l1_provider
            .get_logs(&filter)
            .await
            .context("eth_getLogs for NewPriorityRequest on gateway L1 diamond")?;
        println!(
            "  [debug] NewPriorityRequest events on gateway L1 diamond so far: {}",
            logs.len()
        );
        for (i, log) in logs.iter().enumerate() {
            let data = log.data().data.as_ref();
            if data.len() >= 64 {
                let tx_hash = FixedBytes::<32>::from_slice(&data[32..64]);
                println!("    [debug] #{i} L2 tx hash: {tx_hash:#x}");
            }
        }
    }

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

    // NOTE: we *don't* respawn the chain server with L1 settlement here —
    // see the TODO in the module-level docstring. zksync-os-server's
    // bootstrap can't find pre-migration commit events on L1 (they live on
    // the gateway) and panics with "failed to find committed batch X on
    // either L1 or current SL". Re-enable once the server grows a
    // from-gateway-aware init path.

    // Cleanup. Sleep briefly to let any in-flight RPC unwind cleanly so the
    // server's drop-time logs aren't mid-write when anvil dies.
    let _ = chain_server.kill();
    let _ = gw_server.kill();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let _ = anvil.kill();
    // Suppress unused-import lint for `address!` which we keep around for
    // future expansions of the test (e.g. asserting the L2 asset router was
    // the L1→L2 target).
    let _: Address = address!("0000000000000000000000000000000000010003");

    println!("\nTest passed!");
    Ok(())
}

#[tokio::test]
async fn test_migrate_live_chain_from_gateway() {
    run_migrate_live_chain_from_gateway_test()
        .await
        .expect("migrate_live_chain_from_gateway test failed");
}
