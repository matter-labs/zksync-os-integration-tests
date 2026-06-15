//! Restore an `Ecosystem` from a committed fixed-chain snapshot.
//!
//! A fixed chain is defined by its committed **server config**, which is the
//! source of truth and is self-describing: its `genesis:` block carries the
//! chain id and bridgehub. Deployer artifacts (`intent.yaml`/`state.json`) and
//! any manifest are NOT read here — only the server config(s), `genesis.json`,
//! and the `l1-state` snapshot.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::anvil::{default_builder, spawn_from_file};
use crate::ecosystem::{ChainSpec, Ecosystem};
use crate::server_runtime::ChainRuntime;
use crate::workdir::WorkDir;
use alloy::primitives::Address;
use anyhow::{Context, Result};

#[derive(serde::Deserialize)]
struct ConfigIdentity {
    genesis: GenesisIdentity,
}

#[derive(serde::Deserialize)]
struct GenesisIdentity {
    chain_id: u64,
    bridgehub_address: Address,
}

/// Parse a committed server config's `genesis:` block for the chain's identity.
fn parse_server_config_identity(yaml: &str) -> Result<(u64, Address)> {
    let id: ConfigIdentity =
        serde_yaml::from_str(yaml).context("parse chain identity from server config")?;
    Ok((id.genesis.chain_id, id.genesis.bridgehub_address))
}

/// Bring up an `Ecosystem` from a committed fixed-chain snapshot directory.
///
/// The directory must contain:
/// - `l1-state.json.gz` — the anvil L1 state dump,
/// - `genesis.json` — the genesis input,
/// - one or more server configs: `server.yaml` (single chain) or
///   `server-<id>.yaml` (one per chain).
///
/// Each server config is the source of truth and is self-describing (its
/// `genesis:` block carries `chain_id` + `bridgehub_address`).
pub async fn restore(dir: &Path) -> Result<Ecosystem> {
    super::init_logging().await;

    let workdir = Arc::new(WorkDir::new().context("create workdir")?);
    let ecosystem_dir = workdir.path().join("ecosystem");
    std::fs::create_dir_all(&ecosystem_dir).context("create ecosystem dir")?;
    let genesis_dst = ecosystem_dir.join("genesis.json");
    std::fs::copy(dir.join("genesis.json"), &genesis_dst).context("copy genesis.json")?;

    let anvil = spawn_from_file(default_builder(), &dir.join("l1-state.json.gz"))
        .await
        .context("spawn anvil from committed l1-state")?;

    let mut specs = Vec::new();
    for server_config in collect_server_configs(dir)? {
        let yaml = std::fs::read_to_string(&server_config)
            .with_context(|| format!("read {}", server_config.display()))?;
        let (chain_id, bridgehub) = parse_server_config_identity(&yaml)?;

        // The committed server config is the deployment slice; `rt` applies the
        // per-run runtime values (ports/paths/L1 URL/genesis) onto the loaded
        // Config in `Ecosystem::assemble`.
        let rt = ChainRuntime::allocate(workdir.path(), &chain_id.to_string());

        specs.push(ChainSpec {
            chain_id,
            bridgehub,
            config_paths: vec![server_config],
            runtime: rt,
            genesis_path: genesis_dst.clone(),
            wallets: vec![],
        });
    }
    anyhow::ensure!(
        !specs.is_empty(),
        "no server config found in {}",
        dir.display()
    );

    Ecosystem::assemble(anvil, workdir, specs).await
}

/// Committed server config files in `dir`: `server.yaml` or `server-<id>.yaml`,
/// sorted for deterministic chain order.
fn collect_server_configs(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("read dir {}", dir.display()))? {
        let path = entry?.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name == "server.yaml" || (name.starts_with("server-") && name.ends_with(".yaml")) {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_identity_from_server_config_genesis_block() {
        let yaml = "\
genesis:
  bridgehub_address: \"0xd8f8df05efacd52f28cdf11be22ce3d6ae0fabf7\"
  bytecode_supplier_address: \"0x9f3f32ea83c8a1c8e993fd9035d1d077545467ac\"
  chain_id: 6565
l1_sender:
  pubdata_mode: Blobs
";
        let (chain_id, bridgehub) = parse_server_config_identity(yaml).unwrap();
        assert_eq!(chain_id, 6565);
        assert_eq!(
            bridgehub,
            "0xd8f8df05efacd52f28cdf11be22ce3d6ae0fabf7"
                .parse::<Address>()
                .unwrap()
        );
    }
}
