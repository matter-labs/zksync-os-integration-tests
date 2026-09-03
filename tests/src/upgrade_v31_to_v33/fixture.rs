//! The frozen v31.1 ecosystem: anvil restored from a committed L1 state plus one in-process
//! zksync-os-server per chain.
//!
//! Two chains, so an upgrade can be exercised on both shapes a v31 chain comes in — and so the
//! upgraded ecosystem has the two interop-capable chains an atomic swap needs:
//!
//! | chain | shape | DA registration | pricing |
//! |---|---|---|---|
//! | [`ROLLUP_CHAIN_ID`] | rollup | blobs validator, `BlobsZKSyncOS` | `Rollup` |
//! | [`VALIDIUM_CHAIN_ID`] | validium | no-DA validator, `EmptyNoDA` | `Validium` |
//!
//! Brought up through the generic [`crate::fixtures::restore`] path — the committed
//! `server-<id>.yaml` files are the source of truth. The ecosystem is owned by a Governance
//! contract whose owner is anvil account #0; that account is the funded EOA the ecosystem-level
//! runbook steps sign with, while each chain's ChainAdmin has its own owner.
//!
//! `versions.yaml` records what the snapshot was generated from; `tests/README.md` describes how
//! to regenerate and how to check the result.
//!
//! The upgrade-runbook steps live in [`super::protocol`], the whole runbook in [`super::runbook`].

use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::ecosystem::Ecosystem;

/// Anvil account #0 — owner of the fixture's Governance contract and the only
/// pre-funded EOA in the snapshot, so it signs both the deployer bundle and
/// the governance calls (on a real ecosystem those are different signers).
pub const GOVERNOR_KEY: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
pub const DEPLOYER_KEY: &str = GOVERNOR_KEY;
pub const DEPLOYER_ADDR: &str = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";

/// The rollup chain.
pub const ROLLUP_CHAIN_ID: u64 = 506;
/// The no-DA validium chain.
pub const VALIDIUM_CHAIN_ID: u64 = 507;

/// Owners of the two ChainAdmins (each chain's `owner` wallet in `wallets.yaml`) — the signers for
/// ChainAdmin-targeted bundles, which on this fixture are EOAs distinct from the governance owner.
pub const ROLLUP_CHAIN_ADMIN_OWNER_KEY: &str =
    "0x17c15e218e597be033bed93a4b051dfe42400a06cc568216125085b667fbed40";
pub const VALIDIUM_CHAIN_ADMIN_OWNER_KEY: &str =
    "0x82197630a78960071d4f9b5c1936147e1bd89e83e1cc01d8dc31c377ee1f836c";

/// Every key that may need to sign a bundle emitted for one of the chains.
pub const CHAIN_SIGNING_KEYS: &[&str] = &[
    GOVERNOR_KEY,
    ROLLUP_CHAIN_ADMIN_OWNER_KEY,
    VALIDIUM_CHAIN_ADMIN_OWNER_KEY,
];

/// Upgrade-env input for local anvil fixtures, committed in era-contracts under
/// `l1-contracts/`.
///
/// Value-for-value a copy of `upgrade-envs/v0.33.0-atomic-interop/local.toml`. It exists
/// under its own basename purely to select a different *permanent-values* file:
/// `CoreUpgrade_v33` pairs the two by basename, so this input resolves to
/// `upgrade-envs/permanent-values/zksync-os-integration-test.toml` while `local.toml`
/// would resolve to `upgrade-envs/permanent-values/local.toml`.
///
/// Under the v31 flow that separation was load-bearing: `permanent-values/local.toml`
/// declares `[legacy_gateway] chain_id = 506`, which on this fixture is the chain being
/// upgraded, so stage 2 would have emitted decommission calls against it. v33 has no
/// stage-2 decommission step and never reads that section, so the pairing is now only
/// about keeping the fixture's permanent values addressable on their own.
///
/// The leading `/` is the protocol-ops path convention, not an absolute path:
/// protocol-ops resolves it as `contracts_root/l1-contracts/<path>` after
/// `trim_start_matches('/')`.
pub const UPGRADE_INPUT_PATH: &str =
    "/upgrade-envs/v0.33.0-atomic-interop/zksync-os-integration-test.toml";

/// Start the v31.1 fixture by restoring the committed snapshot.
///
/// Forge-based upgrade steps require compiled contracts, so callers that drive
/// the upgrade must ensure contracts are built (the upgrade test does).
///
/// Returns once the server is up and has produced an initial batch, matching
/// the guarantee made by the standard `ecosystem` fixture.
pub async fn start() -> Result<Ecosystem> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("local-chains/v31.1");
    let eco = crate::fixtures::restore::restore(&dir)
        .await
        .context("restore v31.1")?;
    futures::future::try_join_all(eco.chains().map(|chain| chain.wait_for_block_finalized(1)))
        .await
        .context("wait for initial batch")?;
    Ok(eco)
}
