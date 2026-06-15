use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use tempfile::TempDir;

static RUN_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Workspace root, baked in at compile time via CARGO_MANIFEST_DIR (the
/// `tests/` package dir). Used to resolve relative ZKOS_TEST_DIR paths.
const WORKSPACE_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/..");

/// Either a temporary directory (auto-cleaned on drop) or a persistent named
/// directory under `ZKOS_TEST_DIR` (kept for post-failure inspection).
///
/// Set `ZKOS_TEST_DIR=.test-runs` (or any path) to preserve every test run's
/// workdir — server DB, anvil state, genesis, rendered server configs — so you
/// can inspect them after a failure. Each run gets a unique subdirectory named
/// `<pid>-<counter>`. Relative paths are resolved against the workspace root,
/// so `ZKOS_TEST_DIR=.test-runs` always creates `.test-runs/` next to the
/// top-level `Cargo.toml` regardless of what directory the test binary
/// considers its working directory.
pub(crate) enum WorkDir {
    Temp(TempDir),
    Persistent(PathBuf),
}

impl WorkDir {
    pub fn new() -> Result<Self> {
        if let Ok(base) = std::env::var("ZKOS_TEST_DIR") {
            let n = RUN_COUNTER.fetch_add(1, Ordering::Relaxed);
            let base_path = PathBuf::from(&base);
            // Relative paths are anchored to the workspace root so that
            // `ZKOS_TEST_DIR=.test-runs` from the project root puts the dir
            // where the user expects, regardless of cargo's CWD for tests.
            let base_path = if base_path.is_relative() {
                PathBuf::from(WORKSPACE_ROOT).join(base_path)
            } else {
                base_path
            };
            let dir = base_path.join(format!("{}-{n}", std::process::id()));
            std::fs::create_dir_all(&dir)
                .with_context(|| format!("create workdir {}", dir.display()))?;
            eprintln!("[tests] workdir: {}", dir.display());
            Ok(Self::Persistent(dir))
        } else {
            Ok(Self::Temp(TempDir::new().context("create tempdir")?))
        }
    }

    pub fn path(&self) -> &Path {
        match self {
            Self::Temp(d) => d.path(),
            Self::Persistent(p) => p.as_path(),
        }
    }
}
