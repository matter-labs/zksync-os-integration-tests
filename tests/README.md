# ZKsync OS Integration Test Framework

A composable, declarative test infrastructure for ZKsync OS. Declare what you need;
the framework handles setup and teardown.

## Quick Start

```rust
use rstest::rstest;
use tests::fixtures::ecosystem;
use tests::Ecosystem;

#[rstest]
#[tokio::test(flavor = "multi_thread")]
async fn my_test(#[future] ecosystem: Ecosystem) {
    let eco = ecosystem.await;
    let hash = eco.chain().ping().await;
    eco.chain().wait_for_tx_finalized(hash).await;
}
```

Run tests:

```bash
cargo test -p tests --release -- --nocapture
```

---

## Fixtures

Declare as `#[future] name: Type` parameters on your test function. Import from
`tests::fixtures::*`.

| Fixture | Type | Description |
|---------|------|-------------|
| `ecosystem` | `Ecosystem` | N L1-settling ZKsync OS chains on one Anvil L1 (default: one chain, ID 6565), 10 wallets pre-funded with 100 ETH each per chain |
| `v30_chain::start()` | `Ecosystem` | Frozen v30.2 chain restored from a committed snapshot via `restore()`; used by the protocol-upgrade test. The chain has no test wallets (driven through governor/deployer L1 keys, which are pre-funded in the committed `l1-state`). |

**Restoring a fixed chain:** `fixtures::restore::restore(dir)` brings up an
`Ecosystem` from a committed snapshot directory (`l1-state.json.gz` +
`genesis.json` + `server.yaml`/`server-<id>.yaml`). The committed server config
is the source of truth — its `genesis:` block supplies the chain id + bridgehub.
Deployer artifacts (`intent.yaml`/`state.json`) are not used at restore time.

The number of chains is controlled at the call site with rstest's `#[with]`
attribute — no separate fixture per chain count:

```rust
#[rstest]
#[tokio::test(flavor = "multi_thread")]
async fn two_chains(
    #[future]
    #[with(vec![6565, 6566])]
    ecosystem: Ecosystem,
) {
    let eco = ecosystem.await;
    for chain in eco.chains() {
        let hash = chain.ping().await;
        chain.wait_for_tx_finalized(hash).await;
    }
}
```

### Setup times (release build)

| Fixture | Cold (cache miss) | Warm (cache hit) |
|---------|------------------|------------------|
| `ecosystem` (1 chain) | ~2 min | ~1 min |
| `ecosystem` (2 chains) | ~3 min | ~1.5 min |

See **Deployment Cache** below — the warm path skips bootstrap/apply/deposits
and only pays for server start + batch 1.

When the fixture returns, batch 1 is already finalized on every chain — wallets
are funded and the chains are ready for test operations.

---

## `Ecosystem` API

```rust
eco.chain()    // &Chain — the first chain (the only one in single-chain setups)
eco.chains     // Vec<Chain> — all chains, in intent order
eco.chains()   // impl Iterator<Item = &Chain>
```

`Ecosystem` owns the full process lifecycle (Anvil, in-process servers, workdir).
Dropping it tears everything down. `Chain` handles are pure RPC/wallet references.

## `Chain` API

### Chain identity

```rust
chain.chain_id() -> u64
chain.bridgehub_addr() -> Address
chain.l1_rpc_url() -> &str
chain.l2_rpc_url() -> &str
```

### Wallets

Ecosystem chains carry ten pre-funded wallets (100 ETH each on L2). These are
funded by `zk-deployer apply` (the canonical set is `zk_deployer`'s
`DEFAULT_L2_RICH_KEYS`, re-exported as `WALLET_KEYS`) — the same well-known dev
accounts Anvil pre-funds on L1, now rich on L2 as well:

```rust
chain.wallet(0)   // 0x36615Cf... (ZKsync rich account)
chain.wallet(1)   // 0x70997970... (Anvil #1)
chain.wallet(2)   // 0x3C44CdDd... (Anvil #2)
// chain.wallet(3..=9) → Anvil #3–#9
chain.wallets()   // &[PrivateKeySigner]
```

A chain may have no test wallets (e.g. the v30 upgrade fixture, driven through
L1 admin keys instead); the wallet-backed operations panic on such a chain.

### Operations

```rust
// ETH transfer from wallet(0) to any address
chain.transfer(to: Address, amount: U256).await -> TxHash

// ETH transfer from an explicit wallet
chain.transfer_from(wallet: &PrivateKeySigner, to, amount).await -> TxHash

// Arbitrary L2 tx, signed by wallet(0)
chain.send_tx(tx: TransactionRequest).await -> TxHash

// Arbitrary L2 tx, explicit signer
chain.send_tx_from(wallet: &PrivateKeySigner, tx).await -> TxHash

// 1 wei self-transfer — gives the sequencer work to seal a batch
chain.ping().await -> TxHash

// L2 ETH balance
chain.balance(addr: Address).await -> U256
```

### Waiting

