use std::path::PathBuf;

use alloy::primitives::Address;
use anyhow::{Context as _, Result};
use clap::Parser;

use crate::intent::{ChainIntent, DaMode, IntentConfig, ValidiumDa};
use crate::state::{
    ChainInitPreparedOutput, EcosystemInitOutput, State, StepKey, TokenDeployedOutput,
};
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

    /// Chain ID to generate config for. Defaults to the first chain in
    /// intent.yaml.
    #[arg(long)]
    pub chain: Option<u64>,

    /// Output path for the server config YAML
    #[arg(long, default_value = "server.yaml")]
    pub output: PathBuf,
}

pub async fn run(args: ServerConfigArgs) -> Result<()> {
    let intent = IntentConfig::load(&args.intent)
        .with_context(|| format!("loading intent file {}", args.intent.display()))?;
    let state = State::load(&args.state)
        .with_context(|| format!("loading state file {}", args.state.display()))?;

    let chain_intent = pick_chain(&intent, args.chain)?;
    let chain_id = chain_intent.chain_id;

    let eco_out: EcosystemInitOutput = state
        .get_output(StepKey::EcosystemInit)
        .context("ecosystem.init not found in state — run `bootstrap` first")?;

    // Validate that `apply` ran for this chain (the output itself isn't needed
    // to render config — the server resolves the diamond from bridgehub + id).
    let _chain_out: ChainInitPreparedOutput = state
        .get_output(StepKey::ChainInitPrepared(chain_id))
        .with_context(|| {
            format!("chain.init.{chain_id}.prepared not found in state — run `apply` first")
        })?;

    let wallets = load_wallets(&args.wallets)?;
    let chain_wallets = wallets
        .chains
        .get(&chain_id.to_string())
        .with_context(|| format!("chain {chain_id} not found in wallets.yaml"))?;

    let genesis_path = {
        let dir = args.intent.parent().unwrap_or(std::path::Path::new("."));
        dir.join("genesis.json")
    };
    if !genesis_path.exists() {
        anyhow::bail!(
            "genesis.json not found at {} — run `bootstrap` first",
            genesis_path.display()
        );
    }
    let genesis_path = genesis_path
        .canonicalize()
        .with_context(|| format!("canonicalizing {}", genesis_path.display()))?;

    let pubdata_mode = resolve_pubdata_mode(chain_intent);

    let base_token_addr = resolve_base_token_addr(chain_intent, &state)?;

    let yaml = render_config(
        &eco_out,
        chain_wallets,
        chain_intent,
        pubdata_mode,
        base_token_addr,
        &genesis_path,
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
    let l1_url_hint = intent.l1_rpc_url.as_deref().unwrap_or("<L1_RPC_URL>");
    println!(
        "  L1_PROVIDER_RPC_URL={} zksync-os-server --config local_dev.yaml --config {}",
        l1_url_hint,
        args.output.display()
    );

    Ok(())
}

// ---------------------------------------------------------------------------

/// Render the **deployment slice** of a chain's `zksync-os-server` config: the
/// values fixed by the deployment itself (bridgehub + bytecode-supplier
/// addresses, chain id, operator signing keys, base-token price). Runtime
/// concerns (ports, db/scratch paths, the L1 RPC URL) are NOT emitted here —
/// consumers apply those by mutating the typed `Config` after load (see the
/// test framework's `ChainRuntime`), mirroring zksync-os-server's own
/// integration-test practice.
///
/// `genesis_input_path` defaults to `./genesis.json` so the file is directly
/// usable as a CLI deliverable (`zk-deployer server-config`); tests override it
/// with an absolute path via the typed `Config`.
pub(crate) fn render_config(
    eco: &EcosystemInitOutput,
    wallets: &ChainWallets,
    intent: &ChainIntent,
    pubdata_mode: &str,
    base_token_addr: Option<Address>,
    genesis_path: &std::path::Path,
) -> Result<String> {
    let bytecodes_supplier = eco
        .bytecodes_supplier
        .context("bytecodes_supplier not recorded in state — re-run `bootstrap` to populate it")?;

    let mut out = String::new();

    out.push_str(&format!(
        "\
genesis:
  bridgehub_address: \"{:#x}\"
  bytecode_supplier_address: \"{:#x}\"
  genesis_input_path: \"{}\"
  chain_id: {}

l1_sender:
  pubdata_mode: {pubdata_mode}
  operator_commit_sk: \"{}\"
  operator_prove_sk: \"{}\"
  operator_execute_sk: \"{}\"
",
        eco.bridgehub_proxy,
        bytecodes_supplier,
        genesis_path.display(),
        intent.chain_id,
        wallets
            .operator_commit_sk
            .private_key_b256()
            .context("operator_commit_sk has no private key")?,
        wallets
            .operator_prove_sk
            .private_key_b256()
            .context("operator_prove_sk has no private key")?,
        wallets
            .operator_execute_sk
            .private_key_b256()
            .context("operator_execute_sk has no private key")?,
    ));

    if let Some(addr) = base_token_addr {
        out.push_str(&format!(
            "
external_price_api_client:
  forced_prices:
    \"0x0000000000000000000000000000000000000001\": 1
    \"{addr:#x}\": 1

base_token_price_updater:
  fallback_prices:
    \"0x0000000000000000000000000000000000000001\": 1
    \"{addr:#x}\": 1
"
        ));
    }

    Ok(out)
}

pub(crate) fn resolve_base_token_addr(
    chain: &ChainIntent,
    state: &State,
) -> Result<Option<Address>> {
    match &chain.base_token {
        None => Ok(None),
        Some(t) if t.address.is_some() => Ok(t.address),
        Some(_) => {
            let tok: TokenDeployedOutput = state
                .get_output(StepKey::EcosystemTokenDeploy)
                .context("custom base_token with no address but no token deployed in state — run `bootstrap` first")?;
            Ok(Some(tok.token_address))
        }
    }
}

/// The mechanism the server publishes each batch's pubdata with — the server's `PubdataMode`,
/// which has to agree with the L2 DA commitment scheme the chain was registered with. It says
/// nothing about how *much* pubdata there is; that is the chain's `PubdataContent`, set at init.
pub(crate) fn resolve_pubdata_mode(chain: &ChainIntent) -> &'static str {
    match chain.da_mode {
        DaMode::Rollup | DaMode::Avail | DaMode::Validium(ValidiumDa::Blobs) => "Blobs",
        DaMode::Validium(ValidiumDa::Calldata) => "Calldata",
        // `Validium` is the server's post-nothing mode and the only one its startup check accepts
        // for a chain whose pricing mode is Validium below v33.
        DaMode::Validium(ValidiumDa::DiscouragedNoDa) => "Validium",
    }
}

