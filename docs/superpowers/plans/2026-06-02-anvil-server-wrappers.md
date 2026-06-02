# Anvil & Server Wrapper Crates Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Create `lib/anvil` and `lib/server` wrapper crates, move `zk-deployer` into the workspace as `bin/zk-deployer`, and wire managed gateway server lifecycle into `apply`.

**Architecture:** Two new library crates with clean, focused APIs; `lib/anvil` wraps an `anvil` subprocess with optional state dump/load, `lib/server` wraps `zksync_os_server` in-process behind an `embedded-server` feature flag. `zk-deployer` moves to `bin/` and gains Anvil + server lifecycle for gateway-settling deployments.

**Tech Stack:** Rust (nightly), `tokio`, `alloy 2.0.1`, `zksync_os_server` (git), `reth-tasks`, `flate2`/`tar` for RocksDB archiving.

---

## File Map

### New files
| File | Purpose |
|------|---------|
| `lib/anvil/Cargo.toml` | Crate manifest |
| `lib/anvil/src/lib.rs` | Re-exports, `pub use` |
| `lib/anvil/src/config.rs` | `AnvilConfig` + `Default` |
| `lib/anvil/src/port.rs` | `pick_unused_port()` |
| `lib/anvil/src/anvil.rs` | `Anvil` struct: spawn, stop, set_balance, Drop |
| `lib/anvil/tests/integration.rs` | Integration tests (require `anvil` in PATH) |
| `lib/server/Cargo.toml` | Crate manifest with `embedded-server` feature |
| `lib/server/src/lib.rs` | Re-exports + `wait_for_rpc_ready`, `wait_for_l2_block_finalized` |
| `lib/server/src/wait.rs` | Waiting function implementations |
| `lib/server/src/embedded.rs` | `Server` struct (behind `embedded-server` feature) |
| `bin/zk-deployer/` | Moved from `tools/zk-deployer/` |

### Modified files
| File | Change |
|------|--------|
| `Cargo.toml` | Add `lib/anvil`, `lib/server`, `bin/zk-deployer` to members; remove `tools/zk-deployer` exclude |
| `bin/zk-deployer/Cargo.toml` | Remove `[workspace]` section; add `lib-anvil`, `lib-server` deps |
| `bin/zk-deployer/src/intent.rs` | Add optional `l1_rpc_url` to `L1Config` (or top-level) |
| `bin/zk-deployer/src/commands/apply/mod.rs` | Managed gateway server lifecycle; skip `set_balance` for external L1 |

---

## Task 1: Workspace restructuring — move zk-deployer to bin/

**Files:**
- Modify: `Cargo.toml`
- Move: `tools/zk-deployer/` → `bin/zk-deployer/`
- Modify: `bin/zk-deployer/Cargo.toml`

- [ ] **Step 1: Move the directory**

```bash
mv tools/zk-deployer bin/zk-deployer
```

- [ ] **Step 2: Update workspace Cargo.toml**

Replace the current content of `Cargo.toml`:

```toml
[workspace]
resolver = "2"
members = [
    "integration-tests",
    "deprecated/build-artifacts",
    "deprecated/generate-l1-state",
    "bin/zk-deployer",
    "lib/anvil",
    "lib/server",
]
```

(`lib/anvil` and `lib/server` are listed now so cargo doesn't error when the dirs don't exist yet — add them as empty crates in the next step.)

- [ ] **Step 3: Strip [workspace] from bin/zk-deployer/Cargo.toml**

Remove these lines from `bin/zk-deployer/Cargo.toml`:

```toml
[workspace]
resolver = "2"
```

The crate is now part of the root workspace.

- [ ] **Step 4: Remove empty tools/ directory**

```bash
rmdir tools
```

- [ ] **Step 5: Verify build still passes**

```bash
cargo build -p zk-deployer
```

Expected: compiles successfully (same as before).

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "chore: move zk-deployer from tools/ to bin/"
```

---

## Task 2: Create lib/anvil — scaffolding + config

**Files:**
- Create: `lib/anvil/Cargo.toml`
- Create: `lib/anvil/src/lib.rs`
- Create: `lib/anvil/src/config.rs`
- Create: `lib/anvil/src/port.rs`

- [ ] **Step 1: Create lib/anvil/Cargo.toml**

```toml
[package]
name = "lib-anvil"
version = "0.1.0"
edition = "2021"

