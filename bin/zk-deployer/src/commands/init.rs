use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;

use protocol_ops::common::logger;

#[derive(Parser, Debug)]
pub struct InitArgs {
    /// Output path for the generated intent.yaml
    #[arg(long, default_value = "intent.yaml")]
    pub output: PathBuf,
}

pub async fn run(args: InitArgs) -> Result<()> {
    let template = L1_TEMPLATE;

    if let Some(parent) = args.output.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::write(&args.output, template)
        .with_context(|| format!("writing intent file {}", args.output.display()))?;

    logger::success(format!("intent.yaml written to: {}", args.output.display()));
    logger::info("Edit the file to set your chain IDs, then run:");
    logger::info("  zk-deployer bootstrap --broadcast");
    Ok(())
}

const L1_TEMPLATE: &str = r#"# intent.yaml — declarative topology for zk-deployer bootstrap / apply.
# schema_version must be 1.
schema_version: 1

# L1 RPC endpoint.
# Leave commented out to use auto-managed Anvil (local dev, no setup needed).
# Uncomment to target a real network (Sepolia, Mainnet, etc.).
# l1_rpc_url: "https://..."

chains:
  - chain_id: 6565
    da_mode: rollup           # rollup, logs_only_validium, or avail
"#;
