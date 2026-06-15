# zk-deployer

CLI tool for bootstrapping and deploying ZKsync OS chains.

## Quickstart — single L1-settling chain (local dev)

No setup required. `zk-deployer` manages Anvil automatically when `l1_rpc_url` is omitted.

### 1. Build contract artifacts

```bash
zk-deployer build-contracts
```

### 2. Generate intent.yaml

```bash
zk-deployer init
```

Edit `intent.yaml` — set the chain ID:

```yaml
schema_version: 1

# l1_rpc_url is commented out → Anvil starts automatically
# l1_rpc_url: "https://..."

chains:
  - chain_id: 6565
    da_mode: rollup     # rollup, no_da, or avail
```

Declare more entries under `chains:` to deploy multiple L1-settling chains on
the same L1.

### 3. Bootstrap the ecosystem

Deploys Bridgehub, CTM, and core L1 contracts. Spawns Anvil automatically and saves its state to `l1-state.json`.

```bash
zk-deployer bootstrap --broadcast
```

### 4. Apply chain initialization

Registers the chain on Bridgehub and creates the diamond proxy. Restores Anvil from `l1-state.json`.

```bash
zk-deployer apply --broadcast
```

### 5. Generate server config

```bash
zk-deployer server-config --chain 6565
```

### 6. Start Anvil from saved state

After `apply` exits its managed Anvil process is stopped. Start a persistent Anvil before launching the server:

```bash
anvil --load-state l1-state.json \
  --block-time 0.25 --mixed-mining \
  --slots-in-an-epoch 10 --disable-block-gas-limit &
```

### 7. Start the server

```bash
L1_PROVIDER_RPC_URL=http://localhost:8545 zksync-os-server \
  --config path/to/zksync-os-server/local-chains/local_dev.yaml \
  --config server.yaml
```

> **Note:** [`local_dev.yaml`](https://github.com/matter-labs/zksync-os-server/blob/main/local-chains/local_dev.yaml)
> from the `zksync-os-server` repo enables fake provers and fast poll intervals for local Anvil.
> When running via the integration-test framework these settings are applied automatically.

> **Note:** When using auto-Anvil, `l1-state.json` is written after each command. If you restart
> the sequence from `bootstrap`, delete `state.json` and `l1-state.json` first to start clean.

---

## Command reference

| Command | Purpose |
|---------|---------|
| `zk-deployer init` | Generate a starter `intent.yaml` |
| `zk-deployer build-contracts` | Build Forge contract artifacts |
| `zk-deployer bootstrap` | Wallets → genesis → ecosystem L1 init |
| `zk-deployer apply` | Chain registration, operator setup, default L2 dev-wallet funding |
| `zk-deployer server-config` | Generate server YAML from state (`--chain <chain_id>`) |
| `zk-deployer wallets generate` | Generate a `wallets.yaml` from seeds |
| `zk-deployer genesis generate` | Recompute genesis root |
| `zk-deployer token deploy` | Deploy and register a testnet ERC20 token |

### `apply` flags

| Flag | Default | Description |
|------|---------|-------------|
| `--broadcast` | false | Broadcast bundles immediately (required for local dev) |
| `--l1-state <path>` | l1-state.json | Anvil state file (auto-Anvil mode) |
| `--no-fund-l2` | false | Skip the default L1→L2 deposits that fund the well-known dev wallets (100 ETH each) on every chain. Funding runs automatically on local/Anvil so a fresh chain is ready to operate. |