[dependencies]
alloy = { version = "2.0.1", default-features = false, features = [
    "providers",
    "transport-http",
    "reqwest",
] }
anyhow = "1.0"
tokio = { version = "1", features = ["full"] }
```

- [ ] **Step 2: Create lib/anvil/src/config.rs**

```rust
use std::path::PathBuf;

pub struct AnvilConfig {
    /// TCP port to listen on. `None` → auto-pick an unused port.
    pub port: Option<u16>,
    /// EVM chain ID. Default: 31337.
    pub chain_id: u64,
    /// Block production interval in seconds. Default: 0.25.
    pub block_time_secs: f64,
    /// If set, Anvil is started with `--dump-state <path>` and
    /// `--preserve-historical-states`. `stop()` blocks until the file exists.
    pub dump_state: Option<PathBuf>,
    /// If set, Anvil is started with `--load-state <path>`.
    pub load_state: Option<PathBuf>,
}

impl Default for AnvilConfig {
    fn default() -> Self {
        Self {
            port: None,
            chain_id: 31337,
            block_time_secs: 0.25,
            dump_state: None,
            load_state: None,
        }
    }
}
```

- [ ] **Step 3: Create lib/anvil/src/port.rs**

```rust
use anyhow::Context;
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener};

/// Ask the OS for an unused port by binding to port 0.
/// There is a brief TOCTOU window between returning the port and the caller
/// binding to it, which is acceptable for local dev / CI use.
pub fn pick_unused_port() -> anyhow::Result<u16> {
    let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
        .context("bind port 0 to pick unused port")?;
    Ok(listener.local_addr()?.port())
}
```

- [ ] **Step 4: Create lib/anvil/src/lib.rs**

```rust
mod config;
mod port;
pub mod anvil;

pub use anvil::Anvil;
pub use config::AnvilConfig;
```

- [ ] **Step 5: Create a placeholder anvil.rs so it compiles**

```rust
// placeholder — implemented in Task 3
pub struct Anvil;
```

- [ ] **Step 6: Verify it compiles**

```bash
cargo build -p lib-anvil
```

Expected: compiles with no errors.

- [ ] **Step 7: Commit**

```bash
git add lib/anvil/
git commit -m "feat(lib-anvil): add crate scaffolding, AnvilConfig, port picker"
```

---

## Task 3: lib/anvil — Anvil spawn + readiness wait

**Files:**
- Modify: `lib/anvil/src/anvil.rs`

- [ ] **Step 1: Write the failing test**

Create `lib/anvil/tests/integration.rs`:

```rust
//! Integration tests — require `anvil` binary in PATH.

use lib_anvil::{Anvil, AnvilConfig};
use std::time::Duration;

#[tokio::test]
async fn test_anvil_spawns_and_rpc_responds() {
    let anvil = Anvil::spawn(AnvilConfig::default()).await.unwrap();
    let url = anvil.rpc_url().to_string();

    // Basic JSON-RPC call must succeed.
    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1,
            "method": "eth_chainId", "params": []
        }))
        .send()
        .await
        .unwrap();
    let json: serde_json::Value = resp.json().await.unwrap();
    assert!(json["result"].is_string());

    anvil.stop().await.unwrap();
}
```

Add to `lib/anvil/Cargo.toml`:

```toml
[dev-dependencies]
reqwest = { version = "0.12", features = ["json"] }
serde_json = "1.0"
tokio = { version = "1", features = ["full"] }
```

- [ ] **Step 2: Run test to confirm it fails**

```bash
cargo test -p lib-anvil --test integration test_anvil_spawns_and_rpc_responds
```

Expected: compile error — `Anvil::spawn` not implemented yet.

- [ ] **Step 3: Implement lib/anvil/src/anvil.rs**

```rust
use std::{
    path::PathBuf,
    process::Stdio,
    time::{Duration, Instant},
};

