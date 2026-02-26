use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use anyhow::Context;
use chrono::Local;

use crate::presets::{Preset, RepoRef};
use crate::server::{get_or_create_run_id, read_toolchain_from_dir};
use crate::utils::find_project_root;

const ERA_CONTRACTS_PROTOCOL_IMAGE_REPO: &str =
    "us-docker.pkg.dev/matterlabs-infra/matterlabs-docker/era-contracts";

/// Env vars for protocol_ops Docker runs. Telemetry must be enabled for foundry to work
/// (Dockerfile sets ~/.config/zksync-tooling/telemetry.json {"enabled":true}).
const PROTOCOL_OPS_DOCKER_ENV: &[(&str, &str)] = &[("FOUNDRY_DISABLE_NIGHTLY_WARNING", "1")];

const PROTOCOL_OPS_COMMANDS_LOG: &str = "protocol_ops_commands.log";
const PROTOCOL_OPS_OUT_PREFIX: &str = "protocol_ops";
const PROTOCOL_OPS_OUT_SUFFIX: &str = "_out.json";

fn protocol_ops_log_path() -> Option<PathBuf> {
    let project_root = find_project_root().ok()?;
    let run_id = get_or_create_run_id();
    let logs_dir = project_root.join("integration-tests/logs").join(run_id);
    std::fs::create_dir_all(&logs_dir).ok()?;
    Some(logs_dir.join(PROTOCOL_OPS_COMMANDS_LOG))
}

/// Directory used for protocol_ops logs and out files (integration-tests/logs/<run_id>).
/// Use this when writing or reading protocol_ops --out files from tests so they land in the same run dir.
pub fn protocol_ops_logs_dir() -> Option<PathBuf> {
    protocol_ops_log_path().and_then(|p| p.parent().map(PathBuf::from))
}

/// Parse args for --out=path or --out path; return the path if present.
fn extract_out_path_from_args(args: &[&str]) -> Option<String> {
    let mut i = 0;
    while i < args.len() {
        let a = args[i];
        if let Some(path) = a.strip_prefix("--out=") {
            return Some(path.to_string());
        }
        if a == "--out" && i + 1 < args.len() {
            return Some(args[i + 1].to_string());
        }
        i += 1;
    }
    None
}

fn log_protocol_ops_command_and_output(
    mode: &str,
    args: &[&str],
    extra: &str,
    output: &Output,
) {
    let Some(log_path) = protocol_ops_log_path() else { return };
    let _ = (|| -> anyhow::Result<()> {
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)?;
        let ts = Local::now().format("%Y-%m-%d %H:%M:%S");
        writeln!(f, "\n--- [{}] [{}] ---", ts, mode)?;
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

    let output = cmd
        .output()
        .with_context(|| format!("Failed to run protocol_ops with args {:?}", args))?;

    log_protocol_ops_command_and_output(
        "local",
        args,
        &format!("cwd={} PROTOCOL_CONTRACTS_ROOT={}", era_contracts_path.display(), root_str),
        &output,
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
pub fn run_protocol_ops_in_image(image: &str, args: &[&str]) -> anyhow::Result<String> {
    let mut cmd = Command::new("docker");
    cmd.arg("run")
        .arg("--rm")
        .arg("--platform=linux/amd64")
        .arg("--add-host=host.docker.internal:host-gateway");
    for (k, v) in PROTOCOL_OPS_DOCKER_ENV {
        cmd.arg("-e").arg(format!("{}={}", k, v));
    }
    cmd.arg(image).arg("protocol_ops");
    cmd.args(args);

    let output = cmd.output().with_context(|| {
        format!(
            "Failed to run protocol_ops in docker image {} with args {:?}",
            image, args
        )
    })?;

    log_protocol_ops_command_and_output(
        "docker",
        args,
        &format!("image={}", image),
        &output,
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
    match &preset.era_contracts {
        RepoRef::Path(path) => run_protocol_ops_local(path, args),
        RepoRef::DockerTag(tag) => {
            let image = format!("{}:{}", ERA_CONTRACTS_PROTOCOL_IMAGE_REPO, tag);
            run_protocol_ops_in_image(&image, args)
        }
    }
}
