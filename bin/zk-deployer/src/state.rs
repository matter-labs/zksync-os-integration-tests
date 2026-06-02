use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

/// Bump this when a new version adds non-backwards-compatible state fields.
/// `State::migrate()` handles upgrades from older versions.
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

use alloy::primitives::{Address, B256};
use anyhow::Context;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Typed identifiers for every resumable step in the bootstrap / apply pipeline.
///
/// Serialises to the dotted-string form expected by `state.json`
/// (e.g. `StepKey::ChainInit("era".into())` → `"chain.init.era"`),
/// so existing state files remain compatible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepKey {
    WalletsGenerate,
    GenesisGenerate,
    EcosystemInit,
    EcosystemBundlesApply,
    EcosystemTokenDeploy,
    /// `chain.init.<name>`
    ChainInit(String),
    /// `chain.gateway.convert`
    GatewayConvert,
    /// `chain.migrate.<name>.phase1`
    GatewayMigratePhase1(String),
    /// `chain.migrate.<name>.phase2`
    GatewayMigratePhase2(String),
    /// `chain.migrate.<name>.phase3`
    GatewayMigratePhase3(String),
}

impl fmt::Display for StepKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WalletsGenerate => write!(f, "wallets.generate"),
            Self::GenesisGenerate => write!(f, "genesis.generate"),
            Self::EcosystemInit => write!(f, "ecosystem.init"),
            Self::EcosystemBundlesApply => write!(f, "ecosystem.bundles.apply"),
            Self::EcosystemTokenDeploy => write!(f, "ecosystem.token_deploy"),
            Self::ChainInit(name) => write!(f, "chain.init.{name}"),
            Self::GatewayConvert => write!(f, "chain.gateway.convert"),
            Self::GatewayMigratePhase1(name) => write!(f, "chain.migrate.{name}.phase1"),
            Self::GatewayMigratePhase2(name) => write!(f, "chain.migrate.{name}.phase2"),
            Self::GatewayMigratePhase3(name) => write!(f, "chain.migrate.{name}.phase3"),
        }
    }
}

