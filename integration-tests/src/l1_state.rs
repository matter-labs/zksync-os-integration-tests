//! Parsing and helpers for `ecosystem.yaml` — the top-level descriptor
//! produced by the `generate-l1-state` tool alongside the Anvil state dump.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use crate::presets::{Preset, RepoRef};

// ---------------------------------------------------------------------------
// Ecosystem config (ecosystem.yaml)
// ---------------------------------------------------------------------------

#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct EcosystemConfig {
    /// Relative or absolute path to the Anvil l1-state.json dump.
    pub l1_state: String,
    pub bridgehub: String,
    pub bytecodes_supplier: String,
    pub gateway: GatewayMeta,
    pub gateway_settling_chains: Vec<ChainMeta>,
    pub l1_settling_chains: Vec<ChainMeta>,
}

#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct GatewayMeta {
    pub chain_id: u64,
    pub diamond_proxy: String,
    pub ephemeral_state: String,
    /// Chain name used as config file prefix and wallets.yaml key (e.g. "gateway").
    pub name: String,
}

#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct ChainMeta {
    pub chain_id: u64,
    pub diamond_proxy: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ephemeral_state: Option<String>,
    /// Chain name used as config file prefix and wallets.yaml key
    /// (e.g. "gateway_settling_a", "l1_settling").
    pub name: String,
}

// ---------------------------------------------------------------------------
// Wallets
// ---------------------------------------------------------------------------

/// A single wallet entry (address + private key).
#[derive(serde::Deserialize, Debug, Clone)]
pub struct WalletEntry {
    pub address: String,
    pub private_key: String,
}

/// Ecosystem-level wallets (shared across chains).
#[derive(serde::Deserialize, Debug, Clone)]
pub struct EcosystemWallets {
    pub deployer: WalletEntry,
    pub governor: WalletEntry,
    pub token_multiplier_setter: WalletEntry,
}

/// Per-chain wallets.
#[derive(serde::Deserialize, Debug, Clone)]
pub struct ChainWallets {
    pub operator: WalletEntry,
    pub blob_operator: WalletEntry,
    pub commit_operator: WalletEntry,
    pub prove_operator: WalletEntry,
    pub execute_operator: WalletEntry,
    pub fee_account: WalletEntry,
}

/// Full wallets.yaml — ecosystem keys + per-chain keys keyed by chain name.
#[derive(serde::Deserialize, Debug)]
pub struct WalletsFile {
    pub ecosystem: EcosystemWallets,
    #[serde(flatten)]
    pub chains: std::collections::HashMap<String, ChainWallets>,
}

// ---------------------------------------------------------------------------
// Cache key helpers
// ---------------------------------------------------------------------------

pub const CACHE_DIR: &str = ".l1-state-cache";

