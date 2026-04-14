use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::sync::OnceLock;
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::Context;
use uuid::Uuid;

use crate::anvil::Anvil;
use crate::docker::{docker_available, DockerContainer, DockerError};
use crate::find_ports::pick_unused_port_sync;
use crate::preset_paths::server_paths_for_preset;
use crate::presets::{Preset, RepoRef};
use crate::server_utils::{
    fund_l2_via_l1_deposit_ex, get_total_batches_executed, send_traffic_tx,
    strip_ansi_escape_codes_in_file, wait_for_chain_to_be_ready,
};
use crate::utils::find_project_root;

/// Number of *new* executed batches beyond the current count that
/// [`Server::wait_for_executed_batches_with_traffic`] waits for.
const DEFAULT_EXTRA_BATCHES: u64 = 3;

/// Default timeout used by [`Server::wait_for_executed_batches_with_traffic`].
const DEFAULT_BATCH_WAIT_TIMEOUT: Duration = Duration::from_secs(120);

/// Default L2 balance-poll timeout used by
/// [`Server::fund_account_via_l1_deposit`].
const DEFAULT_DEPOSIT_POLL_TIMEOUT: Duration = Duration::from_secs(120);

/// Chain base-token mode for [`Server::fund_account_via_l1_deposit`].
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum L1DepositBaseToken {
    /// Chain's base token is ETH. The deposit is paid via `msg.value`.
    Eth,
    /// Chain's base token is a custom ERC-20 that the caller has already
    /// approved to the bridgehub on L1.
    PreApprovedCustom,
}

static TEST_RUN_ID: OnceLock<String> = OnceLock::new();
const SERVER_READY_MAX_ATTEMPTS: usize = 30;
const SERVER_READY_RETRY_DELAY: Duration = Duration::from_millis(500);
const ZKSYNC_OS_SERVER_IMAGE_REPO: &str = "ghcr.io/matter-labs/zksync-os-server";

pub fn get_or_create_run_id(name: &str) -> &'static str {
    TEST_RUN_ID
        .get_or_init(|| name.replace([' ', ':'], "_"))
        .as_str()
}

/// Return the run ID if it has already been initialised (by a prior `get_or_create_run_id` call).
pub(crate) fn get_run_id() -> Option<&'static str> {
    TEST_RUN_ID.get().map(|s| s.as_str())
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

/// On Linux, return the host `uid:gid` so the server container can write to
/// bind mounts as the host user (otherwise container-root writes leave
/// root-owned files on the host that the non-root test user cannot clean up).
///
/// On macOS, Docker Desktop's file-sharing layer translates container uid to
/// the host user transparently, so we leave `--user` unset and let the image
/// default (`app`) apply.
#[cfg(target_os = "linux")]
fn host_user_arg() -> Option<String> {
    let uid = Command::new("id").arg("-u").output().ok()?;
    let gid = Command::new("id").arg("-g").output().ok()?;
    let uid = String::from_utf8_lossy(&uid.stdout).trim().to_string();
    let gid = String::from_utf8_lossy(&gid.stdout).trim().to_string();
    if uid.is_empty() || gid.is_empty() {
        return None;
    }
    Some(format!("{}:{}", uid, gid))
}

#[cfg(not(target_os = "linux"))]
fn host_user_arg() -> Option<String> {
    None
}

/// Extract a top-level or nested YAML value by key name. Handles both
/// quoted and unquoted values. Very simple line-based parser — good enough
/// for the flat configs `ServerConfigBuilder` produces.
fn extract_yaml_field(yaml: &str, key: &str) -> Option<String> {
    for line in yaml.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix(key).and_then(|s| s.strip_prefix(':')) {
            let val = rest.trim().trim_matches('\'').trim_matches('"').trim();
            if !val.is_empty() {
                return Some(val.to_string());
            }
        }
    }
    None
}

