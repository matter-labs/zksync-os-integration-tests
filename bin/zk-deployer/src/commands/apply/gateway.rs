use std::path::{Path, PathBuf};
use std::time::Duration;

use alloy::primitives::Address;
use anyhow::{Context, Result};

use crate::intent::{ChainIntent, ChainRole, IntentConfig};
use crate::resolved::ResolvedEcosystem;
use crate::state::{
    GatewayConvertOutput, GatewayMigratePhase1Output, GatewayMigratePhase2Output,
    GatewayMigratePhase3Output, State, StepKey,
};
use protocol_ops::commands::chain::gateway::convert::{
    stage_deploy_filterer, stage_governance_execute, stage_grant_whitelist, stage_revoke_whitelist,
    stage_vote_prepare, VotePrepareInputs,
};
use protocol_ops::commands::chain::gateway::migrate_to::{
    capture_priority_op_hash_after_submit, finalize_migration, stage_enable_validators,
    stage_notify_server, stage_set_da_validator_pair, stage_submit, EnableValidatorsInputs,
    SetDaValidatorPairInputs, VotePreparationOutput, DEFAULT_FINALIZE_LOOKBACK_BLOCKS,
};
use protocol_ops::commands::dev::execute_manifest::{apply_manifest_from, count_manifest_bundles};
use protocol_ops::common::output::write_output_if_requested;
use protocol_ops::common::wallets::WalletsYaml;
use protocol_ops::common::{
    args::SharedRunArgs, forge::ForgeRunner, logger, preflight, private_key::pk_to_address,
};
use protocol_ops::types::{L2DACommitmentScheme, VMOption};