fn to_snake_case(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// Hash a filesystem path to produce a stable hex key.
fn hash_path(path: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(path.to_string_lossy().as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Produce a `{label}-{short_hash}` cache key segment for a [`RepoRef`].
///
/// Uses `tip_sha` (the latest commit) for the hash when available, so the
/// cache key is stable even when the actual image falls back to an older commit.
/// - `DockerTag` with `original_ref` → `{branch_snake_case}-{tip6}`
/// - `DockerTag` without `original_ref` → `docker-{sha6}`
/// - `Path` → `local-{path_hash6}`
fn repo_ref_cache_segment(r: &RepoRef) -> String {
    match r {
        RepoRef::Path(p) => {
            let h = hash_path(p);
            format!("local-{}", &h[..h.len().min(6)])
        }
        RepoRef::DockerTag {
            tag,
            original_ref,
            tip_sha,
        } => {
            let label = original_ref
                .as_deref()
                .map(to_snake_case)
                .unwrap_or_else(|| "docker".to_string());
            let sha = tip_sha.as_deref().unwrap_or(tag);
            let short = &sha[..sha.len().min(6)];
            format!("{}-{}", label, short)
        }
    }
}

/// Deterministic cache directory name for a preset:
/// `{server_segment}-{contracts_segment}`
///
/// Example: `main-a1b2c3-gateway_commands-d4e5f6`
pub fn cache_dir_name(preset: &Preset) -> String {
    let server = repo_ref_cache_segment(&preset.zksync_os_server);
    let contracts = repo_ref_cache_segment(&preset.era_contracts);
    format!("{}-{}", server, contracts)
}

/// Return the full cache directory path for a preset under the project root.
pub fn cache_dir_for_preset(preset: &Preset) -> Result<PathBuf> {
    let root = crate::infra::utils::find_project_root()?;
    Ok(root.join(CACHE_DIR).join(cache_dir_name(preset)))
}

// ---------------------------------------------------------------------------
// Resolvers
// ---------------------------------------------------------------------------

/// Resolve the ecosystem cache directory for the given preset.
///
/// Computes the deterministic cache directory from the preset and verifies
/// that `metadata.json` exists inside it — this file is written last by
/// `generate-l1-state` and proves the directory is complete.
pub fn resolve_ecosystem_dir(preset: &Preset) -> Result<PathBuf> {
    let dir = cache_dir_for_preset(preset)?;
    let marker = dir.join("metadata.json");
    anyhow::ensure!(
        marker.exists(),
        "No completed l1-state generation found at {}\n\
         Run `cargo run -p generate-l1-state -- {}` first.",
        dir.display(),
        preset.name,
    );
    Ok(dir)
}

/// Load and parse ecosystem.yaml from the preset's cache directory.
pub fn load_ecosystem(preset: &Preset) -> Result<EcosystemConfig> {
    let dir = resolve_ecosystem_dir(preset)?;
    let eco_path = dir.join("ecosystem.yaml");
    eprintln!("Ecosystem: {}", eco_path.display());
    let content =
        fs::read_to_string(&eco_path).with_context(|| format!("read {}", eco_path.display()))?;
    let config: EcosystemConfig = serde_yaml::from_str(&content).context("parse ecosystem.yaml")?;
    let l1_state_path = resolve_l1_state(preset, &config)?;
    eprintln!("  l1_state: {}", l1_state_path.display());
    Ok(config)
}

/// Resolve the l1-state.json path from the `l1_state` field in ecosystem.yaml.
pub fn resolve_l1_state(preset: &Preset, config: &EcosystemConfig) -> Result<PathBuf> {
    let dir = cache_dir_for_preset(preset)?;
    let l1_path = PathBuf::from(&config.l1_state);
    let resolved = if l1_path.is_absolute() {
        l1_path
    } else {
        dir.join(&l1_path)
    };
    anyhow::ensure!(
        resolved.exists(),
        "l1_state path does not exist: {} (from ecosystem.yaml)",
        resolved.display(),
    );
    Ok(resolved)
}

/// Resolve the chain config file: `<cache_dir>/<chain_name>.yaml`
pub fn chain_config_path(preset: &Preset, chain_name: &str) -> Result<PathBuf> {
    let dir = cache_dir_for_preset(preset)?;
    Ok(dir.join(format!("{chain_name}.yaml")))
}

/// Load wallets.yaml from the preset's cache directory.
pub fn load_wallets(preset: &Preset) -> Result<WalletsFile> {
    let dir = cache_dir_for_preset(preset)?;
    let wallets_path = dir.join("wallets.yaml");
    anyhow::ensure!(
        wallets_path.exists(),
        "wallets.yaml not found: {}",
        wallets_path.display()
    );
    let content = fs::read_to_string(&wallets_path)
        .with_context(|| format!("read {}", wallets_path.display()))?;
    serde_yaml::from_str(&content).context("parse wallets.yaml")
}

/// Get the local era-contracts path from the preset.
pub fn get_era_contracts_path(preset: &Preset) -> Result<PathBuf> {
    match &preset.era_contracts {
        RepoRef::Path(p) => Ok(p.clone()),
        RepoRef::DockerTag { tag: t, .. } => {
            anyhow::bail!("need local era-contracts, got docker tag {t}")
        }
    }
}
