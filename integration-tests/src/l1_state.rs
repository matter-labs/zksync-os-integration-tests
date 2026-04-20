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

/// Well-known chain name for the gateway in the `chains` map.
pub const GATEWAY_CHAIN_NAME: &str = "gateway";

/// Well-known chain name for the single L1-settling chain in the fixture
/// ecosystem produced by `generate-l1-state`.
pub const L1_SETTLING_CHAIN_NAME: &str = "l1_settling";

/// Well-known chain name for the first gateway-settling chain.
pub const CHAIN_A_NAME: &str = "gateway_settling_a";

/// Well-known chain name for the second gateway-settling chain.
pub const CHAIN_B_NAME: &str = "gateway_settling_b";

/// Well-known filename for the Anvil L1 state dump within the cache directory.
pub const L1_STATE_FILENAME: &str = "l1-state.json";

#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct EcosystemConfig {
    pub bridgehub: String,
    /// Ecosystem-wide deployer EOA — a convenience default consumed by
    /// callers that need a deployer-role EOA (e.g. the chain-init workflow,
    /// ecosystem upgrade-prepare). Optional: callers can omit the field and
    /// pass the deployer explicitly per invocation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deployer: Option<String>,
    /// All chains, keyed by chain name → chain ID.
    /// The gateway chain is stored under the key [`GATEWAY_CHAIN_NAME`].
    pub chains: std::collections::BTreeMap<String, u64>,
}

impl EcosystemConfig {
    /// Gateway chain ID.
    pub fn gateway_chain_id(&self) -> u64 {
        self.chains[GATEWAY_CHAIN_NAME]
    }

    /// Look up a fixture-known chain by its well-known name. Returns the
    /// `(name, id)` tuple — the name is borrowed from a `'static` constant
    /// so callers can avoid an allocation.
    fn chain(&self, name: &'static str) -> (&'static str, u64) {
        let id = *self
            .chains
            .get(name)
            .unwrap_or_else(|| panic!("ecosystem.yaml missing well-known chain '{name}'"));
        (name, id)
    }

    /// The gateway chain (`{name = "gateway", id = 506}` in the fixture).
    pub fn gateway(&self) -> (&'static str, u64) {
        self.chain(GATEWAY_CHAIN_NAME)
    }

    /// The single L1-settling chain in the fixture.
    pub fn l1_settling(&self) -> (&'static str, u64) {
        self.chain(L1_SETTLING_CHAIN_NAME)
    }

    /// First gateway-settling chain in the fixture.
    pub fn chain_a(&self) -> (&'static str, u64) {
        self.chain(CHAIN_A_NAME)
    }

    /// Second gateway-settling chain in the fixture.
    pub fn chain_b(&self) -> (&'static str, u64) {
        self.chain(CHAIN_B_NAME)
    }
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
    /// Ecosystem owner (bridgehub admin, governance).
    pub owner: WalletEntry,
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
    /// Chain owner (controls ChainAdmin).
    pub owner: WalletEntry,
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

pub const CACHE_DIR: &str = "l1-state-cache";

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
/// First checks the exact cache dir for the current tip SHA. If not found
/// (e.g. the remote branch advanced but no image was published), scans
/// existing cache directories that share the same contracts segment and
/// picks the most recently modified one. This avoids requiring a full
/// regeneration when only the server tip moves.
///
/// The resolution is memoized per cache-dir name for the lifetime of the
/// process so the fallback scan + "Cache miss for …" log line only happens
/// once per preset — callers inside a test invoke this indirectly many
/// times (via `load_ecosystem`, `load_wallets`, `resolve_l1_state`,
/// `chain_config_path`, …).
pub fn resolve_ecosystem_dir(preset: &Preset) -> Result<PathBuf> {
    use std::sync::{Mutex, OnceLock};

    static CACHE: OnceLock<Mutex<std::collections::HashMap<String, PathBuf>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    let key = cache_dir_name(preset);
    if let Some(hit) = cache
        .lock()
        .expect("ecosystem-dir cache poisoned")
        .get(&key)
        .cloned()
    {
        return Ok(hit);
    }

    let resolved = resolve_ecosystem_dir_uncached(preset)?;
    cache
        .lock()
        .expect("ecosystem-dir cache poisoned")
        .insert(key, resolved.clone());
    Ok(resolved)
}

fn resolve_ecosystem_dir_uncached(preset: &Preset) -> Result<PathBuf> {
    let dir = cache_dir_for_preset(preset)?;
    let marker = dir.join("metadata.json");
    if marker.exists() {
        return Ok(dir);
    }

    // Fallback: find a cache dir with the same contracts segment.
    let contracts_segment = repo_ref_cache_segment(&preset.era_contracts);
    let cache_root = dir
        .parent()
        .context("cache dir has no parent")?;
    if let Ok(entries) = fs::read_dir(cache_root) {
        let mut candidates: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .filter(|e| {
                let name = e.file_name();
                let name = name.to_string_lossy();
                name.ends_with(&contracts_segment) && e.path().join("metadata.json").exists()
            })
            .map(|e| e.path())
            .collect();
        // Pick the most recently modified cache dir.
        candidates.sort_by_key(|p| {
            fs::metadata(p.join("metadata.json"))
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
        });
        if let Some(best) = candidates.last() {
            eprintln!(
                "  Cache miss for {} — using closest match: {}",
                dir.display(),
                best.display(),
            );
            return Ok(best.clone());
        }
    }

    anyhow::ensure!(
        false,
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
    let l1_state_path = resolve_l1_state(preset)?;
    eprintln!("  l1_state: {}", l1_state_path.display());
    Ok(config)
}

/// Resolve the l1-state.json path within the preset's cache directory.
pub fn resolve_l1_state(preset: &Preset) -> Result<PathBuf> {
    let dir = resolve_ecosystem_dir(preset)?;
    let resolved = dir.join(L1_STATE_FILENAME);
    anyhow::ensure!(
        resolved.exists(),
        "l1-state.json not found at {}",
        resolved.display(),
    );
    Ok(resolved)
}

/// Resolve the chain config file: `<cache_dir>/<chain_name>.yaml`
pub fn chain_config_path(preset: &Preset, chain_name: &str) -> Result<PathBuf> {
    let dir = resolve_ecosystem_dir(preset)?;
    Ok(dir.join(format!("{chain_name}.yaml")))
}

/// Load wallets.yaml from the preset's cache directory.
pub fn load_wallets(preset: &Preset) -> Result<WalletsFile> {
    let dir = resolve_ecosystem_dir(preset)?;
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
