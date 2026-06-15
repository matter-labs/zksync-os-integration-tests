# zksync-os-integration-tests

Integration tests for ZKsync OS. Spins up a full ecosystem — Anvil L1, deployed
contracts, in-process `zksync-os-server` per chain — and runs end-to-end tests
against it. No Docker required.

## Prerequisites

- **Rust** nightly-2026-01-22 (pinned in `rust-toolchain.toml`)
- **Foundry** (`anvil`, `forge`, `cast`)
- **Yarn** (forge deployment scripts call node helpers via FFI)

## Quick start

```bash
# Run the full integration suite (release strongly recommended — debug-build
# servers are too slow for the test timeouts)
cargo test -p tests --release

# Run a single test
cargo test -p tests --release --test l1 chain_executes_a_batch
```

Contracts are compiled automatically on first use (`forge build` inside the
era-contracts checkout that the `protocol_ops` dependency pins).

## Project structure

```
.
├── bin/zk-deployer/    # Deployment engine + CLI: intent.yaml → bootstrap → apply
├── lib/server/         # In-process zksync-os-server wrapper + RPC wait helpers
├── tests/              # Test framework (rstest fixtures) + integration tests
│   ├── src/fixtures/   #   ecosystem (N L1-settling chains), v30_chain (frozen fixture)
│   ├── tests/          #   l1.rs, upgrade_v30_to_v31.rs
│   └── local-chains/   #   committed v30.2 frozen state for upgrade tests
└── .zkos-test-cache/   # Deployment snapshot cache (gitignored)
```

See `tests/README.md` for the test-framework API and `bin/zk-deployer/README.md`
for the standalone CLI.

## Version pinning

`era-contracts` (via the `protocol_ops` library crate) and `zksync-os-server`
are git dependencies in `Cargo.toml`; the exact revisions are pinned by
`Cargo.lock`. To test against a different revision:

```bash
cargo update -p protocol_ops          # or: --precise <rev>
cargo update -p zksync_os_server
```

## Local development

### Contracts (`era-contracts`)

1. Uncomment the `[patch."https://github.com/matter-labs/era-contracts"]` block
   in `Cargo.toml` and point it at your local checkout.

2. Set `PROTOCOL_CONTRACTS_ROOT` to the same path:

   ```bash
   export PROTOCOL_CONTRACTS_ROOT=../era-contracts
   cargo test -p tests --release
   ```

   Without `PROTOCOL_CONTRACTS_ROOT` the deployment cache keys on the git rev and
   ignores local Solidity edits — the env var switches it to a content hash so any
   edit correctly invalidates the cached deployment.

### Server (`zksync-os-server`)

Uncomment the `[patch."https://github.com/matter-labs/zksync-os-server"]` block
in `Cargo.toml` and point it at your local checkout. No env var needed — server
code is deliberately excluded from the cache key, so local server builds still
get cache hits on the deployment half (contracts, wallet deposits) and only pay
for the server recompile.

### VM crates (`zksync-os`)

Same pattern: uncomment the `[patch."https://github.com/matter-labs/zksync-os"]`
block and fill in the crate paths within your local checkout.

## Writing tests

Tests are Rust integration tests in `tests/tests/`, using rstest fixtures:

```rust
use rstest::rstest;
use tests::fixtures::ecosystem;
use tests::Ecosystem;

#[rstest]
#[tokio::test(flavor = "multi_thread")]
async fn my_test(#[future] ecosystem: Ecosystem) {
    let eco = ecosystem.await;
    eco.chain().ping().await;
    eco.chain().wait_for_batch().await;
}
```

Multi-chain topologies are declared at the call site:

```rust
#[with(vec![6565, 6566])] ecosystem: Ecosystem
```

Full API reference: `tests/README.md`, including environment variables for debugging
(`ZKOS_TEST_DIR`, `ZKOS_CACHE`, `RUST_LOG`).
