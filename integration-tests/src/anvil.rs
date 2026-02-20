use anyhow::Context;
use std::process::Command;
use std::time::Duration;

use alloy::providers::ProviderBuilder;
use crate::find_ports::LockedPort;
use crate::preset_paths::server_paths_for_preset;
use crate::presets::Preset;
use crate::server_utils::wait_for_chain_to_be_ready;

/// Get L1 state path for a given preset.
fn get_l1_state_path(preset: &Preset) -> anyhow::Result<String> {
    let paths = server_paths_for_preset(preset)?;
    let legacy_state = paths.chain_dir.join("zkos-l1-state.json");
    if legacy_state.exists() {
        return Ok(legacy_state.to_string_lossy().to_string());
    }

    let version_dir = paths
        .chain_dir
        .parent()
        .context("Failed to resolve protocol version directory from chain_dir")?;

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

/// L1 chain id as expected by contracts deployed in `zkos-l1-state.json`
const L1_CHAIN_ID: u64 = 31337;
const L1_READY_MAX_ATTEMPTS: usize = 10;
const L1_READY_RETRY_DELAY: Duration = Duration::from_secs(1);

fn decompress_l1_state_gz(state_gz: &std::path::Path, state_json: &std::path::Path) -> anyhow::Result<()> {
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

/// A running anvil L1 instance
/// The provider is stored to keep the anvil process alive
pub struct Anvil {
    _provider: Box<dyn std::any::Any + Send + Sync>,
    port: u16,
    rpc_url: String,
}

impl Anvil {
    /// Spawn a new anvil instance on an unused port (loads v30.2 state for upgrade tests)
    pub async fn spawn(preset: &Preset) -> anyhow::Result<Self> {
        let locked_port = LockedPort::acquire_unused().await
            .context("Failed to acquire unused port for anvil")?;
        let port = locked_port.port;

        let rpc_url = format!("http://localhost:{}", port);

        // Get L1 state path from the requested preset.
        let l1_state_path = get_l1_state_path(preset)?;

        // Spawn anvil using alloy ProviderBuilder
        // The provider manages the anvil process internally
        let provider = ProviderBuilder::new()
            .on_anvil_with_wallet_and_config(|anvil| {
                anvil
                    .port(port)
                    .chain_id(L1_CHAIN_ID)
                    .arg("--load-state")
                    .arg(l1_state_path)
            })
            ;

        let provider: Box<dyn std::any::Any + Send + Sync> = Box::new(provider);

        wait_for_l1_ready(&rpc_url)
            .context("Anvil L1 is not reachable after spawn")?;

        Ok(Self {
            _provider: provider,
            port,
            rpc_url,
        })
    }

    /// Spawn a fresh anvil instance (empty chain, no pre-loaded state)
    pub async fn spawn_fresh() -> anyhow::Result<Self> {
        let locked_port = LockedPort::acquire_unused().await
            .context("Failed to acquire unused port for anvil")?;
        let port = locked_port.port;

        let rpc_url = format!("http://localhost:{}", port);

        let provider = ProviderBuilder::new()
            .on_anvil_with_wallet_and_config(|anvil| {
                anvil
                    .port(port)
                    .chain_id(L1_CHAIN_ID)
            })
            ;

        let provider: Box<dyn std::any::Any + Send + Sync> = Box::new(provider);

        wait_for_l1_ready(&rpc_url)
            .context("Fresh Anvil L1 is not reachable after spawn")?;

        Ok(Self {
            _provider: provider,
            port,
            rpc_url,
        })
    }

    /// Get the RPC URL for this anvil instance
    pub fn rpc_url(&self) -> &str {
        &self.rpc_url
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
    )
}

impl Drop for Anvil {
    fn drop(&mut self) {
        // Dropping the provider will clean up the anvil process
        // Replace with empty box to drop the provider
        self._provider = Box::new(());
    }
}

