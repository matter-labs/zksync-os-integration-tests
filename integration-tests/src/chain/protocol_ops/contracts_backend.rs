use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::presets::{Preset, RepoRef};

use super::{run_protocol_ops_local, EraContainerSession, ERA_CONTRACTS_PROTOCOL_IMAGE_REPO};

/// Unified backend for running era-contracts tools (protocol_ops, forge, cast).
/// Local mode runs binaries from the source tree; Docker mode execs into a
/// long-lived container (single Rosetta boot on Apple Silicon).
///
/// All outputs (protocol_ops `--out`, forge script-out, genesis, wallets) are
/// written to a shared `work_dir`. Callers read results from `work_dir`
/// regardless of backend mode.
pub enum EraContractsBackend {
    Local {
        era_path: PathBuf,
        work_dir: PathBuf,
    },
    Docker {
        session: EraContainerSession,
        work_dir: PathBuf,
    },
}

/// Container-side path prefix for the work directory.
const CONTAINER_WORK_PREFIX: &str = "/contracts/work";

impl EraContractsBackend {
    /// Create an `EraContractsBackend` from a preset configuration.
    ///
    /// - `RepoRef::Path` → `EraContractsBackend::Local`, work_dir lives at `era_path/work/{work_name}`
    /// - `RepoRef::DockerTag` → `EraContractsBackend::Docker`, host `work_dir` mounted at `/contracts/work/{work_name}`
    ///
    /// `work_name` is a subdirectory name for run isolation (e.g. run-id or UUID).
    ///
    /// `extra_mounts` are additional `(host_path, container_path)` volume mounts
    /// for Docker mode. They are ignored in local mode.
    pub fn from_preset(
        preset: &Preset,
        work_name: &str,
        extra_mounts: &[(&Path, &str)],
    ) -> Result<Self> {
        match &preset.era_contracts {
            RepoRef::Path(p) => Self::local(p, work_name),
            RepoRef::DockerTag { tag, .. } => {
                let image = format!("{}:{}", ERA_CONTRACTS_PROTOCOL_IMAGE_REPO, tag);
                Self::docker(&image, work_name, extra_mounts)
            }
        }
    }

    /// Create a local backend. Work directory is `era_contracts_path/work/{work_name}`.
    pub fn local(era_contracts_path: &Path, work_name: &str) -> Result<Self> {
        let era_path = fs::canonicalize(era_contracts_path).with_context(|| {
            format!(
                "canonicalize era-contracts path: {}",
                era_contracts_path.display()
            )
        })?;
        let work_dir = era_path.join("work").join(work_name);
        fs::create_dir_all(&work_dir)?;
        fs::create_dir_all(work_dir.join("script-out"))?;
        let work_dir = fs::canonicalize(&work_dir)?;
        Ok(EraContractsBackend::Local { era_path, work_dir })
    }

    /// Create a Docker backend. The host-side work_dir lives under
    /// `.test-run-logs/era_work_{work_name}` in the project tree so it is
    /// captured alongside other test artifacts.
    pub fn docker(image: &str, work_name: &str, extra_mounts: &[(&Path, &str)]) -> Result<Self> {
        let project_root = crate::infra::utils::find_project_root()
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let work_dir = project_root
            .join(".test-run-logs")
            .join(format!("era_work_{}", work_name));
        fs::create_dir_all(&work_dir)?;
        let work_dir = fs::canonicalize(&work_dir)?;
        let container_work = format!("{}/{}", CONTAINER_WORK_PREFIX, work_name);
        let session = EraContainerSession::start(image, &work_dir, &container_work, extra_mounts)?;
        Ok(EraContractsBackend::Docker { session, work_dir })
    }

    /// Host-side work directory where all outputs are stored.
    pub fn work_dir(&self) -> &Path {
        match self {
            EraContractsBackend::Local { work_dir, .. } => work_dir,
            EraContractsBackend::Docker { work_dir, .. } => work_dir,
        }
    }

    /// Return a path suitable for passing to tools running inside the backend.
    /// Both modes resolve this to the same physical location.
    ///
    /// - Local: absolute host path `{era_path}/work/{work_name}/{relative}`
    /// - Docker: absolute container path `/contracts/work/{work_name}/{relative}`
    pub fn work_path(&self, relative: &str) -> String {
        match self {
            EraContractsBackend::Local { work_dir, .. } => {
                work_dir.join(relative).to_string_lossy().to_string()
            }
            EraContractsBackend::Docker { session, .. } => {
                format!("{}/{}", session.container_work_dir(), relative)
            }
        }
    }

    /// Return a path relative to the era-contracts repo root, suitable for
    /// passing as a working directory or argument to tools.
    ///
    /// - Local: `{era_path}/{relative}`
    /// - Docker: `/contracts/{relative}`
    pub fn repo_path(&self, relative: &str) -> String {
        match self {
            EraContractsBackend::Local { era_path, .. } => {
                era_path.join(relative).to_string_lossy().to_string()
            }
            EraContractsBackend::Docker { .. } => format!("/contracts/{}", relative),
        }
    }

