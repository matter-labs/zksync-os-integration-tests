use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::Deserialize;

use crate::infra::docker::docker_image_exists;
use crate::infra::git_utils::{is_full_sha, list_remote_branch_commits, resolve_git_ref};
use crate::utils::find_project_root;

const ERA_CONTRACTS_GITHUB_REPO: &str = "https://github.com/matter-labs/era-contracts.git";
const ZKSYNC_OS_SERVER_GITHUB_REPO: &str = "https://github.com/matter-labs/zksync-os-server.git";

const ERA_CONTRACTS_IMAGE_REPO: &str = "ghcr.io/matter-labs/protocol-ops";
const ZKSYNC_OS_SERVER_IMAGE_REPO: &str = "ghcr.io/matter-labs/zksync-os-server";

/// Maximum number of ancestor commits to try when the latest image is not yet available.
const IMAGE_FALLBACK_DEPTH: usize = 10;

#[derive(Debug, Clone, Deserialize)]
pub struct RawPreset {
    pub era_contracts: String,

    pub zksync_os_server: String,

    #[serde(default)]
    pub tests: Vec<String>,

    /// Arbitrary extra key-value pairs that tests can read for custom parameters.
    #[serde(default)]
    pub extra_keys: HashMap<String, serde_yaml::Value>,
}

#[derive(Debug, Clone)]
pub struct Preset {
    pub name: String,
    pub era_contracts: RepoRef,
    pub zksync_os_server: RepoRef,
    pub tests: Vec<String>,
    /// Arbitrary extra key-value pairs from the preset YAML.
    pub extra_keys: HashMap<String, serde_yaml::Value>,
}

impl Preset {
    /// Get an extra key as a string, or `None` if missing or not a string.
    pub fn extra_str(&self, key: &str) -> Option<&str> {
        self.extra_keys.get(key).and_then(|v| v.as_str())
    }
}

#[derive(Debug, Clone)]
pub enum RepoRef {
    /// Local directory on disk.
    Path(PathBuf),
    /// Docker image tag. When the preset value was a git branch/tag, `tag` holds
    /// the resolved full commit SHA and `original_ref` preserves what the user wrote.
    /// `tip_sha` is the latest commit on the branch (may differ from `tag` if the
    /// tip image wasn't published yet and we fell back to an older commit).
    DockerTag {
        tag: String,
        original_ref: Option<String>,
        tip_sha: Option<String>,
    },
}

impl RepoRef {
    /// The docker image tag (commit SHA or raw tag).  Panics on `Path`.
    pub fn docker_tag(&self) -> &str {
        match self {
            RepoRef::DockerTag { tag, .. } => tag,
            RepoRef::Path(_) => panic!("docker_tag() called on RepoRef::Path"),
        }
    }
}

pub type RawPresets = HashMap<String, RawPreset>;
pub type Presets = HashMap<String, Preset>;

pub fn load_default_presets() -> anyhow::Result<Presets> {
    let root = find_project_root()?;
    let file = std::env::var("PRESETS_FILE").unwrap_or_else(|_| "presets.yaml".to_string());
    let path = if PathBuf::from(&file).is_absolute() {
        PathBuf::from(&file)
    } else {
        root.join(&file)
    };
    let raw = load_presets_file(path)?;
    resolve_presets(&root, raw)
}

/// Load the preset selected by the orchestrator via the `PRESET_NAME` env var.
///
/// Falls back to the first preset (sorted by name) if `PRESET_NAME` is not set.
pub fn load_current_preset() -> anyhow::Result<Preset> {
    let presets = load_default_presets()?;
    let name = match std::env::var("PRESET_NAME") {
        Ok(n) if !n.is_empty() => n,
        _ => {
            let mut names: Vec<String> = presets.keys().cloned().collect();
            names.sort();
            names
                .into_iter()
                .next()
                .ok_or_else(|| anyhow::anyhow!("No presets found"))?
        }
    };
    let preset = presets
        .get(&name)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("Preset '{}' not found in presets file", name))?;

    eprintln!("Preset '{}':", preset.name);
    eprintln!(
        "  era_contracts:    {}",
        format_repo_ref(&preset.era_contracts)
    );
    eprintln!(
        "  zksync_os_server: {}",
        format_repo_ref(&preset.zksync_os_server)
    );

    Ok(preset)
}

fn format_repo_ref(r: &RepoRef) -> String {
    match r {
        RepoRef::Path(p) => format!("local path {}", p.display()),
        RepoRef::DockerTag {
            tag,
            original_ref: Some(orig),
            ..
        } => {
            format!("docker {}  (ref: {})", &tag[..tag.len().min(12)], orig)
        }
        RepoRef::DockerTag {
            tag,
            original_ref: None,
            ..
        } => {
            format!("docker {}", tag)
        }
    }
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
                anyhow::bail!(
                    "Duplicate preset '{}' found while loading {}",
                    name,
                    path.display()
                );
            }
            out.insert(name, preset);
        }
    }

    Ok(out)
}

fn is_yaml_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("yaml") | Some("yml")
    )
}

