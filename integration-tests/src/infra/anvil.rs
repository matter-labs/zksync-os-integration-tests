use anyhow::Context;
use flate2::read::GzDecoder;
use std::fs::File;
use std::io::{self, BufReader};
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
    let gz_file =
        File::open(state_gz).with_context(|| format!("Failed to open {}", state_gz.display()))?;
    let decoder = GzDecoder::new(BufReader::new(gz_file));
    let mut out_file = File::create(state_json)
        .with_context(|| format!("Failed to create {}", state_json.display()))?;
    io::copy(&mut { decoder }, &mut out_file)
        .with_context(|| format!("Failed to decompress {}", state_gz.display()))?;
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
    /// Held to prevent other tests from acquiring the same port while anvil is running.
    _locked_port: Option<LockedPort>,
    port: u16,
    rpc_url: String,
}

impl Anvil {
    /// Wrap an externally-managed Anvil process. The caller is responsible for
    /// the process lifetime — this struct only provides the RPC URL and port.
    pub fn wrap_external(port: u16) -> Self {
        Self {
            _provider: Box::new(()),
            _locked_port: None,
            port,
            rpc_url: format!("http://127.0.0.1:{}", port),
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
        let rpc_url = format!("http://127.0.0.1:{}", port);

        let provider = ProviderBuilder::new().on_anvil_with_wallet_and_config(|anvil| {
            anvil
                .port(port)
                .chain_id(L1_CHAIN_ID)
                .arg("--host")
                .arg("0.0.0.0")
                .arg("--load-state")
                .arg(&state_str)
                .arg("--disable-block-gas-limit")
                .arg("--block-time")
                .arg("0.25")
                .arg("--mixed-mining")
                .arg("--slots-in-an-epoch")
                .arg("10")
            // Instamine — anvil's default. Every tx mines its own block,
            // so commit/prove/execute bundles from the server's L1 sender
            // land instantly and the L1 watcher sees them on its next 100ms
            // poll. Safe because our server config pins
            // `l1_watcher.confirmations: 0` — the watcher scans up to tip
            // and doesn't need idle blocks to expose the `tip - N` range.
        });

        let provider: Box<dyn std::any::Any + Send + Sync> = Box::new(provider);
        wait_for_l1_ready(&rpc_url).context("Anvil L1 is not reachable after spawn")?;

        Ok(Self {
            _provider: provider,
            _locked_port: Some(locked_port),
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
