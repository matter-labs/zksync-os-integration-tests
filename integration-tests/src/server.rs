use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;

use chrono::Local;
use uuid::Uuid;

use crate::anvil::Anvil;
use crate::docker::{docker_available, DockerContainer, DockerError};
use crate::find_ports::pick_unused_port_sync;
use crate::preset_paths::server_paths_for_preset;
use crate::presets::{Preset, RepoRef};
use crate::server_utils::{strip_ansi_escape_codes_in_file, wait_for_chain_to_be_ready};
use crate::utils::find_project_root;

static TEST_RUN_ID: OnceLock<String> = OnceLock::new();
const SERVER_READY_MAX_ATTEMPTS: usize = 30;
const SERVER_READY_RETRY_DELAY: Duration = Duration::from_millis(500);
const ZKSYNC_OS_SERVER_IMAGE_REPO: &str = "ghcr.io/matter-labs/zksync-os-server";

pub(crate) fn get_or_create_run_id() -> &'static str {
    TEST_RUN_ID
        .get_or_init(|| {
            let timestamp = Local::now().format("%Y-%m-%d_%H-%M-%S");
            let current = std::thread::current();
            let name = current.name().unwrap_or("unknown");
            let fn_part = name.rsplit("::").next().unwrap_or(name);
            let fn_part = fn_part.strip_prefix("test_").unwrap_or(fn_part);
            let test_name = fn_part.replace([' ', ':'], "_");
            let uuid = Uuid::new_v4().to_string();
            format!("{}_{}_{}", test_name, timestamp, &uuid[..8])
        })
        .as_str()
}

/// Read the Rust toolchain channel from a repo's rust-toolchain.toml or rust-toolchain file,
/// so we can set RUSTUP_TOOLCHAIN when building that repo (and avoid using the integration-tests toolchain).
pub fn read_toolchain_from_dir(dir: &Path) -> Option<String> {
    let toml_path = dir.join("rust-toolchain.toml");
    if toml_path.exists() {
        let content = fs::read_to_string(&toml_path).ok()?;
        for line in content.lines() {
            let line = line.trim();
            if line.starts_with("channel") {
                let rest = line
                    .strip_prefix("channel")?
                    .trim()
                    .trim_start_matches('=')
                    .trim();
                let channel = rest.trim_matches('"').trim_matches('\'').trim();
                if !channel.is_empty() {
                    return Some(channel.to_string());
                }
            }
        }
    }
    let legacy_path = dir.join("rust-toolchain");
    if legacy_path.exists() {
        let content = fs::read_to_string(&legacy_path).ok()?;
        return content.trim().to_string().into();
    }
    None
}

fn resolve_local_server_binary(server_root: &Path) -> Result<PathBuf, DockerError> {
    let release_bin = server_root.join("target/release/zksync-os-server");
    // Always rebuild to ensure the latest local code is used.
    // Use the server repo's toolchain (RUSTUP_TOOLCHAIN) so we don't inherit the integration-tests toolchain.
    let mut cmd = Command::new("cargo");
    cmd.arg("build")
        .arg("--release")
        .current_dir(server_root)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    if let Some(toolchain) = read_toolchain_from_dir(server_root) {
        cmd.env("RUSTUP_TOOLCHAIN", &toolchain);
    }
    let status = cmd.status().map_err(|e| {
        DockerError::CommandFailed(format!("Failed to run cargo build --release: {}", e))
    })?;
    if !status.success() {
        return Err(DockerError::CommandFailed(format!(
            "cargo build --release failed in '{}' with status {}",
            server_root.display(),
            status
        )));
    }

    if release_bin.exists() {
        Ok(release_bin)
    } else {
        Err(DockerError::CommandFailed(format!(
            "Expected binary not found after build: {}",
            release_bin.display()
        )))
    }
}

