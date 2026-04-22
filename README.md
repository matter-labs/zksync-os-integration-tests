# zksync-os-integration-tests

Integration tests for zkSync OS. Spins up a full multi-chain ecosystem on a local Anvil node and runs end-to-end tests against it.

Supports two modes of operation:
- **Docker images** (default, CI) -- preset references a git branch/tag/SHA which is resolved to a published Docker image from `ghcr.io`. No local clones needed.
- **Local repositories** -- preset points to a local checkout of `era-contracts` or `zksync-os-server`. Useful for testing local changes before pushing.

## Prerequisites

- **Rust** nightly-2026-02-20 (pinned in `rust-toolchain.toml`)
- **Foundry** (`anvil`, `forge`, `cast`)
- **Docker** (for `protocol-ops` and `zksync-os-server` images)

## Quick start

```bash
# Run all presets and their tests
./run-tests.sh

# Run a single preset
./run-tests.sh --preset v31_draft_with_main_server

# Run a single test across all presets that include it
./run-tests.sh --test l1_settling_test

# Run a specific preset + test combination
./run-tests.sh --preset v31_draft_with_main_server --test l1_settling_test

# Skip ecosystem generation (reuse cached L1 state)
./run-tests.sh --skip-generate

# Delete and regenerate the L1 state cache
./run-tests.sh --rebuild-cache

# Use a custom presets file
./run-tests.sh --presets custom-presets.yaml
```

## Presets

A **preset** defines a testing scenario by pinning specific versions of `era-contracts` and `zksync-os-server`, along with a list of tests to run. Presets are defined in `presets.yaml`:

```yaml
v31_draft_with_main_server:
  era_contracts: gateway-commands-in-protocol-ops   # git branch, tag, SHA, or local path
  zksync_os_server: main                            # git branch, tag, SHA, or local path
  tests:
    - l1_settling_test
    - gateway_settling_test
    - interop_test
```

### How preset values are resolved

Each `era_contracts` / `zksync_os_server` value goes through this resolution chain:

1. **Local path** -- if the value is an existing directory (absolute or relative to project root), use it directly.
2. **Git ref to Docker image** -- resolve the branch/tag/SHA against the GitHub repo, then look up a published Docker image for that commit (`ghcr.io/matter-labs/protocol-ops:{sha}` or `ghcr.io/matter-labs/zksync-os-server:{sha}`).
3. **Fallback** -- if the tip commit has no image yet, walk back up to 10 ancestor commits and use the most recent one that does.

### Extra keys

Besides the required `era_contracts`, `zksync_os_server`, and `tests` fields, presets support an optional `extra_keys` map for arbitrary parameters. These are passed through to tests and can be read via `preset.extra_str("key")`. This is useful for parameterizing tests without creating separate test files:

```yaml
custom_base_token_test:
  era_contracts: main
  zksync_os_server: main
  tests:
    - l1_settling_test
  extra_keys:
    base_token_address: "0x1234..."
    custom_flag: "true"
```

### Adding a new preset

Add an entry to `presets.yaml` (or a separate YAML file passed via `--presets`). Each preset must have `era_contracts`, `zksync_os_server`, and a `tests` list.

## Local development

To test against local checkouts of `era-contracts` or `zksync-os-server`, point the preset values to their paths on disk:

```yaml
my_local_preset:
  era_contracts: ../era-contracts          # relative to project root
  zksync_os_server: ../zksync-os-server
  tests:
    - l1_settling_test
```

**Do not add local presets to `presets.yaml`** -- that file is committed and shared. Instead, create a `local_presets.yaml` (gitignored) and pass it explicitly:

```bash
./run-tests.sh --presets local_presets.yaml --preset my_local_preset
```

When `era_contracts` points to a local path, the generation step will build contracts from source inside the Docker container instead of using a pre-built image. When `zksync_os_server` points to a local path, the test infra resolves it as the server root directory.

## Ecosystem generation

Before tests run, `generate-l1-state` builds the full L1+L2 ecosystem:

1. Start Anvil with `--dump-state`
2. Deploy L1 contracts via `protocol_ops ecosystem init`
3. Register the **gateway chain** (chain ID `506`)
4. Register **gateway-settling chains** (chain IDs `6566`, `6567`) with `--pause-deposits --skip-priority-txs`
5. Register **L1-settling chains** (chain ID `6565`)
6. Fund all operator accounts on L1
7. Generate `genesis.json`, per-chain configs, and `wallets.yaml`
8. Start the gateway server, fund L2 accounts, wait for executed batches
9. Convert gateway chain, migrate gateway-settling chains
10. Submit L1 deposits for L1-settling chains
11. Stop servers, dump Anvil state, archive gateway RocksDB

