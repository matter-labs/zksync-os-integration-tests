use std::path::PathBuf;
use crate::docker_utils::DockerError;

/// Find the project root by looking for presets.yaml
pub fn find_project_root() -> Result<PathBuf, DockerError> {
    let mut current_dir = std::env::current_dir()
        .map_err(|e| DockerError::CommandFailed(format!(
            "Failed to get current working directory: {}",
            e
        )))?;

    loop {
        let presets_yaml = current_dir.join("presets.yaml");
        if presets_yaml.exists() && presets_yaml.is_file() {
            return Ok(current_dir);
        }

        // Move up one directory
        match current_dir.parent() {
            Some(parent) => current_dir = parent.to_path_buf(),
            None => {
                return Err(DockerError::CommandFailed(format!(
                    "Failed to find project root: 'presets.yaml' not found. \
                    Current directory: '{}'. Please run from the project root.",
                    std::env::current_dir()
                        .unwrap_or_else(|_| PathBuf::from("unknown"))
                        .display()
                )));
            }
        }
    }
}