/// Query `bridgehub.getZKChain(chain_id)` on L1 to resolve the diamond
/// proxy address for a chain.
fn resolve_diamond_proxy(
    bridgehub_addr: &str,
    chain_id: u64,
    l1_rpc_url: &str,
) -> Result<String, DockerError> {
    let output = Command::new("cast")
        .args([
            "call",
            bridgehub_addr,
            "getZKChain(uint256)(address)",
            &chain_id.to_string(),
            "--rpc-url",
            l1_rpc_url,
        ])
        .output()
        .map_err(|e| DockerError::CommandFailed(format!("cast call getZKChain: {e}")))?;
    if !output.status.success() {
        return Err(DockerError::CommandFailed(format!(
            "getZKChain({chain_id}) on {bridgehub_addr} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let addr = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if addr.is_empty() || addr == "0x0000000000000000000000000000000000000000" {
        return Err(DockerError::CommandFailed(format!(
            "getZKChain({chain_id}) returned zero address — chain not registered on bridgehub {bridgehub_addr}"
        )));
    }
    Ok(addr)
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
    /// Human-readable chain name (e.g. "gateway", "l1_settling"). Used for:
    /// - log filenames and RocksDB directory names
    /// - auto-resolving the config path via `chain_config_path(preset, chain_name)`
    ///   when no explicit `.config_path(…)` is set
    chain_name: String,
    /// Host port where server JSON-RPC should listen (None = random)
    host_port: Option<u16>,
    /// Override config path (used instead of preset-derived path when set)
    config_path_override: Option<PathBuf>,
    /// Override RocksDB directory for local server (default: under test-run-logs/{run_id}/)
    rocks_db_path_override: Option<PathBuf>,
    /// Override the logs directory (default: test-run-logs/{run_id}/)
    logs_dir_override: Option<PathBuf>,
    /// Override gateway RPC URL (set via env var at runtime, overrides config YAML value)
    gateway_rpc_url: Option<String>,
    /// Diamond-proxy address on the settlement layer for this chain.
    /// Auto-resolved from L1 via `bridgehub.getZKChain(chain_id)` if not
    /// set; required by [`Server::wait_for_executed_batches_with_traffic`].
    diamond_proxy_addr: Option<String>,
    /// Bridgehub proxy address on L1. Auto-read from config YAML if not set;
    /// required by [`Server::fund_account_via_l1_deposit`].
    bridgehub_addr: Option<String>,
    /// Chain ID. Auto-read from config YAML if not set; required by
    /// [`Server::fund_account_via_l1_deposit`].
    chain_id: Option<u64>,
    /// When true, do NOT set general_rocks_db_path / sequencer_rocks_db_path env vars.
    /// Required for ephemeral mode configs where the server manages its own tempdir.
    ephemeral: bool,
}

impl ServerBuilder {
    /// Create a new ServerBuilder.
    ///
    /// `chain_name` identifies the chain (e.g. `"l1_settling"`,
    /// `"gateway_settling_a"`). It is used for log filenames, RocksDB
    /// directory names, and — when no explicit `.config_path(…)` is set —
    /// to auto-resolve the config YAML from the preset's l1-state cache
    /// via `chain_config_path(preset, chain_name)`.
    ///
    /// Backend (local vs docker) is determined by `preset.zksync_os_server`.
    pub fn new(preset: Preset, chain_name: impl Into<String>) -> Self {
        Self {
            preset,
            chain_name: chain_name.into(),
            host_port: None,
            config_path_override: None,
            rocks_db_path_override: None,
            logs_dir_override: None,
            gateway_rpc_url: None,
            diamond_proxy_addr: None,
            bridgehub_addr: None,
            chain_id: None,
            ephemeral: false,
        }
    }

    /// Enable ephemeral mode: skip setting rocks_db env vars so the server's
    /// `general.ephemeral = true` config controls all RocksDB paths via a tempdir.
    pub fn ephemeral(mut self) -> Self {
        self.ephemeral = true;
        self
    }

    /// Override the config path (used instead of preset-derived path when set).
    pub fn config_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.config_path_override = Some(path.into());
        self
    }

    /// Use a fixed RocksDB path (local server only). Lets a later process reuse replay / tree state.
    pub fn rocks_db_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.rocks_db_path_override = Some(path.into());
        self
    }

    /// Override the directory where server logs are stored.
    /// Default: `test-run-logs/{run_id}/`.
    pub fn logs_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.logs_dir_override = Some(path.into());
        self
    }

    /// Set gateway RPC URL at runtime (overrides whatever is in config YAML).
    /// Used for gateway-settling chains where the gateway port is only known at test time.
    pub fn gateway_rpc_url(mut self, url: impl Into<String>) -> Self {
        self.gateway_rpc_url = Some(url.into());
        self
    }

    /// Spawn the server with the given Anvil L1.
    ///
    /// If no `.config_path(…)` was set, the config is auto-resolved from
    /// the preset's l1-state cache as `{cache_dir}/{chain_name}.yaml`.
    ///
    /// `chain_id` and `bridgehub_addr` are read from the config YAML
    /// (`genesis.chain_id`, `genesis.bridgehub_address`) if not explicitly set
    /// via builder methods. `diamond_proxy_addr` is resolved on-chain via
    /// `bridgehub.getZKChain(chain_id)` if not explicitly set.
    pub fn spawn(mut self, anvil: &Anvil) -> Result<Server, DockerError> {
        let project_root = find_project_root()?;
        let local_chains_path = project_root.join("local-chains");

        // Auto-resolve config from preset cache if not overridden.
        let config_path = if let Some(p) = &self.config_path_override {
            p.to_string_lossy().to_string()
        } else {
            let path = crate::l1_state::chain_config_path(&self.preset, &self.chain_name).map_err(
                |e| {
                    DockerError::CommandFailed(format!(
                        "auto-resolve config for chain '{}': {e}",
                        self.chain_name
                    ))
                },
            )?;
            path.to_string_lossy().to_string()
        };

        // --- Auto-fill chain_id, bridgehub_addr, diamond_proxy_addr from config + L1 ---
        if self.chain_id.is_none() || self.bridgehub_addr.is_none() {
            if let Ok(content) = std::fs::read_to_string(&config_path) {
                if self.chain_id.is_none() {
                    self.chain_id = extract_yaml_field(&content, "chain_id")
                        .and_then(|s| s.parse::<u64>().ok());
                }
                if self.bridgehub_addr.is_none() {
                    self.bridgehub_addr = extract_yaml_field(&content, "bridgehub_address");
                }
            }
        }

        let l1_rpc_url = anvil.rpc_url_for(&self.preset.zksync_os_server);
        // Host-side L1 URL for use from the test harness (cast calls, etc.),
        // as opposed to `l1_rpc_url` which may be rewritten to
        // `host.docker.internal` for the server container.
        let host_l1_rpc_url = anvil.rpc_url().to_string();

        if self.diamond_proxy_addr.is_none() {
            if let (Some(bridgehub), Some(chain_id)) = (&self.bridgehub_addr, self.chain_id) {
                self.diamond_proxy_addr =
                    resolve_diamond_proxy(bridgehub, chain_id, &host_l1_rpc_url).ok();
            }
        }
        let (server_root, use_local, image) = match &self.preset.zksync_os_server {
            RepoRef::Path(_) => {
                let paths = server_paths_for_preset(&self.preset).map_err(|e| {
                    DockerError::CommandFailed(format!("Failed to resolve preset paths: {}", e))
                })?;
                (
                    Some(paths.server_root.clone()),
                    true,
                    format!("{}:latest", ZKSYNC_OS_SERVER_IMAGE_REPO),
                )
            }
            RepoRef::DockerTag { tag, .. } => (
                None,
                false,
                format!("{}:{}", ZKSYNC_OS_SERVER_IMAGE_REPO, tag),
            ),
        };
        let builder = InnerServerBuilder {
            host_port: self.host_port,
            l1_rpc_url,
            host_l1_rpc_url,
            local_chains_path,
            config_path,
            image,
            use_local,
            rocks_db_path: self.rocks_db_path_override,
            logs_dir: self.logs_dir_override,
            chain_name: Some(self.chain_name),
            gateway_rpc_url: self.gateway_rpc_url,
            diamond_proxy_addr: self.diamond_proxy_addr,
            bridgehub_addr: self.bridgehub_addr,
            chain_id: self.chain_id,
            ephemeral: self.ephemeral,
        };
        Server::spawn_inner(builder, server_root)
    }
}

