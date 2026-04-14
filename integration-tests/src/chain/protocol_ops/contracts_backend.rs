use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::presets::{Preset, RepoRef};

use super::{run_protocol_ops_local, ContractsContainerSession, ERA_CONTRACTS_PROTOCOL_IMAGE_REPO};

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
        session: ContractsContainerSession,
        work_dir: PathBuf,
    },
}

/// Container-side mount point for the stable `test-run-logs` directory.
const CONTAINER_LOGS_MOUNT: &str = "/contracts/test-run-logs";

impl EraContractsBackend {
    /// Create an `EraContractsBackend` from a preset configuration.
    ///
    /// `run_name` identifies the test run (e.g. `"gateway_settling"`). In Docker
    /// mode, artifacts land under `test-run-logs/{run_name}/contracts_artifacts/`.
    ///
    /// `extra_mounts` are additional `(host_path, container_path)` volume mounts
    /// for Docker mode. They are ignored in local mode.
    pub fn from_preset(
        preset: &Preset,
        run_name: &str,
        extra_mounts: &[(&Path, &str)],
    ) -> Result<Self> {
        match &preset.era_contracts {
            RepoRef::Path(p) => Self::local(p, run_name),
            RepoRef::DockerTag { tag, .. } => {
                let image = format!("{}:{}", ERA_CONTRACTS_PROTOCOL_IMAGE_REPO, tag);
                Self::docker(&image, run_name, extra_mounts)
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
    /// `test-run-logs/{run_id}/contracts_artifacts/` in the project tree so
    /// it is captured alongside other test artifacts.
    ///
    /// Only the stable `test-run-logs/` directory is bind-mounted into Docker;
    /// sub-directories are created inside the container after startup, working
    /// around a macOS Docker/VirtioFS bug where newly-created host directories
    /// are invisible to the VM.
    pub fn docker(image: &str, _run_name: &str, extra_mounts: &[(&Path, &str)]) -> Result<Self> {
        let project_root =
            crate::infra::utils::find_project_root().map_err(|e| anyhow::anyhow!("{e}"))?;
        let run_id = crate::infra::server::get_run_id().ok_or_else(|| {
            anyhow::anyhow!(
                "No run ID set. Call `integration_tests::server::get_or_create_run_id(\"test_name\")` \
                 before creating an EraContractsBackend."
            )
        })?;
        let logs_root = project_root.join("test-run-logs");
        fs::create_dir_all(&logs_root)?;
        let work_dir = logs_root.join(run_id).join("contracts_artifacts");
        fs::create_dir_all(&work_dir)?;
        let work_dir = fs::canonicalize(&work_dir)?;
        // container_work_dir is a sub-path under the logs mount.
        let relative = work_dir
            .strip_prefix(fs::canonicalize(&logs_root)?)
            .context("work_dir must be under test-run-logs")?;
        let container_work = format!("{}/{}", CONTAINER_LOGS_MOUNT, relative.display());
        let session = ContractsContainerSession::start(
            image,
            &work_dir,
            &container_work,
            &logs_root,
            CONTAINER_LOGS_MOUNT,
            extra_mounts,
        )?;
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

    /// Read a protocol_ops output file (written via `--out`) from the work directory.
    ///
    /// - Local: `fs::read_to_string({work_dir}/{relative})`
    /// - Docker: `docker exec cat {container_work_dir}/{relative}`
    pub fn read_protocol_ops_output(&self, relative: &str) -> Result<String> {
        match self {
            EraContractsBackend::Local { work_dir, .. } => {
                let path = work_dir.join(relative);
                fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))
            }
            EraContractsBackend::Docker { session, .. } => {
                let path = format!("{}/{}", session.container_work_dir(), relative);
                session
                    .exec(&["cat", &path], &[], None)
                    .with_context(|| format!("read {}", relative))
            }
        }
    }

    /// Read a file relative to the era-contracts repo root.
    ///
    /// - Local: `fs::read_to_string({era_path}/{relative})`
    /// - Docker: `docker exec cat /contracts/{relative}`
    pub fn read_repo_file(&self, relative: &str) -> Result<String> {
        match self {
            EraContractsBackend::Local { era_path, .. } => {
                let path = era_path.join(relative);
                fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))
            }
            EraContractsBackend::Docker { session, .. } => {
                let path = format!("/contracts/{}", relative);
                session
                    .exec(&["cat", &path], &[], None)
                    .with_context(|| format!("read {}", relative))
            }
        }
    }

    /// Write a file relative to the era-contracts repo root. Parent
    /// directories are created if missing.
    ///
    /// - Local: `fs::write({era_path}/{relative}, contents)`
    /// - Docker: writes through the mounted work_dir, then `mv` inside the
    ///   container into `/contracts/{relative}` (avoids `docker cp` stdin
    ///   plumbing and host-side temp file handling).
    pub fn write_repo_file(&self, relative: &str, contents: &str) -> Result<()> {
        match self {
            EraContractsBackend::Local { era_path, .. } => {
                let path = era_path.join(relative);
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)
                        .with_context(|| format!("create {}", parent.display()))?;
                }
                fs::write(&path, contents).with_context(|| format!("write {}", path.display()))
            }
            EraContractsBackend::Docker { session, work_dir } => {
                let tempfile_name = format!(".write_repo_{}.tmp", uuid::Uuid::new_v4().simple());
                let host_tempfile = work_dir.join(&tempfile_name);
                fs::write(&host_tempfile, contents)
                    .with_context(|| format!("write temp {}", host_tempfile.display()))?;

                let container_target = format!("/contracts/{}", relative);
                let container_tempfile =
                    format!("{}/{}", session.container_work_dir(), tempfile_name);

                let result: Result<()> = (|| {
                    if let Some(parent) = Path::new(&container_target).parent() {
                        let parent_str = parent.to_string_lossy().to_string();
                        session
                            .exec(&["mkdir", "-p", &parent_str], &[], None)
                            .with_context(|| format!("mkdir -p {parent_str} in container"))?;
                    }
                    session
                        .exec(&["mv", &container_tempfile, &container_target], &[], None)
                        .with_context(|| {
                            format!("mv {container_tempfile} -> {container_target}")
                        })?;
                    Ok(())
                })();

                let _ = fs::remove_file(&host_tempfile);
                result
            }
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

    /// Run `forge <args>` from the `l1-contracts` directory.
    /// Unlike `forge_script`, this runs an arbitrary forge subcommand (e.g. `inspect`).
    pub fn forge(&self, args: &[&str]) -> Result<String> {
        match self {
            EraContractsBackend::Local { era_path, .. } => {
                let l1_contracts = era_path.join("l1-contracts");
                let output = std::process::Command::new("forge")
                    .current_dir(&l1_contracts)
                    .args(args)
                    .output()
                    .context("forge")?;
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
                Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
            }
            EraContractsBackend::Docker { session, .. } => {
                let mut command: Vec<String> = vec!["forge".into()];
                command.extend(args.iter().map(|a| a.to_string()));
                let cmd_refs: Vec<&str> = command.iter().map(|s| s.as_str()).collect();
                session
                    .exec(&cmd_refs, &[], Some("/contracts/l1-contracts"))
                    .map(|s| s.trim().to_string())
            }
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
    /// `protocol_ops dev execute-transactions`. Renamed from
    /// `chain execute-simulated-transactions` upstream.
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
            "dev",
            "execute-transactions",
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
