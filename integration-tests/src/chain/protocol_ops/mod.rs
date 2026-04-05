use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use anyhow::Context;
use chrono::Local;

use crate::presets::{Preset, RepoRef};
use crate::server::{get_or_create_run_id, get_run_id, read_toolchain_from_dir};
use crate::utils::find_project_root;

pub mod chain;
pub mod contracts_backend;

pub use contracts_backend::EraContractsBackend;

/// High-level helpers for running `protocol_ops` (RPC URL, backend).
/// Output files are written to `contracts_backend.work_dir()`.
pub struct ProtocolOps<'a> {
    pub l1_rpc_url: String,
    pub contracts_backend: &'a EraContractsBackend,
}

impl<'a> ProtocolOps<'a> {
    pub fn new(l1_rpc_url: impl Into<String>, contracts_backend: &'a EraContractsBackend) -> Self {
        Self {
            l1_rpc_url: l1_rpc_url.into(),
            contracts_backend,
        }
    }

    pub fn chain_set_upgrade_timestamp(&self) -> chain::ChainSetUpgradeTimestamp<'_> {
        chain::ChainSetUpgradeTimestamp::new(self)
    }
}

pub use chain::ProtocolOpsTransactions;

pub const ERA_CONTRACTS_PROTOCOL_IMAGE_REPO: &str = "ghcr.io/matter-labs/protocol-ops";

/// Env vars for protocol_ops Docker runs. Telemetry must be enabled for foundry to work
/// (Dockerfile sets ~/.config/zksync-tooling/telemetry.json {"enabled":true}).
const PROTOCOL_OPS_DOCKER_ENV: &[(&str, &str)] = &[("FOUNDRY_DISABLE_NIGHTLY_WARNING", "1")];

// ---------------------------------------------------------------------------
// Long-lived era-contracts container session
// ---------------------------------------------------------------------------

/// A long-lived Docker container for the era-contracts image.
///
/// Avoids repeated `docker run` overhead (significant on Apple Silicon where
/// the amd64 image runs under Rosetta). Start once, exec many.
pub struct EraContainerSession {
    container_name: String,
    /// Host-side work directory.
    host_work_dir: PathBuf,
    /// Container-side work directory (e.g. `/contracts/work/{name}`).
    container_work_dir: String,
}

impl EraContainerSession {
    /// Start a detached container with `work_dir` mounted at `container_work_dir`
    /// and `work_dir/script-out` mounted at `/contracts/l1-contracts/script-out`
    /// (for forge `fs_permissions`). Additional mounts may be passed.
    pub fn start(
        image: &str,
        work_dir: &Path,
        container_work_dir: &str,
        extra_mounts: &[(&Path, &str)],
    ) -> anyhow::Result<Self> {
        let script_out = work_dir.join("script-out");
        fs::create_dir_all(&script_out)?;
        let abs_work = fs::canonicalize(work_dir)?;
        let abs_script_out = fs::canonicalize(&script_out)?;

        let name = format!("era-session-{}", uuid::Uuid::new_v4());
        let mut cmd = Command::new("docker");
        cmd.arg("run")
            .arg("-d")
            .arg("--name")
            .arg(&name)
            .arg("--platform=linux/amd64")
            .arg("--add-host=host.docker.internal:host-gateway");
        for (k, v) in PROTOCOL_OPS_DOCKER_ENV {
            cmd.arg("-e").arg(format!("{}={}", k, v));
        }
        // Core mounts: work_dir and script-out overlay
        cmd.arg("-v")
            .arg(format!("{}:{}", abs_work.display(), container_work_dir));
        cmd.arg("-v").arg(format!(
            "{}:/contracts/l1-contracts/script-out",
            abs_script_out.display()
        ));
        for (host, container) in extra_mounts {
            fs::create_dir_all(host)?;
            let abs = fs::canonicalize(host)?;
            cmd.arg("-v")
                .arg(format!("{}:{}", abs.display(), container));
        }
        cmd.arg(image).arg("sleep").arg("infinity");
        let output = cmd.output().context("docker run (era session)")?;
        if !output.status.success() {
            anyhow::bail!(
                "failed to start era container session:\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(Self {
            container_name: name,
            host_work_dir: abs_work,
            container_work_dir: container_work_dir.to_string(),
        })
    }

