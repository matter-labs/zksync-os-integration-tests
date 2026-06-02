use clap::{Parser, Subcommand};
use zk_deployer::commands;

#[derive(Parser, Debug)]
#[command(name = "zk-deployer", about)]
struct ZkDeployer {
    #[command(subcommand)]
    command: ZkDeployerSubcommands,
}

#[derive(Subcommand, Debug)]
enum ZkDeployerSubcommands {
    /// Generate a starter intent.yaml template
    Init(Box<commands::init::InitArgs>),
    /// Bootstrap an ecosystem from an intent.yaml (genesis → wallets → ecosystem init → token)
    Bootstrap(Box<commands::bootstrap::BootstrapArgs>),
    /// Apply chain init for all chains declared in intent.yaml
    Apply(Box<commands::apply::ApplyArgs>),
    /// Generate a ZKsync OS server config YAML from state.json + wallets.yaml
    ServerConfig(Box<commands::server_config::ServerConfigArgs>),
    /// Build all Forge contract artifacts required by bootstrap and apply
    BuildContracts(commands::build_contracts::DevBuildContractsArgs),
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    human_panic::setup_panic!();
    let cli_args = ZkDeployer::parse();
    match cli_args.command {
        ZkDeployerSubcommands::Init(args) => commands::init::run(*args).await?,
        ZkDeployerSubcommands::Bootstrap(args) => commands::bootstrap::run(*args).await?,
        ZkDeployerSubcommands::Apply(args) => commands::apply::run(*args).await?,
        ZkDeployerSubcommands::ServerConfig(args) => commands::server_config::run(*args).await?,
        ZkDeployerSubcommands::BuildContracts(args) => commands::build_contracts::run(args).await?,
    }
    Ok(())
}
