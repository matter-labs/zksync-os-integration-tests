use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

pub const CURRENT_SCHEMA_VERSION: u32 = 1;

use alloy::primitives::Address;
use anyhow::Context;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Typed identifiers for every resumable step in the bootstrap / apply pipeline.
///
/// Serialises to the dotted-string form expected by `state.json`
/// (e.g. `StepKey::ChainInitPrepared(270)` → `"chain.init.270.prepared"`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepKey {
    WalletsGenerate,
    GenesisGenerate,
    ZiskPlonkVerifierDeploy,
    EcosystemInit,
    EcosystemBundlesApply,
    EcosystemTokenDeploy,
    /// `chain.init.<chain_id>.prepared` — forge script ran, manifest slice recorded
    ChainInitPrepared(u64),
    /// `chain.init.<chain_id>.applied` — manifest bundles broadcast successfully
    ChainInitApplied(u64),
    /// `chain.fund_l2.<chain_id>` — default dev wallets funded via L1→L2 deposits
    ChainL2Funded(u64),
}

impl fmt::Display for StepKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WalletsGenerate => write!(f, "wallets.generate"),
            Self::GenesisGenerate => write!(f, "genesis.generate"),
            Self::ZiskPlonkVerifierDeploy => write!(f, "zisk.plonk_verifier.deploy"),
            Self::EcosystemInit => write!(f, "ecosystem.init"),
            Self::EcosystemBundlesApply => write!(f, "ecosystem.bundles.apply"),
            Self::EcosystemTokenDeploy => write!(f, "ecosystem.token_deploy"),
            Self::ChainInitPrepared(id) => write!(f, "chain.init.{id}.prepared"),
            Self::ChainInitApplied(id) => write!(f, "chain.init.{id}.applied"),
            Self::ChainL2Funded(id) => write!(f, "chain.fund_l2.{id}"),
        }
    }
}

#[derive(thiserror::Error, Debug)]
pub enum StateError {
    #[error("step '{key}' not found in state")]
    StepNotFound { key: String },
    #[error("output of step '{key}' cannot be deserialized: {source}")]
    OutputTypeMismatch {
        key: String,
        source: serde_json::Error,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Done,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepRecord {
    pub status: StepStatus,
    pub completed_at: DateTime<Utc>,
    #[serde(flatten)]
    pub output: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct State {
    #[serde(default)]
    pub schema_version: u32,
    pub steps: BTreeMap<String, StepRecord>,
}

impl State {
    pub fn new() -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            steps: BTreeMap::new(),
        }
    }

    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read state file {}", path.display()))?;
        let state: Self = serde_json::from_str(&content)
            .with_context(|| format!("failed to parse state file {}", path.display()))?;
        state.migrate()
    }

    fn migrate(self) -> anyhow::Result<Self> {
        anyhow::ensure!(
            self.schema_version == CURRENT_SCHEMA_VERSION,
            "state schema version {} is unknown (current is {}); \
             delete state.json to start fresh",
            self.schema_version,
            CURRENT_SCHEMA_VERSION
        );
        Ok(self)
    }

    pub fn load_or_new(path: &Path) -> anyhow::Result<Self> {
        if path.exists() {
            Self::load(path)
        } else {
            Ok(Self::new())
        }
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let content = serde_json::to_string_pretty(self)?;
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, &content)
            .with_context(|| format!("failed to write state tmp file {}", tmp.display()))?;
        std::fs::rename(&tmp, path)
            .with_context(|| format!("failed to rename state tmp file to {}", path.display()))
    }

    pub fn is_done(&self, key: impl fmt::Display) -> bool {
        let key = key.to_string();
        self.steps
            .get(&key)
            .map(|r| r.status == StepStatus::Done)
            .unwrap_or(false)
    }

    pub fn mark_done<T: Serialize>(
        &mut self,
        key: impl fmt::Display,
        output: &T,
    ) -> anyhow::Result<()> {
        let key = key.to_string();
        self.steps.insert(
            key,
            StepRecord {
                status: StepStatus::Done,
                completed_at: Utc::now(),
                output: serde_json::to_value(output)?,
            },
        );
        Ok(())
    }

    pub fn get_output<T: for<'de> Deserialize<'de>>(
        &self,
        key: impl fmt::Display,
    ) -> Result<T, StateError> {
        let key = key.to_string();
        let record = self
            .steps
            .get(&key)
            .ok_or_else(|| StateError::StepNotFound { key: key.clone() })?;
        serde_json::from_value(record.output.clone())
            .map_err(|source| StateError::OutputTypeMismatch { key, source })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenesisGeneratedOutput {
    pub output_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletsGeneratedOutput {
    pub output_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZiskPlonkVerifierDeployedOutput {
    pub verifier_address: Address,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EcosystemInitOutput {
    pub bridgehub_proxy: Address,
    pub ctm_proxy: Address,
    /// Populated by bootstrap v2+. None when loading state written by an older run.
    pub bytecodes_supplier: Option<Address>,
    pub rollup_l1_da_validator: Address,
    pub no_da_l1_validator: Address,
    pub avail_l1_da_validator: Address,
    /// ZKsync OS blob DA validator. Populated by bootstrap v2+; None for older state files.
    pub blobs_zksync_os_l1_da_validator: Option<Address>,
    pub governance: Address,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenDeployedOutput {
    pub token_address: Address,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainInitPreparedOutput {
    pub diamond_proxy: Address,
    pub chain_admin: Address,
    pub manifest_start: usize,
    pub manifest_end: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_error_step_not_found() {
        let state = State::new();
        let err = state
            .get_output::<serde_json::Value>(StepKey::WalletsGenerate)
            .unwrap_err();
        assert!(err.to_string().contains("wallets.generate"));
    }

    #[test]
    fn state_mark_and_get_roundtrip() {
        let mut state = State::new();
        #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
        struct Out {
            val: u32,
        }
        state
            .mark_done(&StepKey::WalletsGenerate, &Out { val: 7 })
            .unwrap();
        let out: Out = state.get_output(StepKey::WalletsGenerate).unwrap();
        assert_eq!(out, Out { val: 7 });
    }

    #[test]
    fn zisk_plonk_deployment_has_a_stable_step_key() {
        assert_eq!(
            StepKey::ZiskPlonkVerifierDeploy.to_string(),
            "zisk.plonk_verifier.deploy"
        );
    }
}
