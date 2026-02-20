use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;

use uuid::Uuid;

use crate::anvil::Anvil;
use crate::docker_utils::{docker_available, DockerContainer, DockerError};
use crate::presets::{load_default_presets, RepoRef};
use crate::server_utils::wait_for_chain_to_be_ready;
use crate::utils::find_project_root;

static TEST_RUN_ID: OnceLock<String> = OnceLock::new();
const SERVER_READY_MAX_ATTEMPTS: usize = 30;
const SERVER_READY_RETRY_DELAY: Duration = Duration::from_millis(500);
const ZKSYNC_OS_SERVER_IMAGE_REPO: &str =
    "ghcr.io/matter-labs/zksync-os-server";

fn get_or_create_run_id() -> &'static str {
    TEST_RUN_ID
        .get_or_init(|| Uuid::new_v4().to_string())
        .as_str()
}

fn resolve_local_server_binary(server_root: &Path) -> Result<PathBuf, DockerError> {
    let release_bin = server_root.join("target/release/zksync-os-server");
    // Always rebuild to ensure the latest local code is used.
    println!(
        "Building zksync-os-server (release) from {} ...",
        server_root.display()
    );
    let status = Command::new("cargo")
        .arg("build")
        .arg("--release")
        .current_dir(server_root)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|e| DockerError::CommandFailed(format!("Failed to run cargo build --release: {}", e)))?;
    if !status.success() {
        return Err(DockerError::CommandFailed(format!(
            "cargo build --release failed in '{}' with status {}",
            server_root.display(), status
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

/// Builder for configuring a zksync-os-server instance
#[derive(Debug, Clone)]
pub struct ServerBuilder {
    /// Host port where server JSON-RPC should listen
    host_port: u16,
    /// L1 RPC URL
    l1_rpc_url: String,
    /// Path to local-chains directory
    local_chains_path: PathBuf,
    /// Config file path relative to server repo root
    config_path: String,
    /// Docker image tag to use for docker mode
    image: String,
    /// Backend selection strategy
    backend_mode: ServerBackendMode,
}

#[derive(Debug, Clone, Copy)]
enum ServerBackendMode {
    Auto,
    Local,
    Docker,
}

impl Default for ServerBuilder {
    fn default() -> Self {
        Self {
            host_port: 5050,
            l1_rpc_url: "http://host.docker.internal:8545".to_string(),
            // Use relative path; will be resolved relative to project root in spawn()
            local_chains_path: PathBuf::from("zksync-os-server/local-chains"),
            config_path: "./local-chains/v30.2/default/config.yaml".to_string(),
            image: "ghcr.io/matter-labs/zksync-os-server:latest".to_string(),
            backend_mode: ServerBackendMode::Auto,
        }
    }
}

impl ServerBuilder {
    /// Create a new ServerBuilder with default settings
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the host port
    pub fn host_port(mut self, port: u16) -> Self {
        self.host_port = port;
        self
    }

    /// Set the L1 RPC URL
    pub fn l1_rpc_url(mut self, url: String) -> Self {
        self.l1_rpc_url = url;
        self
    }

    /// Set the local-chains path
    pub fn local_chains_path(mut self, path: PathBuf) -> Self {
        self.local_chains_path = path;
        self
    }

    /// Set the config path
    pub fn config_path(mut self, path: String) -> Self {
        self.config_path = path;
        self
    }

    /// Set the Docker image used in docker backend mode.
    pub fn image(mut self, image: String) -> Self {
        self.image = image;
        self
    }

    /// Force local-binary backend.
    pub fn use_local_backend(mut self) -> Self {
        self.backend_mode = ServerBackendMode::Local;
        self
    }

    /// Force docker backend.
    pub fn use_docker_backend(mut self) -> Self {
        self.backend_mode = ServerBackendMode::Docker;
        self
    }

    /// Spawn the server with Anvil L1 (default behavior)
    pub async fn spawn(self) -> anyhow::Result<ServerWithAnvil> {
        let presets = load_default_presets()
            .map_err(|e| DockerError::CommandFailed(format!("Failed to load presets.yaml: {}", e)))?;
        let mut names: Vec<String> = presets.keys().cloned().collect();
        names.sort();
        let preset_name = names
            .first()
            .ok_or_else(|| DockerError::CommandFailed("No presets found in presets.yaml".to_string()))?
            .clone();
        let preset = presets
            .get(&preset_name)
            .ok_or_else(|| DockerError::CommandFailed(format!("Preset '{}' disappeared", preset_name)))?
            .clone();

        let anvil = Anvil::spawn(&preset).await
            .map_err(|e| DockerError::CommandFailed(format!("Failed to spawn anvil: {}", e)))?;
        
        let mut builder = self;
        let is_local_preset = matches!(preset.zksync_os_server, RepoRef::Path(_));
        let use_local = match builder.backend_mode {
            ServerBackendMode::Local => true,
            ServerBackendMode::Docker => false,
            ServerBackendMode::Auto => is_local_preset,
        };

        if let RepoRef::DockerTag(tag) = &preset.zksync_os_server {
            builder.image = format!("{}:{}", ZKSYNC_OS_SERVER_IMAGE_REPO, tag);
        }

        // For local binary run, point directly to local anvil endpoint.
        // Docker cannot use localhost of host and needs host.docker.internal.
        if use_local {
            builder.l1_rpc_url = anvil.rpc_url().to_string();
        } else {
            builder.l1_rpc_url = format!("http://host.docker.internal:{}", anvil.port());
        }
        
        let server_root = match &preset.zksync_os_server {
            RepoRef::Path(path) => Some(path.clone()),
            RepoRef::DockerTag(_) => None,
        };

        let server = Server::spawn(builder, server_root)
            .map_err(|e| anyhow::anyhow!("Failed to spawn server: {:?}", e))?;
        
        Ok(ServerWithAnvil {
            server,
            anvil,
        })
    }

    /// Spawn the server without Anvil (using the configured L1 RPC URL)
    pub fn spawn_without_anvil(self) -> Result<Server, DockerError> {
        Server::spawn(self, None)
    }

    /// Spawn anvil L1 and then spawn the server with anvil's RPC URL
    /// This is an alias for `spawn()` for backwards compatibility
    pub async fn spawn_with_anvil(self) -> anyhow::Result<ServerWithAnvil> {
        self.spawn().await
    }
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
    fn spawn(builder: ServerBuilder, server_root: Option<PathBuf>) -> Result<Self, DockerError> {
        let server_name = format!("integration-tests-zksync-os-server-{}", Uuid::new_v4());

        // Find project root and resolve paths relative to it
        let project_root = find_project_root()?;

        // Group all server logs in this test run under logs/run_{run_id}.
        let run_id = get_or_create_run_id();
        let logs_root = project_root.join("integration-tests/logs");
        let logs_dir = logs_root.join(format!("run_{}", run_id));
        fs::create_dir_all(&logs_dir).map_err(|e| DockerError::CommandFailed(format!(
            "Failed to create logs directory at '{}': {}",
            logs_dir.display(),
            e
        )))?;

        let is_local_available = server_root.is_some();
        let use_local = match builder.backend_mode {
            ServerBackendMode::Local => true,
            ServerBackendMode::Docker => false,
            ServerBackendMode::Auto => is_local_available,
        };

        let local_chains_path = if builder.local_chains_path.is_absolute() {
            builder.local_chains_path.clone()
        } else {
            project_root.join(&builder.local_chains_path)
        };
        
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
            let server_root = server_root.unwrap_or_else(|| project_root.join("../zksync-os-server"));
            let binary_path = resolve_local_server_binary(&server_root)?;
            let runtime = LocalServerRuntime::new(
                server_name.clone(),
                binary_path,
                server_root,
                builder.config_path.clone(),
                builder.l1_rpc_url.clone(),
                builder.host_port,
                local_chains_abs,
                logs_dir.join(format!("db_{}", server_name)),
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
                .arg(format!("{}:3050", builder.host_port))
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
                    server_name,
                    e
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

        // Record latest server container metadata for external tooling.
        let run_latest_path = logs_root.join("run_latest.json");
        let run_latest = serde_json::json!({
            "run_id": run_id,
            "container_name": server_name,
        });
        let run_latest_content = serde_json::to_string_pretty(&run_latest).map_err(|e| {
            DockerError::CommandFailed(format!(
                "Failed to serialize latest server metadata '{}': {}",
                run_latest_path.display(),
                e
            ))
        })?;
        fs::write(&run_latest_path, run_latest_content).map_err(|e| {
            DockerError::CommandFailed(format!(
                "Failed to write latest server metadata '{}': {}",
                run_latest_path.display(),
                e
            ))
        })?;

        Ok(Self {
            runtime,
            server_name,
            host_port: builder.host_port,
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
        self.logs_dir.join(format!(
            "server_run{}_{}.json",
            run_index, self.server_name
        ))
    }
}

/// A server instance with its own anvil L1
pub struct ServerWithAnvil {
    server: Server,
    anvil: Anvil,
}

impl ServerWithAnvil {
    /// Get a reference to the server
    pub fn server(&self) -> &Server {
        &self.server
    }

    /// Get a reference to the anvil instance
    pub fn anvil(&self) -> &Anvil {
        &self.anvil
    }

    /// Get the L1 RPC URL
    pub fn l1_rpc_url(&self) -> &str {
        self.anvil.rpc_url()
    }

    /// Stop only the server container, preserving its data for restart.
    pub fn stop_server(&self) -> Result<(), DockerError> {
        self.server.stop()
    }

    /// Start the same server container again after `stop_server`.
    pub fn start_server(&self) -> Result<(), DockerError> {
        self.server.start()
    }

    /// Kill both the server and anvil
    pub fn kill(self) -> anyhow::Result<()> {
        self.server.kill()
            .map_err(|e| anyhow::anyhow!("Failed to kill server: {:?}", e))?;
        self.anvil.kill()?;
        Ok(())
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
            .map_err(|e| DockerError::CommandFailed(format!(
                "Failed to open log file '{}': {}",
                log_path.display(),
                e
            )))?;
        let log_file_err = log_file
            .try_clone()
            .map_err(|e| DockerError::CommandFailed(format!("Failed to clone log file handle: {}", e)))?;

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
            let mut guard = self
                .child
                .lock()
                .map_err(|_| DockerError::CommandFailed("Failed to lock server process".to_string()))?;
            *guard = Some(child);
        }
        {
            let mut path_guard = self
                .current_log_path
                .lock()
                .map_err(|_| DockerError::CommandFailed("Failed to lock log path".to_string()))?;
            *path_guard = Some(log_path.to_path_buf());
        }

        if let Err(err) = self.wait_until_rpc_ready() {
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
                DockerError::CommandFailed(format!("Failed to stop local server '{}': {}", self.name, e))
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
            strip_ansi_escape_codes(path)?;
        }
        Ok(())
    }

    fn kill(&self) -> Result<(), DockerError> {
        self.stop()
    }

    fn wait_until_rpc_ready(&self) -> Result<(), String> {
        let url = format!("http://127.0.0.1:{}/", self.host_port);
        wait_for_chain_to_be_ready(
            &url,
            "Server RPC",
            SERVER_READY_MAX_ATTEMPTS,
            SERVER_READY_RETRY_DELAY,
        )
        .map_err(|e| e.to_string())
    }
}

fn strip_ansi_escape_codes(log_path: &Path) -> Result<(), DockerError> {
    let path_str = log_path.to_string_lossy();
    let escaped_path = path_str.replace('\'', "'\"'\"'");
    let status = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "perl -i -pe 's/\\e\\[[0-9;]*[a-zA-Z]//g' '{}'",
            escaped_path
        ))
        .status()
        .map_err(|e| DockerError::CommandFailed(format!(
            "Failed to strip ANSI escapes from '{}': {}",
            log_path.display(),
            e
        )))?;
    if !status.success() {
        return Err(DockerError::CommandFailed(format!(
            "Failed to strip ANSI escapes from '{}' (exit status: {})",
            log_path.display(),
            status
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presets::RepoRef;
    use crate::server_utils::{wait_for_executed_batches_with_traffic, DEFAULT_TEST_PRIVATE_KEY};
    use crate::upgrade_config::Contracts;
    use std::time::Duration;

    #[tokio::test]
    async fn test_server_start_and_kill() {
        let thread = std::thread::current();
        let test_name = thread.name().unwrap_or("unknown_test").to_string();
        println!("Starting server test...");

        // Configure server paths from the default preset instead of hardcoded defaults.
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

        let project_root = crate::utils::find_project_root()
            .expect("Failed to resolve project root for server test");
        let server_root = match &preset.zksync_os_server {
            RepoRef::Path(path) => path.clone(),
            // For docker-tag presets we still use the local workspace checkout for test artifacts,
            // while ServerBuilder auto-selects docker backend.
            RepoRef::DockerTag(_) => project_root.join("../zksync-os-server"),
        };
        assert!(
            server_root.exists(),
            "Server root does not exist for test: {}",
            server_root.display()
        );

        let local_chains_path = server_root.join("local-chains");
        let config_path = format!(
            "./local-chains/{}/default/config.yaml",
            preset.protocol_versions.previous
        );
        let contracts_path = local_chains_path
            .join(&preset.protocol_versions.previous)
            .join("default")
            .join("contracts.yaml");

        let server_with_anvil = ServerBuilder::new()
            .local_chains_path(local_chains_path)
            .config_path(config_path)
            .spawn()
            .await
            .expect("Failed to spawn server with anvil");

        let (container_name, l2_rpc_url) = {
            let server = server_with_anvil.server();
            (
                server.container_name().to_string(),
                format!("http://127.0.0.1:{}", server.host_port()),
            )
        };
        let contracts = Contracts::load_from_path(&contracts_path)
            .expect("Failed to load contracts.yaml for batch tracking");

        std::thread::sleep(Duration::from_secs(1));
        // Verify server is running
        println!("Checking if server is running...");
        let is_running = server_with_anvil
            .server()
            .is_running()
            .map_err(|e| format!("Failed to check server status: {:?}", e))
            .unwrap();
        assert!(is_running, "Server {} is not running", container_name);

        // Drive server activity until at least 3 batches are executed on L1.
        println!("Sending txs every 3s until >=3 executed L1 batches...");
        wait_for_executed_batches_with_traffic(
            &l2_rpc_url,
            server_with_anvil.l1_rpc_url(),
            &contracts.l1.diamond_proxy_addr,
            DEFAULT_TEST_PRIVATE_KEY,
            3,
            Duration::from_secs(120),
        )
        .expect("Failed while waiting for executed L1 batches with traffic");

        // Restart cycle #1 (stop + start same container)
        println!("Restart cycle #1: stopping server...");
        server_with_anvil
            .stop_server()
            .expect("Failed to stop server in restart cycle #1");
        assert!(
            !server_with_anvil
                .server()
                .is_running()
                .map_err(|e| format!("Failed to check status after stop #1: {:?}", e))
                .unwrap(),
            "Server {} is still running after stop #1",
            container_name
        );

        println!("Restart cycle #1: starting server...");
        server_with_anvil
            .start_server()
            .expect("Failed to start server in restart cycle #1");
        assert!(
            server_with_anvil
                .server()
                .is_running()
                .map_err(|e| format!("Failed to check status after start #1: {:?}", e))
                .unwrap(),
            "Server {} is not running after start #1",
            container_name
        );

        // Restart cycle #2 (stop + start same container)
        println!("Restart cycle #2: stopping server...");
        server_with_anvil
            .stop_server()
            .expect("Failed to stop server in restart cycle #2");
        assert!(
            !server_with_anvil
                .server()
                .is_running()
                .map_err(|e| format!("Failed to check status after stop #2: {:?}", e))
                .unwrap(),
            "Server {} is still running after stop #2",
            container_name
        );

        println!("Restart cycle #2: starting server...");
        server_with_anvil
            .start_server()
            .expect("Failed to start server in restart cycle #2");
        assert!(
            server_with_anvil
                .server()
                .is_running()
                .map_err(|e| format!("Failed to check status after start #2: {:?}", e))
                .unwrap(),
            "Server {} is not running after start #2",
            container_name
        );

        // Kill the server and anvil
        println!("Killing server and anvil...");
        match server_with_anvil.kill() {
            Ok(()) => println!("Server and anvil killed successfully"),
            Err(e) => panic!("Failed to kill server and anvil: {:?}", e),
        }

        println!(
            "{} completed successfully! (container: {})",
            test_name, container_name
        );
    }
}

