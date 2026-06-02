use std::path::PathBuf;

use alloy::primitives::Address;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scenario {
    L1Only,
    WithGateway,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VmType {
    Zksyncos,
    Eravm,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChainRole {
    Gateway,
    GatewaySettling,
    L1Settling,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DaMode {
    Rollup,
    NoDa,
    Avail,
    Eigen,
}

/// Base token for a chain: ETH, the ecosystem token, or an explicit address.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BaseToken {
    Eth,
    EcosystemToken,
    #[serde(untagged)]
    Address(Address),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletsIntent {
    pub generate: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ecosystem_seed: Option<String>,
    /// Path to existing wallets.yaml when generate: false.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EcosystemIntent {
    pub era_chain_id: u64,
    pub vm_type: VmType,
    #[serde(default)]
    pub with_testnet_verifier: bool,
    #[serde(default)]
    pub with_legacy_bridge: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EcosystemTokenIntent {
    pub deploy: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    /// Use an already-deployed token instead of deploying a new one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<Address>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainIntent {
    pub name: String,
    pub chain_id: u64,
    pub role: ChainRole,
    pub base_token: BaseToken,
    pub da_mode: DaMode,
    #[serde(default)]
    pub deploy_paymaster: bool,
    #[serde(default)]
    pub pause_deposits: bool,
    #[serde(default)]
    pub skip_priority_txs: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntentConfig {
    pub schema_version: u32,
    pub scenario: Scenario,
    pub l1_rpc_url: String,
    pub wallets: WalletsIntent,
    pub ecosystem: EcosystemIntent,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ecosystem_token: Option<EcosystemTokenIntent>,
    pub chains: Vec<ChainIntent>,
}

impl IntentConfig {
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("failed to read intent file {}: {e}", path.display()))?;
        serde_yaml::from_str(&content)
            .map_err(|e| anyhow::anyhow!("failed to parse intent file {}: {e}", path.display()))
    }
}
