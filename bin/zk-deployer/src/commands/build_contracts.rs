use anyhow::{bail, Context, Result};
use clap::Parser;

use protocol_ops::common::{logger, paths::contracts_root, preflight};

/// Build all Forge contract artifacts required by `bootstrap` and `apply`.
///
/// Runs `forge build` in:
///   - `l1-contracts/`  — required by every forge script
///   - `da-contracts/`  — required by `DeployCTM.s.sol` (EIP7702Checker + RollupL1DAValidator)
///   - `l2-contracts/`  — only when `--with-l2` is passed (not needed for ZKsync OS chains,
///     where L2 system contracts live in genesis)
#[derive(Debug, Clone, Parser)]
pub struct DevBuildContractsArgs {
    /// Also build `l2-contracts/`.
    #[arg(long, default_value = "false")]
    pub with_l2: bool,

    /// Generate and build the standalone ZiSK Plonk verifier.
    #[arg(long, default_value = "false")]
    pub with_zisk: bool,
}

fn run_command(
    dir: &std::path::Path,
    program: &str,
    args: &[&str],
    description: &str,
) -> Result<()> {
    let mut command = std::process::Command::new(program);
    command.args(args).current_dir(dir);
    if program == "cargo" {
        // `cargo run` exports its own override to child processes; removing it
        // lets verifier-gen honor its older, boojum-compatible rust-toolchain.
        command.env_remove("RUSTUP_TOOLCHAIN");
    }
    let status = command
        .status()
        .with_context(|| format!("failed to spawn {description} in {}", dir.display()))?;
    anyhow::ensure!(
        status.success(),
        "{description} failed in {} (exit {})",
        dir.display(),
        status.code().unwrap_or(-1)
    );
    Ok(())
}

fn generate_zisk_verifier(root: &std::path::Path) -> Result<()> {
    let generator = root.join("tools/verifier-gen");
    if !generator.exists() {
        bail!(
            "Directory not found: {}\nEnsure PROTOCOL_CONTRACTS_ROOT points to the protocol-contracts repository root.",
            generator.display()
        );
    }

    logger::step("Generating ZiSK Plonk verifier...");
    run_command(&generator, "npm", &["ci"], "`npm ci`")?;
    run_command(
        &generator,
        "node",
        &[
            "render_plonk_verifier.js",
            "data/ZiSK_plonk_verification_key.json",
            "data/PlonkVerifier.sol",
        ],
        "ZiSK snarkJS verifier rendering",
    )?;
    run_command(
        &generator,
        "cargo",
        &[
            "run",
            "--",
            "--variant",
            "zisk",
            "--zisk_vk_path",
            "data/ZiSK_vk.json",
            "--zisk_output_path",
            "../../l1-contracts/contracts/state-transition/verifiers/ZiskVerifier.sol",
            "--zisk_plonk_input_path",
            "data/PlonkVerifier.sol",
        ],
        "ZiSK verifier generation",
    )
}

/// Run `yarn install --frozen-lockfile` at the workspace root if `node_modules/` is absent.
///
/// Forge deployment scripts invoke node scripts via FFI (e.g. `blake2s256.js`).
/// Those scripts require npm dependencies that are not committed to the repo.
/// No-op when `node_modules/` already exists (fast path for repeat runs).
fn yarn_install_if_needed(root: &std::path::Path) -> Result<()> {
    if root.join("node_modules").exists() {
        return Ok(());
    }
    eprintln!(
        "zk-deployer: installing yarn dependencies in {} ...",
        root.display()
    );
    let status = std::process::Command::new("yarn")
        .arg("install")
        .arg("--frozen-lockfile")
        .current_dir(root)
        .status()
        .with_context(|| {
            format!(
                "failed to spawn `yarn install` in {} — is yarn installed?",
                root.display()
            )
        })?;
    anyhow::ensure!(
        status.success(),
        "`yarn install` failed in {} (exit {})",
        root.display(),
        status.code().unwrap_or(-1)
    );
    Ok(())
}

pub async fn run(args: DevBuildContractsArgs) -> Result<()> {
    let root = contracts_root();

    if args.with_zisk {
        generate_zisk_verifier(&root)?;
    }

    let mut dirs = vec![root.join("l1-contracts"), root.join("da-contracts")];
    if args.with_l2 {
        dirs.push(root.join("l2-contracts"));
    }

    yarn_install_if_needed(&root)?;
    for dir in &dirs {
        if !dir.exists() {
            bail!(
                "Directory not found: {}\n\
                 Ensure PROTOCOL_CONTRACTS_ROOT points to the protocol-contracts repository root.",
                dir.display()
            );
        }
        preflight::forge_build(dir)?;
    }

    logger::success("All contract artifacts built.");
    Ok(())
}
