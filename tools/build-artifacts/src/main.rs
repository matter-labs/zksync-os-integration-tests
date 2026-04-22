//! Compile every contract + Rust binary that the integration tests load at
//! runtime, for the subset of presets selected via `--preset` (or all presets
//! if none is specified).
//!
//! Invoked by `run-tests.sh` before each preset's tests so edits to
//! era-contracts Solidity (`.sol` → `zkout/` via `yarn build-all-contracts`)
//! and to the Rust tools / server (`cargo build --release` in each project)
//! actually reach the test run. This replaces the build logic that used to
//! live in `integration-tests/build.rs`, which was invisibly broken: cargo
//! only re-runs `build.rs` when its own rerun-if-changed inputs change, so
//! edits to external source trees (era-contracts, zksync-os-server) never
//! triggered a rebuild and stale artifacts silently shipped into tests.
//!
//! The tool is dumb: it always invokes `yarn build-all-contracts` and
//! `cargo build --release` for each local path it finds. yarn/forge/cargo
//! already do their own incremental checks, so the no-op cost of running
//! this unconditionally is negligible and we avoid re-inventing change
//! detection.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use clap::Parser;

#[derive(Parser)]
#[command(
    name = "build-artifacts",
    about = "Build era-contracts artifacts + Rust binaries for integration tests"
)]
struct Args {
    /// Build artifacts only for this preset. If omitted, build for every
    /// preset in the file.
    #[arg(long)]
    preset: Option<String>,

    /// Presets file path (relative to CWD if not absolute).
    #[arg(long, default_value = "presets.yaml")]
    presets: PathBuf,
}

fn main() -> Result<()> {
    let args = Args::parse();

    let project_root = std::env::current_dir().context("get cwd")?;
    let presets_path = if args.presets.is_absolute() {
        args.presets.clone()
    } else {
        project_root.join(&args.presets)
    };

    let yaml = std::fs::read_to_string(&presets_path)
        .with_context(|| format!("read {}", presets_path.display()))?;

    let (era_paths, server_paths) =
        collect_local_paths(&yaml, &project_root, args.preset.as_deref())?;

    if era_paths.is_empty() && server_paths.is_empty() {
        eprintln!(
            "build-artifacts: no local era-contracts / zksync-os-server paths to build \
             (preset={:?})",
            args.preset
        );
        return Ok(());
    }

    for era in &era_paths {
        eprintln!(
            "build-artifacts: building era-contracts in {}",
            era.display()
        );
        build_era(era)?;
    }
    for server in &server_paths {
        eprintln!("build-artifacts: building server in {}", server.display());
        cargo_build_release(server)?;
    }

    Ok(())
}

fn collect_local_paths(
    yaml: &str,
    project_root: &Path,
    preset_filter: Option<&str>,
) -> Result<(BTreeSet<PathBuf>, BTreeSet<PathBuf>)> {
    let mut era = BTreeSet::new();
    let mut server = BTreeSet::new();

    let parsed: serde_yaml::Value = serde_yaml::from_str(yaml).context("parse presets.yaml")?;
    let map = parsed
        .as_mapping()
        .context("presets.yaml root is not a mapping")?;

    for (name, preset) in map {
        let name_str = name.as_str().unwrap_or("");
        if let Some(f) = preset_filter {
            if name_str != f {
                continue;
            }
        }
        if let Some(v) = preset.get("era_contracts").and_then(|v| v.as_str()) {
            if let Some(p) = resolve_local(project_root, v) {
                era.insert(p);
            }
        }
        if let Some(v) = preset.get("zksync_os_server").and_then(|v| v.as_str()) {
            if let Some(p) = resolve_local(project_root, v) {
                server.insert(p);
            }
        }
    }

    if let Some(f) = preset_filter {
        if !map.keys().any(|k| k.as_str() == Some(f)) {
            anyhow::bail!("preset {f:?} not found in presets file");
        }
    }

    Ok((era, server))
}

fn resolve_local(project_root: &Path, value: &str) -> Option<PathBuf> {
    let p = Path::new(value);
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        project_root.join(p)
    };
    if abs.is_dir() {
        Some(std::fs::canonicalize(&abs).unwrap_or(abs))
    } else {
        None
    }
}

