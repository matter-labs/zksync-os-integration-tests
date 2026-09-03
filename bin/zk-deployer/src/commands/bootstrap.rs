use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;

use crate::commands::execute_manifest::apply_manifest;
use crate::commands::genesis::{self, GenesisCommands, GenesisGenerateArgs};
use crate::commands::token::deploy::{deploy as token_deploy, TokenDeployArgs};
use crate::commands::wallets::generate::generate_wallets_yaml;
use crate::intent::IntentConfig;
use crate::state::{
    EcosystemInitOutput, GenesisGeneratedOutput, State, StepKey, TokenDeployedOutput,
    WalletsGeneratedOutput,
};
use protocol_ops::commands::ecosystem::init::{ecosystem_init, EcosystemInitInput};
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

    /// Private key of the deployer (hex, with or without 0x prefix).
    /// Defaults to Anvil account #0 for local dev.
    #[arg(
        long,
        default_value = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
    )]
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

    /// Path to persist/restore Anvil L1 state between commands.
    /// Only used when `l1_rpc_url` is absent from intent.yaml (auto-Anvil mode).
    #[arg(long, default_value = "l1-state.json")]
    pub l1_state: PathBuf,

    /// Optional subdirectory for all per-run forge script IO inside the
    /// contracts checkout (`script-config/<subdir>/` etc.), so concurrent
    /// runs don't collide. Default: the conventional fixed paths.
    #[arg(long)]
    pub subdir: Option<String>,
}

pub async fn run(args: BootstrapArgs) -> Result<()> {
    let intent = IntentConfig::load(&args.intent)
        .with_context(|| format!("loading intent file {}", args.intent.display()))?;

    let (l1_rpc_url, _anvil) =
        crate::anvil::resolve_l1(intent.l1_rpc_url.as_deref(), &args.l1_state).await?;

    if _anvil.is_none() {
        preflight::check_l1_connectivity(&l1_rpc_url)?;
    }
    preflight::check_required_artifacts()?;

    let mut state = State::load_or_new(&args.state)?;

    let deployer_address = pk_to_address(args.private_key.expose())?;
    logger::info(format!("Deployer: {deployer_address:#x}"));

    // --- Step 1: Generate wallets ----------------------------------------
    if let Some(ref src) = intent.wallets.path {
        // User supplied an existing wallets file. Copy it to wallets_out so
        // subsequent steps (apply, server-config) find it at the expected path.
        if src != &args.wallets_out {
            if let Some(parent) = args.wallets_out.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent)?;
                }
            }
            std::fs::copy(src, &args.wallets_out).with_context(|| {
                format!(
                    "copying wallets from {} to {}",
                    src.display(),
                    args.wallets_out.display()
                )
            })?;
            logger::info(format!("  wallets copied from: {}", src.display()));
        }
    } else if state.is_done(StepKey::WalletsGenerate) {
        logger::info("Skipping wallets.generate (already done)");
    } else {
        logger::step("Generating wallets...");
        let chain_ids: Vec<u64> = intent.chains.iter().map(|c| c.chain_id).collect();
        let seed = intent
            .wallets
            .ecosystem_seed
            .as_deref()
            .unwrap_or("ecosystem");
        let yaml = generate_wallets_yaml(&chain_ids, seed)?;
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

    // --- Step 2: Generate genesis -----------------------------------------
    if state.is_done(StepKey::GenesisGenerate) {
        logger::info("Skipping genesis.generate (already done)");
    } else {
        logger::step("Generating genesis...");
        genesis::run(GenesisCommands::Generate(GenesisGenerateArgs {
            genesis_config: protocol_ops::common::paths::path_from_root(
                "configs/genesis/zksync-os/latest.json",
            ),
            l1_contracts_out: protocol_ops::common::paths::resolve_l1_contracts_path()?.join("out"),
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
        let shared = SharedRunArgs {
            l1_rpc_url: l1_rpc_url.clone(),
            out: Some(args.out.clone()),
            subdir: args.subdir.clone(),
            forge_args: ForgeScriptArgs::default(),
        };
        let mut runner = ForgeRunner::new(&shared)?;
        let sender = runner.prepare_sender(deployer_address).await?;
        let owner = Wallet::resolve(None, None, &sender)?;

        let eco_input = EcosystemInitInput {
            sender: sender.address,
            owner: owner.address,
            era_chain_id: intent.main_chain_id()?,
            vm_type: VMOption::ZKSyncOsVM,
            with_testnet_verifier: true,
            zk_token_asset_id: None,
            create2_factory_salt: None,
            // Only ever deployed against a throwaway L1, where the mainnet WETH default is as
            // meaningless as any other address and nothing bridges WETH anyway.
            token_weth_address: None,
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
        let deployer_key = args.private_key.expose().to_string();
        let funder = preflight::is_local_rpc(&l1_rpc_url).then_some(deployer_key.as_str());
        apply_manifest(
            &manifest_path,
            std::slice::from_ref(&deployer_key),
            None,
            &l1_rpc_url,
            funder,
        )
        .await?;
        state.mark_done(StepKey::EcosystemBundlesApply, &serde_json::json!({}))?;
        state.save(&args.state)?;
    }

    // --- Step 5: Deploy custom base token (if any chain needs one) --------
    // Collect all chains that need a token deployed (base_token present, no address).
    // At most one distinct token may be deployed per ecosystem; if two chains declare
    // different symbols with no address, fail early rather than silently using the
    // wrong address for the second chain.
    let tokens_needing_deploy: Vec<_> = intent
        .chains
        .iter()
        .filter_map(|c| c.base_token.as_ref())
        .filter(|t| t.address.is_none())
        .collect();
    if let Some(second) = tokens_needing_deploy
        .iter()
        .skip(1)
        .find(|t| t.symbol != tokens_needing_deploy[0].symbol)
    {
        anyhow::bail!(
            "multiple chains declare different custom base tokens with no address \
             ('{}' and '{}'). Only one token can be deployed per ecosystem; \
             supply an explicit `address` for all but one.",
            tokens_needing_deploy[0].symbol,
            second.symbol,
        );
    }
    let deploy_token = tokens_needing_deploy.into_iter().next();
    if let Some(token) = deploy_token {
        if state.is_done(StepKey::EcosystemTokenDeploy) {
            logger::info("Skipping ecosystem.token_deploy (already done)");
        } else {
            logger::step("Deploying ecosystem token...");
            let eco_out: EcosystemInitOutput = state.get_output(StepKey::EcosystemInit)?;

            let symbol = token.symbol.clone();
            let name = format!("{symbol} Token");

            let token_address = token_deploy(TokenDeployArgs {
                l1_rpc_url: l1_rpc_url.clone(),
                private_key: args.private_key.clone(),
                l1_contracts_out: protocol_ops::common::paths::resolve_l1_contracts_path()?
                    .join("out"),
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

    if let Some(ref anvil) = _anvil {
        crate::anvil::save_state(anvil, &args.l1_state).await?;
        logger::info(format!("L1 state saved to: {}", args.l1_state.display()));
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
