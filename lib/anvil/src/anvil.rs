use std::{
    path::PathBuf,
    process::Stdio,
    time::{Duration, Instant},
};

use alloy_primitives::{Address, U256};
use anyhow::{Context, Result};
use tokio::process::{Child, Command};

use crate::config::AnvilConfig;
use crate::port::pick_unused_port;

/// A running `anvil` process.
pub struct Anvil {
    child: Option<Child>,
    rpc_url: String,
    port: u16,
    /// Path to the state dump file, if dump_state was configured.
    dump_state: Option<PathBuf>,
}

impl Anvil {
    /// Spawn an `anvil` process with the given config.
    ///
    /// Blocks until the RPC endpoint is reachable (up to 30 s).
    pub async fn spawn(config: AnvilConfig) -> Result<Self> {
        let port = config.port.map(Ok).unwrap_or_else(pick_unused_port)?;
        let rpc_url = format!("http://127.0.0.1:{port}");

        let mut cmd = Command::new("anvil");
        cmd.arg("--port")
            .arg(port.to_string())
            .arg("--host")
            .arg("0.0.0.0")
            .arg("--chain-id")
            .arg(config.chain_id.to_string())
            .arg("--block-time")
            .arg(config.block_time_secs.to_string())
            .arg("--mixed-mining")
            .arg("--slots-in-an-epoch")
            .arg("10")
            .arg("--disable-block-gas-limit")
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        if let Some(ref path) = config.load_state {
            cmd.arg("--load-state").arg(path);
        }
        if let Some(ref path) = config.dump_state {
            cmd.arg("--dump-state")
                .arg(path)
                .arg("--preserve-historical-states");
        }

        let child = cmd
            .spawn()
            .context("failed to spawn `anvil` — is it installed and in PATH?")?;

        wait_for_rpc_ready(&rpc_url, Duration::from_secs(30)).await?;

        Ok(Self {
            child: Some(child),
            rpc_url,
            port,
            dump_state: config.dump_state,
        })
    }

    pub fn rpc_url(&self) -> &str {
        &self.rpc_url
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Graceful stop: SIGTERM → 10 s wait → SIGKILL fallback.
    ///
    /// If this Anvil was started with `dump_state`, blocks until the state
    /// file appears on disk (Anvil writes it on clean exit).
    ///
    /// Returns the dump path if configured, `None` otherwise.
    pub async fn stop(mut self) -> Result<Option<PathBuf>> {
        if let Some(mut child) = self.child.take() {
            send_sigterm(&child);
            match tokio::time::timeout(Duration::from_secs(10), child.wait()).await {
                Ok(Ok(_)) => {}
                _ => {
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                }
            }
        }

        if let Some(ref path) = self.dump_state {
            let deadline = Instant::now() + Duration::from_secs(15);
            while !path.exists() {
                if Instant::now() >= deadline {
                    anyhow::bail!("Anvil dump-state file never appeared at {}", path.display());
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }

        Ok(self.dump_state.clone())
    }

    /// Set an account balance via `anvil_setBalance`.
    pub async fn set_balance(&self, address: Address, wei: U256) -> Result<()> {
        let client = reqwest::Client::new();
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "anvil_setBalance",
            "params": [format!("{address:#x}"), format!("{wei:#x}")]
        });
        let resp = client
            .post(&self.rpc_url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("anvil_setBalance({address:#x})"))?;
        let json: serde_json::Value = resp
            .json()
            .await
            .context("anvil_setBalance response was not JSON")?;
        if let Some(err) = json.get("error") {
            anyhow::bail!("anvil_setBalance error: {err}");
        }
        Ok(())
    }
}

impl Drop for Anvil {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
        }
    }
}

#[cfg(unix)]
fn send_sigterm(child: &Child) {
    if let Some(pid) = child.id() {
        // SAFETY: pid is a valid OS pid from the child process.
        unsafe { libc::kill(pid as i32, libc::SIGTERM) };
    }
}

#[cfg(not(unix))]
fn send_sigterm(_child: &Child) {}

async fn wait_for_rpc_ready(rpc_url: &str, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1,
        "method": "eth_chainId", "params": []
    });
    loop {
        if let Ok(resp) = client.post(rpc_url).json(&body).send().await {
            if resp.status().is_success() {
                return Ok(());
            }
        }
        if Instant::now() >= deadline {
            anyhow::bail!("Anvil RPC at {rpc_url} did not become ready within {timeout:?}");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}
