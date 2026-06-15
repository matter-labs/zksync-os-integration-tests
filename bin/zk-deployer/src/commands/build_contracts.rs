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
}

/// Run `yarn install --frozen-lockfile` in `dir` if `node_modules/` is absent.
///
/// Forge deployment scripts invoke node scripts via FFI (e.g. `blake2s256.js`).
/// Those scripts require npm dependencies that are not committed to the repo.
/// No-op when `node_modules/` already exists (fast path for repeat runs).
fn yarn_install_if_needed(dir: &std::path::Path) -> Result<()> {
    if dir.join("node_modules").exists() {
        return Ok(());
    }
    eprintln!(
        "zk-deployer: installing yarn dependencies in {} ...",
        dir.display()
    );
    let status = std::process::Command::new("yarn")
        .arg("install")
        .arg("--frozen-lockfile")
        .current_dir(dir)
        .status()
        .with_context(|| {
            format!(
                "failed to spawn `yarn install` in {} — is yarn installed?",
                dir.display()
            )
        })?;
    anyhow::ensure!(
        status.success(),
        "`yarn install` failed in {} (exit {})",
        dir.display(),
        status.code().unwrap_or(-1)
    );
    Ok(())
}

pub async fn run(args: DevBuildContractsArgs) -> Result<()> {
    let root = contracts_root();

    let mut dirs = vec![root.join("l1-contracts"), root.join("da-contracts")];
    if args.with_l2 {
        dirs.push(root.join("l2-contracts"));
    }

    for dir in &dirs {
        if !dir.exists() {
            bail!(
                "Directory not found: {}\n\
                 Ensure PROTOCOL_CONTRACTS_ROOT points to the protocol-contracts repository root.",
                dir.display()
            );
        }
        yarn_install_if_needed(dir)?;
        preflight::forge_build(dir)?;
    }

    logger::success("All contract artifacts built.");
    Ok(())
}
