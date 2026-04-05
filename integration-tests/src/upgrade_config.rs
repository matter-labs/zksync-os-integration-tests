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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EcosystemContracts {
    pub bridgehub_proxy_addr: String,
    pub message_root_proxy_addr: String,
    pub transparent_proxy_admin_addr: String,
    pub stm_deployment_tracker_proxy_addr: String,
    pub native_token_vault_addr: String,
    pub chain_asset_handler_proxy_addr: Option<String>,
    pub governance: String,
    pub chain_admin: String,
    pub proxy_admin: String,
    pub state_transition_proxy_addr: String,
    pub validator_timelock_addr: String,
    pub diamond_cut_data: String,
    pub force_deployments_data: String,
    pub l1_bytecodes_supplier_addr: String,
    #[serde(default)]
    pub l1_wrapped_base_token_store: Option<String>,
    pub server_notifier_proxy_addr: String,
    pub default_upgrade_addr: String,
    pub genesis_upgrade_addr: String,
    pub verifier_addr: String,
    pub rollup_l1_da_validator_addr: String,
    pub no_da_validium_l1_validator_addr: String,
    pub blobs_zksync_os_l1_da_validator_addr: String,
    pub avail_l1_da_validator_addr: String,
    pub l1_rollup_da_manager: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct L1Config {
    pub default_upgrade_addr: String,
    pub diamond_proxy_addr: String,
    pub governance_addr: String,
    pub chain_admin_addr: String,
    pub access_control_restriction_addr: String,
    pub chain_proxy_admin_addr: String,
    pub multicall3_addr: String,
    pub verifier_addr: String,
    pub validator_timelock_addr: String,
    pub base_token_addr: String,
    pub base_token_asset_id: String,
    pub rollup_l1_da_validator_addr: String,
    pub blobs_zksync_os_l1_da_validator_addr: String,
    pub avail_l1_da_validator_addr: String,
    pub no_da_validium_l1_validator_addr: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct L2Config {
    pub testnet_paymaster_addr: String,
    pub default_l2_upgrader: String,
    pub l2_native_token_vault_proxy_addr: String,
    pub consensus_registry: String,
    pub multicall3: String,
    pub timestamp_asserter_addr: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bridges {
    pub erc20: BridgeConfig,
    pub shared: BridgeConfig,
    pub l1_nullifier_addr: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeConfig {
    pub l1_address: String,
    pub l2_address: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Contracts {
    pub create2_factory_addr: String,
    pub create2_factory_salt: String,
    pub ecosystem_contracts: EcosystemContracts,
    pub bridges: Bridges,
    pub l1: L1Config,
    pub l2: L2Config,
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
