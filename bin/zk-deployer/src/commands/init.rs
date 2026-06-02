use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;

use protocol_ops::common::logger;

#[derive(Debug, Clone, clap::ValueEnum)]
pub enum Scenario {
    /// Chains settle directly on L1 (no gateway).
    #[value(name = "l1-only")]
    L1Only,
    /// One gateway chain + one or more gateway-settling chains.
    #[value(name = "with-gateway")]
    WithGateway,
}

#[derive(Parser, Debug)]
pub struct InitArgs {
    /// Output path for the generated intent.yaml
    #[arg(
        long,
        default_value = "intent.yaml",
        help = "Path to write the generated intent.yaml"
    )]
    pub output: PathBuf,

    /// Scenario template to generate
    #[arg(
        long,
        default_value = "l1-only",
        help = "Topology template: l1-only or with-gateway"
    )]
    pub scenario: Scenario,
}

pub async fn run(args: InitArgs) -> Result<()> {
    let template = match args.scenario {
        Scenario::L1Only => L1_ONLY_TEMPLATE,
        Scenario::WithGateway => WITH_GATEWAY_TEMPLATE,
    };

    if let Some(parent) = args.output.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::write(&args.output, template)
        .with_context(|| format!("writing intent file {}", args.output.display()))?;

    logger::success(format!("intent.yaml written to: {}", args.output.display()));
    logger::info("Edit the file to fill in your L1 RPC URL and chain IDs, then run:");
    logger::info("  zk-deployer bootstrap --private-key <DEPLOYER_KEY>");
    Ok(())
}

// ---------------------------------------------------------------------------
// Templates
// ---------------------------------------------------------------------------

const L1_ONLY_TEMPLATE: &str = r#"# intent.yaml — declarative topology for zk-deployer bootstrap / apply.
# schema_version must be 1.
schema_version: 1

# Scenario: l1_only (no gateway) or with_gateway.
scenario: l1_only

# L1 RPC endpoint.
# For local development, start Anvil with:
#   anvil --chain-id 31337 --preserve-historical-states --disable-block-gas-limit \
#     --dump-state l1-state.json
l1_rpc_url: "http://localhost:8545"

# Wallets section: set generate: true to let `bootstrap` derive keys from seeds.
# Set generate: false and provide path: to use an existing wallets.yaml.
wallets:
  generate: true
  ecosystem_seed: "ecosystem"   # change for production — must be secret

# Ecosystem parameters (common to all chains in this run).
ecosystem:
  era_chain_id: 6565
  vm_type: zksyncos          # zksyncos or eravm
  with_testnet_verifier: true
  with_legacy_bridge: false

# Optional: deploy a testnet ERC20 token as the ecosystem token.
# Remove this block if you want ETH as the only base token.
#ecosystem_token:
#  deploy: true
#  symbol: "ZK"

# Chains to deploy via `apply`.
chains:
  - name: my-chain
    chain_id: 6565
    role: l1_settling         # l1_settling, gateway, or gateway_settling
    base_token: eth           # eth, ecosystem_token, or 0x<address>
    da_mode: rollup           # rollup, no_da, avail, or eigen
    deploy_paymaster: false
    pause_deposits: false
    skip_priority_txs: true
"#;

const WITH_GATEWAY_TEMPLATE: &str = r#"# intent.yaml — declarative topology with a Gateway chain.
schema_version: 1

scenario: with_gateway

# For local development, start Anvil with:
#   anvil --chain-id 31337 --preserve-historical-states --disable-block-gas-limit \
#     --dump-state l1-state.json
l1_rpc_url: "http://localhost:8545"

wallets:
  generate: true
  ecosystem_seed: "ecosystem"

ecosystem:
  era_chain_id: 505   # must match the gateway chain's chain_id
  vm_type: zksyncos
  with_testnet_verifier: true
  with_legacy_bridge: false

chains:
  # The gateway chain (settles directly on L1).
  - name: gateway
    chain_id: 505
    role: gateway
    base_token: eth
    da_mode: rollup
    deploy_paymaster: false
    pause_deposits: false
    skip_priority_txs: true

  # An application chain that settles on the gateway.
  - name: my-chain
    chain_id: 6565
    role: gateway_settling
    base_token: eth
    da_mode: rollup
    deploy_paymaster: false
    pause_deposits: true   # always paused until migration completes
    skip_priority_txs: true
"#;
