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
    get_l2_finalized_block_number, print_deposit_failure_server_logs, send_traffic_tx,
    send_traffic_tx_returning_hash, strip_ansi_escape_codes_in_file, wait_for_chain_to_be_ready,
    wait_for_l2_tx_block_number,
};
use crate::utils::find_project_root;

use crate::DEFAULT_WAIT_TIMEOUT;

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
const SERVER_READY_MAX_ATTEMPTS: usize = 60;
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

/// Read `PRESET_NAME` (set by `run-tests.sh` and `generate-l1-state`). All
/// callers that want to construct a `test-run-logs/<preset>/...` path go
/// through this so the layout stays consistent across presets.
pub(crate) fn current_preset_name() -> anyhow::Result<String> {
    let name = std::env::var("PRESET_NAME").map_err(|_| {
        anyhow::anyhow!(
            "PRESET_NAME env var not set. `run-tests.sh` sets it per-preset; \
             when invoking a test binary directly, export \
             PRESET_NAME=<preset-name-from-presets.yaml>."
        )
    })?;
    anyhow::ensure!(!name.is_empty(), "PRESET_NAME env var is empty");
    Ok(name)
}

/// `{project_root}/test-run-logs/{preset_name}` — the per-preset root under
/// which every test and generator keeps its ephemeral outputs. Created on
/// access. See [`current_preset_name`] for how the preset name is resolved.
pub fn preset_logs_root() -> anyhow::Result<std::path::PathBuf> {
    let project_root = crate::infra::utils::find_project_root()
        .map_err(|e| anyhow::anyhow!("find project root: {e}"))?;
    let dir = project_root
        .join("test-run-logs")
        .join(current_preset_name()?);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("create preset logs root {}", dir.display()))?;
    Ok(dir)
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

