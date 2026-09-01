use std::path::PathBuf;

use alloy::primitives::Address;
use anyhow::Context;
use serde::{Deserialize, Serialize};

/// What the chain does with its pubdata.
///
/// There is no knob for "validium, but full pubdata" or "validium, but nothing posted": a
/// validium here always commits the logs-only region and always posts it through blobs. Both
/// were configurable once and both are footguns — full pubdata makes the "validium" pay
/// rollup DA costs, and posting nothing puts the chain's interop (IMT) leaves out of reach of
/// L1, which silently breaks atomic interop for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DaMode {
    /// Publishes the whole pubdata on L1, through blobs.
    Rollup,
    /// Publishes only the mandatory L2->L1 log region on L1 — including the interop commitment
    /// tree leaves, which is what keeps the chain interop-capable — and drops the state diffs
    /// and message preimages. That region still goes through blobs, so the chain runs the same
    /// L1 DA validator and `PubdataPricingMode::Rollup` a rollup does; it differs from a rollup
    /// only in its `PubdataContent`.
    LogsOnlyValidium,
    /// Hands the full pubdata to Avail.
    Avail,
}

/// A custom (non-ETH) base token. If `address` is absent the token is deployed
/// during `bootstrap`; if present the existing contract is used.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomToken {
    pub symbol: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<Address>,
}

/// Wallet configuration. Omit entirely to auto-generate from a default seed.
/// Provide `path` to load an existing wallets.yaml instead of generating.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WalletsIntent {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ecosystem_seed: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
}

/// An L1-settling chain. (Gateway settlement was removed; every chain
/// settles directly on L1.)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainIntent {
    pub chain_id: u64,
    /// Omit for ETH (default). Provide a `CustomToken` to use a non-ETH base token.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_token: Option<CustomToken>,
    pub da_mode: DaMode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntentConfig {
    pub schema_version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub l1_rpc_url: Option<String>,
    #[serde(default)]
    pub wallets: WalletsIntent,
    pub chains: Vec<ChainIntent>,
}

impl IntentConfig {
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("failed to read intent file {}: {e}", path.display()))?;
        serde_yaml::from_str(&content)
            .map_err(|e| anyhow::anyhow!("failed to parse intent file {}: {e}", path.display()))
    }

    /// The primary chain ID used for ecosystem L1 contract initialisation:
    /// the first chain in the intent.
    pub fn main_chain_id(&self) -> anyhow::Result<u64> {
        self.chains
            .first()
            .map(|c| c.chain_id)
            .context("intent has no chains")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Locks the intent-YAML shape of `da_mode` — hand-written intents depend on it, and the
    /// test-cache key hashes the serialized form.
    #[test]
    fn da_mode_yaml_roundtrip() {
        for (mode, yaml) in [
            (DaMode::Rollup, "rollup\n"),
            (DaMode::LogsOnlyValidium, "logs_only_validium\n"),
            (DaMode::Avail, "avail\n"),
        ] {
            assert_eq!(serde_yaml::to_string(&mode).unwrap(), yaml);
            let parsed: DaMode = serde_yaml::from_str(yaml).unwrap();
            assert_eq!(serde_yaml::to_string(&parsed).unwrap(), yaml);
        }
    }
}
