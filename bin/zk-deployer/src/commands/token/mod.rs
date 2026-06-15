use anyhow::Result;
use clap::Subcommand;

pub mod deploy;

#[derive(Subcommand, Debug)]
pub enum TokenCommands {
    /// Deploy a testnet ERC20 token via CREATE2 and register on NTV
    Deploy(deploy::TokenDeployArgs),
}

pub async fn run(args: TokenCommands) -> Result<()> {
    match args {
        TokenCommands::Deploy(args) => deploy::run(args).await,
    }
}
