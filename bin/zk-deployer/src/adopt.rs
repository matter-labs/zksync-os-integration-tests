//! Adopting an ecosystem that was deployed elsewhere.
//!
//! `bootstrap` writes the ecosystem's addresses into `state.json` as it deploys them, and
//! everything downstream (`apply`, `server-config`, [`crate::deployed::DeployedEcosystem`]) reads
//! them back from there. A caller that did not run `bootstrap` — a test that restored a frozen
//! chain, or an operator adding a chain to a live ecosystem — has the same addresses in hand but
//! no journal to put them in. This writes that one entry, so the rest of the deployer works
//! unchanged against an ecosystem it did not create.

use std::path::Path;

use alloy::primitives::Address;
use anyhow::{Context, Result};

use crate::state::{EcosystemInitOutput, State, StepKey};

/// The ecosystem-level addresses `apply` needs to register a chain.
#[derive(Debug, Clone)]
pub struct ExistingEcosystem {
    pub bridgehub: Address,
    pub ctm: Address,
    pub governance: Address,
    /// `None` on ecosystems predating the supplier; chain init then publishes bytecodes itself.
    pub bytecodes_supplier: Option<Address>,
    pub rollup_l1_da_validator: Address,
    pub no_da_l1_validator: Address,
    pub avail_l1_da_validator: Address,
    /// ZKsync OS blob DA validator; `None` when the ecosystem has none.
    pub blobs_zksync_os_l1_da_validator: Option<Address>,
}

/// Seed `state_path` with `eco` so `apply` can run against it.
pub fn seed_state(state_path: &Path, eco: ExistingEcosystem) -> Result<()> {
    let mut state = State::load_or_new(state_path)?;
    state.mark_done(
        StepKey::EcosystemInit,
        &EcosystemInitOutput {
            bridgehub_proxy: eco.bridgehub,
            ctm_proxy: eco.ctm,
            bytecodes_supplier: eco.bytecodes_supplier,
            rollup_l1_da_validator: eco.rollup_l1_da_validator,
            no_da_l1_validator: eco.no_da_l1_validator,
            avail_l1_da_validator: eco.avail_l1_da_validator,
            blobs_zksync_os_l1_da_validator: eco.blobs_zksync_os_l1_da_validator,
            governance: eco.governance,
        },
    )?;
    state
        .save(state_path)
        .with_context(|| format!("write {}", state_path.display()))
}
