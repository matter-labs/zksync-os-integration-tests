use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;

use crate::intent::{IntentConfig, VmType};
use crate::state::{
    EcosystemInitOutput, GenesisGeneratedOutput, State, StepKey, TokenDeployedOutput,
    WalletsGeneratedOutput,
};
use protocol_ops::commands::dev::execute_manifest::apply_manifest;
use protocol_ops::commands::ecosystem::init::{ecosystem_init, EcosystemInitInput};
use protocol_ops::commands::genesis::{self, GenesisCommands, GenesisGenerateArgs};
use protocol_ops::commands::token::deploy::{deploy as token_deploy, TokenDeployArgs};
use protocol_ops::commands::wallets::generate::generate_wallets_yaml;
use protocol_ops::common::output::write_output_if_requested;
use protocol_ops::common::{
    args::SharedRunArgs,
    forge::{ForgeRunner, ForgeScriptArgs},
    logger, preflight,
    private_key::pk_to_address,
    wallets::Wallet,
    PrivateKey,
};
use protocol_ops::types::VMOption;

#[derive(Parser, Debug)]
pub struct BootstrapArgs {
    /// Path to intent.yaml
    #[arg(long, default_value = "intent.yaml")]
    pub intent: PathBuf,

    /// Path to state.json (tracks completed steps for resumability)
    #[arg(long, default_value = "state.json")]
    pub state: PathBuf,

    /// Output directory for Safe Transaction Builder bundles (ecosystem init)
    #[arg(long, default_value = "out")]
    pub out: PathBuf,

    /// Private key of the deployer (hex, with or without 0x prefix)
    #[arg(long)]
    pub private_key: PrivateKey,

    /// Path to write generated wallets.yaml
    #[arg(long, default_value = "wallets.yaml")]
    pub wallets_out: PathBuf,

    /// Path to write generated genesis.json copy
    #[arg(long, default_value = "genesis.json")]
    pub genesis_out: PathBuf,

    /// Broadcast the generated Safe bundles to `--l1-rpc-url` immediately after
    /// generating them. On localhost Anvil, target addresses are funded
    /// automatically via `anvil_setBalance`. Use this flag for local dev
    /// to skip the manual `dev execute-manifest` step.
    #[arg(long, default_value = "false")]
    pub broadcast: bool,
}