fn resolve_presets(project_root: &Path, raw: RawPresets) -> anyhow::Result<Presets> {
    let mut out: Presets = HashMap::new();
    for (name, r) in raw {
        let era_contracts = parse_repo_ref(project_root, &name, "era-contracts", &r.era_contracts)?;
        let zksync_os_server =
            parse_repo_ref(project_root, &name, "zksync-os-server", &r.zksync_os_server)?;

        out.insert(
            name.clone(),
            Preset {
                name,
                era_contracts,
                zksync_os_server,
                tests: r.tests,
                extra_keys: r.extra_keys,
            },
        );
    }
    Ok(out)
}

fn github_repo_for_key(key: &str) -> Option<&'static str> {
    match key {
        "era-contracts" | "era_contracts" => Some(ERA_CONTRACTS_GITHUB_REPO),
        "zksync-os-server" | "zksync_os_server" => Some(ZKSYNC_OS_SERVER_GITHUB_REPO),
        _ => None,
    }
}

fn docker_image_repo_for_key(key: &str) -> Option<&'static str> {
    match key {
        "era-contracts" | "era_contracts" => Some(ERA_CONTRACTS_IMAGE_REPO),
        "zksync-os-server" | "zksync_os_server" => Some(ZKSYNC_OS_SERVER_IMAGE_REPO),
        _ => None,
    }
}

/// Resolve a git ref to the most recent commit SHA that has a published docker image.
///
/// Tries the tip of the branch first. If no image exists, walks back through
/// ancestor commits (up to `IMAGE_FALLBACK_DEPTH`) and returns the first one
/// that has a published image. Returns `None` if no image is found for any
/// ancestor.
/// Returns `(image_sha, tip_sha, fell_back)`.
fn resolve_git_ref_with_image(
    repo_url: &str,
    image_repo: &str,
    git_ref: &str,
) -> Option<(String, String, bool)> {
    // Resolve ref → tip SHA.
    let tip_sha = resolve_git_ref(repo_url, git_ref)?;
    let tip_image = format!("{}:{}", image_repo, tip_sha);

    if docker_image_exists(&tip_image) {
        return Some((tip_sha.clone(), tip_sha, false));
    }

    // Tip image not available yet — walk back through ancestors.
    // Only makes sense for branch names (not raw SHAs).
    let is_branch = !is_full_sha(git_ref);
    if !is_branch {
        return None;
    }

    eprintln!(
        "  Image {} not found, checking {} previous commits...",
        &tip_sha[..12],
        IMAGE_FALLBACK_DEPTH
    );

    let commits = list_remote_branch_commits(repo_url, git_ref, IMAGE_FALLBACK_DEPTH + 1);
    // commits[0] is the tip (already checked), try the rest.
    for sha in commits.iter().skip(1) {
        let image = format!("{}:{}", image_repo, sha);
        if docker_image_exists(&image) {
            eprintln!(
                "  Falling back to {} (latest commit with image)",
                &sha[..12]
            );
            return Some((sha.clone(), tip_sha.clone(), true));
        }
    }

    None
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

    // 2) Try to resolve as a git ref (branch/tag/sha) → commit SHA with a published image.
    if let (Some(repo_url), Some(image_repo)) =
        (github_repo_for_key(key), docker_image_repo_for_key(key))
    {
        match resolve_git_ref_with_image(repo_url, image_repo, value) {
            Some((image_sha, tip, fell_back)) => {
                if fell_back {
                    eprintln!(
                        "Preset '{}': {} ref '{}' — tip image not ready, using {}",
                        preset_name,
                        key,
                        value,
                        &image_sha[..12]
                    );
                }
                return Ok(RepoRef::DockerTag {
                    tag: image_sha,
                    original_ref: Some(value.to_string()),
                    tip_sha: Some(tip),
                });
            }
            None => {
                // Could resolve the git ref but no image found for any recent commit.
                if let Some(sha) = resolve_git_ref(repo_url, value) {
                    eprintln!(
                        "Warning: Preset '{}': resolved {} ref '{}' → {} but no docker image found \
                         (checked {} ancestors). Using SHA as tag anyway.",
                        preset_name,
                        key,
                        value,
                        &sha[..12],
                        IMAGE_FALLBACK_DEPTH
                    );
                    return Ok(RepoRef::DockerTag {
                        tag: sha.clone(),
                        original_ref: Some(value.to_string()),
                        tip_sha: Some(sha),
                    });
                }
            }
        }
    }

    // 3) Fallback: use raw value as docker tag.
    Ok(RepoRef::DockerTag {
        tag: value.to_string(),
        original_ref: None,
        tip_sha: None,
    })
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

        for (idx, content) in lines.iter().enumerate().take(end).skip(start) {
            let line_no = idx + 1;
            let prefix = if line_no == line { ">" } else { " " };
            msg.push_str(&format!("{prefix} {line_no:4} | {}\n", content));
            if line_no == line {
                // caret under column (best-effort; columns are 1-based)
                let caret_pad = col.saturating_sub(1);
                msg.push_str(&format!("  {:4} | {}^\n", "", " ".repeat(caret_pad)));
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
        let presets =
            load_default_presets().expect("Failed to load presets.yaml from project root");
        println!("{:#?}", presets);
    }
}
