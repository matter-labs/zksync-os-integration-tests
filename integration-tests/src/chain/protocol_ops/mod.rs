use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use anyhow::Context;
use chrono::Local;

use crate::server::{get_run_id, read_toolchain_from_dir};
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

fn protocol_ops_log_path() -> Option<PathBuf> {
    let run_name = get_run_id()?;
    let project_root = find_project_root().ok()?;
    let logs_dir = project_root.join("test-run-logs").join(run_name);
    std::fs::create_dir_all(&logs_dir).ok()?;
    Some(logs_dir.join(PROTOCOL_OPS_COMMANDS_LOG))
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

    let Some(log_path) = protocol_ops_log_path() else {
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