/// Shared context threaded through all gateway convert / migrate operations
/// within a single `apply` run.
pub(super) struct GatewayApplyCtx<'a> {
    /// Path to state.json (for saving after each step).
    pub(super) state_path: &'a Path,
    /// Whether to broadcast generated bundles immediately after generation.
    pub(super) broadcast: bool,
    /// Path to manifest.json for bundle tracking.
    pub(super) manifest_path: PathBuf,
    /// Raw deployer private key for bundle signing.
    pub(super) deployer_key: String,
    /// Path to wallets.yaml for multi-signer bundles.
    pub(super) wallets_path: &'a Path,
    /// Shared forge / RPC args for ForgeRunner construction.
    pub(super) shared: SharedRunArgs,
    /// L1 Bridgehub proxy address.
    pub(super) bridgehub: Address,
    /// Deployer EOA address.
    pub(super) deployer_address: Address,
    /// L1 RPC URL (for bundle application and funding).
    pub(super) l1_rpc_url: &'a str,
    /// L1 gas price in wei for priority transactions.
    pub(super) l1_gas_price: u64,
    /// VM type for DA commitment scheme resolution.
    pub(super) vm_type: VMOption,
    /// Resolved ecosystem addresses for DA validator lookup.
    pub(super) eco: ResolvedEcosystem,
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run gateway conversion and all migration phases for any `gateway_settling`
/// chains declared in the intent. No-ops if there are no such chains.
///
/// Builds a [`GatewayApplyCtx`] internally so callers don't need to know about
/// that type.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_gateway_section(
    state: &mut State,
    args: &super::ApplyArgs,
    intent: &IntentConfig,
    wallets: &WalletsYaml,
    bridgehub: Address,
    deployer_key: &str,
    deployer_address: Address,
    vm_type: VMOption,
    shared: SharedRunArgs,
    eco: &ResolvedEcosystem,
) -> Result<()> {
    let gateway_settling: Vec<&ChainIntent> = intent
        .chains
        .iter()
        .filter(|c| c.role == ChainRole::GatewaySettling)
        .collect();

    if gateway_settling.is_empty() {
        return Ok(());
    }

    let gateway_chain = intent
        .chains
        .iter()
        .find(|c| c.role == ChainRole::Gateway)
        .ok_or_else(|| {
            anyhow::anyhow!("intent has gateway_settling chains but no chain with role: gateway")
        })?;
    let gateway_chain_id = gateway_chain.chain_id;

    let ctx = GatewayApplyCtx {
        state_path: &args.state,
        broadcast: args.broadcast,
        manifest_path: args.out.join("manifest.json"),
        deployer_key: deployer_key.to_string(),
        wallets_path: &args.wallets,
        shared,
        bridgehub,
        deployer_address,
        l1_rpc_url: &intent.l1_rpc_url,
        l1_gas_price: args.l1_gas_price,
        vm_type,
        eco: eco.clone(),
    };

    run_gateway_convert(state, &ctx, gateway_chain_id).await?;

    let has_pending = gateway_settling
        .iter()
        .any(|c| !state.is_done(StepKey::GatewayMigratePhase3(c.name.clone())));

    if has_pending {
        match (args.gateway_rpc_url.as_deref(), args.auto_gateway) {
            (None, false) => {
                logger::info(
                    "Gateway conversion complete. Start the gateway server, then re-run:\n\
                     \n\
                     \x20  # Generate the gateway server config (once):\n\
                     \x20  zk-deployer server-config --chain gateway --output gateway.yaml\n\
                     \n\
                     \x20  # Start the server:\n\
                     \x20  L1_PROVIDER_RPC_URL=<l1-rpc-url> zksync-os-server \\\n\
                     \x20    --config local_dev.yaml --config gateway.yaml\n\
                     \n\
                     \x20  # Complete migration:\n\
                     \x20  zk-deployer apply --broadcast \\\n\
                     \x20    --gateway-rpc-url <gateway-l2-rpc-url>\n\
                     \n\
                     \x20  # Or let zk-deployer manage the server automatically:\n\
                     \x20  zk-deployer apply --broadcast --auto-gateway",
                );
            }
            (None, true) => {
                run_with_managed_gateway_server(
                    state,
                    &ctx,
                    &gateway_settling,
                    wallets,
                    gateway_chain_id,
                    args,
                    intent,
                )
                .await?;
            }
            (Some(gw_rpc), _) => {
                run_migration_phases(state, &ctx, &gateway_settling, wallets, gw_rpc).await?;
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Migration phases helper (shared between managed and external server paths)
// ---------------------------------------------------------------------------

async fn run_migration_phases(
    state: &mut State,
    ctx: &GatewayApplyCtx<'_>,
    gateway_settling: &[&ChainIntent],
    wallets: &WalletsYaml,
    gw_rpc: &str,
) -> Result<()> {
    let relayed_sl_da_validator = load_relayed_sl_da_validator()?;

    // gateway_chain_id is stored in state from the GatewayConvert step
    let convert_out: GatewayConvertOutput = state
        .get_output(StepKey::GatewayConvert)
        .context("GatewayConvert not in state — run apply to complete gateway convert first")?;
    let gw_chain_id = convert_out.gateway_chain_id;

    for chain in gateway_settling {
        let chain_wallets = wallets
            .chains
            .get(&chain.name)
            .with_context(|| format!("chain '{}' not found in wallets.yaml", chain.name))?;
        let commit_operator = pk_to_address(&chain_wallets.operator_commit_sk)?;
        let prove_operator = pk_to_address(&chain_wallets.operator_prove_sk)?;
        let execute_operator = pk_to_address(&chain_wallets.operator_execute_sk)?;

        run_migrate_phase1(state, ctx, chain, gw_chain_id).await?;
        run_migrate_phase2(state, ctx, chain, gw_rpc).await?;
        run_migrate_phase3(
            state,
            ctx,
            chain,
            gw_rpc,
            relayed_sl_da_validator,
            commit_operator,
            prove_operator,
            execute_operator,
        )
        .await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Managed gateway server lifecycle
// ---------------------------------------------------------------------------

/// Start an in-process gateway server, run all migration phases, then stop
/// and archive the RocksDB state.
async fn run_with_managed_gateway_server(
    state: &mut State,
    ctx: &GatewayApplyCtx<'_>,
    gateway_settling: &[&ChainIntent],
    wallets: &WalletsYaml,
    gateway_chain_id: u64,
    args: &super::ApplyArgs,
    intent: &crate::intent::IntentConfig,
) -> Result<()> {
    use lib_server::{
        get_l2_finalized_block, load_config_from_yaml, wait_for_l2_block_finalized, Server,
    };

    // --- Build gateway server config YAML ---
    let gateway_chain = intent
        .chains
        .iter()
        .find(|c| c.role == crate::intent::ChainRole::Gateway)
        .context("no gateway chain in intent")?;

    let eco_out: crate::state::EcosystemInitOutput = state
        .get_output(crate::state::StepKey::EcosystemInit)
        .context("ecosystem.init not found — run `bootstrap` first")?;
    let chain_out: crate::state::ChainInitOutput = state
        .get_output(crate::state::StepKey::ChainInit(gateway_chain.name.clone()))
        .context("gateway chain.init not found — run `apply` chain-init phase first")?;
    let gw_wallets = wallets
        .chains
        .get(&gateway_chain.name)
        .with_context(|| format!("gateway chain '{}' not in wallets.yaml", gateway_chain.name))?;

    let config_yaml = crate::commands::server_config::render_config(
        &eco_out,
        &chain_out,
        gw_wallets,
        gateway_chain,
        crate::commands::server_config::resolve_pubdata_mode_pub(gateway_chain),
        crate::commands::server_config::resolve_base_token_addr_pub(gateway_chain, state)
            .ok()
            .flatten()
            .as_deref(),
        None, // gateway itself doesn't have a gateway_chain_id
    )?;

    let config_dir = args.out.join("auto-gateway");
    std::fs::create_dir_all(&config_dir).context("create auto-gateway config dir")?;
    let config_path = config_dir.join("gateway.yaml");
    std::fs::write(&config_path, &config_yaml)
        .with_context(|| format!("write gateway config to {}", config_path.display()))?;

    logger::info(format!(
        "Generated gateway config: {}",
        config_path.display()
    ));

    // --- Load Config and start server ---
    let local_dev = PathBuf::from("local-chains/local_dev.yaml");
    let config_paths: Vec<PathBuf> = if local_dev.exists() {
        vec![local_dev, config_path]
    } else {
        vec![config_path]
    };

    logger::step("Starting managed gateway server...");
    let server_config = load_config_from_yaml(&config_paths, &intent.l1_rpc_url).await;
    let server = Server::start(server_config)
        .await
        .context("start managed gateway server")?;

    let gw_rpc = server.rpc_url().to_string();
    logger::info(format!("Gateway server ready at {gw_rpc}"));

    // --- Run migration phases ---
    let phase_result = run_migration_phases_with_chain_id(
        state,
        ctx,
        gateway_settling,
        wallets,
        &gw_rpc,
        gateway_chain_id,
    )
    .await;

    // Always stop the server, even if phases failed.
    if let Err(ref e) = phase_result {
        logger::info(format!("Migration phases failed ({e}), stopping server..."));
    }

    // Wait for at least one finalized block before archiving.
    if phase_result.is_ok() {
        let current = get_l2_finalized_block(&gw_rpc).await.unwrap_or(0);
        let _ = wait_for_l2_block_finalized(&gw_rpc, current + 1, Duration::from_secs(300)).await;
    }

    let archive_dest = args.out.join("gateway-state.tar.gz");
    logger::step(format!(
        "Archiving gateway RocksDB → {}",
        archive_dest.display()
    ));
    server
        .stop(Some(archive_dest.clone()))
        .await
        .context("stop managed gateway server and archive RocksDB")?;

    phase_result?;

    logger::success(format!(
        "Gateway state archived to {}",
        archive_dest.display()
    ));
    Ok(())
}

async fn run_migration_phases_with_chain_id(
    state: &mut State,
    ctx: &GatewayApplyCtx<'_>,
    gateway_settling: &[&ChainIntent],
    wallets: &WalletsYaml,
    gw_rpc: &str,
    gateway_chain_id: u64,
) -> Result<()> {
    let relayed_sl_da_validator = load_relayed_sl_da_validator()?;

    for chain in gateway_settling {
        let chain_wallets = wallets
            .chains
            .get(&chain.name)
            .with_context(|| format!("chain '{}' not found in wallets.yaml", chain.name))?;
        let commit_operator = pk_to_address(&chain_wallets.operator_commit_sk)?;
        let prove_operator = pk_to_address(&chain_wallets.operator_prove_sk)?;
        let execute_operator = pk_to_address(&chain_wallets.operator_execute_sk)?;

        run_migrate_phase1(state, ctx, chain, gateway_chain_id).await?;
        run_migrate_phase2(state, ctx, chain, gw_rpc).await?;
        run_migrate_phase3(
            state,
            ctx,
            chain,
            gw_rpc,
            relayed_sl_da_validator,
            commit_operator,
            prove_operator,
            execute_operator,
        )
        .await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Phase helpers (private to this module)
// ---------------------------------------------------------------------------

/// Run all five gateway-convert stages in one Anvil fork and record the step.
async fn run_gateway_convert(
    state: &mut State,
    ctx: &GatewayApplyCtx<'_>,
    gateway_chain_id: u64,
) -> Result<()> {
    let convert_step = StepKey::GatewayConvert;
    if state.is_done(&convert_step) {
        return Ok(());
    }

    logger::step("Converting gateway chain (preparing for migration)...");
    let pre = if ctx.broadcast {
        count_manifest_bundles(&ctx.manifest_path)
    } else {
        0
    };

    let mut runner = ForgeRunner::new(&ctx.shared)?;

    let admin_sender = runner
        .prepare_chain_admin_owner(ctx.bridgehub, gateway_chain_id)
        .await
        .context("resolving gateway chain admin owner")?;
    let admin_owner = admin_sender.address;

    stage_deploy_filterer(&mut runner, &admin_sender, ctx.bridgehub, gateway_chain_id)
        .await
        .context("convert stage 1 (deploy-filterer)")?;
    stage_grant_whitelist(
        &mut runner,
        &admin_sender,
        ctx.bridgehub,
        gateway_chain_id,
        &[ctx.deployer_address],
    )
    .await
    .context("convert stage 2 (grant-whitelist)")?;

    let deployer_sender = runner.prepare_sender(ctx.deployer_address).await?;
    stage_vote_prepare(
        &mut runner,
        &deployer_sender,
        ctx.bridgehub,
        gateway_chain_id,
        &VotePrepareInputs {
            ctm_representative_chain_id: gateway_chain_id,
            vote_preparation_toml: "script-out/gateway-vote-preparation.toml",
            refund_recipient: ctx.deployer_address,
            gateway_settlement_fee: 1_000_000_000,
        },
    )
    .await
    .context("convert stage 3 (vote-prepare)")?;

    let gov_sender = runner
        .prepare_governance_owner(ctx.bridgehub)
        .await
        .context("resolving governance owner")?;
    stage_governance_execute(
        &mut runner,
        &gov_sender,
        ctx.bridgehub,
        "script-out/gateway-vote-preparation.toml",
    )
    .await
    .context("convert stage 4 (governance-execute)")?;

    let admin_sender = runner.prepare_sender(admin_owner).await?;
    stage_revoke_whitelist(
        &mut runner,
        &admin_sender,
        ctx.bridgehub,
        gateway_chain_id,
        ctx.deployer_address,
    )
    .await
    .context("convert stage 5 (revoke-whitelist)")?;

    write_output_if_requested(
        &convert_step.to_string(),
        &ctx.shared,
        &runner,
        &serde_json::json!({}),
        &GatewayConvertOutput {
            gateway_chain_id,
            ctm_representative_chain_id: gateway_chain_id,
        },
    )
    .await?;

    if ctx.broadcast {
        let fund = preflight::is_local_rpc(ctx.l1_rpc_url);
        apply_manifest_from(
            &ctx.manifest_path,
            pre,
            std::slice::from_ref(&ctx.deployer_key),
            Some(ctx.wallets_path),
            ctx.l1_rpc_url,
            fund,
        )
        .await
        .context("applying gateway convert bundles")?;
    }

    state.mark_done(
        &convert_step,
        &GatewayConvertOutput {
            gateway_chain_id,
            ctm_representative_chain_id: gateway_chain_id,
        },
    )?;
    state.save(ctx.state_path)?;
    Ok(())
}

/// Phase 1: notify-server + submit migration tx to L1.
async fn run_migrate_phase1(
    state: &mut State,
    ctx: &GatewayApplyCtx<'_>,
    chain: &ChainIntent,
    gateway_chain_id: u64,
) -> Result<()> {
    let step = StepKey::GatewayMigratePhase1(chain.name.clone());
    let chain_id = chain.chain_id;
    if state.is_done(&step) {
        return Ok(());
    }
    state
        .assert_gateway_phase_ready(&chain.name, 1)
        .with_context(|| {
            format!(
                "gateway migration ordering check for chain '{}'",
                chain.name
            )
        })?;
    logger::step(format!(
        "Migrating '{}' to gateway — phase 1 (submit)...",
        chain.name
    ));
    let pre = if ctx.broadcast {
        count_manifest_bundles(&ctx.manifest_path)
    } else {
        0
    };
    let mut runner = ForgeRunner::new(&ctx.shared)?;

    stage_notify_server(&mut runner, ctx.bridgehub, chain_id)
        .await
        .context("phase-1 notify-server")?;
    stage_submit(
        &mut runner,
        ctx.bridgehub,
        chain_id,
        gateway_chain_id,
        ctx.l1_gas_price,
        "script-out/gateway-vote-preparation.toml",
        ctx.deployer_address,
    )
    .await
    .context("phase-1 submit")?;

    // Write Safe bundles before broadcasting — apply_manifest_from needs them
    // to exist. priority_op_hash is not yet known at this point.
    write_output_if_requested(
        &step.to_string(),
        &ctx.shared,
        &runner,
        &serde_json::json!({}),
        &GatewayMigratePhase1Output {
            chain_id,
            gateway_chain_id,
            priority_op_hash: None,
        },
    )
    .await?;

    // After broadcast, capture the priority op hash from the recent L1 event
    // so phase-2 can skip the 216k-block lookback.
    let priority_op_hash = if ctx.broadcast {
        let fund = preflight::is_local_rpc(ctx.l1_rpc_url);
        apply_manifest_from(
            &ctx.manifest_path,
            pre,
            std::slice::from_ref(&ctx.deployer_key),
            Some(ctx.wallets_path),
            ctx.l1_rpc_url,
            fund,
        )
        .await
        .context("applying phase-1 bundles")?;

        match capture_priority_op_hash_after_submit(
            ctx.l1_rpc_url,
            ctx.bridgehub,
            chain_id,
            gateway_chain_id,
        )
        .await
        {
            Ok(h) => {
                logger::info(format!("Priority op hash captured for phase-2: {h:#x}"));
                Some(h)
            }
            Err(e) => {
                logger::warn(format!(
                    "Could not capture priority op hash after broadcast \
                     (phase-2 will scan L1 for it): {e}"
                ));
                None
            }
        }
    } else {
        None
    };

    state.mark_done(
        step,
        &GatewayMigratePhase1Output {
            chain_id,
            gateway_chain_id,
            priority_op_hash,
        },
    )?;
    state.save(ctx.state_path)?;
    Ok(())
}

/// Phase 2: wait for the gateway to settle the migration tx, then finalize on L1.
async fn run_migrate_phase2(
    state: &mut State,
    ctx: &GatewayApplyCtx<'_>,
    chain: &ChainIntent,
    gw_rpc: &str,
) -> Result<()> {
    let step = StepKey::GatewayMigratePhase2(chain.name.clone());
    let chain_id = chain.chain_id;
    if state.is_done(&step) {
        return Ok(());
    }
    state
        .assert_gateway_phase_ready(&chain.name, 2)
        .with_context(|| {
            format!(
                "gateway migration ordering check for chain '{}'",
                chain.name
            )
        })?;
    logger::step(format!(
        "Migrating '{}' to gateway — phase 2 (finalize)...",
        chain.name
    ));
    let pre = if ctx.broadcast {
        count_manifest_bundles(&ctx.manifest_path)
    } else {
        0
    };

    // Use the priority op hash saved by phase 1 to skip the 216k-block L1
    // event scan. Falls back to the full scan when the hash is absent (e.g.
    // dry-run, old state file, or post-broadcast capture failed).
    let priority_op_hint = state
        .get_output::<GatewayMigratePhase1Output>(StepKey::GatewayMigratePhase1(chain.name.clone()))
        .ok()
        .and_then(|o| o.priority_op_hash);

    let (runner, result) = finalize_migration(
        &ctx.shared,
        ctx.bridgehub,
        chain_id,
        ctx.deployer_address,
        gw_rpc,
        "script-out/gateway-vote-preparation.toml",
        DEFAULT_FINALIZE_LOOKBACK_BLOCKS,
        priority_op_hint,
    )
    .await
    .context("phase-2 finalize")?;

    write_output_if_requested(
        &step.to_string(),
        &ctx.shared,
        &runner,
        &serde_json::json!({}),
        &GatewayMigratePhase2Output {
            chain_id,
            gateway_chain_id: result.gateway_chain_id,
        },
    )
    .await?;

    if ctx.broadcast {
        let fund = preflight::is_local_rpc(ctx.l1_rpc_url);
        apply_manifest_from(
            &ctx.manifest_path,
            pre,
            std::slice::from_ref(&ctx.deployer_key),
            Some(ctx.wallets_path),
            ctx.l1_rpc_url,
            fund,
        )
        .await
        .context("applying phase-2 bundles")?;
    }

    state.mark_done(
        step,
        &GatewayMigratePhase2Output {
            chain_id,
            gateway_chain_id: result.gateway_chain_id,
        },
    )?;
    state.save(ctx.state_path)?;
    Ok(())
}

/// Phase 3: enable validators + set DA validator pair on the gateway chain.
#[allow(clippy::too_many_arguments)]
async fn run_migrate_phase3(
    state: &mut State,
    ctx: &GatewayApplyCtx<'_>,
    chain: &ChainIntent,
    gw_rpc: &str,
    relayed_sl_da_validator: Address,
    commit_operator: Address,
    prove_operator: Address,
    execute_operator: Address,
) -> Result<()> {
    let step = StepKey::GatewayMigratePhase3(chain.name.clone());
    if state.is_done(&step) {
        return Ok(());
    }
    state
        .assert_gateway_phase_ready(&chain.name, 3)
        .with_context(|| {
            format!(
                "gateway migration ordering check for chain '{}'",
                chain.name
            )
        })?;
    logger::step(format!(
        "Migrating '{}' to gateway — phase 3 (validators)...",
        chain.name
    ));
    let pre = if ctx.broadcast {
        count_manifest_bundles(&ctx.manifest_path)
    } else {
        0
    };
    let mut runner = ForgeRunner::new(&ctx.shared)?;

    let enable_inputs = EnableValidatorsInputs {
        commit_operator,
        prove_operator,
        execute_operator,
        gateway_validator_timelock: None,
        gateway_rpc_url: gw_rpc,
        l1_gas_price: ctx.l1_gas_price,
    };
    stage_enable_validators(&mut runner, ctx.bridgehub, chain.chain_id, &enable_inputs)
        .await
        .context("phase-3 enable-validators")?;

    let (da_type, _) = super::resolve_da(chain, ctx.vm_type, &ctx.eco)?;
    let da_inputs = SetDaValidatorPairInputs {
        l1_da_validator: relayed_sl_da_validator,
        l2_da_commitment_scheme: L2DACommitmentScheme::from_da_and_vm_types(da_type, ctx.vm_type),
        gateway_rpc_url: gw_rpc,
        l1_gas_price: ctx.l1_gas_price,
    };
    stage_set_da_validator_pair(&mut runner, ctx.bridgehub, chain.chain_id, &da_inputs)
        .await
        .context("phase-3 set-da-validator-pair")?;

    write_output_if_requested(
        &step.to_string(),
        &ctx.shared,
        &runner,
        &serde_json::json!({}),
        &GatewayMigratePhase3Output {
            chain_id: chain.chain_id,
            relayed_sl_da_validator,
        },
    )
    .await?;

    if ctx.broadcast {
        let fund = preflight::is_local_rpc(ctx.l1_rpc_url);
        apply_manifest_from(
            &ctx.manifest_path,
            pre,
            std::slice::from_ref(&ctx.deployer_key),
            Some(ctx.wallets_path),
            ctx.l1_rpc_url,
            fund,
        )
        .await
        .context("applying phase-3 bundles")?;
    }

    state.mark_done(
        step,
        &GatewayMigratePhase3Output {
            chain_id: chain.chain_id,
            relayed_sl_da_validator,
        },
    )?;
    state.save(ctx.state_path)?;
    Ok(())
}

/// Read the `relayed_sl_da_validator` address from the vote-preparation TOML
/// produced by `chain gateway convert vote-prepare`.
fn load_relayed_sl_da_validator() -> Result<Address> {
    let contracts_path = protocol_ops::common::paths::resolve_l1_contracts_path()?;
    let toml_path = contracts_path.join("script-out/gateway-vote-preparation.toml");
    let content = std::fs::read_to_string(&toml_path).context(
        "reading gateway vote preparation TOML — run `chain gateway convert vote-prepare` first",
    )?;
    let prep: VotePreparationOutput =
        toml::from_str(&content).context("parsing gateway vote preparation TOML")?;
    prep.relayed_sl_da_validator
        .ok_or_else(|| {
            anyhow::anyhow!("relayed_sl_da_validator missing from gateway-vote-preparation.toml")
        })?
        .parse()
        .context("parsing relayed_sl_da_validator as address")
}
