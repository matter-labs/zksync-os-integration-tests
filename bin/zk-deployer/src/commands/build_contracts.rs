use anyhow::{bail, Result};
use clap::Parser;

use protocol_ops::common::{logger, paths::contracts_root, preflight};

/// Build all Forge contract artifacts required by `bootstrap` and `apply`.
///
/// Runs `forge build` in:
///   - `l1-contracts/`  — required by every forge script
///   - `da-contracts/`  — required by `DeployCTM.s.sol` (EIP7702Checker + RollupL1DAValidator)
///   - `l2-contracts/`  — only when `--with-l2` is passed (needed when `skip_priority_txs: false`)
///
/// # Example
///
/// ```text
/// # Build prerequisites for a ZKsync OS chain (skip_priority_txs: true):
/// protocol-ops dev build-contracts
///
/// # Build everything including l2-contracts (EraVM chains):
/// protocol-ops dev build-contracts --with-l2
/// ```
#[derive(Debug, Clone, Parser)]
pub struct DevBuildContractsArgs {
    /// Also build `l2-contracts/`. Required when `skip_priority_txs: false`
    /// (EraVM chains). For ZKsync OS chains this is not needed because L2
    /// system contracts live in genesis.
    #[arg(long, default_value = "false")]
    pub with_l2: bool,
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
                 Ensure PROTOCOL_CONTRACTS_ROOT points to the era-contracts repository root.",
                dir.display()
            );
        }
        // Always build unconditionally — the user explicitly asked for a rebuild.
        preflight::forge_build(dir)?;
    }

    logger::success("All contract artifacts built.");
    Ok(())
}