#[derive(Debug, Clone)]
struct InnerServerBuilder {
    host_port: Option<u16>,
    /// L1 URL passed to the server process (may be `host.docker.internal:...` for docker mode).
    l1_rpc_url: String,
    /// L1 URL usable from the test harness itself (always localhost-side).
    host_l1_rpc_url: String,
    local_chains_path: PathBuf,
    config_path: String,
    image: String,
    use_local: bool,
    rocks_db_path: Option<PathBuf>,
    logs_dir: Option<PathBuf>,
    chain_name: Option<String>,
    gateway_rpc_url: Option<String>,
    diamond_proxy_addr: Option<String>,
    bridgehub_addr: Option<String>,
    chain_id: Option<u64>,
    ephemeral: bool,
}

/// A running zksync-os-server instance
#[derive(Debug)]
pub struct Server {
    runtime: ServerRuntime,
    /// Unique Docker container name (UUID-based).
    server_name: String,
    /// Human-readable label used in log filenames and DB directory names.
    log_label: String,
    host_port: u16,
    logs_dir: PathBuf,
    run_index: std::cell::Cell<u32>,
    /// L1 URL as seen from the test harness (localhost-side, not docker-rewritten).
    host_l1_rpc_url: String,
    /// Gateway RPC URL if this server was configured to settle via a gateway.
    gateway_rpc_url: Option<String>,
    /// Diamond-proxy address on the settlement layer.
    diamond_proxy_addr: Option<String>,
    /// Bridgehub proxy address on L1.
    bridgehub_addr: Option<String>,
    /// Chain ID of the chain this server runs.
    chain_id: Option<u64>,
}