/// Builder for configuring a zksync-os-server instance. Requires a preset and always uses Anvil for L1.
#[derive(Debug, Clone)]
pub struct ServerBuilder {
    preset: Preset,
    /// Host port where server JSON-RPC should listen (None = random)
    host_port: Option<u16>,
    /// Override config path (used instead of preset-derived path when set)
    config_path_override: Option<PathBuf>,
    /// Override RocksDB directory for local server (default: under integration-tests/logs/{run_id}/)
    rocks_db_path_override: Option<PathBuf>,
}

impl ServerBuilder {
    /// Create a new ServerBuilder from a preset. Server always uses Anvil for L1.
    /// Backend (local vs docker) is determined by preset.zksync_os_server.
    pub fn new(preset: Preset) -> Self {
        Self {
            preset,
            host_port: None,
            config_path_override: None,
            rocks_db_path_override: None,
        }
    }

    /// Override the config path (used instead of preset-derived path when set).
    pub fn config_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.config_path_override = Some(path.into());
        self
    }

    /// Set the host port (omit to use a random port)
    pub fn host_port(mut self, port: u16) -> Self {
        self.host_port = Some(port);
        self
    }

    /// Use a fixed RocksDB path (local server only). Lets a later process reuse replay / tree state.
    pub fn rocks_db_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.rocks_db_path_override = Some(path.into());
        self
    }

    /// Spawn the server with the given Anvil L1.
    pub fn spawn(self, anvil: &Anvil) -> Result<Server, DockerError> {
        let paths = server_paths_for_preset(&self.preset).map_err(|e| {
            DockerError::CommandFailed(format!("Failed to resolve preset paths: {}", e))
        })?;
        let local_chains_path = paths.server_root.join("local-chains");
        let config_path = self
            .config_path_override
            .as_ref()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| {
                format!(
                    "./local-chains/{}/default/config.yaml",
                    self.preset.protocol_versions.previous
                )
            });
        let l1_rpc_url = anvil.rpc_url_for(&self.preset.zksync_os_server);
        let (server_root, use_local, image) = match &self.preset.zksync_os_server {
            RepoRef::Path(_) => (
                Some(paths.server_root.clone()),
                true,
                format!("{}:latest", ZKSYNC_OS_SERVER_IMAGE_REPO),
            ),
            RepoRef::DockerTag(tag) => (
                None,
                false,
                format!("{}:{}", ZKSYNC_OS_SERVER_IMAGE_REPO, tag),
            ),
        };
        let builder = InnerServerBuilder {
            host_port: self.host_port,
            l1_rpc_url,
            local_chains_path,
            config_path,
            image,
            use_local,
            rocks_db_path: self.rocks_db_path_override,
        };
        Server::spawn_inner(builder, server_root)
    }

    /// Spawn Anvil from preset, then spawn the server. Returns (server, anvil) separately.
    pub async fn spawn_with_anvil(self) -> anyhow::Result<(Server, Anvil)> {
        let anvil = Anvil::spawn(&self.preset)
            .await
            .map_err(|e| DockerError::CommandFailed(format!("Failed to spawn anvil: {}", e)))?;
        let server = self
            .spawn(&anvil)
            .map_err(|e| anyhow::anyhow!("Failed to spawn server: {:?}", e))?;
        Ok((server, anvil))
    }
}

#[derive(Debug, Clone)]
struct InnerServerBuilder {
    host_port: Option<u16>,
    l1_rpc_url: String,
    local_chains_path: PathBuf,
    config_path: String,
    image: String,
    use_local: bool,
    rocks_db_path: Option<PathBuf>,
}

/// A running zksync-os-server instance
#[derive(Debug)]
pub struct Server {
    runtime: ServerRuntime,
    server_name: String,
    host_port: u16,
    logs_dir: PathBuf,
    run_index: std::cell::Cell<u32>,
}

#[derive(Debug)]
enum ServerRuntime {
    Local(LocalServerRuntime),
    Docker(DockerContainer),
}