    pub fn host_work_dir(&self) -> &Path {
        &self.host_work_dir
    }

    pub fn container_work_dir(&self) -> &str {
        &self.container_work_dir
    }

    /// Execute a command inside the running container.
    pub fn exec(
        &self,
        command: &[&str],
        envs: &[(&str, &str)],
        workdir: Option<&str>,
    ) -> anyhow::Result<String> {
        let mut cmd = Command::new("docker");
        cmd.arg("exec");
        for (k, v) in envs {
            let v = v
                .replace("://localhost:", "://host.docker.internal:")
                .replace("://127.0.0.1:", "://host.docker.internal:");
            cmd.arg("-e").arg(format!("{}={}", k, v));
        }
        if let Some(wd) = workdir {
            cmd.arg("-w").arg(wd);
        }
        cmd.arg(&self.container_name);
        cmd.args(command);

        let start = std::time::Instant::now();
        let output = cmd
            .output()
            .with_context(|| format!("docker exec {:?}", command))?;
        let elapsed = start.elapsed();

        log_protocol_ops_command_and_output(
            "docker-exec",
            command,
            &format!("container={}", self.container_name),
            &output,
            elapsed,
        );

        if !output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "{:?} failed in container {} with status: {}\n\nSTDOUT:\n{}\n\nSTDERR:\n{}",
                command,
                self.container_name,
                output.status,
                stdout,
                stderr
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    pub fn container_name(&self) -> &str {
        &self.container_name
    }

    /// Run `protocol_ops <args>` inside the container.
    /// Rewrites `--l1-rpc-url`, `--gateway-rpc-url`, and `--out` args for Docker.
    pub fn protocol_ops(&self, args: &[&str]) -> anyhow::Result<String> {
        let mut rewritten: Vec<String> = Vec::with_capacity(args.len());
        let mut i = 0;
        while i < args.len() {
            if args[i] == "--out" && i + 1 < args.len() {
                // Rewrite host path → container path. Strip the host work_dir
                // prefix and map into the container work_dir.
                let host_path = std::path::Path::new(args[i + 1]);
                let relative = host_path
                    .strip_prefix(&self.host_work_dir)
                    .unwrap_or_else(|_| {
                        // Fallback: just use filename
                        Path::new(host_path.file_name().unwrap_or_default())
                    });
                rewritten.push(args[i].to_string());
                rewritten.push(format!(
                    "{}/{}",
                    self.container_work_dir,
                    relative.display()
                ));
                i += 2;
                continue;
            }
            if (args[i] == "--l1-rpc-url" || args[i] == "--gateway-rpc-url") && i + 1 < args.len() {
                rewritten.push(args[i].to_string());
                rewritten.push(
                    args[i + 1]
                        .replace("://localhost:", "://host.docker.internal:")
                        .replace("://127.0.0.1:", "://host.docker.internal:"),
                );
                i += 2;
                continue;
            }
            rewritten.push(args[i].to_string());
            i += 1;
        }
        let mut full: Vec<&str> = vec!["protocol_ops"];
        full.extend(rewritten.iter().map(|s| s.as_str()));
        self.exec(&full, &[], None)
    }

    /// Run `forge script <forge_args>` from `/contracts/l1-contracts`.
    pub fn forge_script(
        &self,
        forge_args: &[&str],
        extra_envs: &[(&str, &str)],
    ) -> anyhow::Result<String> {
        let mut command: Vec<String> = vec!["forge".into(), "script".into()];
        for arg in forge_args {
            let remapped = arg
                .replace("://localhost:", "://host.docker.internal:")
                .replace("://127.0.0.1:", "://host.docker.internal:");
            command.push(remapped);
        }
        let cmd_refs: Vec<&str> = command.iter().map(|s| s.as_str()).collect();
        self.exec(&cmd_refs, extra_envs, Some("/contracts/l1-contracts"))
    }

    /// Run `cast <args>` inside the container.
    pub fn cast(&self, args: &[&str]) -> anyhow::Result<String> {
        let mut command: Vec<String> = vec!["cast".into()];
        for arg in args {
            let remapped = arg
                .replace("://localhost:", "://host.docker.internal:")
                .replace("://127.0.0.1:", "://host.docker.internal:");
            command.push(remapped);
        }
        let cmd_refs: Vec<&str> = command.iter().map(|s| s.as_str()).collect();
        self.exec(&cmd_refs, &[], None)
    }
}

impl Drop for EraContainerSession {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["rm", "-f", &self.container_name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

const PROTOCOL_OPS_COMMANDS_LOG: &str = "protocol_ops_commands.log";

fn protocol_ops_log_path_for_run(run_id: &str) -> Option<PathBuf> {
    let project_root = find_project_root().ok()?;
    let logs_dir = project_root.join(".test-run-logs").join(run_id);
    std::fs::create_dir_all(&logs_dir).ok()?;
    Some(logs_dir.join(PROTOCOL_OPS_COMMANDS_LOG))
}

/// Directory used for protocol_ops logs and out files (.test-run-logs/<run_id>).
/// Use this when writing or reading protocol_ops --out files from tests so they land in the same run dir.
/// `run_name` is the explicit test/run label (e.g. `"upgrade_v30_to_v31"`).
pub fn protocol_ops_logs_dir(run_name: &str) -> Option<PathBuf> {
    let run_id = get_or_create_run_id(run_name);
    protocol_ops_log_path_for_run(run_id).and_then(|p| p.parent().map(PathBuf::from))
}

fn log_protocol_ops_command_and_output(
    mode: &str,
    args: &[&str],
    extra: &str,
    output: &Output,
    elapsed: Duration,
) {
    // Print a concise summary to stdout.
    let cmd_summary: String = args.iter().take(3).copied().collect::<Vec<_>>().join(" ");
    let status = if output.status.success() {
        "ok"
    } else {
        "FAILED"
    };
    println!(
        "  [{mode}] {cmd_summary} ... {status} ({:.1}s)",
        elapsed.as_secs_f64()
    );

    let Some(log_path) = get_run_id().and_then(protocol_ops_log_path_for_run) else {
        return;
    };
    let _ = (|| -> anyhow::Result<()> {
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)?;
        let ts = Local::now().format("%Y-%m-%d %H:%M:%S");
        writeln!(
            f,
            "\n--- [{}] [{}] ({:.1}s) ---",
            ts,
            mode,
            elapsed.as_secs_f64()
        )?;
        writeln!(f, "{}", extra)?;
        writeln!(f, "args:")?;
        for arg in args {
            writeln!(f, "  {}", arg)?;
        }
        writeln!(f, "exit_status: {}", output.status)?;
        writeln!(f, "stdout:")?;
        f.write_all(&output.stdout)?;
        if !output.stdout.ends_with(b"\n") {
            writeln!(f)?;
        }
        writeln!(f, "stderr:")?;
        f.write_all(&output.stderr)?;
        if !output.stderr.ends_with(b"\n") {
            writeln!(f)?;
        }
        Ok(())
    })();
}

/// Escape a string for use in a single-quoted shell argument.
fn shell_escape(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', "'\\''"))
}

/// Run protocol_ops locally (not in Docker) against the given era-contracts path.
/// Returns stdout on success. Uses PROTOCOL_CONTRACTS_ROOT so protocol_ops finds contracts.
/// Builds silently first, clears broadcast dir, then runs the binary. Runs `nvm use` before execution.
pub fn run_protocol_ops_local(era_contracts_path: &Path, args: &[&str]) -> anyhow::Result<String> {
    let protocol_ops_dir = era_contracts_path.join("protocol-ops");
    let protocol_ops_manifest = protocol_ops_dir.join("Cargo.toml");
    if !protocol_ops_manifest.exists() {
        anyhow::bail!(
            "protocol-ops not found at {}",
            protocol_ops_manifest.display()
        );
    }

    let canonical_path = std::fs::canonicalize(era_contracts_path)
        .with_context(|| format!("Failed to canonicalize {}", era_contracts_path.display()))?;
    let root_str = canonical_path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("Path contains invalid UTF-8"))?;