/// Tracks which phase of gateway migration a chain has completed.
///
/// Phases must execute in order; this enum encodes the dependency so callers
/// can enforce ordering without re-querying multiple StepKeys manually.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatewayMigrationPhase {
    NotStarted,
    Phase1Done,
    Phase2Done,
    Complete,
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

    /// Upgrade `self` from an older schema version to `CURRENT_SCHEMA_VERSION`.
    ///
    /// Each migration branch must be additive: existing StepRecord data is
    /// preserved. New optional fields on typed outputs already use
    /// `#[serde(default)]`, so they deserialize to `None`/default on old files.
    fn migrate(mut self) -> anyhow::Result<Self> {
        if self.schema_version == 0 {
            // Version 0: schema_version field missing (defaulted to 0 by serde).
            // No structural changes — all output structs use Option for new fields
            // so deserialization of v0 state files is already correct.
            self.schema_version = 1;
        }
        // Add future migrations here:
        // if self.schema_version == 1 {
        //     // ... migrate fields ...
        //     self.schema_version = 2;
        // }
        anyhow::ensure!(
            self.schema_version == CURRENT_SCHEMA_VERSION,
            "state schema version {} is unknown (current is {}); \
             this binary may be too old to read this state file",
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

    /// Return the highest completed gateway migration phase for `chain_name`.
    pub fn gateway_migration_phase(&self, chain_name: &str) -> GatewayMigrationPhase {
        if self.is_done(StepKey::GatewayMigratePhase3(chain_name.into())) {
            GatewayMigrationPhase::Complete
        } else if self.is_done(StepKey::GatewayMigratePhase2(chain_name.into())) {
            GatewayMigrationPhase::Phase2Done
        } else if self.is_done(StepKey::GatewayMigratePhase1(chain_name.into())) {
            GatewayMigrationPhase::Phase1Done
        } else {
            GatewayMigrationPhase::NotStarted
        }
    }

    /// Assert that `chain_name` is ready to run `target_phase` (1, 2, or 3).
    /// Returns `Err` with a clear message if the prerequisite phase is missing.
    pub fn assert_gateway_phase_ready(
        &self,
        chain_name: &str,
        target_phase: u8,
    ) -> anyhow::Result<()> {
        let current = self.gateway_migration_phase(chain_name);
        let required = match target_phase {
            1 => GatewayMigrationPhase::NotStarted,
            2 => GatewayMigrationPhase::Phase1Done,
            3 => GatewayMigrationPhase::Phase2Done,
            _ => anyhow::bail!("invalid target phase {target_phase}"),
        };
        anyhow::ensure!(
            current == required,
            "chain '{chain_name}' is in phase {current:?} but phase {target_phase} requires \
             {required:?} — check gateway migration step order"
        );
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Typed step outputs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenesisGeneratedOutput {
    pub output_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletsGeneratedOutput {
    pub output_path: String,
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
pub struct ChainInitOutput {
    pub diamond_proxy: Address,
    pub chain_admin: Address,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayConvertOutput {
    pub gateway_chain_id: u64,
    pub ctm_representative_chain_id: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayMigratePhase1Output {
    pub chain_id: u64,
    pub gateway_chain_id: u64,
    /// Priority op hash on the gateway L2, captured right after the phase-1
    /// broadcast. `None` in dry-run mode or if the capture failed. When set,
    /// phase 2 uses it directly and skips the 216k-block L1 event scan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority_op_hash: Option<B256>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayMigratePhase2Output {
    pub chain_id: u64,
    pub gateway_chain_id: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayMigratePhase3Output {
    pub chain_id: u64,
    /// Address of the RelayedSLDAValidator used for the DA validator pair on the gateway.
    pub relayed_sl_da_validator: Address,
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
    fn gateway_phase_not_started() {
        let state = State::new();
        assert_eq!(
            state.gateway_migration_phase("era"),
            GatewayMigrationPhase::NotStarted
        );
    }

    #[test]
    fn gateway_phase_advances() {
        let mut state = State::new();
        #[derive(serde::Serialize)]
        struct Empty {}

        state
            .mark_done(&StepKey::GatewayMigratePhase1("era".into()), &Empty {})
            .unwrap();
        assert_eq!(
            state.gateway_migration_phase("era"),
            GatewayMigrationPhase::Phase1Done
        );

        state
            .mark_done(&StepKey::GatewayMigratePhase2("era".into()), &Empty {})
            .unwrap();
        assert_eq!(
            state.gateway_migration_phase("era"),
            GatewayMigrationPhase::Phase2Done
        );

        state
            .mark_done(&StepKey::GatewayMigratePhase3("era".into()), &Empty {})
            .unwrap();
        assert_eq!(
            state.gateway_migration_phase("era"),
            GatewayMigrationPhase::Complete
        );
    }

    #[test]
    fn assert_gateway_phase_ready_enforces_order() {
        let state = State::new();
        // Can start phase 1 from NotStarted
        assert!(state.assert_gateway_phase_ready("era", 1).is_ok());
        // Cannot skip to phase 2 from NotStarted
        assert!(state.assert_gateway_phase_ready("era", 2).is_err());
    }

    #[test]
    fn state_v0_migrates_to_current() {
        // A state file written before schema_version was introduced deserializes
        // with schema_version == 0 (the serde default). migrate() should bump it.
        let raw = r#"{"steps":{}}"#;
        let state: State = serde_json::from_str(raw).unwrap();
        assert_eq!(
            state.schema_version, 0,
            "serde default is 0 for missing field"
        );
        let migrated = state.migrate().unwrap();
        assert_eq!(migrated.schema_version, CURRENT_SCHEMA_VERSION);
    }

    #[test]
    fn gateway_phase2_output_roundtrip() {
        let mut state = State::new();
        let out = GatewayMigratePhase2Output {
            chain_id: 42,
            gateway_chain_id: 100,
        };
        state
            .mark_done(&StepKey::GatewayMigratePhase2("era".into()), &out)
            .unwrap();
        let got: GatewayMigratePhase2Output = state
            .get_output(StepKey::GatewayMigratePhase2("era".into()))
            .unwrap();
        assert_eq!(got.chain_id, 42);
        assert_eq!(got.gateway_chain_id, 100);
    }

    #[test]
    fn state_current_version_loads_without_migration() {
        let raw = format!(
            r#"{{"schema_version":{},"steps":{{}}}}"#,
            CURRENT_SCHEMA_VERSION
        );
        let state: State = serde_json::from_str(&raw).unwrap();
        let migrated = state.migrate().unwrap();
        assert_eq!(migrated.schema_version, CURRENT_SCHEMA_VERSION);
    }
}
