use std::path::PathBuf;

use anyhow::Result;

use crate::presets::{Preset, RepoRef};
use crate::utils::find_project_root;

#[derive(Debug, Clone)]
pub struct ServerPresetPaths {
    pub server_root: PathBuf,
    pub chain_dir: PathBuf,
    pub wallets_yaml: PathBuf,
    pub contracts_yaml: PathBuf,
    pub config_yaml: PathBuf,
}

/// Resolve zksync-os-server config paths for a given preset.
pub fn server_paths_for_preset(preset: &Preset) -> Result<ServerPresetPaths> {
    let server_root = match &preset.zksync_os_server {
        RepoRef::Path(path) => path.clone(),
        RepoRef::DockerTag(_) => {
            let project_root = find_project_root()?;
            project_root.join("../zksync-os-server")
        }
    };
    if !server_root.exists() {
        anyhow::bail!(
            "Resolved zksync-os-server path does not exist: {}",
            server_root.display()
        );
    }

    let chain_dir = server_root
        .join("local-chains")
        .join(&preset.protocol_versions.previous)
        .join("default");

    Ok(ServerPresetPaths {
        server_root,
        wallets_yaml: chain_dir.join("wallets.yaml"),
        contracts_yaml: chain_dir.join("contracts.yaml"),
        config_yaml: chain_dir.join("config.yaml"),
        chain_dir,
    })
}
