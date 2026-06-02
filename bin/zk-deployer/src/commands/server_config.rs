use std::path::PathBuf;

use anyhow::{Context as _, Result};
use clap::Parser;

use crate::intent::{BaseToken, ChainIntent, ChainRole, DaMode, IntentConfig};
use crate::state::{ChainInitOutput, EcosystemInitOutput, State, StepKey, TokenDeployedOutput};
use protocol_ops::common::wallets::{load_wallets, ChainWallets};

#[derive(Parser, Debug)]
pub struct ServerConfigArgs {
    /// Path to intent.yaml
    #[arg(long, default_value = "intent.yaml")]
    pub intent: PathBuf,

    /// Path to state.json (produced by bootstrap + apply)
    #[arg(long, default_value = "state.json")]
    pub state: PathBuf,

    /// Path to wallets.yaml (produced by bootstrap)
    #[arg(long, default_value = "wallets.yaml")]
    pub wallets: PathBuf,

    /// Chain name to generate config for. Defaults to the first chain in
    /// intent.yaml.
    #[arg(long)]
    pub chain: Option<String>,

    /// Output path for the server config YAML
    #[arg(long, default_value = "server.yaml")]
    pub output: PathBuf,
}

pub async fn run(args: ServerConfigArgs) -> Result<()> {
    let intent = IntentConfig::load(&args.intent)
        .with_context(|| format!("loading intent file {}", args.intent.display()))?;
    let state = State::load(&args.state)
        .with_context(|| format!("loading state file {}", args.state.display()))?;

    let chain_intent = pick_chain(&intent, args.chain.as_deref())?;
    let chain_name = &chain_intent.name;

    let eco_out: EcosystemInitOutput = state
        .get_output(StepKey::EcosystemInit)
        .context("ecosystem.init not found in state — run `bootstrap` first")?;

    let chain_out: ChainInitOutput = state
        .get_output(StepKey::ChainInit(chain_name.clone()))
        .with_context(|| {
            format!("chain.init.{chain_name} not found in state — run `apply` first")
        })?;

    let wallets = load_wallets(&args.wallets)?;
    let chain_wallets = wallets
        .chains
        .get(chain_name)
        .with_context(|| format!("chain '{chain_name}' not found in wallets.yaml"))?;

    if !PathBuf::from("genesis.json").exists() {
        anyhow::bail!("genesis.json not found in current directory — run `bootstrap` first");
    }

    let pubdata_mode = resolve_pubdata_mode(chain_intent);

    let base_token_addr = resolve_base_token_addr(chain_intent, &state)?;

    // For gateway-settling chains, read the gateway chain ID from the convert step output.
    let gateway_chain_id = if chain_intent.role == ChainRole::GatewaySettling {
        let convert_out: serde_json::Value = state
            .get_output(StepKey::GatewayConvert)
            .context("chain.gateway.convert not found in state — run `apply` first to complete gateway conversion")?;
        let id = convert_out["gateway_chain_id"]
            .as_u64()
            .context("gateway_chain_id not recorded in chain.gateway.convert state")?;
        Some(id)
    } else {
        None
    };

    let yaml = render_config(
        &eco_out,
        &chain_out,
        chain_wallets,
        chain_intent,
        pubdata_mode,
        base_token_addr.as_deref(),
        gateway_chain_id,
    )?;

    if let Some(parent) = args.output.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::write(&args.output, &yaml)
        .with_context(|| format!("writing server config to {}", args.output.display()))?;

    println!("Server config written to: {}", args.output.display());
    println!();
    println!("Start the server with:");
    if gateway_chain_id.is_some() {
        println!(
            "  L1_PROVIDER_RPC_URL={l1} zksync-os-server --config local_dev.yaml --config {}",
            args.output.display(),
            l1 = intent.l1_rpc_url,
        );
    } else {
        println!(
            "  L1_PROVIDER_RPC_URL={} zksync-os-server --config local_dev.yaml --config {}",
            intent.l1_rpc_url,
            args.output.display()
        );
    }

    Ok(())
}

// ---------------------------------------------------------------------------