use alloy::primitives::{Address, U256};
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
        cmd.arg("--port").arg(port.to_string())
            .arg("--host").arg("0.0.0.0")
            .arg("--chain-id").arg(config.chain_id.to_string())
            .arg("--block-time").arg(config.block_time_secs.to_string())
            .arg("--mixed-mining")
            .arg("--slots-in-an-epoch").arg("10")
            .arg("--disable-block-gas-limit")
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        if let Some(ref path) = config.load_state {
            cmd.arg("--load-state").arg(path);
        }
        if let Some(ref path) = config.dump_state {
            cmd.arg("--dump-state").arg(path)
               .arg("--preserve-historical-states");
        }

        let child = cmd.spawn().context("failed to spawn `anvil` — is it installed?")?;

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

    /// Graceful stop: SIGTERM → 10 s wait → SIGKILL.
    ///
    /// If this Anvil was started with `dump_state`, blocks until the state
    /// file appears on disk (Anvil writes it on clean exit).
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
            // Wait up to 15 s for the state file to materialise.
            let deadline = Instant::now() + Duration::from_secs(15);
            while !path.exists() {
                if Instant::now() >= deadline {
                    anyhow::bail!(
                        "Anvil dump-state file never appeared at {}",
                        path.display()
                    );
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
            .with_context(|| format!("anvil_setBalance({address:#x}, {wei})"))?;
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

/// Send SIGTERM to the child process (Unix only).
#[cfg(unix)]
fn send_sigterm(child: &Child) {
    if let Some(pid) = child.id() {
        // SAFETY: pid is a valid OS pid obtained from the child.
        unsafe { libc::kill(pid as i32, libc::SIGTERM) };
    }
}

#[cfg(not(unix))]
fn send_sigterm(_child: &Child) {
    // On non-Unix platforms fall through to SIGKILL in stop().
}

/// Poll the RPC endpoint until it responds to `eth_chainId`, up to `timeout`.
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
```

Add `libc` to `lib/anvil/Cargo.toml`:

```toml
[target.'cfg(unix)'.dependencies]
libc = "0.2"
```

Also add `reqwest` and `serde_json`:

```toml
reqwest = { version = "0.12", features = ["json"] }
serde_json = "1.0"
```

- [ ] **Step 4: Run test — expect pass**

```bash
cargo test -p lib-anvil --test integration test_anvil_spawns_and_rpc_responds
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add lib/anvil/
git commit -m "feat(lib-anvil): implement Anvil spawn, readiness wait, stop, set_balance"
```

---

## Task 4: lib/anvil — dump_state and load_state tests

**Files:**
- Modify: `lib/anvil/tests/integration.rs`

- [ ] **Step 1: Write failing tests**

Append to `lib/anvil/tests/integration.rs`:

```rust
#[tokio::test]
async fn test_dump_and_load_state() {
    let dir = tempfile::tempdir().unwrap();
    let state_path = dir.path().join("state.json");

    // Start with dump enabled, set a balance, then stop (triggers dump).
    let config = lib_anvil::AnvilConfig {
        dump_state: Some(state_path.clone()),
        ..Default::default()
    };
    let anvil = Anvil::spawn(config).await.unwrap();
    let addr: Address = "0x1111111111111111111111111111111111111111"
        .parse()
        .unwrap();
    anvil
        .set_balance(addr, U256::from(12345678u64))
        .await
        .unwrap();
    let returned = anvil.stop().await.unwrap();
    assert_eq!(returned, Some(state_path.clone()));
    assert!(state_path.exists(), "state file must exist after stop");

    // Reload state and verify balance persisted.
    let config2 = lib_anvil::AnvilConfig {
        load_state: Some(state_path),
        ..Default::default()
    };
    let anvil2 = Anvil::spawn(config2).await.unwrap();
    let client = reqwest::Client::new();
    let resp = client
        .post(anvil2.rpc_url())
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1,
            "method": "eth_getBalance",
            "params": [format!("{addr:#x}"), "latest"]
        }))
        .send()
        .await
        .unwrap();
    let json: serde_json::Value = resp.json().await.unwrap();
    let hex = json["result"].as_str().unwrap();
    let balance = U256::from_str_radix(hex.trim_start_matches("0x"), 16).unwrap();
    assert_eq!(balance, U256::from(12345678u64));
    anvil2.stop().await.unwrap();
}
```

Add to `[dev-dependencies]` in `lib/anvil/Cargo.toml`:

```toml
tempfile = "3"
alloy = { version = "2.0.1", default-features = false, features = ["primitives"] }
```

- [ ] **Step 2: Run tests — expect pass**

```bash
cargo test -p lib-anvil --test integration
```

Expected: both tests PASS.

- [ ] **Step 3: Commit**

```bash
git add lib/anvil/
git commit -m "test(lib-anvil): add dump/load state round-trip test"
```

---

## Task 5: Create lib/server — scaffolding + waiting functions

**Files:**
- Create: `lib/server/Cargo.toml`
- Create: `lib/server/src/lib.rs`
- Create: `lib/server/src/wait.rs`

- [ ] **Step 1: Create lib/server/Cargo.toml**

```toml
[package]
name = "lib-server"
version = "0.1.0"
edition = "2021"

