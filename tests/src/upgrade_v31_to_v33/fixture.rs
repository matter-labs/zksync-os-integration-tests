//! v31.0 frozen-fixture chain: anvil restored from a committed L1 state plus an
//! in-process zksync-os-server running on v31.
//!
//! Brought up through the generic [`crate::fixtures::restore`] path — the
//! committed `server.yaml` is the source of truth. The state was deployed from
//! era-contracts `e09169106` with this repo's zk-deployer (it is the same
//! snapshot zksync-os-server ships as `local-chains/v31.0`), so the ecosystem
//! is owned by a Governance contract whose owner is anvil account #0; that
//! account is the funded EOA every runbook step below signs with.
//!
//! The upgrade-runbook steps live in [`super::protocol`].

use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::ecosystem::Ecosystem;

/// Anvil account #0 — owner of the fixture's Governance contract and the only
/// pre-funded EOA in the snapshot, so it signs both the deployer bundle and
/// the governance calls (on a real ecosystem those are different signers).
pub const GOVERNOR_KEY: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
pub const DEPLOYER_KEY: &str = GOVERNOR_KEY;
pub const DEPLOYER_ADDR: &str = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";

/// Owner of chain 506's ChainAdmin (the chain's `owner` wallet in
/// `wallets.yaml`) — the signer for every ChainAdmin-targeted bundle, which on
/// this fixture is a different EOA from the governance owner above.
pub const CHAIN_ADMIN_OWNER_KEY: &str =
    "0x17c15e218e597be033bed93a4b051dfe42400a06cc568216125085b667fbed40";

/// Upgrade-env input for local anvil fixtures, committed in era-contracts under
/// `l1-contracts/`.
///
/// Value-for-value a copy of `upgrade-envs/v0.31.0-interopB/local.toml` (only
/// the comment headers differ). It exists as a separate file purely to select a
/// different *permanent-values* file: `CoreUpgrade_v31` pairs the two by
/// basename (`_permanentValuesPathFromV31Input`), so this input resolves to
/// `upgrade-envs/permanent-values/zksync-os-integration-test.toml`, where
/// passing `local.toml` would resolve to `upgrade-envs/permanent-values/local.toml`.
///
/// Those two permanent-values files differ in exactly one thing: the `local.toml`
/// one carries a `[legacy_gateway] chain_id = 506` section and this one does not.
/// This fixture never had a gateway chain — and 506 is the chain being upgraded —
/// so the stage-2 decommission calls generated from that section would target the
/// chain itself and revert with `SettlementLayersMustSettleOnL1()`.
///
/// The leading `/` is the protocol-ops path convention, not an absolute path:
/// protocol-ops resolves it as `contracts_root/l1-contracts/<path>` after
/// `trim_start_matches('/')` (see `v31_upgrade_inner.rs`).
pub const V31_UPGRADE_INPUT_PATH: &str =
    "/upgrade-envs/v0.31.0-interopB/zksync-os-integration-test.toml";

/// Start the v31.0 fixture by restoring the committed snapshot.
///
/// Forge-based upgrade steps require compiled contracts, so callers that drive
/// the upgrade must ensure contracts are built (the upgrade test does).
///
/// Returns once the server is up and has produced an initial batch, matching
/// the guarantee made by the standard `ecosystem` fixture.
pub async fn start() -> Result<Ecosystem> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("local-chains/v31.0");
    let eco = crate::fixtures::restore::restore(&dir)
        .await
        .context("restore v31.0")?;
    futures::future::try_join_all(eco.chains().map(|chain| chain.wait_for_block_finalized(1)))
        .await
        .context("wait for initial batch")?;
    Ok(eco)
}
