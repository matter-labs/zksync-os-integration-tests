//! Git helpers: resolve remote refs, list branch commits via shallow clone.

use std::process::{Command, Stdio};

/// Try to resolve a string as a git ref (branch, tag, or SHA) via `git ls-remote`.
///
/// Returns the full 40-char SHA if the ref exists, or `None`.
pub fn resolve_git_ref(repo_url: &str, git_ref: &str) -> Option<String> {
    // Already a full SHA — use as-is.
    if is_full_sha(git_ref) {
        return Some(git_ref.to_string());
    }

    let output = Command::new("git")
        .args(["ls-remote", repo_url, git_ref])
        .output()
        .ok()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().next())
        .filter(|sha| sha.len() == 40)
        .map(|s| s.to_string())
}

/// List recent commit SHAs on a branch from a remote repo (newest first).
///
/// Uses a shallow bare clone + `git log` so we never download the full repo.
/// Returns up to `depth` SHAs.
pub fn list_remote_branch_commits(repo_url: &str, branch: &str, depth: usize) -> Vec<String> {
    let tmp = std::env::temp_dir().join(format!(
        "git-shallow-{}-{}",
        branch.replace('/', "_"),
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&tmp);

    let clone_ok = Command::new("git")
        .args([
            "clone",
            "--bare",
            "--single-branch",
            "--branch",
            branch,
            "--depth",
            &depth.to_string(),
            repo_url,
            &tmp.to_string_lossy(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if !clone_ok {
        let _ = std::fs::remove_dir_all(&tmp);
        return Vec::new();
    }

    let output = Command::new("git")
        .args([
            "-C",
            &tmp.to_string_lossy(),
            "log",
            "--format=%H",
            &format!("-{}", depth),
        ])
        .output();

    let _ = std::fs::remove_dir_all(&tmp);

    match output {
        Ok(o) => String::from_utf8_lossy(&o.stdout)
            .lines()
            .map(|s| s.to_string())
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Returns `true` when the string looks like a full 40-hex-char git SHA.
pub fn is_full_sha(s: &str) -> bool {
    s.len() == 40 && s.chars().all(|c| c.is_ascii_hexdigit())
}