#[derive(Debug)]
enum ServerRuntime {
    Local(Box<LocalServerRuntime>),
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
        let log_label = builder
            .chain_name
            .clone()
            .unwrap_or_else(|| server_name.clone());

        // Find project root and resolve paths relative to it
        let project_root = find_project_root()?;

        // Group all server logs in this test run under test-run-logs/{run_id}.
        let chain_name = builder.chain_name.as_deref().unwrap_or("unknown");
        let run_id = get_or_create_run_id(chain_name);
        let logs_dir = if let Some(override_dir) = builder.logs_dir.as_ref() {
            override_dir.clone()
        } else {
            project_root.join("test-run-logs").join(run_id)
        };
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

        let first_run_log = logs_dir.join(format!("{}_run_1.json", log_label));
        let runtime = if use_local {
            let server_root =
                server_root.unwrap_or_else(|| project_root.join("../zksync-os-server"));
            let binary_path = resolve_local_server_binary(&server_root)?;
            let rocks_path = builder
                .rocks_db_path
                .clone()
                .unwrap_or_else(|| logs_dir.join(format!("db_{}", log_label)));
            let runtime = LocalServerRuntime::new(
                server_name.clone(),
                binary_path,
                server_root,
                builder.config_path.clone(),
                builder.l1_rpc_url.clone(),
                host_port,
                local_chains_abs,
                rocks_path,
                builder.gateway_rpc_url.clone(),
                builder.ephemeral,
            );
            runtime.start_with_log_path(&first_run_log)?;
            ServerRuntime::Local(Box::new(runtime))
        } else {
            if !docker_available() {
                return Err(DockerError::DockerNotAvailable(
                    "Docker is not installed or not in PATH".to_string(),
                ));
            }
            // Resolve the config file's parent directory and mount it into
            // the container so the server can read the config, genesis.json,
            // and any sibling files (wallets, chain configs, etc.).
            let config_host_path = std::fs::canonicalize(&builder.config_path).map_err(|e| {
                DockerError::CommandFailed(format!(
                    "Failed to canonicalize config path '{}': {}",
                    builder.config_path, e
                ))
            })?;
            let config_dir = config_host_path.parent().ok_or_else(|| {
                DockerError::CommandFailed(format!(
                    "Config path '{}' has no parent directory",
                    config_host_path.display()
                ))
            })?;
            let config_filename = config_host_path.file_name().ok_or_else(|| {
                DockerError::CommandFailed(format!(
                    "Config path '{}' has no filename",
                    config_host_path.display()
                ))
            })?;
            let container_config_dir = "/app/config";
            let container_config_path = format!(
                "{}/{}",
                container_config_dir,
                config_filename.to_string_lossy()
            );

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
                .arg(format!("GENERAL_L1_RPC_URL={}", builder.l1_rpc_url));
            if let Some(ref gw_url) = builder.gateway_rpc_url {
                // Remap localhost to host.docker.internal so the container can
                // reach the gateway server running on the host.
                let docker_gw_url = gw_url
                    .replace("://localhost:", "://host.docker.internal:")
                    .replace("://127.0.0.1:", "://host.docker.internal:");
                cmd.arg("-e")
                    .arg(format!("general_gateway_rpc_url={}", docker_gw_url));
            }
            // genesis.json must sit next to config.yaml in the same directory.
            if config_dir.join("genesis.json").exists() {
                cmd.arg("-e").arg(format!(
                    "genesis_genesis_input_path={}/genesis.json",
                    container_config_dir
                ));
            }
            cmd.arg("-e")
                .arg(format!("LOCAL_CHAINS_PATH={}", container_config_dir));
            // Mount RocksDB directory so data persists on the host (needed for
            // ephemeral state archival after server shutdown).
            let rocks_path = builder
                .rocks_db_path
                .clone()
                .unwrap_or_else(|| logs_dir.join(format!("db_{}", log_label)));
            fs::create_dir_all(&rocks_path).map_err(|e| {
                DockerError::CommandFailed(format!(
                    "Failed to create rocks db directory '{}': {}",
                    rocks_path.display(),
                    e
                ))
            })?;
            // Mount `rocks_path` at `/db` so that both the RocksDB databases
            // (`general_rocks_db_path = /db/rocksdb`) AND the proof-storage /
            // block-dump directories (relative paths `./db/fri_proofs/`,
            // `./db/block_dumps/` from workdir `/`) land inside the same
            // host-writable bind mount. Previously we mounted at `/db/rocksdb`
            // which left `/db/fri_proofs/` in the anonymous Docker layer — and
            // on Linux CI, where `--user UID:GID` is set, the host user
            // couldn't create directories there (`Permission denied`).
            let container_db_mount = "/db";
            // Point RocksDB directly at the mount root so the host-side
            // layout (`rocks_path/tree`, `rocks_path/block_replay_wal`, …)
            // stays flat — matching what the ephemeral-state archiver
            // (`tar.append_dir_all("node", &gw_rocks_db)`) and unpacker
            // expect. Previously we used `/db/rocksdb` which added a
            // subdirectory on the host that broke the archive layout.
            let container_rocks_path = "/db";
            if builder.ephemeral {
                // Remap ephemeral_state path: the archive lives in the config
                // dir on the host, which is mounted at container_config_dir.
                // Parse the config to find the ephemeral_state filename and
                // override it to the container-mounted path.
                let config_content = fs::read_to_string(&config_host_path).map_err(|e| {
                    DockerError::CommandFailed(format!(
                        "Failed to read config '{}': {}",
                        config_host_path.display(),
                        e
                    ))
                })?;
                if let Some(state_line) = config_content
                    .lines()
                    .find(|l| l.contains("ephemeral_state:"))
                {
                    if let Some(host_path) = state_line
                        .split('\'')
                        .nth(1)
                        .or_else(|| state_line.split(':').nth(1).map(|s| s.trim()))
                    {
                        let filename = std::path::Path::new(host_path)
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy();
                        cmd.arg("-e").arg(format!(
                            "general_ephemeral_state={}/{}",
                            container_config_dir, filename
                        ));
                    }
                }
            } else {
                cmd.arg("-e")
                    .arg(format!("general_rocks_db_path={}", container_rocks_path))
                    .arg("-e")
                    .arg(format!("sequencer_rocks_db_path={}", container_rocks_path));
            }
            cmd.arg("--add-host")
                .arg("host.docker.internal:host-gateway")
                .arg("-v")
                .arg(format!("{}:/app/local-chains", local_chains_abs.display()))
                .arg("-v")
                .arg(format!("{}:{}", config_dir.display(), container_config_dir))
                .arg("-v")
                .arg(format!("{}:{}", rocks_path.display(), container_db_mount))
                // Workdir `/` (not `/app`) so the server's relative config
                // paths (`./db/fri_proofs/`, `./db/block_dumps/`) resolve to
                // `/db/...` — the image's writable volume — rather than
                // `/app/db/...` which `app` cannot create under root-owned /app.
                .arg("--workdir")
                .arg("/");
            if let Some(user) = host_user_arg() {
                cmd.arg("--user").arg(user);
            }
            cmd.arg(&builder.image)
                .arg("--config")
                .arg(&container_config_path);

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
            let container = DockerContainer::new(server_name.clone());

            // Wait for the server inside the container to be ready,
            // the same way we do for local servers.
            let rpc_url = format!("http://127.0.0.1:{}/", host_port);
            let _ = container.save_logs(&first_run_log);
            if let Err(err) = crate::server_utils::wait_for_chain_to_be_ready(
                &rpc_url,
                "Docker server RPC",
                SERVER_READY_MAX_ATTEMPTS,
                SERVER_READY_RETRY_DELAY,
                Some(first_run_log.as_path()),
            ) {
                // Capture container logs before reporting the error.
                let _ = container.save_logs(&first_run_log);
                let _ = container.kill(&first_run_log);
                return Err(DockerError::CommandFailed(format!(
                    "Server container '{}' started but RPC did not become ready on port {}:\n{}\nCheck logs at '{}'",
                    server_name,
                    host_port,
                    err,
                    first_run_log.display(),
                )));
            }
            // Save a snapshot of logs now that the server is ready.
            let _ = container.save_logs(&first_run_log);

            ServerRuntime::Docker(container)
        };

