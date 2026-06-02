# Anvil & Server Wrapper Crates — Design Spec

**Date:** 2026-06-02
**Status:** Approved

## Problem

`zk-deployer` needs to orchestrate Anvil and a gateway `zksync-os-server` to fully replace
`generate-l1-state`. Currently there is no reusable Anvil or server wrapper — the only
implementations are tightly coupled to the deprecated `integration-tests/src/infra/` code
(subprocess + Docker paths, heavy legacy assumptions). We build from scratch.

## Goals

- `lib/anvil`: clean Anvil process wrapper usable by both `integration-tests` (new) and `zk-deployer`
- `lib/server`: in-process server wrapper using `zksync_os_server` as a library dep, with heavy
  deps behind an `embedded-server` feature flag
- `bin/zk-deployer`: moved into workspace, gains Anvil + server lifecycle for gateway support
- Existing `integration-tests/src/infra/` left as-is; deprecated and replaced by new tests over time

## Non-Goals

- Refactoring existing `integration-tests` tests
- Binary server support (noted as future work; trait hook reserved)
- Caching / metadata.json (separate concern)

---

## Workspace Structure

```
zksync-os-integration-tests/
├── Cargo.toml                    # workspace root
├── bin/
│   └── zk-deployer/              # moved from tools/zk-deployer
├── lib/
│   ├── anvil/                    # new
│   └── server/                   # new
├── integration-tests/            # kept as-is, deprecated over time
└── deprecated/
    ├── build-artifacts/
    └── generate-l1-state/
```

`tools/` is removed once `zk-deployer` moves to `bin/`.

---

## `lib/anvil`

### Design decisions

- **Single `Anvil` struct** with `AnvilConfig` — no trait, no two types. `dump_state` is
  `Option<PathBuf>` in the config; `stop()` returns `Option<PathBuf>`.
- `wrap_external` intentionally omitted — was a coupling artifact of the old `ServerBuilder`.
  New server takes an RPC URL string directly.
- Default config values baked in: everything a caller normally needs works out of the box.

### API

```rust
pub struct AnvilConfig {
    pub port: Option<u16>,           // None = auto-pick unused port
    pub chain_id: u64,               // default: 31337
    pub block_time_secs: f64,        // default: 0.25
    pub dump_state: Option<PathBuf>, // triggers --dump-state + --preserve-historical-states
    pub load_state: Option<PathBuf>, // triggers --load-state
}

impl Default for AnvilConfig {
    // chain_id: 31337, block_time_secs: 0.25, port: None, dump/load: None
}

pub struct Anvil { /* private */ }

impl Anvil {
    pub async fn spawn(config: AnvilConfig) -> Result<Self>

    pub fn rpc_url(&self) -> &str
    pub fn port(&self) -> u16

    /// Graceful stop: SIGTERM → wait → SIGKILL fallback.
    /// Returns the dump path if dump_state was configured, None otherwise.
    pub async fn stop(self) -> Result<Option<PathBuf>>

    /// anvil_setBalance JSON-RPC call.
    pub async fn set_balance(&self, address: Address, wei: U256) -> Result<()>
}

impl Drop for Anvil { /* SIGKILL fallback */ }
```

### Baked-in defaults

Always passed to the `anvil` process:
- `--host 0.0.0.0`
- `--disable-block-gas-limit`
- `--mixed-mining`
- `--slots-in-an-epoch 10`

### Dependencies

`alloy` (for `set_balance`), `tokio` (process + async). No heavy deps.

---

## `lib/server`

### Design decisions

- Heavy `zksync_os_server` + `reth-tasks` deps gated behind `embedded-server` feature.
- Waiting functions always available (plain HTTP polling, no feature needed) — usable by
  `zk-deployer` even in configurations that don't run a server.
- `Config` re-exported from `zksync_os_server::config::Config` so callers don't need a direct
  dep on `zksync_os_server` just to build a config.
- `stop()` takes `archive_db_to: Option<PathBuf>` — integration tests pass `None`, `zk-deployer`
  gateway phase passes `Some(dest)`.
- **Future binary support:** when needed, add `ServerHandle` trait + `BinaryServer` impl.
  Current `Server` implements `ServerHandle`. No changes needed to call sites.

### Feature flag

`embedded-server` (not `gateway`) — names what the feature *does*, not why you'd want it.
When binary support arrives, `external-server` feature is added alongside it coherently.

### API

```rust
// Always available — no feature needed

pub async fn wait_for_rpc_ready(url: &str, timeout: Duration) -> Result<()>
pub async fn wait_for_batch_executed(
    l2_rpc: &str,
    l1_rpc: &str,
    timeout: Duration,
) -> Result<()>

// Behind `embedded-server` feature

pub use zksync_os_server::config::Config;

pub struct Server { /* runtime, task manager handle, rpc_url, rocks_db_path */ }

impl Server {
    pub async fn start(config: Config) -> Result<Self>
    pub fn rpc_url(&self) -> &str
    /// Shuts down the server. If archive_db_to is Some, tars the RocksDB
    /// directory to that path before stopping.
    pub async fn stop(self, archive_db_to: Option<PathBuf>) -> Result<()>
}

impl Drop for Server { /* abort tasks fallback */ }

// Reserved for future binary support — not implemented yet
pub trait ServerHandle {
    fn rpc_url(&self) -> &str;
    async fn stop(self, archive_db_to: Option<PathBuf>) -> Result<()>;
}
```

### Dependencies (behind `embedded-server`)

`zksync_os_server` (git, main branch), `zksync_os_state_full_diffs`, `reth-tasks`, `tokio`.

---

## `bin/zk-deployer`

- Moved from `tools/zk-deployer` → `bin/zk-deployer`
- Removed from workspace `exclude`, added to `members`
- Gains dependencies:
  ```toml
  lib-anvil = { path = "../../lib/anvil" }
  lib-server = { path = "../../lib/server", features = ["embedded-server"] }
  ```

### Two operating modes

`zk-deployer` must work against both local test environments and real networks
(Sepolia, mainnet). Anvil and server lifecycle management are **optional conveniences**,
not requirements.

**Managed mode** (local dev / CI): `zk-deployer` owns Anvil and gateway server lifecycle.
- Starts Anvil if no `l1.rpc_url` is provided in `intent.yaml`
- Starts gateway server if no `gateway.rpc_url` is provided
- Funds operators via `anvil_setBalance`
- Stops and archives after the gateway phase

**External mode** (real networks): user provides URLs, `zk-deployer` does no lifecycle management.
- `l1.rpc_url` in `intent.yaml` (or `--l1-rpc-url` flag) → skip Anvil start/stop
- `gateway.rpc_url` in `intent.yaml` (or `--gateway-rpc-url` flag) → skip server start/stop
- Operator funding is the user's responsibility (no `anvil_setBalance` on Sepolia/mainnet)

`intent.yaml` expression:
```yaml
l1:
  rpc_url: "https://sepolia.infura.io/..."  # external — no Anvil managed
  # omit rpc_url → zk-deployer manages a local Anvil

gateway:
  rpc_url: "https://..."                    # external — no server managed
  # omit rpc_url → zk-deployer manages a local server
```

`wait_for_batch_executed` and `wait_for_rpc_ready` from `lib/server` are used in both modes —
they poll external URLs regardless of whether the server is managed or external.

---

## What Stays As-Is

`integration-tests/src/infra/` is left untouched. It will be deprecated and replaced
incrementally as new tests are written against `zk-deployer` + `lib/anvil` + `lib/server`.
The deprecated `generate-l1-state` and `build-artifacts` remain in `deprecated/` for reference.
