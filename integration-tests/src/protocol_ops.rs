use std::path::Path;
use std::process::Command;

use anyhow::Context;

use crate::presets::{Preset, RepoRef};

const ERA_CONTRACTS_PROTOCOL_IMAGE_REPO: &str =
    "us-docker.pkg.dev/matterlabs-infra/matterlabs-docker/era-contracts";

/// Env vars for protocol_ops Docker runs. Telemetry must be enabled for foundry to work
/// (Dockerfile sets ~/.config/zksync-tooling/telemetry.json {"enabled":true}).
const PROTOCOL_OPS_DOCKER_ENV: &[(&str, &str)] = &[("FOUNDRY_DISABLE_NIGHTLY_WARNING", "1")];

/// Escape a string for use in a single-quoted shell argument.
fn shell_escape(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', "'\\''"))
}

/// Run protocol_ops locally (not in Docker) against the given era-contracts path.
/// Uses PROTOCOL_CONTRACTS_ROOT so protocol_ops finds contracts in that directory.
/// Runs `nvm use` before protocol_ops so the correct Node version (from .nvmrc) is active.
pub fn run_protocol_ops_local(era_contracts_path: &Path, args: &[&str]) -> anyhow::Result<()> {
    let protocol_ops_manifest = era_contracts_path.join("protocol-ops").join("Cargo.toml");
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

    let escaped_args: Vec<String> = args.iter().map(|a| shell_escape(a)).collect();
    let args_str = escaped_args.join(" ");
    let manifest_str = protocol_ops_manifest
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("Manifest path contains invalid UTF-8"))?;

    // Run in bash so we can source nvm and run nvm use before cargo. protocol_ops spawns
    // forge/yarn which need the correct Node version from .nvmrc.
    let shell_cmd = format!(
        r#"source "$HOME/.nvm/nvm.sh" 2>/dev/null || true; nvm use 2>/dev/null || true; exec cargo run --manifest-path {} --release -- {}"#,
        shell_escape(manifest_str),
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
