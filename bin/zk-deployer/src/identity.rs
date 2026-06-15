//! Content identities of the deployment inputs, for test-setup cache keys.
//!
//! The rule: anything that can change the deployed L1 state must be
//! identifiable here, and local (editable) inputs are identified by
//! *content*, never by revision labels — uncommitted edits, untracked
//! files and switched branches all just produce a different hash.

use std::path::Path;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

/// Identity of the era-contracts tree the deployment runs against
/// (Solidity sources, deploy scripts, genesis configs **and** the
/// protocol-ops Rust when it is path-patched to the same tree).
///
/// Fast path: a pristine cargo git checkout (`~/.cargo/git/checkouts/...`)
/// is immutable per revision, so its directory name (the short rev) is the
/// identity. Any other root — `PROTOCOL_CONTRACTS_ROOT` pointing at a
/// working copy — gets a content hash, so local edits are always seen.
pub fn contracts_identity() -> Result<String> {
    let root = protocol_ops::common::paths::contracts_root();

    if is_cargo_checkout(&root) {
        // .../era-contracts-<hash>/<short-rev>
        let rev = root
            .file_name()
            .and_then(|n| n.to_str())
            .context("cargo checkout dir has no name")?;
        let repo = root
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("checkout");
        return Ok(format!("checkout:{repo}/{rev}"));
    }

    // Everything that feeds the deployment: contract sources, deploy
    // scripts, foundry config, genesis inputs, and protocol-ops itself.
    const HASHED_SUBPATHS: &[&str] = &[
        "l1-contracts/contracts",
        "l1-contracts/deploy-scripts",
        "l1-contracts/foundry.toml",
        "l1-contracts/script-config/v31-bridged-tokens.toml",
        "da-contracts",
        "configs/genesis",
        "protocol-ops/src",
    ];
    let hash = hash_paths(&root, HASHED_SUBPATHS)?;
    Ok(format!("local:{hash}"))
}

/// Content hash of zk-deployer's own sources (deployment logic compiled
/// into the caller). Always hashed — it's a small local workspace crate.
pub fn self_src_hash() -> Result<String> {
    hash_paths(Path::new(env!("CARGO_MANIFEST_DIR")), &["src"])
}

/// Hash the contents of `subpaths` (files or directories) under `root`:
/// SHA-256 over sorted relative paths and file bytes. Missing subpaths are
/// skipped (not all roots have every optional piece).
pub fn hash_paths(root: &Path, subpaths: &[&str]) -> Result<String> {
    let mut files: Vec<std::path::PathBuf> = Vec::new();
    for sub in subpaths {
        let path = root.join(sub);
        if path.is_file() {
            files.push(path);
        } else if path.is_dir() {
            collect_files(&path, &mut files)?;
        }
    }
    files.sort();

    let mut hasher = Sha256::new();
    for file in &files {
        let rel = file.strip_prefix(root).unwrap_or(file);
        hasher.update(rel.to_string_lossy().as_bytes());
        hasher.update([0u8]);
        let bytes = std::fs::read(file).with_context(|| format!("read {}", file.display()))?;
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(&bytes);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn is_cargo_checkout(root: &Path) -> bool {
    root.components().any(|c| c.as_os_str() == "checkouts")
        && root
            .ancestors()
            .any(|a| a.ends_with(".cargo/git/checkouts") || a.ends_with(".cargo\\git\\checkouts"))
}

fn collect_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("read_dir {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        // Build artifacts and dependencies are derived state, not inputs.
        if matches!(
            name.to_str(),
            Some("out" | "cache-forge" | "zkout" | "broadcast" | "node_modules" | "lib" | "target")
        ) {
            continue;
        }
        if path.is_dir() {
            collect_files(&path, out)?;
        } else {
            out.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_changes_with_content() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/a.sol"), "contract A {}").unwrap();
        let h1 = hash_paths(dir.path(), &["src"]).unwrap();

        std::fs::write(dir.path().join("src/a.sol"), "contract A { uint x; }").unwrap();
        let h2 = hash_paths(dir.path(), &["src"]).unwrap();
        assert_ne!(h1, h2);

        // New untracked file also changes the hash.
        std::fs::write(dir.path().join("src/b.sol"), "contract B {}").unwrap();
        let h3 = hash_paths(dir.path(), &["src"]).unwrap();
        assert_ne!(h2, h3);
    }

    #[test]
    fn hash_stable_across_calls() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/a.sol"), "contract A {}").unwrap();
        assert_eq!(
            hash_paths(dir.path(), &["src"]).unwrap(),
            hash_paths(dir.path(), &["src"]).unwrap()
        );
    }

    #[test]
    fn missing_subpaths_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let h = hash_paths(dir.path(), &["does-not-exist"]).unwrap();
        assert_eq!(h, hash_paths(dir.path(), &[]).unwrap());
    }
}
