use std::path::PathBuf;

use anyhow::Result;

use crate::presets::{Preset, RepoRef};
use crate::utils::find_project_root;

#[derive(Debug, Clone)]
pub struct ServerPresetPaths {
    pub server_root: PathBuf,
}

/// Resolve the zksync-os-server root path for a given preset.
pub fn server_paths_for_preset(preset: &Preset) -> Result<ServerPresetPaths> {
    let server_root = match &preset.zksync_os_server {
        RepoRef::Path(path) => path.clone(),
        RepoRef::DockerTag { .. } => {
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

    Ok(ServerPresetPaths { server_root })
}

/// Build the chain directory path for a given protocol version.
///
/// Returns `<project_root>/local-chains/<version>/default`.
pub fn chain_dir_for_version(version: &str) -> Result<PathBuf> {
    let project_root = find_project_root()?;
    Ok(project_root
        .join("local-chains")
        .join(version)
        .join("default"))
}
