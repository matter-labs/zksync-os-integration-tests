//! Deployment snapshot cache (Level A).
//!
//! Caches the result of `bootstrap + apply + wallet deposits` — the anvil
//! state dump plus the workdir manifest (`intent.yaml`, `state.json`,
//! `wallets.yaml`, `genesis.json`). Servers are *not* part of the snapshot:
//! they start fresh from genesis on every run, which keeps server code out
//! of the cache key — hacking on `zksync-os-server` with a path override
//! gets cache hits, correctly, because the server plays no role in the
//! deployment.
//!
//! Every input that *does* affect the deployment is identified by content,
//! so local edits are always seen (see [`zk_deployer::identity`]):
//!
//! - contracts tree (Solidity + deploy scripts + genesis configs +
//!   protocol-ops Rust): checkout rev or content hash
//! - zk-deployer sources: content hash
//! - test framework sources (`tests/src/`, deposit logic, wallet keys):
//!   content hash — `tests/tests/` is deliberately excluded, so editing a
//!   test does not invalidate
//! - the topology intent (sans the per-run `l1_rpc_url`)
//! - `anvil --version` (state-dump format compatibility)
//!
//! Knob: `ZKOS_CACHE=auto|off|refresh` (default `auto`).

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::OnceCell;
use zk_deployer::intent::IntentConfig;

/// Bump to invalidate every existing cache entry (layout/semantics change).
const CACHE_SCHEMA: u32 = 1;

/// Files that make up a snapshot, relative to the workdir / cache entry.
const SNAPSHOT_FILES: &[&str] = &[
    "l1-state.json",
    "intent.yaml",
    "state.json",
    "wallets.yaml",
    "genesis.json",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheMode {
    /// Use a hit if present, populate on miss. Default.
    Auto,
    /// Ignore the cache entirely.
    Off,
    /// Rebuild and overwrite the entry.
    Refresh,
}

impl CacheMode {
    pub fn from_env() -> Self {
        match std::env::var("ZKOS_CACHE").as_deref() {
            Ok("off") => Self::Off,
            Ok("refresh") => Self::Refresh,
            Ok("auto") | Err(_) => Self::Auto,
            Ok(other) => {
                eprintln!("[tests] ZKOS_CACHE={other} not recognized — using 'auto'");
                Self::Auto
            }
        }
    }
}

/// Per-key component breakdown, persisted as `meta.json` next to the
/// snapshot for debugging ("why did this miss?").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyComponents {
    pub schema: u32,
    pub intent: String,
    pub contracts: String,
    pub zk_deployer_src: String,
    pub tests_src: String,
    /// Resolved versions/sources of the genesis-affecting zksync-os crates
    /// (`zksync_os_api`, `basic_system`) from the workspace `Cargo.lock`.
    /// `genesis.json` is computed from these, so a bump must invalidate the
    /// snapshot — but the server crates deliberately do *not* (see module docs).
    pub genesis_deps: String,
    pub anvil_version: String,
}

impl KeyComponents {
    /// Compute all components for `intent`. The expensive hashes are
    /// process-cached; only the intent component varies per topology.
    pub async fn compute(intent: &IntentConfig) -> Result<Self> {
        // l1_rpc_url is a per-run anvil endpoint — exclude it from the key.
        let mut canonical = intent.clone();
        canonical.l1_rpc_url = None;
        let intent_yaml = serde_yaml::to_string(&canonical).context("serialize intent for key")?;

        let mut components = shared_components().await?.clone();
        components.intent = hex_digest(intent_yaml.as_bytes());
        Ok(components)
    }

    pub fn key(&self) -> String {
        let mut hasher = Sha256::new();
        for part in [
            &self.schema.to_string(),
            &self.intent,
            &self.contracts,
            &self.zk_deployer_src,
            &self.tests_src,
            &self.genesis_deps,
            &self.anvil_version,
        ] {
            hasher.update(part.as_bytes());
            hasher.update([0u8]);
        }
        hex::encode(&hasher.finalize()[..16])
    }
}

/// A cache entry ready to restore from.
pub struct CacheHit {
    entry_dir: PathBuf,
}

impl CacheHit {
    /// Copy the snapshot files into `workdir`. The anvil state lands at
    /// `<workdir>/l1-state.json` for `spawn_from_file`.
    pub fn restore_into(&self, workdir: &Path) -> Result<()> {
        for file in SNAPSHOT_FILES {
            std::fs::copy(self.entry_dir.join(file), workdir.join(file))
                .with_context(|| format!("restore {file} from {}", self.entry_dir.display()))?;
        }
        Ok(())
    }
}