The result is cached in `l1-state-cache/` keyed by the resolved Docker image SHAs. Subsequent runs with the same preset skip generation automatically.

### Generated artifacts

| File | Description |
|------|-------------|
| `l1-state.json` | Anvil state dump (~400 MB) |
| `ecosystem.yaml` | Chain IDs, diamond proxy addresses, bridgehub address |
| `wallets.yaml` | All keypairs (deployer, governor, operators per chain). **Note:** this format is different from wallets generated by zkstack -- keys are grouped by chain role, not by chain name |
| `genesis.json` | L2 genesis input |
| `{chain-name}.yaml` | Per-chain server config (addresses, operator keys, pubdata mode) |
| `*.gateway-state.tar.gz` | Gateway RocksDB archive |

## Chain topology

```
L1 (Anvil)
 |
 +-- Gateway (chain 506, base token: ZK)
 |     |
 |     +-- gateway_settling_a (chain 6566)   -- settles via gateway
 |     +-- gateway_settling_b (chain 6567)   -- settles via gateway
 |
 +-- l1_settling (chain 6565)                -- settles directly to L1 via blobs
```

## Tests

Tests are Rust integration tests in `integration-tests/tests/`:

| Test | What it covers |
|------|----------------|
| `l1_settling_test` | L1-settling chain: boots server, seals batches, verifies L1 settlement via blobs |
| `gateway_settling_test` | Gateway-settling chains: boots servers, seals batches, verifies settlement through gateway |
| `interop_test` | Cross-chain interoperability and L2 messaging between chains |
| `upgrade_v30_to_v31` | Protocol upgrade from v30 to v31 (uses pre-built state from `local-chains/`) |

Each test receives the preset name via `PRESET_NAME` env var, loads the matching cached L1 state, and orchestrates its own servers via Docker.

## Writing tests with `EraContractsBackend`

`EraContractsBackend` is the main abstraction for interacting with era-contracts tooling (`protocol_ops`, `forge`, `cast`) from tests. It transparently handles both local and Docker modes -- tests don't need to know which one is active.

### Creating a backend

```rust
use integration_tests::protocol_ops::EraContractsBackend;
use integration_tests::presets::load_current_preset;

let preset = load_current_preset()?;
let backend = EraContractsBackend::from_preset(&preset, "my_test_name", &[])?;
```

The `run_name` (second argument) is used for log directory naming. The third argument is optional extra Docker volume mounts.

### Available methods

| Method | Description |
|--------|-------------|
| `protocol_ops(&[args])` | Run `protocol_ops` CLI with the given arguments |
| `forge_script(&[args], &[envs])` | Run `forge script` from the `l1-contracts` directory |
| `forge(&[args])` | Run arbitrary `forge` subcommands (e.g. `inspect`) |
| `cast(&[args])` | Run `cast` commands (e.g. `call`, `send`, `block-number`) |
| `run(&[command], workdir)` | Run an arbitrary command (e.g. `zksync-os-genesis-gen`) |
| `read_protocol_ops_output(relative)` | Read a file written by `protocol_ops --out` from the work directory |
| `read_repo_file(relative)` | Read a file relative to the era-contracts repo root |
| `work_dir()` | Host-side path where all outputs are stored |
| `work_path(relative)` | Resolve a path inside the work directory (host or container, depending on mode) |
| `repo_path(relative)` | Resolve a path relative to the era-contracts root |

### Example: calling a contract via `cast`

```rust
let block = backend.cast(&["block-number", "--rpc-url", &rpc_url])?;
let result = backend.cast(&[
    "call", &bridgehub_addr, "baseToken(uint256)(address)",
    &chain_id.to_string(), "--rpc-url", &l1_rpc_url,
])?;
```

### Example: running `protocol_ops` with output

```rust
let out_path = backend.work_path("my_output.json");
backend.protocol_ops(&[
    "chain", "some-command",
    "--l1-rpc-url", &l1_rpc_url,
    "--out", &out_path,
])?;
let output = backend.read_protocol_ops_output("my_output.json")?;
```

### `ProtocolOps` wrapper

For commands that always need an L1 RPC URL, use the `ProtocolOps` wrapper:

```rust
use integration_tests::protocol_ops::ProtocolOps;

let protocol_ops = ProtocolOps::new(&l1_rpc_url, &backend);
protocol_ops.chain_set_upgrade_timestamp()
    .chain_id(chain_id)
    .diamond_proxy(&diamond_proxy)
    // ...
    .run()?;
```

## Run logs

All test run artifacts are saved under `test-run-logs/` (gitignored). Each test run creates a subdirectory named `{run_name}_{uuid_prefix}/` containing:

