use anyhow::Result;
use clap::Subcommand;

pub mod generate;

#[derive(Subcommand, Debug)]
pub enum WalletsCommands {
    /// Generate a deterministic wallets.yaml from string seeds
    Generate(generate::WalletsGenerateArgs),
}

pub async fn run(args: WalletsCommands) -> Result<()> {
    match args {
        WalletsCommands::Generate(args) => generate::run(args).await,
    }
}
