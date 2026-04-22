use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use anyhow::Context;
use chrono::Local;

use crate::server::get_run_id;
use crate::utils::find_project_root;

pub mod contracts_backend;

pub use contracts_backend::{
    EraContractsBackend, SafeBundleEntry, SafeBundles, CONTAINER_L1_STATE_CACHE_MOUNT,
};

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
pub struct ContractsContainerSession {
    container_name: String,
    /// Container-side work directory (e.g. `/contracts/work/{name}`).
    container_work_dir: String,
}

impl ContractsContainerSession {
    /// Start a detached container with `work_dir` mounted at `container_work_dir`
    /// and `work_dir/script-out` mounted at `/contracts/l1-contracts/script-out`
    /// (for forge `fs_permissions`). Additional mounts may be passed.
    ///
    /// `mount_root` / `container_mount_root` is a stable, pre-existing directory
    /// (typically `test-run-logs/`) that is bind-mounted once.  `work_dir` must
    /// live somewhere beneath `mount_root`.  Sub-directories are created *inside*
    /// the container after startup so that Docker never needs to bind-mount a
    /// freshly-created host path (works around a macOS Docker/VirtioFS bug where
    /// newly created directories are invisible to the VM).
    pub fn start(
        image: &str,
        work_dir: &Path,
        container_work_dir: &str,
        mount_root: &Path,
        container_mount_root: &str,
        extra_mounts: &[(&Path, &str)],
    ) -> anyhow::Result<Self> {
        // Ensure host dirs exist (for reading outputs back later).
        let script_out = work_dir.join("script-out");
        fs::create_dir_all(&script_out)?;
        let abs_mount_root = fs::canonicalize(mount_root)?;

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
        // Single stable bind-mount: mount_root → container_mount_root.
        // Docker can always see this directory because it existed before
        // the current process created any new sub-directories.
        cmd.arg("-v").arg(format!(
            "{}:{}",
            abs_mount_root.display(),
            container_mount_root
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
                "failed to start contracts container session:\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let session = Self {
            container_name: name,
            container_work_dir: container_work_dir.to_string(),
        };

        // Create the work directory and script-out symlink inside the
        // container.  The directories appear on the host via the bind-mount.
        let container_script_out = format!("{}/script-out", container_work_dir);
        session
            .exec(&["mkdir", "-p", &container_script_out], &[], None)
            .context("mkdir work_dir inside container")?;
        session
            .exec(
                &[
                    "ln",
                    "-sfn",
                    &container_script_out,
                    "/contracts/l1-contracts/script-out",
                ],
                &[],
                None,
            )
            .context("symlink script-out inside container")?;

        Ok(session)
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
            let v = remap_localhost_url(v);
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
    /// Rewrites `--l1-rpc-url` and `--gateway-rpc-url` values so `localhost` /
    /// `127.0.0.1` resolve to the Docker host. Path arguments (`--out`,
    /// `--safe-file`, …) are passed through verbatim; callers must build them
    /// with `EraContractsBackend::work_path` so they already refer to
    /// container-visible paths.
    pub fn protocol_ops(&self, args: &[&str]) -> anyhow::Result<String> {
        let mut rewritten: Vec<String> = Vec::with_capacity(args.len());
        let mut i = 0;
        while i < args.len() {
            if (args[i] == "--l1-rpc-url" || args[i] == "--gateway-rpc-url") && i + 1 < args.len() {
                rewritten.push(args[i].to_string());
                rewritten.push(remap_localhost_url(args[i + 1]));
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
            command.push(remap_localhost_url(arg));
        }
        let cmd_refs: Vec<&str> = command.iter().map(|s| s.as_str()).collect();
        self.exec(&cmd_refs, extra_envs, Some("/contracts/l1-contracts"))
    }

    /// Run `cast <args>` inside the container.
    pub fn cast(&self, args: &[&str]) -> anyhow::Result<String> {
        let mut command: Vec<String> = vec!["cast".into()];
        for arg in args {
            command.push(remap_localhost_url(arg));
        }
        let cmd_refs: Vec<&str> = command.iter().map(|s| s.as_str()).collect();
        self.exec(&cmd_refs, &[], None)
    }
}

/// Rewrite `localhost` / `127.0.0.1` URLs so they resolve from inside a
/// Docker container reaching the host. Values without those prefixes pass
/// through unchanged.
///
/// Callers must decide *when* this remap is appropriate (e.g. unconditionally
/// when executing inside `ContractsContainerSession`, or gated on
/// `/.dockerenv` when spawning a native child that inherits an in-container
/// network namespace).
pub fn remap_localhost_url(url: &str) -> String {
    url.replace("://localhost:", "://host.docker.internal:")
        .replace("://127.0.0.1:", "://host.docker.internal:")
}

impl Drop for ContractsContainerSession {
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
    // Print a concise summary to stdout:
    // - path-taking flags (`--safe-file`, `--out`) collapse to just the
    //   basename (the flag label and directory noise are dropped)
    // - `--l1-rpc-url` and `--private-key` values are dropped entirely —
    //   they're noise for traceability and shouldn't leak into logs
    // - everything else is kept verbatim
    let cmd_summary: String = {
        let mut parts: Vec<String> = Vec::new();
        let mut i = 0;
        while i < args.len() {
            match args[i] {
                "--safe-file" | "--out" if i + 1 < args.len() => {
                    let path = std::path::Path::new(args[i + 1]);
                    let name = path
                        .file_name()
                        .and_then(|f| f.to_str())
                        .unwrap_or(args[i + 1]);
                    parts.push(name.to_string());
                    i += 2;
                }
                "--l1-rpc-url" | "--private-key" if i + 1 < args.len() => {
                    i += 2;
                }
                other => {
                    parts.push(other.to_string());
                    i += 1;
                }
            }
        }
        parts.join(" ")
    };
    let status = if output.status.success() {
        "ok"
    } else {
        "FAILED"
    };
    // `dev execute-safe` runs once per bundle during chain init/upgrades —
    // the per-bundle stdout line is noise for successful runs. Callers print
    // their own aggregate timing via `SafeBundles::apply`. Failures still
    // print so the failing bundle is identifiable.
    let suppress_stdout = output.status.success()
        && matches!(
            (args.first().copied(), args.get(1).copied()),
            (Some("dev"), Some("execute-safe"))
        );
    if !suppress_stdout {
        println!(
            "  [{mode}] {cmd_summary} ... {status} ({:.1}s)",
            elapsed.as_secs_f64()
        );
    }

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

    // `integration-tests/build.rs` builds the `protocol_ops` binary at
    // cargo compile time, so by the time we get here it is up-to-date.
    // No per-invocation cargo fingerprint check.
    let _ = manifest_str;
    let binary = protocol_ops_dir
        .join("target/release/protocol_ops")
        .with_extension(std::env::consts::EXE_EXTENSION);
    anyhow::ensure!(
        binary.exists(),
        "protocol_ops binary not found at {}. \
         Run `cargo build --tests` in integration-tests — the binary is \
         produced by integration-tests/build.rs.",
        binary.display(),
    );

    let broadcast_dir = era_contracts_path.join("l1-contracts/broadcast");
    if broadcast_dir.exists() {
        fs::remove_dir_all(&broadcast_dir).context("clear broadcast dir before protocol_ops")?;
    }
    fs::create_dir_all(&broadcast_dir).context("create broadcast dir")?;

    let binary_str = binary
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("Binary path contains invalid UTF-8"))?;

    // Subcommands that are pure Rust (no Node/TS tooling underneath) skip the
    // `bash -c 'source nvm.sh; nvm use'` prologue and exec the binary
    // directly. `source nvm.sh; nvm use` adds ~100–200ms per call, which is
    // significant for hot paths like `dev execute-safe` (called once per
    // Safe bundle during chain init).
    let skip_nvm_shim = matches!(
        (args.first().copied(), args.get(1).copied()),
        (Some("dev"), Some("execute-safe"))
    );

    let mut cmd = if skip_nvm_shim {
        let mut c = Command::new(binary_str);
        c.args(args);
        c
    } else {
        let escaped_args: Vec<String> = args.iter().map(|a| shell_escape(a)).collect();
        let args_str = escaped_args.join(" ");
        let shell_cmd = format!(
            r#"source "$HOME/.nvm/nvm.sh" 2>/dev/null || true; nvm use 2>/dev/null || true; exec {} {}"#,
            shell_escape(binary_str),
            args_str
        );
        let mut c = Command::new("bash");
        c.arg("-c").arg(&shell_cmd);
        c
    };
    cmd.current_dir(era_contracts_path)
        .env("PROTOCOL_CONTRACTS_ROOT", root_str)
        // protocol_ops shells out to forge; keep forge from hitting
        // binaries.soliditylang.org for version checks (matches local
        // forge / cast invocations in EraContractsBackend).
        .env("FOUNDRY_OFFLINE", "true");

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
