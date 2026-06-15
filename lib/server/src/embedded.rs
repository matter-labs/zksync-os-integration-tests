use std::{path::PathBuf, time::Duration};

use anyhow::{Context, Result};
use reth_tasks::{Runtime, RuntimeBuilder, RuntimeConfig, TokioConfig};
use smart_config::{ConfigSources, Environment};
use tokio::runtime::Handle;
use zksync_os_server::config::{build_external_config, load_config_file_sources, Config};
use zksync_os_state_full_diffs::FullDiffsState;

use crate::wait::wait_for_rpc_ready;

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);

/// A running `zksync-os-server` instance started in-process.
pub struct Server {
    runtime: Option<Runtime>,
    rpc_url: String,
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
        })
    }

    pub fn rpc_url(&self) -> &str {
        &self.rpc_url
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(rt) = self.runtime.take() {
            rt.graceful_shutdown_with_timeout(SHUTDOWN_TIMEOUT);
        }
    }
}

/// Build a `Config` by loading one or more YAML/JSON config files.
///
/// # Panics
/// Panics if any config file cannot be read or parsed.
pub async fn load_config_from_yaml(config_paths: &[PathBuf]) -> Config {
    let mut sources = ConfigSources::default();
    load_config_file_sources(&mut sources, config_paths);
    sources.push(Environment::default());
    let schema = Config::schema();
    let repo = smart_config::ConfigRepository::new(&schema).with_all(sources);
    build_external_config(repo).await
}