```
test-run-logs/
└── gateway_settling_643bbb39/
    ├── gateway_settling_a_run_0.json   # Server stdout/stderr for chain, run 0
    ├── gateway_settling_b_run_0.json   # Server stdout/stderr for chain, run 0
    ├── protocol_ops_commands.log       # All protocol-ops commands and their output
    ├── contracts_artifacts/            # protocol-ops Docker working directory (wallets, genesis, chain init outputs)
    └── db_gateway_settling_a/          # RocksDB directory (local server mode only)
```

- **`{chain_name}_run_{N}.json`** -- server logs for each chain. The run index `N` increments when a server is stopped and restarted within the same test (e.g. upgrade tests that restart after a protocol change).
- **`protocol_ops_commands.log`** -- appended log of every `protocol_ops` CLI invocation with full stdout/stderr. Useful for debugging contract deployment or chain registration failures.
- **`contracts_artifacts/`** -- the Docker-mounted working directory used by `protocol-ops`. Contains `wallets.yaml`, `genesis.json`, per-chain init outputs, and gateway RocksDB snapshots.

The `run-tests.sh` orchestrator cleans stale logs between presets (except `contracts_artifacts/` which is preserved due to a macOS Docker/VirtioFS bind-mount limitation).

## Troubleshooting

### "Server unable to connect" on a second run (but the first passed)

Symptom: first `./run-tests.sh` passes cleanly; the next one fails with the server failing to reach L1 or timing out. Almost always one of:

- **Leftover processes from the previous run** holding RocksDB `LOCK` fds. `rm -rf test-run-logs/` deletes the directory entry but a dead `zksync-os-server` still writing through the inode corrupts the next run's state. Fix: `./run-tests.sh` now runs a `pkill` + `docker rm -f` preamble; if you still see it, check `ps -ef | grep -E 'zksync-os-server|anvil'` and `docker ps -a --filter name=integration-tests-zksync-os-server-`.
- **`localhost` resolving to IPv6 (`::1`) on Linux** while anvil listens on IPv4 (`0.0.0.0`). All internal URLs now use `127.0.0.1` explicitly; if you see this after pulling, check for a stray `localhost` in your preset or test code.

### "No image found for SHA …" or preset resolution walks back many commits

Preset resolution falls back up to 10 ancestor commits if the tip of the branch has no published Docker image. When this happens you'll see a warning in the run output (`⚠ era_contracts: tip of 'main' is <sha> but no image was published; using ancestor <sha> instead`). The exact SHAs actually used land in `test-run-logs/resolved-refs.json`:

```bash
cat test-run-logs/resolved-refs.json
```

If a CI run disagrees with your local run, compare these two files — the most common cause of "works for me" is that the branch tip advanced between the two runs.

### Docker backend quirks

- **Docker Desktop (macOS/Windows)** is the developed-against backend. Generation and tests should work out of the box.
- **Native Linux Docker** ≥ 20.10 is required for `--add-host host.docker.internal:host-gateway`. If containers can't reach host anvil, probe with:
  ```bash
  docker run --rm --add-host host.docker.internal:host-gateway alpine getent hosts host.docker.internal
  ```
  Empty output means your Docker is too old.
- **`contracts_artifacts/` is intentionally preserved** across preset cleanups (there's a macOS Docker/VirtioFS bind-mount bug where the host can't see sub-directories created inside the container). Do not add `contracts_artifacts` to cleanup scripts.

### Full reset

If state is ambiguous and you just want a clean slate:

```bash
pkill -f 'target/(release|debug)/zksync-os-server' 2>/dev/null || true
pkill -f 'anvil.*--(load|dump)-state'             2>/dev/null || true
docker ps -aq --filter "name=integration-tests-zksync-os-server-" | xargs -r docker rm -f
rm -rf test-run-logs l1-state-cache
./run-tests.sh --rebuild-cache
```

## Project structure

```
.
├── presets.yaml                  # Preset definitions
├── run-tests.sh                  # Main test orchestrator
├── integration-tests/            # Rust crate: test infra + test files
│   ├── src/
│   │   ├── presets.rs            # Preset loading and resolution
│   │   ├── l1_state.rs           # Ecosystem state loading and caching
│   │   ├── infra/                # Anvil, Docker, port allocation, git utils
│   │   └── chain/                # Chain interaction utilities
│   └── tests/                    # Integration test files
├── tools/generate-l1-state/      # Ecosystem generation binary
├── l1-state-cache/               # Generated L1 state (gitignored)
├── local-chains/                 # Pre-built v30 chain configs for upgrade tests (static, committed)
└── test-run-logs/                # Server logs from test runs
```