fn build_era(era: &Path) -> Result<()> {
    if !era.join("package.json").exists() {
        eprintln!(
            "build-artifacts: no package.json at {}; skipping yarn build",
            era.display()
        );
    } else {
        let mut cmd = Command::new("yarn");
        cmd.arg("build-all-contracts").current_dir(era);
        scrub_outer_cargo_env(&mut cmd);
        pipe_to_tty(&mut cmd);
        run(&mut cmd).with_context(|| format!("yarn build-all-contracts in {}", era.display()))?;
    }

    for sub in [
        "protocol-ops",
        "tools/zksync-os-genesis-gen",
        "tools/wallets-gen",
    ] {
        let dir = era.join(sub);
        if dir.join("Cargo.toml").exists() {
            cargo_build_release(&dir)?;
        }
    }
    Ok(())
}

fn cargo_build_release(dir: &Path) -> Result<()> {
    // Honour the project's pinned toolchain explicitly via
    // RUSTUP_TOOLCHAIN so the nested build doesn't inherit whatever rustup
    // default the caller's shell is using.
    let channel = read_toolchain_channel(dir);

    let mut cmd = Command::new("cargo");
    cmd.arg("build").arg("--release").current_dir(dir);
    scrub_outer_cargo_env(&mut cmd);
    if let Some(ref c) = channel {
        cmd.env("RUSTUP_TOOLCHAIN", c);
    }
    pipe_to_tty(&mut cmd);

    run(&mut cmd).with_context(|| {
        format!(
            "cargo build --release in {} (toolchain: {})",
            dir.display(),
            channel.as_deref().unwrap_or("<rustup default>")
        )
    })
}

/// Cargo bleeds dozens of env vars into build-script children (`CARGO_*`,
/// `RUSTC`, `RUSTFLAGS`, …). For nested `cargo build` invocations those vars
/// pin the inner build to the outer toolchain and produce confusing
/// `unresolved import std::assert_matches` / `unknown feature` errors when
/// the nested project expects a different nightly. Scrub them; keep
/// `CARGO_HOME` (shared download cache) and the user's PATH/RUSTUP_HOME.
fn scrub_outer_cargo_env(cmd: &mut Command) {
    const DROP_EXACT: &[&str] = &[
        "RUSTC",
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
        "RUSTC_LINKER",
        "RUSTC_BOOTSTRAP",
        "RUSTDOC",
        "RUSTDOCFLAGS",
        "RUSTFLAGS",
        "RUSTUP_TOOLCHAIN",
        "OUT_DIR",
        "NUM_JOBS",
        "HOST",
        "TARGET",
        "PROFILE",
        "OPT_LEVEL",
        "DEBUG",
        "LD_LIBRARY_PATH_FOR_TARGET",
    ];
    for (k, _) in std::env::vars() {
        let keep_cargo = k == "CARGO_HOME";
        let is_cargo = k.starts_with("CARGO_") || k == "CARGO";
        if (is_cargo && !keep_cargo) || DROP_EXACT.contains(&k.as_str()) {
            cmd.env_remove(&k);
        }
    }
}

/// When a controlling tty is available, wire stdout/stderr directly to it
/// so subprocess progress stays visible under test runners that capture
/// their own stdout. Falls back to inherited streams in CI.
fn pipe_to_tty(cmd: &mut Command) {
    if let Ok(tty) = std::fs::OpenOptions::new().write(true).open("/dev/tty") {
        if let Ok(tty_err) = tty.try_clone() {
            cmd.stdout(Stdio::from(tty));
            cmd.stderr(Stdio::from(tty_err));
        }
    }
}

fn run(cmd: &mut Command) -> Result<()> {
    let status = cmd.status().with_context(|| format!("spawn {cmd:?}"))?;
    if !status.success() {
        anyhow::bail!("command failed (exit {status}): {cmd:?}");
    }
    Ok(())
}

fn read_toolchain_channel(dir: &Path) -> Option<String> {
    let toml_path = dir.join("rust-toolchain.toml");
    if toml_path.exists() {
        let content = std::fs::read_to_string(&toml_path).ok()?;
        for line in content.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("channel") {
                let channel = rest
                    .trim()
                    .trim_start_matches('=')
                    .trim()
                    .trim_matches('"')
                    .trim_matches('\'')
                    .trim();
                if !channel.is_empty() {
                    return Some(channel.to_string());
                }
            }
        }
    }
    let legacy = dir.join("rust-toolchain");
    if legacy.exists() {
        let content = std::fs::read_to_string(&legacy).ok()?;
        let trimmed = content.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    None
}