fn resolve_local_server_binary(server_root: &Path) -> Result<PathBuf, DockerError> {
    let release_bin = server_root.join("target/release/zksync-os-server");
    // `integration-tests/build.rs` builds the server via `cargo build
    // --release` before any test runs. By the time we get here the binary
    // is up-to-date (cargo's fingerprint check inside build.rs is a no-op
    // when nothing changed).
    if release_bin.exists() {
        Ok(release_bin)
    } else {
        Err(DockerError::CommandFailed(format!(
            "zksync-os-server binary not found at {}. \
             Run `cargo build --tests` in integration-tests — the binary is \
             produced by integration-tests/build.rs.",
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
    /// Override RocksDB directory for local server (default: under test-run-logs/{preset_name}/{run_id}/)
    rocks_db_path_override: Option<PathBuf>,
    /// Override the logs directory (default: test-run-logs/{preset_name}/{run_id}/)
    logs_dir_override: Option<PathBuf>,
    /// Override gateway RPC URL (set via env var at runtime, overrides config YAML value)
    gateway_rpc_url: Option<String>,
    /// Bridgehub proxy address on L1. Auto-read from config YAML if not set;
    /// required by [`Server::fund_account_via_l1_deposit`].
    bridgehub_addr: Option<String>,
    /// Chain ID. Auto-read from config YAML if not set; required by
    /// [`Server::fund_account_via_l1_deposit`].
    chain_id: Option<u64>,
    /// When true, do NOT set general_rocks_db_path / sequencer_rocks_db_path env vars.
    /// Required for ephemeral mode configs where the server manages its own tempdir.
    ephemeral: bool,
    /// Extra env vars to inject into the server process (lowercase
    /// `section_field` form, e.g. `l1_sender_pubdata_mode=RelayedL2Calldata`).
    /// Override values from the config YAML.
    extra_envs: Vec<(String, String)>,
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
            bridgehub_addr: None,
            chain_id: None,
            ephemeral: false,
            extra_envs: Vec::new(),
        }
    }

    /// Inject an extra env var into the server process. Uses lowercase
    /// `section_field` form matching `smart-config`'s env mapping (e.g.
    /// `l1_sender_pubdata_mode=RelayedL2Calldata`). Overrides any value
    /// set in the config YAML.
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.extra_envs.push((key.into(), value.into()));
        self
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
    /// Default: `test-run-logs/{preset_name}/{run_id}/`.
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
            bridgehub_addr: self.bridgehub_addr,
            chain_id: self.chain_id,
            ephemeral: self.ephemeral,
            extra_envs: self.extra_envs,
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
    bridgehub_addr: Option<String>,
    chain_id: Option<u64>,
    ephemeral: bool,
    extra_envs: Vec<(String, String)>,
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

        // Group all server logs in this test run under
        // test-run-logs/{preset_name}/{run_id}. The run ID must be set before
        // any server is spawned — call `get_or_create_run_id("test_name")` at
        // the top of the test. `PRESET_NAME` is set by `run-tests.sh`.
        let run_id = get_run_id().unwrap_or_else(|| {
            panic!(
                "No run ID set. Call `integration_tests::server::get_or_create_run_id(\"test_name\")` \
                 before spawning a server."
            )
        });
        let logs_dir = if let Some(override_dir) = builder.logs_dir.as_ref() {
            override_dir.clone()
        } else {
            preset_logs_root()
                .map_err(|e| {
                    DockerError::CommandFailed(format!("resolve preset logs root: {e:#}"))
                })?
                .join(run_id)
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
                builder.extra_envs.clone(),
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
                .arg(format!("L1_PROVIDER_RPC_URL={}", builder.l1_rpc_url));
            if let Some(ref gw_url) = builder.gateway_rpc_url {
                // Remap localhost to host.docker.internal so the container can
                // reach the gateway server running on the host.
                let docker_gw_url = gw_url
                    .replace("://localhost:", "://host.docker.internal:")
                    .replace("://127.0.0.1:", "://host.docker.internal:");
                cmd.arg("-e")
                    .arg(format!("GATEWAY_PROVIDER_RPC_URL={}", docker_gw_url));
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
            for (k, v) in &builder.extra_envs {
                cmd.arg("-e").arg(format!("{k}={v}"));
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

        let server = Self {
            runtime,
            server_name,
            log_label,
            host_port,
            logs_dir,
            run_index: std::cell::Cell::new(1),
            host_l1_rpc_url: builder.host_l1_rpc_url,
            gateway_rpc_url: builder.gateway_rpc_url,
            bridgehub_addr: builder.bridgehub_addr,
            chain_id: builder.chain_id,
        };

        Ok(server)
    }

    /// Get the container name
    pub fn container_name(&self) -> &str {
        &self.server_name
    }

    /// Get the L2 RPC URL (localhost-side; works for callers running on the host).
    pub fn rpc_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.host_port)
    }

    /// Get the L2 RPC URL appropriate for a consumer matching `repo_ref`.
    /// Path = localhost; DockerTag = `host.docker.internal` (for consumers
    /// running inside a Docker container that need to reach the server's
    /// host-published port). Mirrors [`Anvil::rpc_url_for`].
    pub fn rpc_url_for(&self, repo_ref: &RepoRef) -> String {
        match repo_ref {
            RepoRef::Path(_) => self.rpc_url(),
            RepoRef::DockerTag { .. } => format!("http://host.docker.internal:{}", self.host_port),
        }
    }

    /// Settlement-layer RPC URL: the gateway's L2 RPC when this server was
    /// configured to settle through a gateway, otherwise the L1 (Anvil) URL.
    pub fn settlement_rpc_url(&self) -> &str {
        self.gateway_rpc_url
            .as_deref()
            .unwrap_or(self.host_l1_rpc_url.as_str())
    }

    /// Poll until `address` has a non-zero L2 balance on this server, or
    /// `timeout` expires. Useful when the balance comes from a priority-queue
    /// deposit submitted by `generate-l1-state` that the server hasn't
    /// processed yet at startup.
    pub fn wait_for_l2_balance(&self, address: &str, timeout: Duration) -> anyhow::Result<()> {
        let l2_rpc = self.rpc_url();
        let deadline = Instant::now() + timeout;
        loop {
            let output = std::process::Command::new("cast")
                .args(["balance", address, "--rpc-url", &l2_rpc])
                .output()
                .context("cast balance")?;
            if output.status.success() {
                let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
                let bal: u128 = raw.parse().unwrap_or(0);
                if bal > 0 {
                    return Ok(());
                }
            }
            if Instant::now() > deadline {
                anyhow::bail!("{address} still has zero L2 balance after {timeout:?}");
            }
            sleep(Duration::from_secs(2));
        }
    }

    /// Send a tiny self-driven L2 transaction (1 wei to `0x...01`) to nudge
    /// the server's batch builder. Signed by Anvil's default pre-funded
    /// account.
    pub fn send_traffic_tx(&self) -> anyhow::Result<()> {
        send_traffic_tx(&self.rpc_url(), crate::anvil::DEFAULT_ANVIL_PRIVATE_KEY)
    }

    /// Fund `recipient` on this server's L2 via a Bridgehub L1→L2 deposit of
    /// `amount` units of the chain's base token, then wait for the L2
    /// priority tx to execute.
    ///
    /// Uses Anvil's default pre-funded account as the L1 signer and
    /// [`DEFAULT_WAIT_TIMEOUT`] for the L2 receipt wait. When
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
    ) -> anyhow::Result<()> {
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

        println!(
            "  L1→L2 deposit: funding {recipient} on chain {chain_id} with {amount} base-token units"
        );

        let l2_tx_hash = match crate::l1_l2_deposit::submit_l1_to_l2_deposit_ex(
            &self.host_l1_rpc_url,
            bridgehub_addr,
            chain_id,
            crate::anvil::DEFAULT_ANVIL_PRIVATE_KEY,
            amount,
            Some(recipient),
            matches!(base_token, L1DepositBaseToken::Eth),
        )
        .await
        {
            Ok(hash) => hash,
            Err(err) => {
                print_deposit_failure_server_logs(Some(logs_path.as_path()));
                return Err(err).context("Bridgehub L1→L2 deposit");
            }
        };

        let wait_start = Instant::now();
        if let Err(err) = crate::l1_l2_deposit::wait_for_l2_priority_tx_receipt(
            &self.rpc_url(),
            l2_tx_hash,
            DEFAULT_WAIT_TIMEOUT,
        )
        .await
        {
            print_deposit_failure_server_logs(Some(logs_path.as_path()));
            return Err(err);
        }
        println!(
            "  L1→L2 deposit: executed on L2 in {:.2}s",
            wait_start.elapsed().as_secs_f64()
        );
        Ok(())
    }

    /// Send one L2 traffic tx and wait for its containing L2 block to reach
    /// `finalized` status on this server's L2 RPC.
    ///
    /// On zksync-os the L2 `finalized` tag tracks the highest L2 block whose
    /// containing batch has been executed on the settlement layer. So once
    /// `finalized` >= our tx's block, we know the commit → prove → execute
    /// pipeline has finished for that batch.
    ///
    /// Works uniformly for L1-settling and gateway-settling chains — the
    /// server itself knows what its settlement layer is.
    pub fn wait_for_traffic_tx_executed_on_l1(&self) -> anyhow::Result<u64> {
        let sender_addr =
            crate::server_utils::address_from_private_key(crate::anvil::DEFAULT_ANVIL_PRIVATE_KEY)
                .context("derive traffic sender address")?;
        self.wait_for_l2_balance(&sender_addr, Duration::from_secs(60))
            .context("traffic sender has no L2 balance")?;

        let start = Instant::now();
        let l2_rpc_url = self.rpc_url();

        let tx_hash = send_traffic_tx_returning_hash(
            l2_rpc_url.as_str(),
            crate::anvil::DEFAULT_ANVIL_PRIVATE_KEY,
        )
        .context("send L2 traffic tx")?;

        let target_block =
            wait_for_l2_tx_block_number(l2_rpc_url.as_str(), &tx_hash, DEFAULT_WAIT_TIMEOUT)
                .with_context(|| format!("wait for blockNumber on L2 tx {tx_hash}"))?;

        println!(
            "  Sent L2 tx {tx_hash} in L2 block {target_block}; \
             waiting for that block to reach finalized"
        );

        loop {
            let finalized = get_l2_finalized_block_number(l2_rpc_url.as_str())
                .context("read L2 finalized block number")?;
            if finalized >= target_block {
                println!(
                    "  L2 block {target_block} finalized \
                     (finalized={finalized}, took {:.1}s)",
                    start.elapsed().as_secs_f64(),
                );
                return Ok(target_block);
            }
            if start.elapsed() >= DEFAULT_WAIT_TIMEOUT {
                anyhow::bail!(
                    "Timed out after {:.1}s waiting for L2 block {target_block} \
                     to reach finalized (finalized={finalized})",
                    start.elapsed().as_secs_f64(),
                );
            }
            sleep(Duration::from_millis(100));
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
    extra_envs: Vec<(String, String)>,
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
        extra_envs: Vec<(String, String)>,
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
            extra_envs,
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
            .env("L1_PROVIDER_RPC_URL", &self.l1_rpc_url)
            .env("rpc_address", format!("0.0.0.0:{}", self.host_port));
        if let Some(ref gw_url) = self.gateway_rpc_url {
            cmd.env("GATEWAY_PROVIDER_RPC_URL", gw_url);
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
        for (k, v) in &self.extra_envs {
            cmd.env(k, v);
        }

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