impl Server {
    fn spawn_inner(
        builder: InnerServerBuilder,
        server_root: Option<PathBuf>,
    ) -> Result<Self, DockerError> {
        let host_port = builder
            .host_port
            .unwrap_or_else(|| pick_unused_port_sync().expect("failed to pick random port"));

        let server_name = format!("integration-tests-zksync-os-server-{}", Uuid::new_v4());

        // Find project root and resolve paths relative to it
        let project_root = find_project_root()?;

        // Group all server logs in this test run under logs/{run_id}.
        // Move any existing run directories to previous_runs/ before creating the new one.
        let run_id = get_or_create_run_id();
        let logs_root = project_root.join("integration-tests/logs");
        let previous_runs = logs_root.join("previous_runs");
        if logs_root.exists() {
            if let Ok(entries) = fs::read_dir(&logs_root) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                            if name != "previous_runs" && name != run_id {
                                let dest = previous_runs.join(name);
                                fs::create_dir_all(&previous_runs).ok();
                                let _ = fs::rename(&path, &dest);
                            }
                        }
                    }
                }
            }
        }
        let logs_dir = logs_root.join(run_id);
        fs::create_dir_all(&logs_dir).map_err(|e| {
            DockerError::CommandFailed(format!(
                "Failed to create logs directory at '{}': {}",
                logs_dir.display(),
                e
            ))
        })?;

        let use_local = builder.use_local;

        let local_chains_path = builder.local_chains_path;

        let local_chains_abs = std::fs::canonicalize(&local_chains_path)
            .map_err(|e| DockerError::CommandFailed(format!(
                "Failed to canonicalize local-chains path '{}' (project root: '{}', current directory: '{}'): {}",
                local_chains_path.display(),
                project_root.display(),
                std::env::current_dir()
                    .unwrap_or_else(|_| PathBuf::from("unknown"))
                    .display(),
                e
            )))?;

        let first_run_log = logs_dir.join(format!("server_run1_{}.json", server_name));
        let runtime = if use_local {
            let server_root =
                server_root.unwrap_or_else(|| project_root.join("../zksync-os-server"));
            let binary_path = resolve_local_server_binary(&server_root)?;
            let rocks_path = builder
                .rocks_db_path
                .clone()
                .unwrap_or_else(|| logs_dir.join(format!("db_{}", server_name)));
            let runtime = LocalServerRuntime::new(
                server_name.clone(),
                binary_path,
                server_root,
                builder.config_path.clone(),
                builder.l1_rpc_url.clone(),
                host_port,
                local_chains_abs,
                rocks_path,
            );
            runtime.start_with_log_path(&first_run_log)?;
            ServerRuntime::Local(runtime)
        } else {
            if !docker_available() {
                return Err(DockerError::DockerNotAvailable(
                    "Docker is not installed or not in PATH".to_string(),
                ));
            }
            let mut cmd = Command::new("docker");
            cmd.arg("run")
                .arg("-d")
                .arg("--platform")
                .arg("linux/amd64")
                .arg("--name")
                .arg(&server_name)
                .arg("-p")
                .arg(format!("{}:3050", host_port))
                .arg("-e")
                .arg(format!("GENERAL_L1_RPC_URL={}", builder.l1_rpc_url))
                .arg("--add-host")
                .arg("host.docker.internal:host-gateway")
                .arg("-v")
                .arg(format!("{}:/app/local-chains", local_chains_abs.display()))
                .arg("--workdir")
                .arg("/app")
                .arg("--user")
                .arg("root")
                .arg(&builder.image)
                .arg("--config")
                .arg(&builder.config_path);

            let output = cmd.output().map_err(|e| {
                DockerError::CommandFailed(format!(
                    "Failed to execute docker run command for container '{}': {}",
                    server_name, e
                ))
            })?;
            if !output.status.success() {
                return Err(DockerError::CommandFailed(format!(
                    "docker run failed for container '{}' (image: '{}'):\nSTDERR: {}\nSTDOUT: {}",
                    server_name,
                    builder.image,
                    String::from_utf8_lossy(&output.stderr),
                    String::from_utf8_lossy(&output.stdout)
                )));
            }
            ServerRuntime::Docker(DockerContainer::new(server_name.clone()))
        };

        Ok(Self {
            runtime,
            server_name,
            host_port,
            logs_dir,
            run_index: std::cell::Cell::new(1),
        })
    }

    /// Get the container name
    pub fn container_name(&self) -> &str {
        &self.server_name
    }

    /// Get the host port
    pub fn host_port(&self) -> u16 {
        self.host_port
    }

    /// Get the L2 RPC URL
    pub fn rpc_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.host_port)
    }

    /// Check if the server is running
    pub fn is_running(&self) -> Result<bool, DockerError> {
        match &self.runtime {
            ServerRuntime::Local(r) => r.is_running(),
            ServerRuntime::Docker(c) => c.is_running(),
        }
    }

    /// Stop the server container without removing it.
    /// Use `start` to bring up the same container again.
    pub fn stop(&self) -> Result<(), DockerError> {
        if !self.is_running()? {
            return Ok(());
        }

        match &self.runtime {
            ServerRuntime::Local(r) => r.stop()?,
            ServerRuntime::Docker(c) => {
                c.stop()?;
                let current_run_logs = self.run_logs_path(self.run_index.get());
                c.save_logs(&current_run_logs)?;
            }
        }
        self.run_index.set(self.run_index.get() + 1);
        Ok(())
    }

    /// Start the existing server container again (same container, same data).
    pub fn start(&self) -> Result<(), DockerError> {
        let current_run_logs = self.run_logs_path(self.run_index.get());
        match &self.runtime {
            ServerRuntime::Local(r) => r.start_with_log_path(&current_run_logs)?,
            ServerRuntime::Docker(c) => c.start(Duration::from_secs(30))?,
        }
        Ok(())
    }

    /// Get the logs path
    pub fn logs_path(&self) -> PathBuf {
        self.run_logs_path(self.run_index.get())
    }

    /// Kill the server container (force stop) and remove it
    pub fn kill(self) -> Result<(), DockerError> {
        let current_run_logs = self.run_logs_path(self.run_index.get());
        match &self.runtime {
            ServerRuntime::Local(r) => r.kill(),
            ServerRuntime::Docker(c) => c.kill(&current_run_logs),
        }
    }

    fn run_logs_path(&self, run_index: u32) -> PathBuf {
        self.logs_dir
            .join(format!("server_run{}_{}.json", run_index, self.server_name))
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        match &self.runtime {
            ServerRuntime::Local(r) => {
                if r.is_running().unwrap_or(false) {
                    let _ = r.kill();
                }
            }
            ServerRuntime::Docker(c) => {
                if !c.is_cleaned_up() {
                    let current_run_logs = self.run_logs_path(self.run_index.get());
                    let _ = c.kill(&current_run_logs);
                }
            }
        }
    }
}

