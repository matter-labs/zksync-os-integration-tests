use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use zksync_os_genesis_gen::{build_genesis_root_hash, Genesis, InitialGenesisInput};

pub use zksync_os_genesis_gen::Genesis as GenesisOutput;

#[derive(Subcommand, Debug)]
pub enum GenesisCommands {
    /// Recompute genesis_root from current L2 system contract bytecodes
    Generate(GenesisGenerateArgs),
}

#[derive(Parser, Debug)]
pub struct GenesisGenerateArgs {
    /// Path to the genesis JSON template (e.g. configs/genesis/zksync-os/latest.json)
    #[arg(long)]
    pub genesis_config: PathBuf,

    /// Path to l1-contracts/out directory containing Forge artifacts
    #[arg(long)]
    pub l1_contracts_out: PathBuf,

    /// Output path for the generated genesis.json copy
    #[arg(long, default_value = "genesis.json")]
    pub output: PathBuf,
}

pub async fn run(args: GenesisCommands) -> Result<()> {
    match args {
        GenesisCommands::Generate(args) => generate(args).await,
    }
}

async fn generate(args: GenesisGenerateArgs) -> Result<()> {
    println!("Reading genesis: {}", args.genesis_config.display());
    let content = std::fs::read_to_string(&args.genesis_config)
        .map_err(|e| anyhow::anyhow!("failed to read {}: {e}", args.genesis_config.display()))?;
    let mut genesis: Genesis = serde_json::from_str(&content)
        .map_err(|e| anyhow::anyhow!("failed to parse genesis: {e}"))?;

    let additional_storage_raw = genesis.initial_genesis.additional_storage_raw.clone();
    genesis.initial_genesis = InitialGenesisInput::from_forge_artifacts(&args.l1_contracts_out)?;
    genesis.initial_genesis.additional_storage_raw = additional_storage_raw;
    genesis.genesis_root = build_genesis_root_hash(&genesis.initial_genesis)?;

    let json = serde_json::to_string_pretty(&genesis)?;
    std::fs::write(&args.genesis_config, &json)?;

    if let Some(parent) = args.output.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::write(&args.output, &json)?;

    println!("genesis_root: {}", genesis.genesis_root);
    println!("written to:   {}", args.output.display());
    Ok(())
}
