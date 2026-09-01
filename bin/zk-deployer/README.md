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
    da_mode: rollup     # rollup, avail, or a validium (see below)
```

Declare more entries under `chains:` to deploy multiple L1-settling chains on
the same L1.

A validium chain's pubdata carries only the mandatory L2→L1 log region
(`PubdataContent.LOGS_ONLY`) — state diffs stay private. The validium picks
where that logs-only pubdata goes (serde_yaml tagged syntax):

```yaml
    da_mode: !validium blobs      # EIP-4844 blobs, like a rollup
    da_mode: !validium calldata   # commit-tx calldata
    da_mode: !validium no_da      # nothing posted (EmptyNoDA)
```

`blobs`/`calldata` keep the on-chain `PubdataPricingMode` at `Rollup` (the
server refuses to post pubdata for a `Validium`-priced chain) and keep interop
(IMT) data reconstructible from L1; `no_da` is the only flavor that registers
with `PubdataPricingMode.Validium`. See `DaMode::Validium` in `src/intent.rs`.

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

## Custom base token (non-ETH)

By default a chain uses ETH as its base token. To use a custom ERC20 base token
(e.g. `ZK`) instead, add a `base_token` entry to the chain in `intent.yaml`.
The rest of the flow (`bootstrap` → `apply` → `server-config`) is unchanged.

```yaml
chains:
  - chain_id: 6565
    base_token:
      symbol: ZK          # deploy a fresh token during bootstrap
    da_mode: rollup
```

Three modes, selected by what you put under `base_token`:

| `base_token`                  | Behaviour |
|-------------------------------|-----------|
| *(omitted)*                   | ETH base token (default). |
| `{ symbol }`                  | `bootstrap` deploys a `TestnetERC20Token` with this symbol and registers it on the L1 Native Token Vault. |
| `{ symbol, address }`         | Use an existing ERC20 already deployed on L1; `bootstrap` skips deployment. |

### What each step does

- **`bootstrap`** deploys the token (when no `address` is given) via CREATE2,
  mints `10^27` units to the deployer, and registers it on the Native Token
  Vault. The deployed address is recorded in `state.json`.
- **`apply`** registers the chain against the token (`base_token_addr`) and funds
  the well-known dev wallets on L2. For a custom base token, funding deposits
  are paid in the ERC20: the deployer approves the Native Token Vault for the
  `mintValue` and the L1→L2 deposit is sent with `msg.value = 0` (sending ETH to
  a non-ETH base chain reverts with `MsgValueMismatch`). On a real L1 the
  deployer must already hold enough of the token; on local/Anvil the `10^27`
  bootstrap mint covers it.
- **`server-config`** forces the base token price to `1` so the chain is usable
  immediately for local testing.

### Limitations

- **One base token per ecosystem.** All chains that need a freshly-deployed token
  must share the same `symbol`; `bootstrap` rejects mismatched symbols. (Chains
  pointing at distinct pre-deployed `address`es are unaffected.)

### Example: deploy a token standalone

To deploy and register a token outside the `bootstrap` flow:

```bash
zk-deployer token deploy \
  --l1-rpc-url http://localhost:8545 \
  --private-key 0x... \
  --l1-contracts-out path/to/l1-contracts/zkstack-out \
  --bridgehub 0x... \
  --symbol ZK \
  --name "ZKsync Token"
```

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
| `--no-fund-l2` | false | Skip the default L1→L2 deposits that fund the well-known dev wallets (100 base-token units each — ETH, or the custom base token) on every chain. Funding runs automatically on local/Anvil so a fresh chain is ready to operate. |