#[derive(Debug)]
struct LocalServerRuntime {
    name: String,
    binary_path: PathBuf,
    server_root: PathBuf,
    config_path: String,
    l1_rpc_url: String,
    host_port: u16,
    local_chains_abs: PathBuf,
    rocks_db_path: PathBuf,
    child: Mutex<Option<Child>>,
    current_log_path: Mutex<Option<PathBuf>>,
}

impl LocalServerRuntime {
    fn new(
        name: String,
        binary_path: PathBuf,
        server_root: PathBuf,
        config_path: String,
        l1_rpc_url: String,
        host_port: u16,
        local_chains_abs: PathBuf,
        rocks_db_path: PathBuf,
    ) -> Self {
        Self {
            name,
            binary_path,
            server_root,
            config_path,
            l1_rpc_url,
            host_port,
            local_chains_abs,
            rocks_db_path,
            child: Mutex::new(None),
            current_log_path: Mutex::new(None),
        }
    }

    fn is_running(&self) -> Result<bool, DockerError> {
        let mut guard = self
            .child
            .lock()
            .map_err(|_| DockerError::CommandFailed("Failed to lock server process".to_string()))?;
        if let Some(child) = guard.as_mut() {
            match child.try_wait() {
                Ok(Some(_)) => {
                    *guard = None;
                    Ok(false)
                }
                Ok(None) => Ok(true),
                Err(e) => Err(DockerError::CommandFailed(format!(
                    "Failed to check process status for '{}': {}",
                    self.name, e
                ))),
            }
        } else {
            Ok(false)
        }
    }

