//! v30.2 frozen-fixture chain: anvil restored from a committed L1 state plus an
//! in-process zksync-os-server running on v30.1.
//!
//! Brought up through the generic [`crate::fixtures::restore`] path — the
//! committed `server.yaml` is the source of truth, and the governor/deployer L1
//! keys are pre-funded in the committed `l1-state` (see the `regen_v30_state`
//! maintenance test). The upgrade-runbook steps live in [`super::protocol`].

use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::ecosystem::Ecosystem;

pub const V30_BYTECODES_SUPPLIER: &str = "0x9f3f32ea83c8a1c8e993fd9035d1d077545467ac";
pub const V30_ROLLUP_DA_MANAGER: &str = "0x748977c82e65d7104e5a076cc65febcb692dbe89";

pub const GOVERNOR_KEY: &str = "0xbde651cec2adffb5f98b137edd1a21f4c736261d4a805a5d4469a46c64a777bb";
pub const DEPLOYER_KEY: &str = "0xdf7f80ad7e7b6fd53a92c1e9f42bf59a3fd91fb3a3ca50305eb2ab18386ac4e1";
pub const DEPLOYER_ADDR: &str = "0x73e8bbb7fc2761c7ac966609e900c021d137ce42";

/// Upgrade-env input for this fixture, committed in era-contracts: same content
/// as `local.toml`, but its permanent-values twin (paired by basename) has no
/// `[legacy_gateway]` section. The v30.2 fixture has no gateway chain, so the
/// stage-2 decommission calls generated from that section would revert with
/// SettlementLayersMustSettleOnL1().
///
/// The leading `/` is the protocol-ops path convention, not an absolute path:
/// protocol-ops resolves it as `contracts_root/l1-contracts/<path>` after
/// `trim_start_matches('/')` (see `v31_upgrade_inner.rs`).
pub const V30_UPGRADE_INPUT_PATH: &str =
    "/upgrade-envs/v0.31.0-interopB/zksync-os-integration-test.toml";

/// Start the v30.2 fixture by restoring the committed snapshot.
///
/// Forge-based upgrade steps require compiled contracts, so callers that drive
/// the upgrade must ensure contracts are built (the upgrade test does).
///
/// Returns once the server is up and has produced an initial batch, matching
/// the guarantee made by the standard `ecosystem` fixture.
pub async fn start() -> Result<Ecosystem> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("local-chains/v30.2");
    let eco = crate::fixtures::restore::restore(&dir)
        .await
        .context("restore v30.2")?;
    // Use wait_for_block_finalized(1) rather than wait_for_batch() to avoid the
    // race where batch 1 finalizes before the call, leaving wait_for_batch()
    // waiting for a subsequent batch that never arrives.
    futures::future::try_join_all(eco.chains().map(|chain| chain.wait_for_block_finalized(1)))
        .await
        .context("wait for initial batch")?;
    Ok(eco)
}