[features]
default = []
embedded-server = [
    "dep:zksync_os_server",
    "dep:zksync_os_state_full_diffs",
    "dep:reth-tasks",
    "dep:flate2",
    "dep:tar",
]

[dependencies]
anyhow = "1.0"
tokio = { version = "1", features = ["full"] }
reqwest = { version = "0.12", features = ["json"] }
serde_json = "1.0"

# Optional — behind embedded-server feature
zksync_os_server = { git = "https://github.com/matter-labs/zksync-os-server", branch = "main", optional = true }
zksync_os_state_full_diffs = { git = "https://github.com/matter-labs/zksync-os-server", branch = "main", optional = true }
reth-tasks = { git = "https://github.com/itegulov/reth.git", rev = "ff8afdc5dbc253019df5b68f4b231f55daeb2e00", optional = true }
flate2 = { version = "1.0", optional = true }
tar = { version = "0.4", optional = true }
```

> Note: use the same `reth` git rev that `zksync_os_server` uses in its workspace — check
> `../zksync-os-server/Cargo.toml` for the exact rev.

- [ ] **Step 2: Create lib/server/src/wait.rs**

```rust
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

/// Poll `url` with `eth_chainId` until it responds successfully or `timeout` elapses.
pub async fn wait_for_rpc_ready(url: &str, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1,
        "method": "eth_chainId", "params": []
    });
    loop {
        if let Ok(resp) = client.post(url).json(&body).send().await {
            if resp.status().is_success() {
                return Ok(());
            }
        }
        if Instant::now() >= deadline {
            anyhow::bail!("RPC at {url} did not become ready within {timeout:?}");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Poll L2 `eth_getBlockByNumber("finalized")` until the finalized block number
/// reaches at least `target_block`, or `timeout` elapses.
///
/// The "finalized" tag on a ZKsync OS L2 node advances when the corresponding
/// L1 batch execution transaction is confirmed on L1. Use this after submitting
/// L2 transactions to wait for full finality.
pub async fn wait_for_l2_block_finalized(
    l2_rpc: &str,
    target_block: u64,
    timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        match get_l2_finalized_block(l2_rpc).await {
            Ok(finalized) if finalized >= target_block => return Ok(()),
            _ => {}
        }
        if Instant::now() >= deadline {
            anyhow::bail!(
                "L2 finalized block did not reach {target_block} within {timeout:?}"
            );
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Return the current L2 finalized block number via `eth_getBlockByNumber("finalized")`.
pub async fn get_l2_finalized_block(l2_rpc: &str) -> Result<u64> {
    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1,
        "method": "eth_getBlockByNumber",
        "params": ["finalized", false]
    });
    let resp = client
        .post(l2_rpc)
        .json(&body)
        .send()
        .await
        .context("eth_getBlockByNumber(finalized)")?;
    let json: serde_json::Value = resp.json().await?;
    let hex = json["result"]["number"]
        .as_str()
        .context("finalized block has no number field")?;
    let n = u64::from_str_radix(hex.trim_start_matches("0x"), 16)
        .context("parse finalized block number")?;
    Ok(n)
}
```

- [ ] **Step 3: Create lib/server/src/lib.rs**

```rust
mod wait;

pub use wait::{get_l2_finalized_block, wait_for_l2_block_finalized, wait_for_rpc_ready};

#[cfg(feature = "embedded-server")]
mod embedded;
#[cfg(feature = "embedded-server")]
pub use embedded::Server;
#[cfg(feature = "embedded-server")]
pub use zksync_os_server::config::Config;
```

- [ ] **Step 4: Create a placeholder embedded.rs so the feature compiles**

```rust
// placeholder — implemented in Task 6
pub struct Server;
```

- [ ] **Step 5: Verify lib-server compiles (without feature)**

```bash
cargo build -p lib-server
```

Expected: compiles. The `embedded-server` feature is not enabled so `embedded.rs` is compiled as a stub.

- [ ] **Step 6: Commit**

```bash
git add lib/server/
git commit -m "feat(lib-server): add crate scaffolding and waiting functions"
```

---

## Task 6: lib/server — Server struct (embedded-server feature)

**Files:**
- Modify: `lib/server/src/embedded.rs`

- [ ] **Step 1: Implement lib/server/src/embedded.rs**

```rust
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result};
use reth_tasks::{Runtime, RuntimeBuilder, RuntimeConfig, TokioConfig};
use tokio::runtime::Handle;
use zksync_os_server::config::Config;
use zksync_os_state_full_diffs::FullDiffsState;

use crate::wait::wait_for_rpc_ready;

/// A running `zksync-os-server` instance started in-process.
pub struct Server {
    runtime: Runtime,
    rpc_url: String,
    /// Path to the RocksDB directory — needed for archiving on stop.
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
            RuntimeConfig::default()
                .with_tokio(TokioConfig::existing_handle(Handle::current())),
        )
        .build()
        .expect("failed to build reth runtime");

        zksync_os_server::run::<FullDiffsState>(&runtime, config).await;

        wait_for_rpc_ready(&rpc_url, Duration::from_secs(30))
            .await
            .context("server RPC did not become ready")?;

        Ok(Self {
            runtime,
            rpc_url,
            rocks_db_path,
        })
    }

    pub fn rpc_url(&self) -> &str {
        &self.rpc_url
    }

    /// Shut down the server.
    ///
    /// If `archive_db_to` is `Some(dest)`, the RocksDB directory is packed
    /// into a gzip-compressed tar archive at `dest` before the server stops.
    /// The archive uses a `node/` top-level directory so it can be unpacked
    /// with `zksync_os_server::util::unpack_ephemeral_state`.
    pub async fn stop(self, archive_db_to: Option<PathBuf>) -> Result<()> {
        if let Some(dest) = archive_db_to {
            archive_rocksdb(&self.rocks_db_path, &dest)
                .context("archive RocksDB before server stop")?;
        }

        // Drop the runtime — this aborts all reth-managed tasks.
        drop(self.runtime);
        Ok(())
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // Runtime drop aborts tasks if stop() was never called.
    }
}