    fn start_with_log_path(&self, log_path: &Path) -> Result<(), DockerError> {
        if self.is_running()? {
            return Ok(());
        }

        let log_file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(log_path)
            .map_err(|e| {
                DockerError::CommandFailed(format!(
                    "Failed to open log file '{}': {}",
                    log_path.display(),
                    e
                ))
            })?;
        let log_file_err = log_file.try_clone().map_err(|e| {
            DockerError::CommandFailed(format!("Failed to clone log file handle: {}", e))
        })?;

        let mut cmd = Command::new(&self.binary_path);
        fs::create_dir_all(&self.rocks_db_path).map_err(|e| {
            DockerError::CommandFailed(format!(
                "Failed to create rocks db directory '{}': {}",
                self.rocks_db_path.display(),
                e
            ))
        })?;
        cmd.arg("--config")
            .arg(&self.config_path)
            .current_dir(&self.server_root)
            .env("GENERAL_L1_RPC_URL", &self.l1_rpc_url)
            .env("rpc_address", format!("0.0.0.0:{}", self.host_port))
            .env(
                "general_rocks_db_path",
                self.rocks_db_path.to_string_lossy().to_string(),
            )
            .env(
                "sequencer_rocks_db_path",
                self.rocks_db_path.to_string_lossy().to_string(),
            )
            .env(
                "LOCAL_CHAINS_PATH",
                self.local_chains_abs.to_string_lossy().to_string(),
            )
            .stdout(Stdio::from(log_file))
            .stderr(Stdio::from(log_file_err));

        let child = cmd.spawn().map_err(|e| {
            DockerError::CommandFailed(format!(
                "Failed to spawn local server binary '{}' in '{}': {}",
                self.binary_path.display(),
                self.server_root.display(),
                e
            ))
        })?;

        {
            let mut guard = self.child.lock().map_err(|_| {
                DockerError::CommandFailed("Failed to lock server process".to_string())
            })?;
            *guard = Some(child);
        }
        {
            let mut path_guard = self
                .current_log_path
                .lock()
                .map_err(|_| DockerError::CommandFailed("Failed to lock log path".to_string()))?;
            *path_guard = Some(log_path.to_path_buf());
        }

        if let Err(err) = self.wait_until_rpc_ready(log_path) {
            let _ = self.stop();
            return Err(DockerError::CommandFailed(format!(
                "Server process started but RPC did not become ready on port {}. {}. Check logs at '{}'",
                self.host_port,
                err,
                log_path.display()
            )));
        }

        Ok(())
    }

    fn stop(&self) -> Result<(), DockerError> {
        let mut guard = self
            .child
            .lock()
            .map_err(|_| DockerError::CommandFailed("Failed to lock server process".to_string()))?;
        if let Some(child) = guard.as_mut() {
            child.kill().map_err(|e| {
                DockerError::CommandFailed(format!(
                    "Failed to stop local server '{}': {}",
                    self.name, e
                ))
            })?;
            let _ = child.wait();
            *guard = None;
        }
        drop(guard);

        let path_guard = self
            .current_log_path
            .lock()
            .map_err(|_| DockerError::CommandFailed("Failed to lock log path".to_string()))?;
        if let Some(path) = path_guard.as_ref() {
            strip_ansi_escape_codes_in_file(path).map_err(|e| {
                DockerError::CommandFailed(format!(
                    "Failed to strip ANSI escapes from '{}': {}",
                    path.display(),
                    e
                ))
            })?;
        }
        Ok(())
    }

    fn kill(&self) -> Result<(), DockerError> {
        self.stop()
    }