```rust
// Preferred: wait for a specific tx's block to finalize. Anchors on the block
// the tx landed in — correct even if other batches finalized in the meantime.
chain.wait_for_tx_finalized(hash: TxHash).await -> u64

// Wait for finalized block >= block. Idempotent — returns immediately if already
// past the target. Use in fixtures to wait for a known batch (e.g. block 1 = batch 1).
chain.wait_for_block_finalized(block: u64).await

// Wait for one more finalized batch beyond the current state. Racy if the batch
// may finalize between now and the call — prefer wait_for_tx_finalized instead.
chain.wait_for_batch().await -> u64

// Wait for a specific L2 tx to be mined. Asserts it didn't revert.
chain.wait_for_tx(hash: TxHash).await
```

### Raw provider access (escape hatch)

```rust
chain.l2_provider().await  // alloy Provider for L2
chain.l1_provider().await  // alloy Provider for L1
```

---

## Architecture

```
tests/tests/*.rs          one #[rstest] test per scenario
tests/src/fixtures/       rstest fixtures (ecosystem, v30_chain)
tests/src/server_runtime  ChainRuntime: per-server ports + scratch paths
zk_deployer::deployed     DeployedEcosystem: typed view of a bootstrap+apply
                          workdir (addresses, per-chain server configs)
zk-deployer               intent.yaml → bootstrap → apply deployment engine
lib-server                in-process zksync-os-server (embedded feature)
```

The fixtures' only deployer surface is `IntentConfig` (input),
`bootstrap::run` / `apply::run` (execution), and `DeployedEcosystem` (output).
Deployment internals (`state.json` schema, wallets format, config rendering)
are private to zk-deployer.

---

## Environment Variables

| Variable | Values | Default | Effect |
|----------|--------|---------|--------|
| `RUST_LOG` | tracing filter | `warn,zksync_os=info` | Server log verbosity |
| `ZKOS_TEST_DIR` | path | *(unset)* | When set, each test run writes its workdir to `<path>/<pid>-<n>/` and does **not** clean it up on exit. Useful for post-failure inspection. |

### Inspecting a failed test run

```bash
ZKOS_TEST_DIR=.test-runs cargo test -p tests --release -- --nocapture chain_executes_a_batch
# stderr prints: [tests] workdir: .test-runs/12345-0
```

The preserved workdir contains:

```
.test-runs/<pid>-<n>/
├── ecosystem/
│   ├── l1-state.json      ← Anvil L1 state at test start
│   ├── genesis.json       ← genesis input
│   ├── intent.yaml        ← deployment intent
│   ├── state.json         ← deployed contract addresses
│   └── wallets.yaml       ← funded test wallets
└── server-<chain-id>/
    ├── server.yaml        ← rendered server config
    └── db/
        ├── fri_proofs/    ← prover storage
        ├── block_dumps/   ← sequencer block dumps
        └── ...            ← RocksDB files (server state)
```

---

## Writing Tests

### Example: transfer between wallets

```rust
#[rstest]
#[tokio::test(flavor = "multi_thread")]
async fn transfer_between_wallets(#[future] ecosystem: Ecosystem) {
    let eco = ecosystem.await;
    let chain = eco.chain();

    let to = chain.wallet(1).address();
    let hash = chain.transfer(to, U256::from(1_000_000_000_000_000_000u128)).await;
    chain.wait_for_tx_finalized(hash).await;

    assert!(chain.balance(to).await > U256::from(1_000_000_000_000_000_000u128));
}
```

### Example: protocol upgrade

See `tests/tests/upgrade_v30_to_v31.rs` — starts the frozen v30.2 fixture
(`fixtures::v30_chain`), drives the real upgrade runbook steps
(`tests::protocol_upgrade`), and verifies post-upgrade deposits.

---

## Deployment Cache

The deployment half of every fixture (bootstrap + apply + wallet deposits) is
cached on disk under `.zkos-test-cache/` (gitignored). On a hit, the fixture
restores the anvil state dump + workdir manifest and only pays for server
start + batch 1. Servers always start fresh from genesis — server code is
deliberately **not** part of the cache key, so iterating on
`zksync-os-server` (including via a local path override) gets cache hits,
correctly.

Everything that *does* affect the deployment is keyed by content, so local
edits always invalidate:

| Input | Identified by |
|---|---|
| era-contracts tree (Solidity, deploy scripts, genesis configs, protocol-ops) | checkout rev, or content hash when `PROTOCOL_CONTRACTS_ROOT` points at a working copy |
| `bin/zk-deployer/src` | content hash |
| `tests/src` (fixture/deposit logic, wallet keys) | content hash — `tests/tests/` is excluded, so editing a test does not invalidate |
| genesis crates (`zksync_os_api`, `basic_system`) | resolved `Cargo.lock` stanzas — `genesis.json` is computed from them; the server crates are deliberately *not* keyed |
| topology | the intent (minus the per-run `l1_rpc_url`) |
| anvil | `anvil --version` |

Knob: `ZKOS_CACHE=auto|off|refresh` (default `auto`; `refresh` rebuilds and
overwrites the entry). Each entry stores `meta.json` with the key components
for debugging unexpected misses. Reset everything with
`rm -rf .zkos-test-cache`.

---

## History

The previous framework (a Docker-based `integration-tests/` crate driven by a
monolithic `generate-l1-state` binary and a preset system) was removed, along
with gateway settlement support. If you need it for reference, it last existed
at the commit preceding the `deprecated/` folder removal in git history.
