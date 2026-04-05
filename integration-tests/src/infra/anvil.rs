use anyhow::Context;
use std::process::Command;
use std::time::Duration;

use crate::find_ports::LockedPort;
use crate::presets::RepoRef;
use crate::server_utils::wait_for_chain_to_be_ready;
use alloy::providers::ProviderBuilder;

/// Default private key for Anvil's first pre-funded account.
pub const DEFAULT_ANVIL_PRIVATE_KEY: &str =
    "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

/// L1 chain id as expected by contracts deployed in `zkos-l1-state.json`
const L1_CHAIN_ID: u64 = 31337;
const L1_READY_MAX_ATTEMPTS: usize = 10;
const L1_READY_RETRY_DELAY: Duration = Duration::from_secs(1);

fn decompress_l1_state_gz(
    state_gz: &std::path::Path,
    state_json: &std::path::Path,
) -> anyhow::Result<()> {
    let output = Command::new("gzip")
        .arg("-dfk")
        .arg(state_gz)
        .output()
        .with_context(|| format!("Failed to execute gzip for {}", state_gz.display()))?;

    if !output.status.success() {
        anyhow::bail!(
            "Failed to decompress {}:\nSTDOUT:\n{}\nSTDERR:\n{}",
            state_gz.display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    if !state_json.exists() {
        anyhow::bail!(
            "gzip completed but decompressed file not found: {}",
            state_json.display()
        );
    }

    Ok(())
}

/// Resolve an L1 state file from a version directory (e.g. `local-chains/v30.2`).
///
/// Checks for `<dir>/default/zkos-l1-state.json`, then `<dir>/l1-state.json`,
/// then decompresses `<dir>/l1-state.json.gz` if present.
pub fn resolve_l1_state_in_version_dir(version_dir: &std::path::Path) -> anyhow::Result<String> {
    let legacy_state = version_dir.join("default").join("zkos-l1-state.json");
    if legacy_state.exists() {
        return Ok(legacy_state.to_string_lossy().to_string());
    }

    let state_json = version_dir.join("l1-state.json");
    if state_json.exists() {
        return Ok(state_json.to_string_lossy().to_string());
    }

    let state_gz = version_dir.join("l1-state.json.gz");
    if state_gz.exists() {
        decompress_l1_state_gz(&state_gz, &state_json)?;
        return Ok(state_json.to_string_lossy().to_string());
    }

    anyhow::bail!(
        "Could not locate L1 state file. Checked '{}', '{}' and '{}'",
        legacy_state.display(),
        state_json.display(),
        state_gz.display()
    )
}

/// A running anvil L1 instance
/// The provider is stored to keep the anvil process alive
pub struct Anvil {
    _provider: Box<dyn std::any::Any + Send + Sync>,
    port: u16,
    rpc_url: String,
}

impl Anvil {
    /// Wrap an externally-managed Anvil process. The caller is responsible for
    /// the process lifetime — this struct only provides the RPC URL and port.
    pub fn wrap_external(port: u16) -> Self {
        Self {
            _provider: Box::new(()),
            port,
            rpc_url: format!("http://localhost:{}", port),
        }
    }

    /// Spawn an anvil instance loading state from an explicit file path.
    pub async fn spawn_with_state(state_path: impl AsRef<std::path::Path>) -> anyhow::Result<Self> {
        let state_path = state_path.as_ref();
        anyhow::ensure!(
            state_path.exists(),
            "L1 state file not found: {}",
            state_path.display()
        );
        let state_str = state_path.to_string_lossy().to_string();

        let locked_port = LockedPort::acquire_unused()
            .await
            .context("Failed to acquire unused port for anvil")?;
        let port = locked_port.port;
        let rpc_url = format!("http://localhost:{}", port);

        let provider = ProviderBuilder::new().on_anvil_with_wallet_and_config(|anvil| {
            anvil
                .port(port)
                .chain_id(L1_CHAIN_ID)
                .arg("--host")
                .arg("0.0.0.0")
                .arg("--load-state")
                .arg(&state_str)
                .arg("--disable-block-gas-limit")
        });

        let provider: Box<dyn std::any::Any + Send + Sync> = Box::new(provider);
        wait_for_l1_ready(&rpc_url).context("Anvil L1 is not reachable after spawn")?;

        Ok(Self {
            _provider: provider,
            port,
            rpc_url,
        })
    }

    /// Spawn an anvil instance loading state on a specific port.
    ///
    /// Use this when the loaded state contains RPC URLs that reference a fixed port
    /// (e.g. ephemeral RocksDB state that stores the L1 URL from state generation).
    pub async fn spawn_with_state_on_port(
        state_path: impl AsRef<std::path::Path>,
        port: u16,
    ) -> anyhow::Result<Self> {
        let state_path = state_path.as_ref();
        anyhow::ensure!(
            state_path.exists(),
            "L1 state file not found: {}",
            state_path.display()
        );
        let state_str = state_path.to_string_lossy().to_string();
        let rpc_url = format!("http://localhost:{}", port);

        let provider = ProviderBuilder::new().on_anvil_with_wallet_and_config(|anvil| {
            anvil
                .port(port)
                .chain_id(L1_CHAIN_ID)
                .arg("--host")
                .arg("0.0.0.0")
                .arg("--load-state")
                .arg(&state_str)
        });

        let provider: Box<dyn std::any::Any + Send + Sync> = Box::new(provider);
        wait_for_l1_ready(&rpc_url).context("Anvil L1 is not reachable after spawn")?;

        Ok(Self {
            _provider: provider,
            port,
            rpc_url,
        })
    }

    /// Spawn a fresh anvil instance (empty chain, no pre-loaded state)
    pub async fn spawn_fresh() -> anyhow::Result<Self> {
        let locked_port = LockedPort::acquire_unused()
            .await
            .context("Failed to acquire unused port for anvil")?;
        let port = locked_port.port;

        let rpc_url = format!("http://localhost:{}", port);

        let provider = ProviderBuilder::new().on_anvil_with_wallet_and_config(|anvil| {
            anvil
                .port(port)
                .chain_id(L1_CHAIN_ID)
                .arg("--host")
                .arg("0.0.0.0")
        });

        let provider: Box<dyn std::any::Any + Send + Sync> = Box::new(provider);

        wait_for_l1_ready(&rpc_url).context("Fresh Anvil L1 is not reachable after spawn")?;

        Ok(Self {
            _provider: provider,
            port,
            rpc_url,
        })
    }

    /// Get the RPC URL for this anvil instance (localhost)
    pub fn rpc_url(&self) -> &str {
        &self.rpc_url
    }

    /// Get the RPC URL appropriate for the given repo ref.
    /// Path = localhost; DockerTag = host.docker.internal (for containers reaching host anvil)
    pub fn rpc_url_for(&self, repo_ref: &RepoRef) -> String {
        match repo_ref {
            RepoRef::Path(_) => self.rpc_url().to_string(),
            RepoRef::DockerTag { .. } => format!("http://host.docker.internal:{}", self.port()),
        }
    }

    /// Get the port this anvil instance is running on
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Kill the anvil process
    /// Note: With alloy providers, the anvil process is managed by the provider
    /// and will be cleaned up when the provider is dropped
    pub fn kill(mut self) -> anyhow::Result<()> {
        // Dropping the provider will clean up the anvil process
        self._provider = Box::new(());
        Ok(())
    }
}

fn wait_for_l1_ready(rpc_url: &str) -> anyhow::Result<()> {
    wait_for_chain_to_be_ready(
        rpc_url,
        "Anvil L1",
        L1_READY_MAX_ATTEMPTS,
        L1_READY_RETRY_DELAY,
        None,
    )
}

impl Drop for Anvil {
    fn drop(&mut self) {
        // Dropping the provider will clean up the anvil process
        // Replace with empty box to drop the provider
        self._provider = Box::new(());
    }
}
