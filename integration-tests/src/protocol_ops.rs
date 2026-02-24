use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use anyhow::Context;
use chrono::Local;

use crate::presets::{Preset, RepoRef};
use crate::server::get_or_create_run_id;
use crate::utils::find_project_root;

const ERA_CONTRACTS_PROTOCOL_IMAGE_REPO: &str =
    "us-docker.pkg.dev/matterlabs-infra/matterlabs-docker/era-contracts";

/// Env vars for protocol_ops Docker runs. Telemetry must be enabled for foundry to work
/// (Dockerfile sets ~/.config/zksync-tooling/telemetry.json {"enabled":true}).
const PROTOCOL_OPS_DOCKER_ENV: &[(&str, &str)] = &[("FOUNDRY_DISABLE_NIGHTLY_WARNING", "1")];

const PROTOCOL_OPS_COMMANDS_LOG: &str = "protocol_ops_commands.log";

fn protocol_ops_log_path() -> Option<std::path::PathBuf> {
    let project_root = find_project_root().ok()?;
    let run_id = get_or_create_run_id();
    let logs_dir = project_root.join("integration-tests/logs").join(run_id);
    std::fs::create_dir_all(&logs_dir).ok()?;
    Some(logs_dir.join(PROTOCOL_OPS_COMMANDS_LOG))
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
/// Uses PROTOCOL_CONTRACTS_ROOT so protocol_ops finds contracts in that directory.
/// Builds silently first, then runs the binary. Runs `nvm use` before execution so
/// protocol_ops can spawn forge/yarn with the correct Node version from .nvmrc.
pub fn run_protocol_ops_local(era_contracts_path: &Path, args: &[&str]) -> anyhow::Result<()> {
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

    // Step 1: Build silently (suppress compiler warnings and progress)
    let build_status = Command::new("cargo")
        .args(["build", "--release", "--manifest-path", manifest_str])
        .current_dir(era_contracts_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| "Failed to run cargo build for protocol_ops")?;
    if !build_status.success() {
        anyhow::bail!("cargo build --release for protocol_ops failed with status: {}", build_status);
    }

    // Step 2: Run the binary. Use bash + nvm so protocol_ops can spawn forge/yarn.
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
    Ok(())
}

fn run_protocol_ops_in_image(image: &str, args: &[&str]) -> anyhow::Result<()> {
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
    Ok(())
}

/// Run protocol_ops using the era-contracts source configured by the given preset.
///
/// - `RepoRef::Path` runs protocol_ops from local source.
/// - `RepoRef::DockerTag` runs protocol_ops from `era-contracts:<tag>` Docker image.
pub fn run_protocol_ops_for_preset(preset: &Preset, args: &[&str]) -> anyhow::Result<()> {
    match &preset.era_contracts {
        RepoRef::Path(path) => run_protocol_ops_local(path, args),
        RepoRef::DockerTag(tag) => {
            let image = format!("{}:{}", ERA_CONTRACTS_PROTOCOL_IMAGE_REPO, tag);
            run_protocol_ops_in_image(&image, args)
        }
    }
}