fn pick_chain(intent: &IntentConfig, chain_id: Option<u64>) -> Result<&ChainIntent> {
    match chain_id {
        Some(id) => intent
            .chains
            .iter()
            .find(|c| c.chain_id == id)
            .with_context(|| format!("chain {id} not found in intent.yaml")),
        None => intent
            .chains
            .first()
            .context("intent.yaml has no chains defined"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intent::DaMode;
    use alloy::primitives::Address;
    use alloy::primitives::B256;
    use alloy::signers::local::PrivateKeySigner;
    use protocol_ops::common::wallets::Wallet;

    fn sample_eco() -> EcosystemInitOutput {
        EcosystemInitOutput {
            bridgehub_proxy: Address::repeat_byte(0xbb),
            ctm_proxy: Address::repeat_byte(0xcc),
            bytecodes_supplier: Some(Address::repeat_byte(0xdd)),
            rollup_l1_da_validator: Address::repeat_byte(0x01),
            no_da_l1_validator: Address::repeat_byte(0x02),
            avail_l1_da_validator: Address::repeat_byte(0x03),
            blobs_zksync_os_l1_da_validator: Some(Address::repeat_byte(0x04)),
            governance: Address::repeat_byte(0xaa),
        }
    }

    fn sample_wallets() -> ChainWallets {
        ChainWallets {
            owner: Wallet {
                address: Address::repeat_byte(0x10),
                private_key: None,
            },
            operator_commit_sk: Wallet::new(
                PrivateKeySigner::from_bytes(&B256::repeat_byte(0xaa)).unwrap(),
            ),
            operator_prove_sk: Wallet::new(
                PrivateKeySigner::from_bytes(&B256::repeat_byte(0xbb)).unwrap(),
            ),
            operator_execute_sk: Wallet::new(
                PrivateKeySigner::from_bytes(&B256::repeat_byte(0xcc)).unwrap(),
            ),
        }
    }

    fn sample_intent(chain_id: u64) -> ChainIntent {
        ChainIntent {
            chain_id,
            base_token: None,
            da_mode: DaMode::Rollup,
        }
    }

    /// The deployment slice is hand-rendered YAML; guard it against
    /// format/indentation/missing-key regressions. (Schema validity of the
    /// required `genesis.*` fields is additionally enforced at server startup
    /// in the integration tests, and the runtime slice is compiler-checked.)
    #[test]
    fn renders_valid_deployment_slice() {
        let eco = sample_eco();
        let wallets = sample_wallets();
        let intent = sample_intent(6565);

        let yaml = render_config(
            &eco,
            &wallets,
            &intent,
            "Blobs",
            None,
            std::path::Path::new("/workdir/genesis.json"),
        )
        .unwrap();
        let doc: serde_yaml::Value = serde_yaml::from_str(&yaml).expect("valid YAML");

        let genesis = &doc["genesis"];
        assert_eq!(
            genesis["bridgehub_address"].as_str().unwrap(),
            format!("{:#x}", eco.bridgehub_proxy)
        );
        assert_eq!(
            genesis["bytecode_supplier_address"].as_str().unwrap(),
            format!("{:#x}", eco.bytecodes_supplier.unwrap())
        );
        assert_eq!(genesis["chain_id"].as_u64().unwrap(), 6565);

        let l1_sender = &doc["l1_sender"];
        assert_eq!(l1_sender["pubdata_mode"].as_str().unwrap(), "Blobs");
        assert_eq!(
            l1_sender["operator_commit_sk"].as_str().unwrap(),
            &format!("{}", B256::repeat_byte(0xaa))
        );
        assert_eq!(
            l1_sender["operator_prove_sk"].as_str().unwrap(),
            &format!("{}", B256::repeat_byte(0xbb))
        );
        assert_eq!(
            l1_sender["operator_execute_sk"].as_str().unwrap(),
            &format!("{}", B256::repeat_byte(0xcc))
        );

        // No base token → no price-override sections.
        assert!(doc.get("external_price_api_client").is_none());
        assert!(doc.get("base_token_price_updater").is_none());
    }

    /// A custom base token adds the forced-price sections, keyed by its address.
    #[test]
    fn renders_base_token_price_sections() {
        let yaml = render_config(
            &sample_eco(),
            &sample_wallets(),
            &sample_intent(6565),
            "Blobs",
            Some(
                "0x000000000000000000000000000000000000abcd"
                    .parse()
                    .unwrap(),
            ),
            std::path::Path::new("/workdir/genesis.json"),
        )
        .unwrap();
        let doc: serde_yaml::Value = serde_yaml::from_str(&yaml).expect("valid YAML");

        let eth = "0x0000000000000000000000000000000000000001";
        let erc20 = "0x000000000000000000000000000000000000abcd";
        assert!(doc["external_price_api_client"]["forced_prices"][erc20]
            .as_u64()
            .is_some());
        assert!(doc["external_price_api_client"]["forced_prices"][eth]
            .as_u64()
            .is_some());
        assert!(doc["base_token_price_updater"]["fallback_prices"][erc20]
            .as_u64()
            .is_some());
        assert!(doc["base_token_price_updater"]["fallback_prices"][eth]
            .as_u64()
            .is_some());
    }
}
