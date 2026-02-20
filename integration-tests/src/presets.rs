use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::Deserialize;

use crate::utils::find_project_root;

#[derive(Debug, Clone, Deserialize)]
pub struct RawPreset {
    pub era_contracts: String,

    pub zksync_os_server: String,

    pub protocol_versions: RawProtocolVersions,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawProtocolVersions {
    pub previous: String,
    pub next: String,
}

#[derive(Debug, Clone)]
pub struct Preset {
    pub era_contracts: RepoRef,
    pub zksync_os_server: RepoRef,
    pub protocol_versions: ProtocolVersions,
}

#[derive(Debug, Clone)]
pub struct ProtocolVersions {
    pub previous: String,
    pub next: String,
}

#[derive(Debug, Clone)]
pub enum RepoRef {
    Path(PathBuf),
    DockerTag(String),
}

pub type RawPresets = HashMap<String, RawPreset>;
pub type Presets = HashMap<String, Preset>;

pub fn load_default_presets() -> anyhow::Result<Presets> {
    let root = find_project_root()?;
    let raw = load_presets_file(root.join("presets.yaml"))?;
    resolve_presets(&root, raw)
}

pub fn load_presets_file(path: impl AsRef<Path>) -> anyhow::Result<RawPresets> {
    let path = path.as_ref();
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read presets file: {}", path.display()))?;
    let presets: RawPresets = serde_yaml::from_str(&contents)
        .map_err(|e| anyhow::anyhow!("{}", format_yaml_parse_error(path, &contents, e)))?;
    Ok(presets)
}

pub fn load_presets_from_dir(dir: impl AsRef<Path>) -> anyhow::Result<RawPresets> {
    let dir = dir.as_ref();
    let mut out: RawPresets = HashMap::new();

    let entries = std::fs::read_dir(dir)
        .with_context(|| format!("Failed to read presets directory: {}", dir.display()))?;

    for entry in entries {
        let entry = entry.with_context(|| "Failed to read directory entry")?;
        let path = entry.path();
        if !is_yaml_file(&path) {
            continue;
        }

        let presets = load_presets_file(&path)?;
        for (name, preset) in presets {
            if out.contains_key(&name) {
                anyhow::bail!("Duplicate preset '{}' found while loading {}", name, path.display());
            }
            out.insert(name, preset);
        }
    }

    Ok(out)
}

fn is_yaml_file(path: &PathBuf) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some("yaml") | Some("yml") => true,
        _ => false,
    }
}

fn resolve_presets(project_root: &Path, raw: RawPresets) -> anyhow::Result<Presets> {
    let mut out: Presets = HashMap::new();
    for (name, r) in raw {
        let era_contracts = parse_repo_ref(project_root, &name, "era-contracts", &r.era_contracts)?;
        let zksync_os_server = parse_repo_ref(project_root, &name, "zksync-os-server", &r.zksync_os_server)?;

        out.insert(
            name,
            Preset {
                era_contracts,
                zksync_os_server,
                protocol_versions: ProtocolVersions {
                    previous: r.protocol_versions.previous,
                    next: r.protocol_versions.next,
                },
            },
        );
    }
    Ok(out)
}

fn parse_repo_ref(
    project_root: &Path,
    preset_name: &str,
    key: &str,
    value: &str,
) -> anyhow::Result<RepoRef> {
    // 1) Try local path (absolute or relative to project root).
    let as_path = PathBuf::from(value);
    let candidate = if as_path.is_absolute() {
        as_path
    } else {
        project_root.join(as_path)
    };
    if candidate.exists() {
        return Ok(RepoRef::Path(candidate));
    }

    if value.trim().is_empty() {
        anyhow::bail!(
            "Preset '{}' key '{}' must be either an existing path or a non-empty docker tag",
            preset_name,
            key
        );
    }

    Ok(RepoRef::DockerTag(value.to_string()))
}

fn format_yaml_parse_error(path: &Path, contents: &str, err: serde_yaml::Error) -> String {
    let mut msg = format!("Failed to parse presets YAML: {}\n{}", path.display(), err);

    let err_str = err.to_string();
    if let Some(missing) = err_str
        .split("missing field `")
        .nth(1)
        .and_then(|s| s.split('`').next())
    {
        msg.push_str(&format!("\nMissing required key: {}", missing));
    }

    if let Some(loc) = err.location() {
        let line = loc.line();
        let col = loc.column();
        msg.push_str(&format!("\nAt line {}, column {}", line, col));
        msg.push_str("\nContext:\n");

        // serde_yaml locations are 1-based.
        let lines: Vec<&str> = contents.lines().collect();
        let start = line.saturating_sub(2);
        let end = (line + 1).min(lines.len());

        for idx in start..end {
            let line_no = idx + 1;
            let prefix = if line_no == line { ">" } else { " " };
            msg.push_str(&format!("{prefix} {line_no:4} | {}\n", lines[idx]));
            if line_no == line {
                // caret under column (best-effort; columns are 1-based)
                let caret_pad = col.saturating_sub(1);
                msg.push_str(&format!(
                    "  {:4} | {}^\n",
                    "",
                    " ".repeat(caret_pad)
                ));
            }
        }
    }

    msg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_presets() {
        let presets = load_default_presets().expect("Failed to load presets.yaml from project root");
        println!("{:#?}", presets);
    }
}


