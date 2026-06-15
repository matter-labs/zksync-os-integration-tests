//! Typed view of a completed `bootstrap` + `apply` workdir.
//!
//! This is the boundary between the deployer and its consumers (the test
//! framework): everything a consumer needs after deployment — L1 addresses,
//! per-chain identities, rendered server configs — is reachable from
//! [`DeployedEcosystem`] without touching `state.json` internals, the wallets
//! file format, or step keys. The same struct doubles as the manifest for
//! snapshot/restore: a saved workdir plus `DeployedEcosystem::load` fully
//! reconstitutes a deployment.

use std::path::Path;

use alloy::primitives::Address;
use anyhow::{Context, Result};

use crate::commands::server_config::{
    render_config, resolve_base_token_addr, resolve_pubdata_mode,
};
use crate::intent::{ChainIntent, IntentConfig};
use crate::state::{ChainInitPreparedOutput, EcosystemInitOutput, State, StepKey};
use protocol_ops::common::wallets::{load_wallets, ChainWallets};

/// One deployed L1-settling chain.
pub struct DeployedChain {
    pub chain_id: u64,
    pub diamond_proxy: Address,
    pub chain_admin: Address,
    /// Base token L1 address; `None` for ETH.
    pub base_token_addr: Option<Address>,
    // Held for server-config rendering; not part of the public surface.
    intent: ChainIntent,
    wallets: ChainWallets,
}

/// A fully deployed ecosystem, loaded from a `bootstrap` + `apply` workdir.
pub struct DeployedEcosystem {
    pub bridgehub: Address,
    pub governance: Address,
    pub chains: Vec<DeployedChain>,
    eco: EcosystemInitOutput,
}

impl DeployedEcosystem {
    /// Load from a workdir containing `intent.yaml`, `state.json` and
    /// `wallets.yaml` (the layout `bootstrap` + `apply` produce).
    pub fn load(workdir: &Path) -> Result<Self> {
        let intent = IntentConfig::load(&workdir.join("intent.yaml"))?;
        let state = State::load(&workdir.join("state.json"))?;
        let mut wallets = load_wallets(&workdir.join("wallets.yaml"))?;

        let eco: EcosystemInitOutput = state
            .get_output(StepKey::EcosystemInit)
            .context("ecosystem.init not found in state — run `bootstrap` first")?;

        let mut chains = Vec::with_capacity(intent.chains.len());
        for chain_intent in &intent.chains {
            let chain_id = chain_intent.chain_id;
            let init: ChainInitPreparedOutput = state
                .get_output(StepKey::ChainInitPrepared(chain_id))
                .with_context(|| {
                    format!("chain.init.{chain_id}.prepared not found in state — run `apply` first")
                })?;
            let chain_wallets = wallets
                .chains
                .remove(&chain_id.to_string())
                .with_context(|| format!("chain {chain_id} not found in wallets.yaml"))?;
            let base_token_addr = resolve_base_token_addr(chain_intent, &state)?;

            chains.push(DeployedChain {
                chain_id,
                diamond_proxy: init.diamond_proxy,
                chain_admin: init.chain_admin,
                base_token_addr,
                intent: chain_intent.clone(),
                wallets: chain_wallets,
            });
        }

        Ok(Self {
            bridgehub: eco.bridgehub_proxy,
            governance: eco.governance,
            chains,
            eco,
        })
    }

    pub fn chain(&self, chain_id: u64) -> Result<&DeployedChain> {
        self.chains
            .iter()
            .find(|c| c.chain_id == chain_id)
            .with_context(|| format!("chain {chain_id} not part of this deployment"))
    }

    /// Render the **deployment slice** of the `zksync-os-server` config YAML for
    /// `chain_id` (addresses, chain id, operator keys, base-token price).
    /// Runtime concerns (ports, paths, L1 RPC) are applied separately by the
    /// consumer on the typed `Config` after load.
    pub fn server_config_yaml(&self, chain_id: u64) -> Result<String> {
        let chain = self.chain(chain_id)?;
        render_config(
            &self.eco,
            &chain.wallets,
            &chain.intent,
            resolve_pubdata_mode(&chain.intent),
            chain.base_token_addr,
            // Tests override genesis_input_path on the typed Config after load.
            std::path::Path::new("./genesis.json"),
        )
    }
}