    fn wait_until_rpc_ready(&self, log_path: &Path) -> Result<(), String> {
        let url = format!("http://127.0.0.1:{}/", self.host_port);
        wait_for_chain_to_be_ready(
            &url,
            "Server RPC",
            SERVER_READY_MAX_ATTEMPTS,
            SERVER_READY_RETRY_DELAY,
            Some(log_path),
        )
        .map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presets::load_default_presets;
    use crate::server_utils::{wait_for_executed_batches_with_traffic, DEFAULT_TEST_PRIVATE_KEY};
    use crate::upgrade_config::Contracts;
    use std::time::Duration;

    #[tokio::test]
    async fn test_server_start_and_kill() {
        let thread = std::thread::current();
        let test_name = thread.name().unwrap_or("unknown_test").to_string();
        println!("Starting server test...");

        let presets = load_default_presets().expect("Failed to load presets.yaml");
        let mut names: Vec<String> = presets.keys().cloned().collect();
        names.sort();
        let preset_name = names
            .first()
            .expect("No presets found in presets.yaml")
            .clone();
        let preset = presets
            .get(&preset_name)
            .expect("Preset disappeared while reading presets")
            .clone();

        let paths = crate::preset_paths::server_paths_for_preset(&preset)
            .expect("Failed to resolve server paths from preset");
        let contracts_path = paths.contracts_yaml.clone();

        let (server, anvil) = ServerBuilder::new(preset)
            .spawn_with_anvil()
            .await
            .expect("Failed to spawn server with anvil");

        let container_name = server.container_name().to_string();
        let l2_rpc_url = format!("http://127.0.0.1:{}", server.host_port());
        let contracts = Contracts::load_from_path(&contracts_path)
            .expect("Failed to load contracts.yaml for batch tracking");

        std::thread::sleep(Duration::from_secs(1));
        // Verify server is running
        println!("Checking if server is running...");
        let is_running = server
            .is_running()
            .map_err(|e| format!("Failed to check server status: {:?}", e))
            .unwrap();
        assert!(is_running, "Server {} is not running", container_name);

        // Drive server activity until at least 3 batches are executed on L1.
        println!("Sending txs every 3s until >=3 executed L1 batches...");
        wait_for_executed_batches_with_traffic(
            &l2_rpc_url,
            anvil.rpc_url(),
            &contracts.l1.diamond_proxy_addr,
            DEFAULT_TEST_PRIVATE_KEY,
            3,
            Duration::from_secs(120),
        )
        .expect("Failed while waiting for executed L1 batches with traffic");

        // Restart cycle #1 (stop + start same container)
        println!("Restart cycle #1: stopping server...");
        server
            .stop()
            .expect("Failed to stop server in restart cycle #1");
        assert!(
            !server
                .is_running()
                .map_err(|e| format!("Failed to check status after stop #1: {:?}", e))
                .unwrap(),
            "Server {} is still running after stop #1",
            container_name
        );

        println!("Restart cycle #1: starting server...");
        server
            .start()
            .expect("Failed to start server in restart cycle #1");
        assert!(
            server
                .is_running()
                .map_err(|e| format!("Failed to check status after start #1: {:?}", e))
                .unwrap(),
            "Server {} is not running after start #1",
            container_name
        );

        // Restart cycle #2 (stop + start same container)
        println!("Restart cycle #2: stopping server...");
        server
            .stop()
            .expect("Failed to stop server in restart cycle #2");
        assert!(
            !server
                .is_running()
                .map_err(|e| format!("Failed to check status after stop #2: {:?}", e))
                .unwrap(),
            "Server {} is still running after stop #2",
            container_name
        );

        println!("Restart cycle #2: starting server...");
        server
            .start()
            .expect("Failed to start server in restart cycle #2");
        assert!(
            server
                .is_running()
                .map_err(|e| format!("Failed to check status after start #2: {:?}", e))
                .unwrap(),
            "Server {} is not running after start #2",
            container_name
        );

        // Kill the server and anvil
        println!("Killing server and anvil...");
        server.kill().expect("Failed to kill server");
        anvil.kill().expect("Failed to kill anvil");

        println!(
            "{} completed successfully! (container: {})",
            test_name, container_name
        );
    }
}