    /// Return the local era-contracts path, if this is a local backend.
    pub fn era_path(&self) -> Option<&Path> {
        match self {
            EraContractsBackend::Local { era_path, .. } => Some(era_path),
            EraContractsBackend::Docker { .. } => None,
        }
    }

    /// Run `protocol_ops <args>`.
    pub fn protocol_ops(&self, args: &[&str]) -> Result<String> {
        match self {
            EraContractsBackend::Local { era_path, .. } => run_protocol_ops_local(era_path, args),
            EraContractsBackend::Docker { session, .. } => session.protocol_ops(args),
        }
    }

    /// Run `forge script <args>` from the `l1-contracts` directory.
    ///
    /// In local mode, new files from `l1-contracts/script-out/` are synced
    /// into `work_dir/script-out/` after execution so callers can read them
    /// from `work_dir` regardless of mode.
    pub fn forge_script(&self, args: &[&str], envs: &[(&str, &str)]) -> Result<String> {
        match self {
            EraContractsBackend::Local { era_path, work_dir } => {
                let l1_contracts = era_path.join("l1-contracts");
                let mut cmd = std::process::Command::new("forge");
                cmd.current_dir(&l1_contracts).arg("script");
                for (k, v) in envs {
                    cmd.env(k, v);
                }
                for arg in args {
                    cmd.arg(arg);
                }
                let output = cmd.output().context("forge script")?;
                if !output.status.success() {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    anyhow::bail!(
                        "forge {:?} failed:\n\nSTDOUT:\n{}\n\nSTDERR:\n{}",
                        args,
                        stdout,
                        stderr
                    );
                }
                // Sync script-out files into work_dir so callers can read
                // from a single path regardless of mode.
                let src_dir = l1_contracts.join("script-out");
                let dst_dir = work_dir.join("script-out");
                fs::create_dir_all(&dst_dir)?;
                if src_dir.exists() {
                    for entry in fs::read_dir(&src_dir)? {
                        let entry = entry?;
                        if entry.file_type()?.is_file() {
                            fs::copy(entry.path(), dst_dir.join(entry.file_name()))?;
                        }
                    }
                }
                Ok(String::from_utf8_lossy(&output.stdout).to_string())
            }
            EraContractsBackend::Docker { session, .. } => session.forge_script(args, envs),
        }
    }

    /// Run `cast <args>`.
    pub fn cast(&self, args: &[&str]) -> Result<String> {
        match self {
            EraContractsBackend::Local { .. } => {
                let output = std::process::Command::new("cast")
                    .args(args)
                    .output()
                    .context("cast")?;
                if !output.status.success() {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    anyhow::bail!(
                        "cast {:?} failed:\n\nSTDOUT:\n{}\n\nSTDERR:\n{}",
                        args,
                        stdout,
                        stderr
                    );
                }
                Ok(String::from_utf8_lossy(&output.stdout).to_string())
            }
            EraContractsBackend::Docker { session, .. } => session.cast(args),
        }
    }

    /// Run an arbitrary command (e.g. zksync-os-genesis-gen, wallets-gen).
    pub fn run(&self, command: &[&str], workdir: Option<&str>) -> Result<String> {
        match self {
            EraContractsBackend::Local { .. } => {
                let mut cmd = std::process::Command::new(command[0]);
                if command.len() > 1 {
                    cmd.args(&command[1..]);
                }
                if let Some(wd) = workdir {
                    cmd.current_dir(wd);
                }
                let output = cmd.output().with_context(|| format!("run {:?}", command))?;
                if !output.status.success() {
                    anyhow::bail!(
                        "{:?} failed:\n{}",
                        command,
                        String::from_utf8_lossy(&output.stderr)
                    );
                }
                Ok(String::from_utf8_lossy(&output.stdout).to_string())
            }
            EraContractsBackend::Docker { session, .. } => session.exec(command, &[], workdir),
        }
    }

    /// Execute transactions from a protocol-ops `--out` file by calling
    /// `protocol_ops chain execute-simulated-transactions`.
    ///
    /// `out_relative` is a filename or relative path within the work directory
    /// (e.g. `"no_governance_prepare_out.json"`).
    pub fn execute_protocol_ops_out(
        &self,
        out_relative: &str,
        l1_rpc_url: &str,
        private_key: &str,
    ) -> Result<()> {
        let out_arg = self.work_path(out_relative);
        self.protocol_ops(&[
            "chain",
            "execute-simulated-transactions",
            "--out",
            &out_arg,
            "--l1-rpc-url",
            l1_rpc_url,
            "--private-key",
            private_key,
        ])
        .map(|_| ())
    }
}