        Ok(Self {
            runtime,
            server_name,
            log_label,
            host_port,
            logs_dir,
            run_index: std::cell::Cell::new(1),
            host_l1_rpc_url: builder.host_l1_rpc_url,
            gateway_rpc_url: builder.gateway_rpc_url,
            diamond_proxy_addr: builder.diamond_proxy_addr,
            bridgehub_addr: builder.bridgehub_addr,
            chain_id: builder.chain_id,
        })
    }

    /// Get the container name
    pub fn container_name(&self) -> &str {
        &self.server_name
    }

    /// Get the L2 RPC URL
    pub fn rpc_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.host_port)
    }

    /// Settlement-layer RPC URL: the gateway's L2 RPC when this server was
    /// configured to settle through a gateway, otherwise the L1 (Anvil) URL.
    pub fn settlement_rpc_url(&self) -> &str {
        self.gateway_rpc_url
            .as_deref()
            .unwrap_or(self.host_l1_rpc_url.as_str())
    }

    /// Send a tiny self-driven L2 transaction (1 wei to `0x...01`) to nudge
    /// the server's batch builder. Signed by Anvil's default pre-funded
    /// account.
    pub fn send_traffic_tx(&self) -> anyhow::Result<()> {
        send_traffic_tx(&self.rpc_url(), crate::anvil::DEFAULT_ANVIL_PRIVATE_KEY)
    }

    /// Fund `recipient` on this server's L2 via a Bridgehub L1→L2 deposit of
    /// `amount` units of the chain's base token, then poll L2 until the
    /// recipient's balance strictly increases.
    ///
    /// Uses Anvil's default pre-funded account as the L1 signer and
    /// [`DEFAULT_DEPOSIT_POLL_TIMEOUT`] for the balance-poll. When
    /// `base_token == L1DepositBaseToken::PreApprovedCustom`, the caller must
    /// have already approved the base token to the bridgehub.
    ///
    /// Requires [`ServerBuilder::bridgehub_addr`] and
    /// [`ServerBuilder::chain_id`] to have been set at build time.
    pub async fn fund_account_via_l1_deposit(
        &self,
        recipient: &str,
        amount: f64,
        base_token: L1DepositBaseToken,
    ) -> anyhow::Result<u128> {
        let bridgehub_addr = self.bridgehub_addr.as_deref().ok_or_else(|| {
            anyhow::anyhow!(
                "Server::fund_account_via_l1_deposit requires \
                 ServerBuilder::bridgehub_addr to be set"
            )
        })?;
        let chain_id = self.chain_id.ok_or_else(|| {
            anyhow::anyhow!(
                "Server::fund_account_via_l1_deposit requires \
                 ServerBuilder::chain_id to be set"
            )
        })?;
        let logs_path = self.logs_path();
        // Flush docker logs to host before the deposit so the diagnostic
        // reader can find the file if the server crashes mid-operation.
        let _ = self.save_logs();
        fund_l2_via_l1_deposit_ex(
            &self.host_l1_rpc_url,
            &self.rpc_url(),
            bridgehub_addr,
            chain_id,
            recipient,
            amount,
            DEFAULT_DEPOSIT_POLL_TIMEOUT,
            Some(logs_path.as_path()),
            matches!(base_token, L1DepositBaseToken::Eth),
        )
        .await
    }

    /// Drive L2 traffic on this server until [`DEFAULT_EXTRA_BATCHES`] more
    /// batches have been executed on the settlement-layer diamond proxy.
    ///
    /// The traffic is intentional, not a workaround for idle batch sealing:
    /// closing batches that actually contain transactions doubles as an
    /// end-to-end sanity check that the commit → prove → execute pipeline
    /// still works. Empty-batch behavior isn't what these tests want to
    /// exercise.
    ///
    /// Uses [`DEFAULT_BATCH_WAIT_TIMEOUT`] and Anvil's default pre-funded
    /// account as the traffic signer. Requires
    /// [`ServerBuilder::diamond_proxy_addr`] to have been set at build time.
    pub fn wait_for_executed_batches_with_traffic(&self) -> anyhow::Result<u64> {
        let diamond_proxy_addr = self.diamond_proxy_addr.as_deref().ok_or_else(|| {
            anyhow::anyhow!(
                "Server::wait_for_executed_batches_with_traffic requires \
                 ServerBuilder::diamond_proxy_addr to be set"
            )
        })?;
        let settlement_rpc_url = self.settlement_rpc_url();

        let start_executed = get_total_batches_executed(settlement_rpc_url, diamond_proxy_addr)
            .context("Failed to read initial getTotalBatchesExecuted")?;
        let target = start_executed + DEFAULT_EXTRA_BATCHES;
        println!(
            "Waiting for {} more executed batches (current={}, target={})",
            DEFAULT_EXTRA_BATCHES, start_executed, target
        );

        let start = Instant::now();
        let mut tx_count = 0u64;
        let mut next_progress_at = start + Duration::from_secs(5);

        loop {
            let executed = get_total_batches_executed(settlement_rpc_url, diamond_proxy_addr)
                .context("Failed to read getTotalBatchesExecuted")?;

            let now = Instant::now();
            if now >= next_progress_at {
                println!(
                    "Progress: executed_batches={}, sent_txs={}",
                    executed, tx_count
                );
                next_progress_at = now + Duration::from_secs(5);
            }

            if executed >= target {
                println!(
                    "Reached executed batches target: {} (sent {} txs)",
                    executed, tx_count
                );
                return Ok(executed);
            }

            if start.elapsed() >= DEFAULT_BATCH_WAIT_TIMEOUT {
                anyhow::bail!(
                    "Timed out waiting for executed batches. target={}, current={}, sent_txs={}",
                    target,
                    executed,
                    tx_count
                );
            }

            self.send_traffic_tx()
                .with_context(|| format!("Failed to send traffic tx #{}", tx_count + 1))?;
            tx_count += 1;
            sleep(Duration::from_secs(3));
        }
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

    /// Flush current container logs to the host log file.
    ///
    /// For docker containers this runs `docker logs` and writes the output;
    /// for local servers the logs are already streamed to the file, so this
    /// is a no-op.
    pub fn save_logs(&self) -> Result<(), DockerError> {
        if let ServerRuntime::Docker(c) = &self.runtime {
            let log_path = self.run_logs_path(self.run_index.get());
            c.save_logs(&log_path)?;
        }
        Ok(())
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
            .join(format!("{}_run_{}.json", self.log_label, run_index))
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
    gateway_rpc_url: Option<String>,
    ephemeral: bool,
    child: Mutex<Option<Child>>,
    current_log_path: Mutex<Option<PathBuf>>,
}

impl LocalServerRuntime {
    #[allow(clippy::too_many_arguments)]
    fn new(
        name: String,
        binary_path: PathBuf,
        server_root: PathBuf,
        config_path: String,
        l1_rpc_url: String,
        host_port: u16,
        local_chains_abs: PathBuf,
        rocks_db_path: PathBuf,
        gateway_rpc_url: Option<String>,
        ephemeral: bool,
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
            gateway_rpc_url,
            ephemeral,
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
        // genesis.json must sit next to the config file.
        let config_as_path = std::path::Path::new(&self.config_path);
        let genesis_path = config_as_path.parent().and_then(|dir| {
            let sibling = dir.join("genesis.json");
            if sibling.exists() {
                Some(sibling)
            } else {
                None
            }
        });

        cmd.arg("--config")
            .arg(&self.config_path)
            .current_dir(&self.server_root)
            .env("GENERAL_L1_RPC_URL", &self.l1_rpc_url)
            .env("rpc_address", format!("0.0.0.0:{}", self.host_port));
        if let Some(ref gw_url) = self.gateway_rpc_url {
            cmd.env("general_gateway_rpc_url", gw_url);
        }
        if !self.ephemeral {
            // In ephemeral mode the server creates its own tempdir for RocksDB;
            // setting these env vars would override that and break ephemeral state loading.
            fs::create_dir_all(&self.rocks_db_path).map_err(|e| {
                DockerError::CommandFailed(format!(
                    "Failed to create rocks db directory '{}': {}",
                    self.rocks_db_path.display(),
                    e
                ))
            })?;
            cmd.env(
                "general_rocks_db_path",
                self.rocks_db_path.to_string_lossy().to_string(),
            )
            .env(
                "sequencer_rocks_db_path",
                self.rocks_db_path.to_string_lossy().to_string(),
            );
        }
        cmd.env(
            "LOCAL_CHAINS_PATH",
            self.local_chains_abs.to_string_lossy().to_string(),
        );
        if let Some(gp) = &genesis_path {
            cmd.env(
                "genesis_genesis_input_path",
                gp.to_string_lossy().to_string(),
            );
        }
        // Randomize auxiliary ports to avoid collisions when multiple servers run concurrently.
        let status_port = pick_unused_port_sync()
            .map_err(|e| DockerError::CommandFailed(format!("pick status port: {}", e)))?;
        let prover_port = pick_unused_port_sync()
            .map_err(|e| DockerError::CommandFailed(format!("pick prover port: {}", e)))?;
        let prometheus_port = pick_unused_port_sync()
            .map_err(|e| DockerError::CommandFailed(format!("pick prometheus port: {}", e)))?;
        cmd.env("status_server_address", format!("0.0.0.0:{}", status_port))
            .env("prover_api_address", format!("0.0.0.0:{}", prover_port))
            .env("observability_prometheus_port", prometheus_port.to_string());

        cmd.stdout(Stdio::from(log_file))
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