    let manifest_str = protocol_ops_manifest
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("Manifest path contains invalid UTF-8"))?;

    let mut build_cmd = Command::new("cargo");
    build_cmd
        .args(["build", "--release", "--manifest-path", manifest_str])
        .current_dir(era_contracts_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(toolchain) = read_toolchain_from_dir(era_contracts_path) {
        build_cmd.env("RUSTUP_TOOLCHAIN", &toolchain);
    }
    let build_output = build_cmd
        .output()
        .with_context(|| "Failed to run cargo build for protocol_ops")?;
    if !build_output.status.success() {
        let stderr = String::from_utf8_lossy(&build_output.stderr);
        eprintln!("{}", stderr);
        anyhow::bail!(
            "cargo build --release for protocol_ops failed with status: {}\n\nSTDERR:\n{}",
            build_output.status,
            stderr
        );
    }

    let broadcast_dir = era_contracts_path.join("l1-contracts/broadcast");
    if broadcast_dir.exists() {
        fs::remove_dir_all(&broadcast_dir).context("clear broadcast dir before protocol_ops")?;
    }
    fs::create_dir_all(&broadcast_dir).context("create broadcast dir")?;

    let binary = protocol_ops_dir
        .join("target/release/protocol_ops")
        .with_extension(std::env::consts::EXE_EXTENSION);
    let binary_str = binary
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("Binary path contains invalid UTF-8"))?;
    let escaped_args: Vec<String> = args.iter().map(|a| shell_escape(a)).collect();
    let args_str = escaped_args.join(" ");
    let shell_cmd = format!(
        r#"source "$HOME/.nvm/nvm.sh" 2>/dev/null || true; nvm use 2>/dev/null || true; exec {} {}"#,
        shell_escape(binary_str),
        args_str
    );

