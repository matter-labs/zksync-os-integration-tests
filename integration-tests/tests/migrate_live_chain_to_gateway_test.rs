//! Live migration of an L1-settling chain to gateway, end-to-end.
//!
//! Companion to `migrate_live_chain_from_gateway_test.rs`. Picks the single L1-settling
//! chain out of the cached ecosystem, drives the three migrate-to-gateway
//! phases against a live server, and verifies that the bridgehub
//! `settlementLayer` flips to the gateway chain id.
//!
//!   1. Spawns the gateway server and the L1-settling chain server.
//!   2. Waits for the chain to produce + execute a few batches on L1, so we
//!      know the starting fixture is healthy.
//!   3. Runs the `chain gateway migrate` sequence:
//!        - phase 1 (chain owner + deployer): notify-server + submit
//!        - phase 2 (deployer): finalize (polls gateway for withdrawal proof)
//!        - phase 3 (chain owner + deployer): enable-validators +
//!          set-da-validator-pair
//!   4. Verifies on the L1 bridgehub that the chain's settlementLayer has
//!      flipped to the gateway chain id.
//!
//! The chain server that was running pre-migration panics once phase 1
//! flips the settlement layer: its L1 committer keeps trying to commit new
//! batches to L1 and those txs start reverting. After phase 3, we kill the
//! crashed server and respawn a fresh one for the same chain — reusing its
//! on-disk RocksDB but this time configured with `gateway_rpc_url` — then
//! drive traffic and wait for batches to execute against the chain's *new*
//! diamond proxy on gateway, proving the migrated chain can continue
//! producing end-to-end on its new settlement layer.
//!
//! TODO: zksync-os-server is not ready to handle live migrations end-to-end.
//!   - The running pre-migration server panics when its L1 committer keeps
//!     trying to commit new batches to L1 after the settlement layer flips.
//!   - Restarting the server with `gateway_rpc_url` doesn't help either: the
//!     bootstrap `CommittedBatchProvider` scans the *settlement layer*
//!     diamond for historical commit events, but the pre-migration commits
//!     (batches 1..=N) live on the old L1 diamond — the migration copies
//!     totals via `Migrator.forwardedBridgeMint` but not event history — so
//!     init panics with "failed to find committed batch X on L1".
//!
//! Re-enable this test once the server supports bootstrapping on a
//! post-migration settlement layer (e.g. by trusting local RocksDB state
//! instead of rescanning SL events, or adding a post-migration init
//! codepath). Until then this test is not listed in presets.yaml.
use std::time::Duration;

use alloy::primitives::{Address, U256};
use alloy::providers::ProviderBuilder;
use alloy::sol;
use anyhow::{Context, Result};
use integration_tests::anvil::Anvil;
use integration_tests::l1_state::{
    chain_config_path, load_ecosystem, load_wallets, resolve_ecosystem_dir, resolve_l1_state,
};
use integration_tests::presets::load_current_preset;
use integration_tests::protocol_ops::EraContractsBackend;
use integration_tests::server::ServerBuilder;

sol! {
    #[sol(rpc)]
    interface IBridgehub {
        function settlementLayer(uint256 chainId) external view returns (uint256);
        function getZKChain(uint256 chainId) external view returns (address);
    }
}

/// Pull `relayed_sl_da_validator` out of the vote-preparation TOML.
fn extract_relayed_sl_da_validator(toml_body: &str) -> Result<String> {
    #[derive(serde::Deserialize)]
    struct VotePrep {
        relayed_sl_da_validator: String,
    }
    let parsed: VotePrep = toml::from_str(toml_body).context("parse gateway_vote_prep_out.toml")?;
    Ok(parsed.relayed_sl_da_validator)
}

/// Load the vote-prep TOML from the preset cache dir (preferred — survives
/// era-contracts cleanup), falling back to the era-contracts script-out that
/// `generate-l1-state` wrote during cache creation.
fn load_vote_prep_toml(
    preset: &integration_tests::presets::Preset,
    contracts_backend: &EraContractsBackend,
) -> Result<String> {
    let eco_dir = resolve_ecosystem_dir(preset)?;
    let cached = eco_dir.join("gateway_vote_prep_out.toml");
    if cached.exists() {
        return std::fs::read_to_string(&cached)
            .with_context(|| format!("read cached {}", cached.display()));
    }
    contracts_backend
        .read_repo_file("l1-contracts/script-out/gateway_vote_prep_out.toml")
        .context(
            "gateway_vote_prep_out.toml not found in cache or era-contracts script-out — \
             regenerate l1-state cache",
        )
}

