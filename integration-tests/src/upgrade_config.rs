use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

// Re-export canonical wallet types from l1_state.
pub use crate::l1_state::{ChainWallets, EcosystemWallets, WalletEntry, WalletsFile};

/// Load wallets.yaml from the given path.
pub fn load_wallets(wallets_path: &Path) -> Result<WalletsFile> {
    let content = std::fs::read_to_string(wallets_path)
        .with_context(|| format!("Failed to read wallets file: {}", wallets_path.display()))?;
    serde_yaml::from_str(&content).context("Failed to parse wallets.yaml")
}

/// Subset of ecosystem-level contract addresses needed by the upgrade test.
///
/// TODO(v30-removal): `l1_bytecodes_supplier_addr` and `l1_rollup_da_manager`
/// are only needed because v30 CTMs don't expose on-chain getters. Remove these
/// fields (and the corresponding contracts.yaml entries) once v30 fixtures are
/// retired.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EcosystemContracts {
    pub bridgehub_proxy_addr: String,
    pub governance: String,
    pub state_transition_proxy_addr: String,
    pub l1_bytecodes_supplier_addr: String,
    pub l1_rollup_da_manager: String,
}

/// Subset of per-chain L1 contract addresses needed by the upgrade test.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct L1Config {
    pub diamond_proxy_addr: String,
    pub chain_admin_addr: String,
    pub access_control_restriction_addr: String,
}

/// Contracts loaded from the v30.2 fixture's `contracts.yaml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Contracts {
    pub ecosystem_contracts: EcosystemContracts,
    pub l1: L1Config,
}

impl Contracts {
    pub fn load_from_path(contracts_path: &Path) -> Result<Self> {
        use std::fs;
        let content = fs::read_to_string(contracts_path).with_context(|| {
            format!(
                "Failed to read contracts file: {}",
                contracts_path.display()
            )
        })?;
        let contracts: Contracts =
            serde_yaml::from_str(&content).context("Failed to parse contracts.yaml")?;
        Ok(contracts)
    }
}
