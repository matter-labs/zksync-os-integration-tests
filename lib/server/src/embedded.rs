use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result};
use reth_tasks::{Runtime, RuntimeBuilder, RuntimeConfig, TokioConfig};
use smart_config::{ConfigSources, Environment};
use tokio::runtime::Handle;
use zksync_os_server::config::{build_external_config, load_config_file_sources, Config};
use zksync_os_state_full_diffs::FullDiffsState;

use crate::wait::wait_for_rpc_ready;

/// A running `zksync-os-server` instance started in-process.
pub struct Server {
    /// Wrapped in `Option` so we can move it out in `stop()` despite `Drop`.
    runtime: Option<Runtime>,
    rpc_url: String,
    rocks_db_path: PathBuf,
}

impl Server {
    /// Start the server in-process using the provided config.
    ///
    /// Returns once the RPC endpoint is reachable (up to 30 s).
    pub async fn start(config: Config) -> Result<Self> {
        let rpc_url = config
            .rpc_config
            .address
            .replace("0.0.0.0:", "http://localhost:");
        let rocks_db_path = config.general_config.rocks_db_path.clone();

        let runtime = RuntimeBuilder::new(
            RuntimeConfig::default().with_tokio(TokioConfig::existing_handle(Handle::current())),
        )
        .build()
        .expect("failed to build reth runtime");

        zksync_os_server::run::<FullDiffsState>(&runtime, config).await;

        wait_for_rpc_ready(&rpc_url, Duration::from_secs(30))
            .await
            .context("server RPC did not become ready")?;

        Ok(Self {
            runtime: Some(runtime),
            rpc_url,
            rocks_db_path,
        })
    }

    pub fn rpc_url(&self) -> &str {
        &self.rpc_url
    }

    /// Shut down the server.
    ///
    /// If `archive_db_to` is `Some(dest)`, the RocksDB directory is packed into
    /// a gzip-compressed tar archive at `dest` before stopping. The archive uses
    /// a `node/` top-level prefix matching `zksync_os_server::util::unpack_ephemeral_state`.
    pub async fn stop(mut self, archive_db_to: Option<PathBuf>) -> Result<()> {
        if let Some(dest) = archive_db_to {
            archive_rocksdb(&self.rocks_db_path, &dest)
                .context("archive RocksDB before server stop")?;
        }
        // Drop the runtime to abort all reth-managed tasks.
        drop(self.runtime.take());
        Ok(())
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // Abort tasks if stop() was never called.
        drop(self.runtime.take());
    }
}

/// Build a `Config` by loading one or more YAML/JSON config files.
///
/// `l1_rpc_url` is set as the `L1_PROVIDER_RPC_URL` environment variable
/// before loading so the server can resolve its L1 provider.
///
/// # Panics
/// Panics if any config file cannot be read or parsed (same behaviour as the
/// server binary itself).
pub async fn load_config_from_yaml(config_paths: &[PathBuf], l1_rpc_url: &str) -> Config {
    // SAFETY: single-threaded at this point in the apply flow.
    unsafe { std::env::set_var("L1_PROVIDER_RPC_URL", l1_rpc_url) };

    let mut sources = ConfigSources::default();
    load_config_file_sources(&mut sources, config_paths);
    sources.push(Environment::default());
    let schema = Config::schema();
    let repo = smart_config::ConfigRepository::new(&schema).with_all(sources);
    build_external_config(repo).await
}

/// Pack `src_dir` into a gzip-compressed tar archive at `dest` with a `node/`
/// top-level prefix so it can be unpacked with
/// `zksync_os_server::util::unpack_ephemeral_state`.
fn archive_rocksdb(src_dir: &Path, dest: &Path) -> Result<()> {
    use flate2::{write::GzEncoder, Compression};
    use std::fs::File;

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create parent dir for {}", dest.display()))?;
    }

    let file =
        File::create(dest).with_context(|| format!("create archive at {}", dest.display()))?;
    let gz = GzEncoder::new(file, Compression::default());
    let mut archive = tar::Builder::new(gz);

    archive
        .append_dir_all("node", src_dir)
        .with_context(|| format!("archive {} → {}", src_dir.display(), dest.display()))?;

    archive.finish().context("finalise tar archive")?;
    Ok(())
}