/// Look up a snapshot for `key`, honoring `mode`.
pub fn lookup(mode: CacheMode, key: &str) -> Option<CacheHit> {
    if mode != CacheMode::Auto {
        return None;
    }
    let entry_dir = cache_root().join(key);
    let complete = SNAPSHOT_FILES.iter().all(|f| entry_dir.join(f).exists());
    complete.then_some(CacheHit { entry_dir })
}

/// Persist `workdir`'s snapshot files under `key` (atomic rename).
pub fn save(mode: CacheMode, key: &str, components: &KeyComponents, workdir: &Path) -> Result<()> {
    if mode == CacheMode::Off {
        return Ok(());
    }
    let root = cache_root();
    let final_dir = root.join(key);
    let tmp_dir = root.join(format!("{key}.tmp-{}", std::process::id()));
    std::fs::create_dir_all(&tmp_dir).context("create cache tmp dir")?;

    for file in SNAPSHOT_FILES {
        std::fs::copy(workdir.join(file), tmp_dir.join(file))
            .with_context(|| format!("stage {file} into cache"))?;
    }
    std::fs::write(
        tmp_dir.join("meta.json"),
        serde_json::to_string_pretty(components)?,
    )
    .context("write cache meta.json")?;

    // Concurrent same-key builders may race; first rename wins, later ones silently lose their entry for this run.
    let _ = std::fs::remove_dir_all(&final_dir);
    std::fs::rename(&tmp_dir, &final_dir).context("publish cache entry")?;
    Ok(())
}

fn cache_root() -> PathBuf {
    // tests/.. = repo root
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(".zkos-test-cache")
}

static SHARED: OnceCell<KeyComponents> = OnceCell::const_new();

async fn shared_components() -> Result<&'static KeyComponents> {
    SHARED
        .get_or_try_init(|| async {
            // All three hash functions do recursive directory traversal + synchronous
            // file reads — run them on the blocking thread pool.
            let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf();
            let (contracts, zk_deployer_src, tests_src) = tokio::task::spawn_blocking(move || {
                let contracts = zk_deployer::identity::contracts_identity()?;
                let zk_deployer_src = zk_deployer::identity::self_src_hash()?;
                let tests_src = zk_deployer::identity::hash_paths(&manifest_dir, &["src"])?;
                anyhow::Ok((contracts, zk_deployer_src, tests_src))
            })
            .await
            .context("identity hash task")??;
            let genesis_deps = tokio::task::spawn_blocking(genesis_deps_identity)
                .await
                .context("genesis deps task")??;
            let anvil_version = String::from_utf8_lossy(
                &Command::new("anvil")
                    .arg("--version")
                    .output()
                    .context("anvil --version")?
                    .stdout,
            )
            .trim()
            .to_string();
            Ok(KeyComponents {
                schema: CACHE_SCHEMA,
                intent: String::new(),
                contracts,
                zk_deployer_src,
                tests_src,
                genesis_deps,
                anvil_version,
            })
        })
        .await
}

/// Genesis (`genesis.json`) is computed by zk-deployer from the `zksync_os_api`
/// and `basic_system` crates. Bumping their pinned revision changes the genesis
/// output, so their resolved `Cargo.lock` stanzas are part of the deployment
/// identity — unlike the server crates, which are deliberately excluded.
fn genesis_deps_identity() -> Result<String> {
    const GENESIS_CRATES: &[&str] = &["zksync_os_api", "basic_system"];
    let lock_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("Cargo.lock");
    let lock = std::fs::read_to_string(&lock_path)
        .with_context(|| format!("read {}", lock_path.display()))?;

    // Collect every `[[package]]` stanza whose name is a genesis crate, sorted
    // for determinism (a name may resolve to multiple source revisions).
    let mut stanzas: Vec<String> = lock
        .split("[[package]]")
        .filter(|s| {
            GENESIS_CRATES
                .iter()
                .any(|c| s.contains(&format!("name = \"{c}\"")))
        })
        .map(|s| s.trim().to_string())
        .collect();
    stanzas.sort();
    Ok(hex_digest(stanzas.join("\n").as_bytes()))
}

fn hex_digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Guards the `Cargo.lock` parsing: the genesis crate names must actually
    /// match stanzas (otherwise the component silently degrades to a constant
    /// and stops invalidating on dep bumps).
    #[test]
    fn genesis_deps_identity_matches_crates() {
        let h = genesis_deps_identity().unwrap();
        assert_ne!(h, hex_digest(b""), "no genesis crate stanzas matched");
        assert_eq!(h, genesis_deps_identity().unwrap(), "must be stable");
    }
}