pub(crate) fn render_config(
    eco: &EcosystemInitOutput,
    chain: &ChainInitOutput,
    wallets: &ChainWallets,
    intent: &ChainIntent,
    pubdata_mode: &str,
    base_token_addr: Option<&str>,
    gateway_chain_id: Option<u64>,
) -> Result<String> {
    let _ = chain; // diamond_proxy is resolved by the server from bridgehub + chain_id
    let bytecodes_supplier = eco
        .bytecodes_supplier
        .context("bytecodes_supplier not recorded in state — re-run `bootstrap` to populate it")?;

    let mut out = String::new();

    let chain_name = &intent.name;
    if let Some(gw_id) = gateway_chain_id {
        out.push_str(&format!(
            "general:\n  gateway_chain_id: {gw_id}\n  rocks_db_path: \"./db/{chain_name}\"\n\n"
        ));
    } else {
        out.push_str(&format!(
            "general:\n  rocks_db_path: \"./db/{chain_name}\"\n\n"
        ));
    }

    out.push_str(&format!(
        "\
genesis:
  bridgehub_address: \"{:#x}\"
  bytecode_supplier_address: \"{:#x}\"
  genesis_input_path: \"./genesis.json\"
  chain_id: {}

l1_sender:
  pubdata_mode: {pubdata_mode}
  operator_commit_sk: \"{}\"
  operator_prove_sk: \"{}\"
  operator_execute_sk: \"{}\"
",
        eco.bridgehub_proxy,
        bytecodes_supplier,
        intent.chain_id,
        wallets.operator_commit_sk,
        wallets.operator_prove_sk,
        wallets.operator_execute_sk,
    ));

    if gateway_chain_id.is_some() {
        // gateway_settling chains commit/prove/execute their batches via the gateway.
        // Ports are offset from the gateway defaults to avoid conflicts when both
        // servers run on the same machine.
        out.push_str(&format!(
            "
gateway_sender:
  operator_commit_sk: \"{}\"
  operator_prove_sk: \"{}\"
  operator_execute_sk: \"{}\"

gateway_provider:
  rpc_url: \"http://localhost:3050\"

rpc:
  address: \"0.0.0.0:3051\"

status_server:
  address: \"0.0.0.0:3072\"

prover_api:
  address: \"0.0.0.0:3125\"

observability:
  prometheus:
    port: 3313
",
            wallets.operator_commit_sk, wallets.operator_prove_sk, wallets.operator_execute_sk,
        ));
    }

    if let Some(addr) = base_token_addr {
        // local_dev.yaml already sets source: Forced and ETH price via deep merge.
        // We only need to add the base token entry on top.
        out.push_str(&format!(
            "
external_price_api_client:
  forced_prices:
    \"{addr}\": 1

base_token_price_updater:
  fallback_prices:
    \"{addr}\": 1
"
        ));
    }

    Ok(out)
}

pub(crate) fn resolve_base_token_addr_pub(
    chain: &ChainIntent,
    state: &State,
) -> Result<Option<String>> {
    resolve_base_token_addr(chain, state)
}

pub(crate) fn resolve_pubdata_mode_pub(chain: &ChainIntent) -> &'static str {
    resolve_pubdata_mode(chain)
}

fn resolve_base_token_addr(chain: &ChainIntent, state: &State) -> Result<Option<String>> {
    match &chain.base_token {
        BaseToken::Eth => Ok(None),
        BaseToken::EcosystemToken => {
            let tok: TokenDeployedOutput = state
                .get_output(StepKey::EcosystemTokenDeploy)
                .context("base_token: ecosystem_token but no token deployed in state — run `bootstrap` first")?;
            Ok(Some(format!("{:#x}", tok.token_address)))
        }
        BaseToken::Address(addr) => Ok(Some(format!("{addr:#x}"))),
    }
}

fn resolve_pubdata_mode(chain: &ChainIntent) -> &'static str {
    match chain.role {
        ChainRole::GatewaySettling => "RelayedL2Calldata",
        _ => match chain.da_mode {
            DaMode::NoDa => "RelayedL2Calldata",
            _ => "Blobs",
        },
    }
}

fn pick_chain<'a>(intent: &'a IntentConfig, name: Option<&str>) -> Result<&'a ChainIntent> {
    match name {
        Some(n) => intent
            .chains
            .iter()
            .find(|c| c.name == n)
            .with_context(|| format!("chain '{n}' not found in intent.yaml")),
        None => intent
            .chains
            .first()
            .context("intent.yaml has no chains defined"),
    }
}