async fn run_migrate_live_chain_to_gateway_test() -> Result<()> {
    integration_tests::server::get_or_create_run_id("migrate_live_chain_to_gateway");
    let preset = load_current_preset()?;
    let eco = load_ecosystem(&preset)?;
    let wallets = load_wallets(&preset).context("load wallets.yaml")?;

    println!("\n=== Loading l1-state.json into Anvil ===");
    let state_path = resolve_l1_state(&preset)?;
    let anvil = Anvil::spawn_with_state(&state_path).await?;
    let l1_rpc_url = anvil.rpc_url().to_string();
    println!("Anvil ready at {l1_rpc_url}");

    let (chain_name, chain_id) = eco.l1_settling();
    println!("Gateway     : chain {}", eco.gateway_chain_id());
    println!("Migrating   : chain {chain_id} ({chain_name})");

    let chain_wallets = wallets
        .chains
        .get(chain_name)
        .ok_or_else(|| anyhow::anyhow!("wallets.yaml missing entry for chain '{}'", chain_name))?;
    let chain_owner_pk = chain_wallets.owner.private_key.clone();
    let deployer_pk = wallets.ecosystem.deployer.private_key.clone();
    let commit_op_addr = chain_wallets.commit_operator.address.clone();
    let prove_op_addr = chain_wallets.prove_operator.address.clone();
    let execute_op_addr = chain_wallets.execute_operator.address.clone();

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
        "\n=== Starting L1-settling chain server (chain {}) ===",
        chain_id
    );
    let chain_server = ServerBuilder::new(preset.clone(), chain_name)
        .config_path(&chain_config)
        .spawn(&anvil)
        .map_err(|e| anyhow::anyhow!("Failed to start chain server: {:?}", e))?;

    println!(
        "\n=== Sanity: wait for L1-settled batches on chain {} ===",
        chain_id
    );
    chain_server
        .wait_for_traffic_tx_executed_on_l1()
        .context("L1-settling chain batches (pre-migration)")?;

    let contracts_backend = EraContractsBackend::from_preset(&preset, "migrate_to_gateway", &[])?;

    // Stage the cached vote-prep TOML into the era-contracts script-out
    // directory so phase commands can read it at the forge-canonical path
    // (/script-out/gateway_vote_prep_out.toml). Docker backends see the
    // same file via the mounted script-out symlink.
    let vote_prep_toml = load_vote_prep_toml(&preset, &contracts_backend)?;
    let relayed_sl_da_validator = extract_relayed_sl_da_validator(&vote_prep_toml)?;
    println!("  Gateway relayed SL DA validator: {relayed_sl_da_validator}");
    contracts_backend
        .write_repo_file(
            "l1-contracts/script-out/gateway_vote_prep_out.toml",
            &vote_prep_toml,
        )
        .context("stage gateway_vote_prep_out.toml into era-contracts script-out")?;
    let vote_prep_path_rel = "/script-out/gateway_vote_prep_out.toml".to_string();

    let eco_path = contracts_backend.ecosystem_yaml_path(&preset)?;

    let signers: &[&str] = &[&chain_owner_pk, &deployer_pk];
    let l1_gas_price = "1000000000".to_string();
    // Per-run UUID suffix in the work dir: `contracts_artifacts/` survives
    // across test invocations (to work around a macOS Docker/VirtioFS
    // bind-mount quirk), so bundles emitted by prior runs stay on disk.
    // Without a unique suffix, `--out` would append to an
    // existing `manifest.json` and `dev execute-safe` would replay stale
    // bundles. A uuid per test invocation also ensures concurrent test
    // instances don't scribble over each other's work dirs.
    let migrate_dir = format!(
        "migrate_live_chain_to_gateway_{chain_id}_{}",
        uuid::Uuid::new_v4(),
    );

    let bridgehub_addr: Address = eco.bridgehub.parse().context("parse bridgehub")?;
    let l1_provider = ProviderBuilder::new()
        .on_builtin(&l1_rpc_url)
        .await
        .context("connect L1 provider")?;
    let bh = IBridgehub::new(bridgehub_addr, &l1_provider);
    let chain_diamond_proxy = bh
        .getZKChain(U256::from(chain_id))
        .call()
        .await
        .context("bridgehub.getZKChain")?
        ._0;
    let chain_diamond_proxy_hex = format!("{:#x}", chain_diamond_proxy);

    // ── Phase 0: pause-deposits + notify-server (chain admin) ────────────
    // The Migrator requires deposits to be paused and the chain's L1 state
    // to have `totalBatchesCommitted == totalBatchesExecuted`. Notify-server
    // emits MigrateToGateway on the ServerNotifier; the chain server watches
    // this event, seals a final batch with a `SetSLChainId` system tx, and
    // stops producing new batches. Fresh gateway-settling chains created
    // by `generate-l1-state` skip this because they were born paused.
    let phase0_safe_rel = format!("{migrate_dir}/phase0/safe");
    let phase0_safe_abs = contracts_backend.work_path(&phase0_safe_rel);
    contracts_backend
        .protocol_ops(&[
            "chain",
            "gateway",
            "migrate-to",
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
        .context("migrate-to-gateway phase 0 (pause-deposits + notify-server)")?;
    contracts_backend
        .parse_safe_bundles(&phase0_safe_rel, &l1_rpc_url)?
        .apply(signers)
        .context("apply migrate-to-gateway phase 0 bundles")?;

    // Wait for the chain to drain its commit/execute pipeline after
    // notify-server so the Migrator's `NotAllBatchesExecuted()` check
    // passes when phase 1 submits. We require stability for several
    // seconds because `committed == executed` can briefly be true just
    // before the server seals its final (SetSLChainId) batch on L1.
    println!("  Waiting for L1-settling chain to drain commit/execute pipeline...");
    integration_tests::server_utils::wait_for_committed_eq_executed(
        &l1_rpc_url,
        &chain_diamond_proxy_hex,
        Duration::from_secs(10),
        integration_tests::DEFAULT_WAIT_TIMEOUT,
    )
    .context("wait for chain batches to drain before submit")?;

    // ── Phase 1: notify-server + submit (chain admin) ────────────────────
    //
    // `protocol-ops chain gateway migrate-to phase-1-submit` — one
    // invocation runs both stages against a single anvil fork and emits
    // one Safe bundle dir.
    let deployer_addr = wallets.ecosystem.deployer.address.clone();
    let phase1_safe_rel = format!("{migrate_dir}/phase1/safe");
    let phase1_safe_abs = contracts_backend.work_path(&phase1_safe_rel);
    let gateway_chain_id_str = eco.gateway_chain_id().to_string();
    contracts_backend
        .protocol_ops(&[
            "chain",
            "gateway",
            "migrate-to",
            "phase-1-submit",
            "--l1-rpc-url",
            &l1_rpc_url,
            "--ecosystem",
            &eco_path,
            "--chain",
            chain_name,
            "--gateway-chain-id",
            &gateway_chain_id_str,
            "--l1-gas-price",
            l1_gas_price.as_str(),
            "--vote-preparation-toml",
            vote_prep_path_rel.as_str(),
            "--refund-recipient",
            &deployer_addr,
            "--out",
            &phase1_safe_abs,
        ])
        .context("migrate-to-gateway phase 1 (submit)")?;
    contracts_backend
        .parse_safe_bundles(&phase1_safe_rel, &l1_rpc_url)?
        .apply(signers)
        .context("apply migrate-to-gateway phase 1 bundles")?;

    // Let the gateway pick up and execute the migration priority tx so that
    // phase 2 (finalize) can poll a settled withdrawal proof.
    println!("  Waiting for gateway to process migrate priority tx...");
    gw_server
        .wait_for_traffic_tx_executed_on_l1()
        .context("gateway batches after phase 1")?;

    // ── Phase 2: finalize (deployer) ────────────────────────────────────
    let phase2_safe_rel = format!("{migrate_dir}/phase2/safe");
    let phase2_safe_abs = contracts_backend.work_path(&phase2_safe_rel);
    contracts_backend
        .protocol_ops(&[
            "chain",
            "gateway",
            "migrate-to",
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
            "--vote-preparation-toml",
            vote_prep_path_rel.as_str(),
            "--out",
            &phase2_safe_abs,
        ])
        .context("migrate-to-gateway phase 2 (finalize)")?;
    contracts_backend
        .parse_safe_bundles(&phase2_safe_rel, &l1_rpc_url)?
        .apply(signers)
        .context("apply migrate-to-gateway phase 2 bundles")?;

    // ── Phase 3: enable-validators + set-da-validator-pair (chain admin) ─
    let phase3_safe_rel = format!("{migrate_dir}/phase3/safe");
    let phase3_safe_abs = contracts_backend.work_path(&phase3_safe_rel);
    contracts_backend
        .protocol_ops(&[
            "chain",
            "gateway",
            "migrate-to",
            "phase-3-validators",
            "--l1-rpc-url",
            &l1_rpc_url,
            "--ecosystem",
            &eco_path,
            "--chain",
            chain_name,
            "--gateway-rpc-url",
            gw_l2_rpc.as_str(),
            "--commit-operator",
            commit_op_addr.as_str(),
            "--prove-operator",
            prove_op_addr.as_str(),
            "--execute-operator",
            execute_op_addr.as_str(),
            "--l1-da-validator",
            relayed_sl_da_validator.as_str(),
            "--l2-da-commitment-scheme",
            "blobs-and-pubdata-keccak256",
            "--l1-gas-price",
            l1_gas_price.as_str(),
            "--out",
            &phase3_safe_abs,
        ])
        .context("migrate-to-gateway phase 3 (validators)")?;
    contracts_backend
        .parse_safe_bundles(&phase3_safe_rel, &l1_rpc_url)?
        .apply(signers)
        .context("apply migrate-to-gateway phase 3 bundles")?;

    // ── Verify: settlementLayer flipped to gateway ───────────────────────
    let bridgehub_contract = IBridgehub::new(bridgehub_addr, &l1_provider);
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
        settlement_layer == U256::from(eco.gateway_chain_id()),
        "settlementLayer({}) is {} after migration, expected gateway chain {}",
        chain_id,
        settlement_layer,
        eco.gateway_chain_id(),
    );

    // ── End-to-end: restart chain server on gateway, drive traffic ───────
    // Before we restart the chain with `gateway_rpc_url`, fund its L1 sender
    // operators (commit / prove / execute) on the gateway L2 — their
    // balances there are zero, because generate-l1-state only funds
    // operators for chains that were born gateway-settling. Without this,
    // the first commit tx on gateway panics with "L1 sender's address X has
    // zero balance".
    println!("\n=== Funding migrated chain's operators on gateway ===");
    {
        use integration_tests::server::L1DepositBaseToken;
        use integration_tests::server_utils::address_from_private_key;

        #[derive(serde::Deserialize)]
        struct OperatorSks {
            l1_sender: L1SenderSks,
        }
        #[derive(serde::Deserialize)]
        struct L1SenderSks {
            operator_commit_sk: String,
            operator_prove_sk: String,
            operator_execute_sk: String,
        }
        let chain_yaml = std::fs::read_to_string(&chain_config)
            .with_context(|| format!("read chain config {}", chain_config.display()))?;
        let sks: OperatorSks =
            serde_yaml::from_str(&chain_yaml).context("parse operator sks from chain yaml")?;

        for (label, sk) in [
            ("commit", &sks.l1_sender.operator_commit_sk),
            ("prove", &sks.l1_sender.operator_prove_sk),
            ("execute", &sks.l1_sender.operator_execute_sk),
        ] {
            let addr = address_from_private_key(sk)
                .with_context(|| format!("derive {label} operator address"))?;
            println!("  funding {label} operator {addr} on gateway");
            gw_server
                .fund_account_via_l1_deposit(&addr, 5.0, L1DepositBaseToken::PreApprovedCustom)
                .await
                .with_context(|| format!("fund gateway L2 for {label} operator"))?;
        }
    }

    // The still-running chain server's L1 committer will panic once it
    // notices the settlement-layer flip (it keeps trying to commit to L1
    // even though batches now need to go to gateway). Kill it, then spawn
    // a fresh chain server — same chain_name so it reuses the RocksDB at
    // `test-run-logs/<preset>/<run_id>/db_<chain_name>/` — this time with
    // `gateway_rpc_url` set so its committer targets the gateway.
    println!("\n=== Restarting chain server with gateway settlement ===");
    let _ = chain_server.kill();
    // Give the process + docker teardown a beat to release the RocksDB
    // LOCK so the replacement server's RocksDB open doesn't fail.
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Patch the chain's config YAML to add a forced price for the gateway
    // base token (ZK). `generate-l1-state` only seeds a ZK price for chains
    // born gateway-settling; an L1-settling chain's yaml only has ETH. Post
    // migration, the base_token_price_updater needs a price for the SL's
    // base token or it blocks the sequencer with "no fallback prices are
    // configured, token prices for fees remain unset".
    //
    // Copy the existing gateway-settling chain's forced/fallback price map
    // (already contains ZK) into our chain's yaml, write to a sibling file,
    // and restart using that patched config.
    let patched_chain_config = {
        let chain_yaml = std::fs::read_to_string(&chain_config)
            .with_context(|| format!("read chain config {}", chain_config.display()))?;
        let mut doc: serde_yaml::Value =
            serde_yaml::from_str(&chain_yaml).context("parse chain yaml")?;

        // Find a sibling gateway-settling yaml for the same ecosystem to
        // borrow its price map — any entry in `eco.chains` whose name is
        // neither `gateway` nor our chain_name will do.
        let gw_chain_name = eco
            .chains
            .keys()
            .find(|n| {
                n.as_str() != chain_name
                    && n.as_str() != integration_tests::l1_state::GATEWAY_CHAIN_NAME
                    && eco.chains.get(n.as_str()).copied() != Some(eco.gateway_chain_id())
            })
            .context("no sibling gateway-settling chain to borrow ZK price from")?
            .clone();
        let gw_chain_config_path = chain_config_path(&preset, &gw_chain_name)?;
        let gw_yaml = std::fs::read_to_string(&gw_chain_config_path)
            .with_context(|| format!("read {}", gw_chain_config_path.display()))?;
        let gw_doc: serde_yaml::Value =
            serde_yaml::from_str(&gw_yaml).context("parse gateway-settling yaml")?;

        for (top, nested) in [
            ("external_price_api_client", "forced_prices"),
            ("base_token_price_updater", "fallback_prices"),
        ] {
            let src = gw_doc
                .get(top)
                .and_then(|v| v.get(nested))
                .cloned()
                .with_context(|| format!("missing {top}.{nested} in gateway-settling yaml"))?;
            doc.as_mapping_mut()
                .context("chain yaml root is not a mapping")?
                .entry(serde_yaml::Value::String(top.into()))
                .or_insert_with(|| serde_yaml::Value::Mapping(Default::default()))
                .as_mapping_mut()
                .with_context(|| format!("{top} is not a mapping"))?
                .insert(serde_yaml::Value::String(nested.into()), src);
        }

        let out = chain_config.with_file_name(format!(
            "{}.patched_for_gateway_settlement.yaml",
            chain_name
        ));
        std::fs::write(&out, serde_yaml::to_string(&doc)?).context("write patched yaml")?;
        out
    };

    // The original config was written with `pubdata_mode: Blobs` for L1
    // settlement. Gateway settlement uses calldata relayed via the
    // gateway's L2 bridgehub, so override here — otherwise the server
    // panics with "Pubdata mode Blobs cannot be used when settling on
    // Gateway" before RPC comes up.
    let chain_server = ServerBuilder::new(preset.clone(), chain_name)
        .gateway_rpc_url(&gw_l2_rpc)
        .config_path(&patched_chain_config)
        .env("l1_sender_pubdata_mode", "RelayedL2Calldata")
        .spawn(&anvil)
        .map_err(|e| anyhow::anyhow!("Failed to restart chain server on gateway: {:?}", e))?;

    // Resolve the chain's new ZKChain diamond on the gateway (system
    // bridgehub at 0x10002 on gateway L2) and drive traffic until
    // DEFAULT_EXTRA_BATCHES new batches execute through it. The old
    // L1-side diamond no longer tracks batches.
    let gw_provider = ProviderBuilder::new()
        .on_builtin(&gw_l2_rpc)
        .await
        .context("connect gateway L2 provider")?;
    let gw_l2_bridgehub: Address = "0x0000000000000000000000000000000000010002"
        .parse()
        .context("parse gateway L2 bridgehub")?;
    let gw_bh = IBridgehub::new(gw_l2_bridgehub, &gw_provider);
    let gw_side_diamond = gw_bh
        .getZKChain(U256::from(chain_id))
        .call()
        .await
        .context("gateway L2 bridgehub.getZKChain(chain)")?
        ._0;
    anyhow::ensure!(
        gw_side_diamond != Address::ZERO,
        "Gateway L2 bridgehub has no ZKChain for {} after migration",
        chain_id,
    );
    let gw_side_diamond_hex = format!("{:#x}", gw_side_diamond);
    println!(
        "=== Driving traffic until migrated chain commits to gateway (diamond {}) ===",
        gw_side_diamond_hex
    );
    chain_server
        .wait_for_traffic_tx_executed_on_l1()
        .context("post-migration batches executed on gateway")?;

    // Cleanup.
    let _ = chain_server.kill();
    let _ = gw_server.kill();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let _ = anvil.kill();

    println!("\nTest passed!");
    Ok(())
}

#[tokio::test]
async fn test_migrate_live_chain_to_gateway() {
    run_migrate_live_chain_to_gateway_test()
        .await
        .expect("migrate_live_chain_to_gateway test failed");
}