/// Pack `src_dir` into a gzip-compressed tar archive at `dest`, adding a
/// single `node/` top-level prefix so it can be unpacked with
/// `zksync_os_server::util::unpack_ephemeral_state`.
fn archive_rocksdb(src_dir: &Path, dest: &Path) -> Result<()> {
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::fs::File;

    let file = File::create(dest)
        .with_context(|| format!("create archive at {}", dest.display()))?;
    let gz = GzEncoder::new(file, Compression::default());
    let mut archive = tar::Builder::new(gz);

    // Append the directory under the `node/` prefix.
    archive
        .append_dir_all("node", src_dir)
        .with_context(|| format!("archive {} → {}", src_dir.display(), dest.display()))?;

    archive.finish().context("finalise tar archive")?;
    Ok(())
}
```

- [ ] **Step 2: Verify lib-server compiles with embedded-server feature**

```bash
cargo build -p lib-server --features embedded-server
```

Expected: compiles. The server dep will be fetched from git on first run — this may take a minute.

- [ ] **Step 3: Commit**

```bash
git add lib/server/
git commit -m "feat(lib-server): implement Server struct with embedded-server feature"
```

---

## Task 7: bin/zk-deployer — add lib deps + intent l1_rpc_url

**Files:**
- Modify: `bin/zk-deployer/Cargo.toml`
- Modify: `bin/zk-deployer/src/intent.rs`
- Modify: `bin/zk-deployer/src/commands/apply/mod.rs`

- [ ] **Step 1: Add lib deps to bin/zk-deployer/Cargo.toml**

Add to `[dependencies]`:

```toml
lib-anvil = { path = "../../lib/anvil" }
lib-server = { path = "../../lib/server", features = ["embedded-server"] }
```

- [ ] **Step 2: Read current intent.rs to understand structure**

```bash
cat bin/zk-deployer/src/intent.rs
```

- [ ] **Step 3: Add optional l1_rpc_url to intent**

Find the top-level `Intent` struct (or wherever L1 config lives) in `bin/zk-deployer/src/intent.rs`. Add:

```rust
/// Optional external L1 RPC URL. When provided, zk-deployer uses this
/// endpoint instead of managing a local Anvil instance.
/// Required for real network deployments (Sepolia, mainnet).
/// When absent, zk-deployer expects a local Anvil to already be running
/// (started by the caller or by a future managed-Anvil mode).
#[serde(default, skip_serializing_if = "Option::is_none")]
pub l1_rpc_url: Option<String>,
```

- [ ] **Step 4: Verify build**

```bash
cargo build -p zk-deployer
```

Expected: compiles.

- [ ] **Step 5: Commit**

```bash
git add bin/zk-deployer/
git commit -m "feat(zk-deployer): add lib-anvil/lib-server deps and l1_rpc_url in intent"
```

---

## Task 8: bin/zk-deployer — managed gateway server in apply

**Files:**
- Modify: `bin/zk-deployer/src/commands/apply/mod.rs`

The `apply` command currently requires `--gateway-rpc-url` to be passed manually for the
gateway migration phases. This task makes it **optional**: when absent and gateway-settling
chains exist, `apply` starts a managed server, waits for finality, then stops and archives.

- [ ] **Step 1: Read the current apply command**

```bash
cat bin/zk-deployer/src/commands/apply/mod.rs
```

Identify:
- Where `gateway_rpc_url` is first used (the `if let Some(gw_url) = &args.gateway_rpc_url` branch)
- Where operators are funded on the gateway L2 (the `fund_on_gateway` calls)
- How to get the gateway server config path from state

- [ ] **Step 2: Add server start/stop around gateway migration phase**

In `bin/zk-deployer/src/commands/apply/mod.rs`, find the section where gateway-settling
chains are processed. It currently has a structure like:

```rust
if let Some(gw_url) = &args.gateway_rpc_url {
    // run migration phases
} else {
    // print instructions and exit
}
```

Replace the `else` branch (the "print instructions and exit" path) with managed server startup:

```rust
} else if has_gateway_settling_chains {
    // No --gateway-rpc-url provided — start a managed server.
    use lib_server::{Server, wait_for_l2_block_finalized};
    use std::time::Duration;

    let server_config = build_gateway_server_config(&state, &intent, &args)?;
    let rocks_db_path = server_config.general_config.rocks_db_path.clone();

    logger::info("Starting managed gateway server...");
    let server = Server::start(server_config)
        .await
        .context("start managed gateway server")?;

    let gw_url = server.rpc_url().to_string();

    // Run migration phases using the managed server URL.
    run_gateway_migration_phases(&gw_url, &state, &intent, &args).await?;

    // Wait for operator deposits to reach finality before archiving.
    let finalized = lib_server::get_l2_finalized_block(&gw_url).await
        .context("get finalized block before archive")?;
    wait_for_l2_block_finalized(&gw_url, finalized + 1, Duration::from_secs(300))
        .await
        .context("wait for gateway L2 finality")?;

    // Archive RocksDB so the gateway state can be loaded by tests.
    let archive_dest = args.output_dir.join("gateway-state.tar.gz");
    logger::info(&format!("Archiving gateway RocksDB → {}", archive_dest.display()));
    server.stop(Some(archive_dest.clone())).await
        .context("stop managed gateway server and archive RocksDB")?;

    logger::success(&format!(
        "Gateway state archived to {}",
        archive_dest.display()
    ));
}
```

> Note: `build_gateway_server_config` and `run_gateway_migration_phases` are the names
> you should use for helper functions extracted in the next step.

- [ ] **Step 3: Extract gateway migration into a helper function**

To avoid duplicating migration logic between the managed and external paths, extract the
migration calls into `run_gateway_migration_phases(gw_url: &str, ...)`:

```rust
async fn run_gateway_migration_phases(
    gw_url: &str,
    state: &ResolvedState,
    intent: &Intent,
    args: &ApplyArgs,
) -> anyhow::Result<()> {
    // Move the existing migration phase code here (Phase 1, Phase 2, Phase 3,
    // fund_on_gateway calls). Replace hard-coded `args.gateway_rpc_url.as_deref()`
    // references with `gw_url`.
    todo!("move existing gateway migration code here")
}
```

Fill the `todo!` by cutting and pasting the existing migration code from the `if let Some(gw_url)` branch.

- [ ] **Step 4: Add build_gateway_server_config helper**

Add a helper that builds the `lib_server::Config` from the current state + intent:

```rust
fn build_gateway_server_config(
    state: &ResolvedState,
    intent: &Intent,
    args: &ApplyArgs,
) -> anyhow::Result<lib_server::Config> {
    use zk_deployer::commands::server_config;

    // Re-use the existing server_config command logic to build the Config.
    // The gateway chain is always the first chain with role = gateway.
    let gateway_chain = intent
        .chains
        .iter()
        .find(|c| c.role == crate::intent::ChainRole::Gateway)
        .context("no gateway chain in intent")?;

    server_config::build_config_for_chain(
        gateway_chain,
        state,
        intent,
        &args.output_dir,
    )
}
```

> Note: `server_config::build_config_for_chain` is the function you need to add/expose
> in `commands/server_config.rs`. It should extract the Config-building logic that
> `server_config::run` currently calls inline.

- [ ] **Step 5: Verify build**

```bash
cargo build -p zk-deployer
```

Expected: compiles. Fix any type errors from the refactor.

- [ ] **Step 6: End-to-end smoke check against local Anvil**

```bash
# In one terminal: start a fresh Anvil
anvil --port 8545 --chain-id 31337