    let mut cmd = Command::new("bash");
    cmd.arg("-c")
        .arg(&shell_cmd)
        .current_dir(era_contracts_path)
        .env("PROTOCOL_CONTRACTS_ROOT", root_str);

    let start = Instant::now();
    let output = cmd
        .output()
        .with_context(|| format!("Failed to run protocol_ops with args {:?}", args))?;
    let elapsed = start.elapsed();

    log_protocol_ops_command_and_output(
        "local",
        args,
        &format!(
            "cwd={} PROTOCOL_CONTRACTS_ROOT={}",
            era_contracts_path.display(),
            root_str
        ),
        &output,
        elapsed,
    );

    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "protocol_ops {:?} failed with status: {}\n\nSTDOUT:\n{}\n\nSTDERR:\n{}",
            args,
            output.status,
            stdout,
            stderr
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Run protocol_ops in Docker image. Returns stdout on success.
///
/// If any arg contains a `--out` path, the parent directory is mounted into
/// the container and the path is rewritten so the output file lands on the host.
/// The `--l1-rpc-url` is also rewritten from localhost to `host.docker.internal`.
pub fn run_protocol_ops_in_image(image: &str, args: &[&str]) -> anyhow::Result<String> {
    run_protocol_ops_in_image_with_mounts(image, args, &[])
}

/// Like [`run_protocol_ops_in_image`] but with additional volume mounts.
/// Each entry is `(host_path, container_path)`.
pub fn run_protocol_ops_in_image_with_mounts(
    image: &str,
    args: &[&str],
    extra_mounts: &[(&Path, &str)],
) -> anyhow::Result<String> {
    let mut cmd = Command::new("docker");
    cmd.arg("run")
        .arg("--rm")
        .arg("--platform=linux/amd64")
        .arg("--add-host=host.docker.internal:host-gateway");
    for (k, v) in PROTOCOL_OPS_DOCKER_ENV {
        cmd.arg("-e").arg(format!("{}={}", k, v));
    }
    for (host, container) in extra_mounts {
        fs::create_dir_all(host)?;
        let abs = fs::canonicalize(host)?;
        cmd.arg("-v")
            .arg(format!("{}:{}", abs.display(), container));
    }

    // Scan args for --out <path> and --l1-rpc-url <url> to remap for Docker.
    let mut rewritten_args: Vec<String> = Vec::with_capacity(args.len());
    let mut mount_dir: Option<PathBuf> = None;
    let container_out_dir = "/app/out";

    let mut i = 0;
    while i < args.len() {
        if args[i] == "--out" && i + 1 < args.len() {
            let host_path = Path::new(args[i + 1]);
            let abs_path = if host_path.is_absolute() {
                host_path.to_path_buf()
            } else {
                std::env::current_dir()?.join(host_path)
            };
            if let Some(parent) = abs_path.parent() {
                fs::create_dir_all(parent)?;
                mount_dir = Some(fs::canonicalize(parent)?);
            }
            let filename = abs_path.file_name().unwrap_or_default().to_string_lossy();
            rewritten_args.push("--out".to_string());
            rewritten_args.push(format!("{}/{}", container_out_dir, filename));
            i += 2;
            continue;
        }
        if (args[i] == "--l1-rpc-url" || args[i] == "--gateway-rpc-url") && i + 1 < args.len() {
            rewritten_args.push(args[i].to_string());
            rewritten_args.push(
                args[i + 1]
                    .replace("://localhost:", "://host.docker.internal:")
                    .replace("://127.0.0.1:", "://host.docker.internal:"),
            );
            i += 2;
            continue;
        }
        rewritten_args.push(args[i].to_string());
        i += 1;
    }

    if let Some(ref dir) = mount_dir {
        cmd.arg("-v")
            .arg(format!("{}:{}", dir.display(), container_out_dir));
    }

    cmd.arg(image).arg("protocol_ops");
    for a in &rewritten_args {
        cmd.arg(a);
    }

    let start = Instant::now();
    let output = cmd.output().with_context(|| {
        format!(
            "Failed to run protocol_ops in docker image {} with args {:?}",
            image, args
        )
    })?;
    let elapsed = start.elapsed();

    log_protocol_ops_command_and_output(
        "docker",
        args,
        &format!("image={}", image),
        &output,
        elapsed,
    );

    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "protocol_ops {:?} failed in image {} with status: {}\n\nSTDOUT:\n{}\n\nSTDERR:\n{}",
            args,
            image,
            output.status,
            stdout,
            stderr
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Run protocol_ops using the era-contracts source configured by the given preset.
/// Returns stdout on success.
///
/// - `RepoRef::Path` runs protocol_ops from local source.
/// - `RepoRef::DockerTag` runs protocol_ops from `era-contracts:<tag>` Docker image.
pub fn run_protocol_ops_for_preset(preset: &Preset, args: &[&str]) -> anyhow::Result<String> {
    run_protocol_ops_for_preset_with_mounts(preset, args, &[])
}

/// Like [`run_protocol_ops_for_preset`] but with additional Docker volume mounts.
/// Each entry is `(host_path, container_path)`. Mounts are ignored in local mode.
pub fn run_protocol_ops_for_preset_with_mounts(
    preset: &Preset,
    args: &[&str],
    extra_mounts: &[(&Path, &str)],
) -> anyhow::Result<String> {
    match &preset.era_contracts {
        RepoRef::Path(path) => run_protocol_ops_local(path, args),
        RepoRef::DockerTag { tag, .. } => {
            let image = format!("{}:{}", ERA_CONTRACTS_PROTOCOL_IMAGE_REPO, tag);
            run_protocol_ops_in_image_with_mounts(&image, args, extra_mounts)
        }
    }
}

/// Run an arbitrary command inside the era-contracts Docker image.
///
/// `host_work_dir` is mounted at `/app/work` inside the container.
/// `rpc_url` localhost references are rewritten to `host.docker.internal`.
/// The command runs with working directory `/contracts` (where the image
/// has the full era-contracts tree with pre-compiled artifacts).
pub fn run_in_era_image(
    image: &str,
    command: &[&str],
    host_work_dir: Option<&Path>,
    envs: &[(&str, &str)],
) -> anyhow::Result<String> {
    run_in_era_image_ext(image, command, host_work_dir, envs, None, None)
}

/// Run a command in the era-contracts Docker image with extra options.
///
/// `container_workdir` overrides the working directory inside the container.
/// `container_mount_path` overrides where `host_work_dir` is mounted
/// (default `/app/work`).
pub fn run_in_era_image_ext(
    image: &str,
    command: &[&str],
    host_work_dir: Option<&Path>,
    envs: &[(&str, &str)],
    container_workdir: Option<&str>,
    container_mount_path: Option<&str>,
) -> anyhow::Result<String> {
    let mount_target = container_mount_path.unwrap_or("/app/work");
    let mut cmd = Command::new("docker");
    cmd.arg("run")
        .arg("--rm")
        .arg("--platform=linux/amd64")
        .arg("--add-host=host.docker.internal:host-gateway");
    for (k, v) in PROTOCOL_OPS_DOCKER_ENV {
        cmd.arg("-e").arg(format!("{}={}", k, v));
    }
    for (k, v) in envs {
        let v = v
            .replace("://localhost:", "://host.docker.internal:")
            .replace("://127.0.0.1:", "://host.docker.internal:");
        cmd.arg("-e").arg(format!("{}={}", k, v));
    }
    if let Some(dir) = host_work_dir {
        fs::create_dir_all(dir)?;
        let abs = fs::canonicalize(dir)?;
        cmd.arg("-v")
            .arg(format!("{}:{}", abs.display(), mount_target));
    }
    if let Some(wd) = container_workdir {
        cmd.arg("-w").arg(wd);
    }
    cmd.arg(image);
    cmd.args(command);

    let start = Instant::now();
    let output = cmd
        .output()
        .with_context(|| format!("Failed to run {:?} in image {}", command, image))?;
    let elapsed = start.elapsed();

    log_protocol_ops_command_and_output(
        "docker-cmd",
        command,
        &format!("image={}", image),
        &output,
        elapsed,
    );

    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "{:?} failed in image {} with status: {}\n\nSTDOUT:\n{}\n\nSTDERR:\n{}",
            command,
            image,
            output.status,
            stdout,
            stderr
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Run a forge script against era-contracts, locally or in Docker.
///
/// `forge_args` are the arguments after `forge script` (script path, --sig, etc.).
/// `rpc_url` is automatically remapped for Docker.
/// `host_work_dir` is mounted at `/app/work` for Docker runs (used for file I/O
/// between steps). For local runs, this parameter is ignored.
pub fn run_forge_for_preset(
    preset: &Preset,
    forge_args: &[&str],
    host_work_dir: Option<&Path>,
    extra_envs: &[(&str, &str)],
) -> anyhow::Result<String> {
    run_forge_for_preset_with_mount(preset, forge_args, host_work_dir, extra_envs, None)
}

/// Like [`run_forge_for_preset`] but allows overriding the container mount path
/// (default `/app/work`). Use e.g. `/contracts/l1-contracts/script-out` to write
/// into a foundry-allowed directory.
pub fn run_forge_for_preset_with_mount(
    preset: &Preset,
    forge_args: &[&str],
    host_work_dir: Option<&Path>,
    extra_envs: &[(&str, &str)],
    container_mount_path: Option<&str>,
) -> anyhow::Result<String> {
    match &preset.era_contracts {
        RepoRef::Path(era_path) => {
            let l1_contracts = era_path.join("l1-contracts");
            let mut cmd = Command::new("forge");
            cmd.current_dir(&l1_contracts).arg("script");
            for (k, v) in extra_envs {
                cmd.env(k, v);
            }
            // Rewrite --rpc-url values (no remapping needed for local)
            for arg in forge_args {
                cmd.arg(arg);
            }
            let start = Instant::now();
            let output = cmd.output().context("forge script")?;
            let elapsed = start.elapsed();
            log_protocol_ops_command_and_output(
                "forge-local",
                forge_args,
                &format!("cwd={}", l1_contracts.display()),
                &output,
                elapsed,
            );
            if !output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                anyhow::bail!(
                    "forge {:?} failed:\n\nSTDOUT:\n{}\n\nSTDERR:\n{}",
                    forge_args,
                    stdout,
                    stderr
                );
            }
            Ok(String::from_utf8_lossy(&output.stdout).to_string())
        }
        RepoRef::DockerTag { tag, .. } => {
            let image = format!("{}:{}", ERA_CONTRACTS_PROTOCOL_IMAGE_REPO, tag);
            // Build the full command: forge script <args> with rpc-url remapped
            let mut command: Vec<String> = vec!["forge".into(), "script".into()];
            for arg in forge_args {
                let remapped = arg
                    .replace("://localhost:", "://host.docker.internal:")
                    .replace("://127.0.0.1:", "://host.docker.internal:");
                command.push(remapped);
            }
            let cmd_refs: Vec<&str> = command.iter().map(|s| s.as_str()).collect();
            run_in_era_image_ext(
                &image,
                &cmd_refs,
                host_work_dir,
                extra_envs,
                Some("/contracts/l1-contracts"),
                container_mount_path,
            )
        }
    }
}

/// Run a `cast` command using the host binary (local) or the Docker image (DockerTag).
pub fn run_cast_for_preset(preset: &Preset, cast_args: &[&str]) -> anyhow::Result<String> {
    match &preset.era_contracts {
        RepoRef::Path(_) => {
            let mut cmd = Command::new("cast");
            cmd.args(cast_args);
            let output = cmd.output().context("cast")?;
            if !output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                anyhow::bail!(
                    "cast {:?} failed:\n\nSTDOUT:\n{}\n\nSTDERR:\n{}",
                    cast_args,
                    stdout,
                    stderr
                );
            }
            Ok(String::from_utf8_lossy(&output.stdout).to_string())
        }
        RepoRef::DockerTag { tag, .. } => {
            let image = format!("{}:{}", ERA_CONTRACTS_PROTOCOL_IMAGE_REPO, tag);
            let mut command: Vec<String> = vec!["cast".into()];
            for arg in cast_args {
                let remapped = arg
                    .replace("://localhost:", "://host.docker.internal:")
                    .replace("://127.0.0.1:", "://host.docker.internal:");
                command.push(remapped);
            }
            let cmd_refs: Vec<&str> = command.iter().map(|s| s.as_str()).collect();
            run_in_era_image(&image, &cmd_refs, None, &[])
        }
    }
}

/// Execute transactions from a protocol-ops --out file by calling protocol_ops
/// `chain execute-simulated-transactions`. Uses local or Docker mode depending
/// on the preset's era_contracts ref.
pub fn run_execute_protocol_ops_out(
    preset: &Preset,
    out_path: &Path,
    l1_rpc_url: &str,
    private_key: &str,
) -> anyhow::Result<()> {
    let out_path_abs = std::fs::canonicalize(out_path)
        .with_context(|| format!("Failed to canonicalize out path: {}", out_path.display()))?;
    let out_str = out_path_abs
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("Out path contains invalid UTF-8"))?;
    let args = [
        "chain",
        "execute-simulated-transactions",
        "--out",
        out_str,
        "--l1-rpc-url",
        l1_rpc_url,
        "--private-key",
        private_key,
    ];
    run_protocol_ops_for_preset(preset, &args).map(|_| ())
}
