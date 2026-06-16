//! rstest fixture functions.
//!
//! These are the only entry points for test setup. Tests declare what they
//! need as `#[future]` parameters; rstest wires up the dependency graph.

pub mod cache;
mod l1;
pub mod restore;

pub use l1::ecosystem;

/// Deployer key (Anvil account #0) — used only for ecosystem setup, never as a test wallet.
const DEPLOYER_KEY: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

const TEST_CHAIN_ID: u64 = 6565;

use tokio::sync::OnceCell;
use tracing_subscriber::EnvFilter;

static BUILD_CONTRACTS_ONCE: OnceCell<()> = OnceCell::const_new();

pub fn init_logging() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("warn,zksync_os=info")),
        )
        .with_test_writer()
        .try_init();
}

pub async fn ensure_contracts_built() {
    BUILD_CONTRACTS_ONCE
        .get_or_init(|| async {
            zk_deployer::commands::build_contracts::run(
                zk_deployer::commands::build_contracts::DevBuildContractsArgs { with_l2: false },
            )
            .await
            .expect("build-contracts failed");
        })
        .await;
}