pub async fn run(args: BootstrapArgs) -> Result<()> {
    let intent = IntentConfig::load(&args.intent)
        .with_context(|| format!("loading intent file {}", args.intent.display()))?;

    preflight::check_l1_connectivity(&intent.l1_rpc_url)?;
    preflight::check_required_artifacts()?;

    let mut state = State::load_or_new(&args.state)?;

    let deployer_address = pk_to_address(args.private_key.expose())?;
    logger::info(format!("Deployer: {deployer_address:#x}"));

    // --- Step 1: Generate wallets ----------------------------------------
    if intent.wallets.generate {
        if state.is_done(StepKey::WalletsGenerate) {
            logger::info("Skipping wallets.generate (already done)");
        } else {
            logger::step("Generating wallets...");
            let chain_names: Vec<String> = intent.chains.iter().map(|c| c.name.clone()).collect();
            let seed = intent
                .wallets
                .ecosystem_seed
                .as_deref()
                .unwrap_or("ecosystem");
            let yaml = generate_wallets_yaml(&chain_names, seed)?;
            write_file(&args.wallets_out, yaml.as_bytes())?;
            let path_str = args.wallets_out.display().to_string();
            logger::info(format!("  wallets written to: {path_str}"));
            state.mark_done(
                StepKey::WalletsGenerate,
                &WalletsGeneratedOutput {
                    output_path: path_str,
                },
            )?;
            state.save(&args.state)?;
        }
    }

    // --- Step 2: Generate genesis -----------------------------------------
    if state.is_done(StepKey::GenesisGenerate) {
        logger::info("Skipping genesis.generate (already done)");
    } else {
        logger::step("Generating genesis...");
        genesis::run(GenesisCommands::Generate(GenesisGenerateArgs {
            output: args.genesis_out.clone(),
        }))
        .await?;
        let path_str = args.genesis_out.display().to_string();
        state.mark_done(
            StepKey::GenesisGenerate,
            &GenesisGeneratedOutput {
                output_path: path_str,
            },
        )?;
        state.save(&args.state)?;
    }

    // --- Step 3: Ecosystem init -------------------------------------------
    if state.is_done(StepKey::EcosystemInit) {
        logger::info("Skipping ecosystem.init (already done)");
    } else {
        logger::step("Initializing ecosystem...");
        let vm_type = match intent.ecosystem.vm_type {
            VmType::Zksyncos => VMOption::ZKSyncOsVM,
            VmType::Eravm => VMOption::EraVM,
        };
        let shared = SharedRunArgs {
            l1_rpc_url: intent.l1_rpc_url.clone(),
            out: Some(args.out.clone()),
            forge_args: ForgeScriptArgs::default(),
        };
        let mut runner = ForgeRunner::new(&shared)?;
        let sender = runner.prepare_sender(deployer_address).await?;
        let owner = Wallet::resolve(None, None, &sender)?;

        let eco_input = EcosystemInitInput {
            sender: sender.address,
            owner: owner.address,
            era_chain_id: intent.ecosystem.era_chain_id,
            vm_type,
            with_testnet_verifier: intent.ecosystem.with_testnet_verifier,
            with_legacy_bridge: intent.ecosystem.with_legacy_bridge,
            zk_token_asset_id: None,
            create2_factory_salt: None,
        };
        let eco_output = ecosystem_init(&mut runner, &sender, &owner, &eco_input).await?;

        write_output_if_requested("ecosystem.init", &shared, &runner, &eco_input, &eco_output)
            .await?;

        let deployed = &eco_output.ctm.deployed_addresses;
        let bridgehub_proxy = eco_output
            .hub
            .deployed_addresses
            .bridgehub
            .bridgehub_proxy_addr;
        let ctm_proxy = deployed.state_transition.state_transition_proxy_addr;
        let bytecodes_supplier = deployed.state_transition.bytecodes_supplier_addr;
        let rollup_l1_da_validator = deployed.rollup_l1_da_validator_addr;
        let no_da_l1_validator = deployed.no_da_validium_l1_validator_addr;
        let avail_l1_da_validator = deployed.avail_l1_da_validator_addr;
        let blobs_zksync_os_l1_da_validator = deployed.blobs_zksync_os_l1_da_validator_addr;
        let governance = eco_output.hub.deployed_addresses.governance_addr;

        logger::info(format!("Bridgehub:          {bridgehub_proxy:#x}"));
        logger::info(format!("CTM:                {ctm_proxy:#x}"));
        logger::info(format!("Bytecodes supplier: {bytecodes_supplier:#x}"));

        state.mark_done(
            StepKey::EcosystemInit,
            &EcosystemInitOutput {
                bridgehub_proxy,
                ctm_proxy,
                bytecodes_supplier: Some(bytecodes_supplier),
                rollup_l1_da_validator,
                no_da_l1_validator,
                avail_l1_da_validator,
                blobs_zksync_os_l1_da_validator,
                governance,
            },
        )?;
        state.save(&args.state)?;
    }

    // --- Step 4: Broadcast ecosystem Safe bundles (opt-in) ---------------
    if state.is_done(StepKey::EcosystemBundlesApply) {
        logger::info("Skipping ecosystem.bundles.apply (already done)");
    } else if args.broadcast && args.out.join("manifest.json").exists() {
        logger::step("Broadcasting ecosystem Safe bundles to L1...");
        let manifest_path = args.out.join("manifest.json");
        let fund = preflight::is_local_rpc(&intent.l1_rpc_url);
        apply_manifest(
            &manifest_path,
            &[args.private_key.expose().to_string()],
            None,
            &intent.l1_rpc_url,
            fund,
        )
        .await?;
        state.mark_done(StepKey::EcosystemBundlesApply, &serde_json::json!({}))?;
        state.save(&args.state)?;
    }

    // --- Step 5: Deploy ecosystem token (optional) -----------------------
    if let Some(token_intent) = &intent.ecosystem_token {
        if !token_intent.deploy {
            // Nothing to do
        } else if state.is_done(StepKey::EcosystemTokenDeploy) {
            logger::info("Skipping ecosystem.token_deploy (already done)");
        } else {
            logger::step("Deploying ecosystem token...");
            let eco_out: EcosystemInitOutput = state.get_output(StepKey::EcosystemInit)?;

            let symbol = token_intent
                .symbol
                .as_deref()
                .unwrap_or("TOKEN")
                .to_string();
            let name = format!("{symbol} Token");

            let token_address = token_deploy(TokenDeployArgs {
                l1_rpc_url: intent.l1_rpc_url.clone(),
                private_key: args.private_key.clone(),
                bridgehub: eco_out.bridgehub_proxy,
                symbol,
                name,
                mint_to: vec![],
                mint_amount: None,
                salt: None,
            })
            .await?;

            logger::info(format!("Token deployed: {token_address:#x}"));
            state.mark_done(
                StepKey::EcosystemTokenDeploy,
                &TokenDeployedOutput { token_address },
            )?;
            state.save(&args.state)?;
        }
    }

    logger::success("Bootstrap complete.");
    Ok(())
}

fn write_file(path: &PathBuf, content: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::write(path, content).with_context(|| format!("writing {}", path.display()))
}
