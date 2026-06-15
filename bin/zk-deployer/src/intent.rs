use std::path::PathBuf;

use alloy::primitives::Address;
use anyhow::Context;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DaMode {
    Rollup,
    NoDa,
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