# In another: run bootstrap + apply (L1-settling only, no gateway — quick)
cd bin/zk-deployer
cargo run -- init --scenario l1-only --output intent.yaml
cargo run -- bootstrap --intent intent.yaml --broadcast
cargo run -- apply --intent intent.yaml --broadcast
```

Expected: completes without error.

- [ ] **Step 7: Commit**

```bash
git add bin/zk-deployer/
git commit -m "feat(zk-deployer): managed gateway server lifecycle in apply"
```

---

## Self-Review

**Spec coverage check:**

| Spec requirement | Covered by |
|-----------------|-----------|
| Single `Anvil` struct + `AnvilConfig` | Task 2–4 |
| `spawn`, `rpc_url`, `port`, `stop → Option<PathBuf>` | Task 3 |
| `set_balance` | Task 3 |
| Graceful stop SIGTERM → SIGKILL | Task 3 |
| `Drop` fallback | Task 3 |
| Baked-in defaults (mixed-mining, slots, etc.) | Task 3 |
| `wait_for_rpc_ready` | Task 5 |
| `wait_for_l2_block_finalized` | Task 5 |
| `Server::start`, `rpc_url`, `stop(archive_db_to)` | Task 6 |
| `embedded-server` feature gates heavy deps | Task 5–6 |
| `Config` re-exported from zksync_os_server | Task 5 |
| `Drop` fallback for Server | Task 6 |
| RocksDB archive with `node/` prefix | Task 6 |
| `zk-deployer` moved to `bin/` | Task 1 |
| Workspace Cargo.toml updated | Task 1 |
| `zk-deployer` deps on lib-anvil + lib-server | Task 7 |
| `l1_rpc_url` optional field in intent | Task 7 |
| Managed gateway server in apply | Task 8 |
| External mode (URLs provided → skip lifecycle) | Task 7–8 |
| `ServerHandle` trait reserved for future binary support | Not implemented (per spec: future work) |

**Placeholder scan:** No TBDs except the `todo!()` in Task 8 Step 3, which is intentional — it instructs the engineer to move existing code rather than write new code.

**Type consistency:**
- `Anvil::stop() -> Result<Option<PathBuf>>` — consistent across Tasks 3 and 4.
- `Server::stop(self, archive_db_to: Option<PathBuf>) -> Result<()>` — consistent across Tasks 6 and 8.
- `wait_for_l2_block_finalized(l2_rpc: &str, target_block: u64, timeout: Duration) -> Result<()>` — consistent across Tasks 5 and 8.
- `lib_server::Config` re-export used in Tasks 6 and 8.
