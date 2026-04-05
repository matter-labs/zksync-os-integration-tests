#!/bin/bash
set -ex

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

# Docker image tag for protocol-ops (required)
PROTOCOL_OPS_TAG="${PROTOCOL_OPS_TAG:?Set PROTOCOL_OPS_TAG to the protocol-ops Docker image tag}"

export WORK_DIR="${WORK_DIR:-$(pwd)/protocol-ops-workdir}"
mkdir -p "$WORK_DIR"
WORK_DIR="$(cd "$WORK_DIR" && pwd)"

# Container-side path where WORK_DIR is mounted
CW="/contracts/work/session"

# ── Helpers ──────────────────────────────────────────────────────────────

pcli() {
  "$SCRIPT_DIR/protocol-ops.sh" "$PROTOCOL_OPS_TAG" protocol_ops "$@"
}

dforge() {
  "$SCRIPT_DIR/protocol-ops.sh" "$PROTOCOL_OPS_TAG" forge "$@"
}

dcast() {
  "$SCRIPT_DIR/protocol-ops.sh" "$PROTOCOL_OPS_TAG" cast "$@"
}

# ── Accounts ─────────────────────────────────────────────────────────────

export DEPLOYER="0x36615cf349d7f6344891b1e7ca7c72883f5dc049"
export DEPLOYER_PK="0x7726827caac94a7f9e1b160f7ea819f172f7b6f9d2a97f992c38edeab82d4110"
export ECOSYSTEM_OWNER="0x8002cd98cfb563492a6fb3e7c8243b7b9ad4cc92"
export ECOSYSTEM_OWNER_PK="0xf12e28c0eb1ef4ff90478f6805b68d63737b7f33abfa091601140805da450d93"

export GATEWAY_CHAIN_ID=271
export CHAIN1_CHAIN_ID=272
export CHAIN2_CHAIN_ID=273

export GATEWAY_OWNER=$ECOSYSTEM_OWNER
export GATEWAY_OWNER_PK=$ECOSYSTEM_OWNER_PK
export GATEWAY_COMMIT_OPERATOR="0x5927c313861c01b82a026e35d93cc787e5356c0f"
export GATEWAY_PROVE_OPERATOR="0xff2087417fe10bc436b972aa0460fb5ec1024109"
export GATEWAY_EXECUTE_OPERATOR="0xfe81be741590259904834c895667d7b85e248191"

export CHAIN1_OWNER=$ECOSYSTEM_OWNER
export CHAIN1_OWNER_PK=$ECOSYSTEM_OWNER_PK
export CHAIN1_COMMIT_OPERATOR="0x76936581521a1bC051882BaE54b0A0fB62eDd3BE"
export CHAIN1_PROVE_OPERATOR="0x2552675d2DE8155d4713374A9cc240E5bB3a3c0c"
export CHAIN1_EXECUTE_OPERATOR="0x50235771961984B0eE83a00db9afFf61D23f3460"

export CHAIN2_OWNER=$ECOSYSTEM_OWNER
export CHAIN2_OWNER_PK=$ECOSYSTEM_OWNER_PK
export CHAIN2_COMMIT_OPERATOR="0xDE8675d9758CfDf2A78A2CD9775a6bB9BE6C4C14"
export CHAIN2_PROVE_OPERATOR="0x17B4E8d4211a1d734ff485046a84f79CAD7Ee372"
export CHAIN2_EXECUTE_OPERATOR="0x13bb1fE4A82F01706732fb3dAAF1768d5E48cCdE"

# ─── [0] Start local anvil L1 node ───────────────────────────────────────────

export L1_RPC_URL="http://localhost:8545"
export ETH_RPC_URL="$L1_RPC_URL"

# Kill any existing anvil on port 8545
lsof -ti:8545 | xargs kill -9 2>/dev/null || true

anvil --port 8545 --chain-id 31337 &
ANVIL_PID=$!
trap "kill $ANVIL_PID 2>/dev/null" EXIT
sleep 2

echo "Anvil started on port 8545 (PID: $ANVIL_PID)"

# Fund deployer and ecosystem owner from anvil's default account
ANVIL_DEFAULT_PK="0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
dcast send $DEPLOYER --value 1000ether --private-key $ANVIL_DEFAULT_PK --rpc-url $L1_RPC_URL
dcast send $ECOSYSTEM_OWNER --value 1000ether --private-key $ANVIL_DEFAULT_PK --rpc-url $L1_RPC_URL

# ─── [0] Deploy and mint ZK token ────────────────────────────────────────────

export ERC20_BYTECODE=$(dforge inspect contracts/dev-contracts/TestnetERC20Token.sol:TestnetERC20Token bytecode)
export ERC20_CONSTRUCTOR_ARGS=$(dcast abi-encode "constructor(string,string,uint8)" "ZK" "ZK" 18)
export ERC20_SALT="0000000000000000000000000000000000000000000000000000000000000001"
dcast send 0x4e59b44847b379578588920ca78fbf26c0b4956c "0x${ERC20_SALT}${ERC20_BYTECODE:2}${ERC20_CONSTRUCTOR_ARGS:2}" --private-key=$DEPLOYER_PK --rpc-url=$L1_RPC_URL
export ZK_TOKEN_ADDRESS=$(dcast create2 --salt=0x${ERC20_SALT} --init-code="0x${ERC20_BYTECODE:2}${ERC20_CONSTRUCTOR_ARGS:2}" --deployer=0x4e59b44847b379578588920ca78fbf26c0b4956c)
dcast send $ZK_TOKEN_ADDRESS "mint(address,uint256)" $ECOSYSTEM_OWNER 1000000000000000000000000000000000000000 --private-key=$DEPLOYER_PK --rpc-url=$L1_RPC_URL
dcast send $ZK_TOKEN_ADDRESS "mint(address,uint256)" $DEPLOYER 1000000000000000000000000000000000000000 --private-key=$DEPLOYER_PK --rpc-url=$L1_RPC_URL

# Fund everyone:
dcast send $GATEWAY_COMMIT_OPERATOR --value 10ether --private-key $DEPLOYER_PK --rpc-url $L1_RPC_URL
dcast send $GATEWAY_PROVE_OPERATOR  --value 10ether --private-key $DEPLOYER_PK --rpc-url $L1_RPC_URL
dcast send $GATEWAY_EXECUTE_OPERATOR --value 10ether --private-key $DEPLOYER_PK --rpc-url $L1_RPC_URL
dcast send $CHAIN1_COMMIT_OPERATOR  --value 10ether --private-key $DEPLOYER_PK --rpc-url $L1_RPC_URL
dcast send $CHAIN1_PROVE_OPERATOR   --value 10ether --private-key $DEPLOYER_PK --rpc-url $L1_RPC_URL
dcast send $CHAIN1_EXECUTE_OPERATOR --value 10ether --private-key $DEPLOYER_PK --rpc-url $L1_RPC_URL
dcast send $CHAIN2_COMMIT_OPERATOR  --value 10ether --private-key $DEPLOYER_PK --rpc-url $L1_RPC_URL
dcast send $CHAIN2_PROVE_OPERATOR   --value 10ether --private-key $DEPLOYER_PK --rpc-url $L1_RPC_URL
dcast send $CHAIN2_EXECUTE_OPERATOR --value 10ether --private-key $DEPLOYER_PK --rpc-url $L1_RPC_URL

# ─── [1] Deploy core ecosystem contracts (hub init) ──────────────────────────

pcli hub init \
    --owner $ECOSYSTEM_OWNER \
    --private-key $DEPLOYER_PK \
    --owner-pk $ECOSYSTEM_OWNER_PK \
    --l1-rpc-url=$L1_RPC_URL \
    --out=$CW/hub.init.json \
    -v

export BRIDGEHUB=$(jq -r '.output.deployed_addresses.bridgehub.bridgehub_proxy_addr' $WORK_DIR/hub.init.json)
export SHARED_BRIDGE=$(dcast call $BRIDGEHUB "sharedBridge()(address)" --rpc-url=$L1_RPC_URL)
export NTV=$(dcast call $SHARED_BRIDGE "nativeTokenVault()(address)" --rpc-url=$L1_RPC_URL)
export GOVERNANCE=$(jq -r '.output.deployed_addresses.governance_addr' $WORK_DIR/hub.init.json)
export CREATE2_FACTORY=$(jq -r '.output.contracts.create2_factory_addr' $WORK_DIR/hub.init.json)

# Register ZK token on NTV
dcast send $NTV "registerToken(address)" $ZK_TOKEN_ADDRESS --private-key $DEPLOYER_PK --rpc-url=$L1_RPC_URL
export ZK_TOKEN_ID=$(dcast call $NTV "assetId(address)(bytes32)" $ZK_TOKEN_ADDRESS --rpc-url=$L1_RPC_URL)
dcast send $ZK_TOKEN_ADDRESS "mint(address,uint256)" $GOVERNANCE 1000000000000000000000000000000000000000 --private-key=$DEPLOYER_PK --rpc-url=$L1_RPC_URL

# ─── [2] Deploy ZKSyncOS CTM ──────────────────────────────────────────────────

pcli ctm init \
    --bridgehub=$BRIDGEHUB \
    --vm-type=zksyncos \
    --private-key=$DEPLOYER_PK \
    --bridgehub-owner-pk=$ECOSYSTEM_OWNER_PK \
    --bridgehub-admin-pk=$ECOSYSTEM_OWNER_PK \
    --zk-token-asset-id=$ZK_TOKEN_ID \
    --create2-factory-addr=$CREATE2_FACTORY \
    --l1-rpc-url=$L1_RPC_URL \
    --out=$CW/ctm.zksyncos.init.json \
    -v

export CTM_PROXY=$(jq -r '.output.deployed_addresses.state_transition.state_transition_proxy_addr' $WORK_DIR/ctm.zksyncos.init.json)
export L1_DA_VALIDATOR=$(jq -r '.output.deployed_addresses.blobs_zksync_os_l1_da_validator_addr' $WORK_DIR/ctm.zksyncos.init.json)

# ─── [3] Deploy first chain (gateway chain, ZK base token, chain-id=271) ─────

pcli chain init \
    --ctm-proxy=$CTM_PROXY \
    --l1-da-validator=$L1_DA_VALIDATOR \
    --chain-id=$GATEWAY_CHAIN_ID \
    --owner=$GATEWAY_OWNER \
    --commit-operator=$GATEWAY_COMMIT_OPERATOR \
    --prove-operator=$GATEWAY_PROVE_OPERATOR \
    --execute-operator=$GATEWAY_EXECUTE_OPERATOR \
    --vm-type=zksyncos \
    --base-token-addr=$ZK_TOKEN_ADDRESS \
    --private-key=$DEPLOYER_PK \
    --owner-pk=$GATEWAY_OWNER_PK \
    --bridgehub-admin-pk=$ECOSYSTEM_OWNER_PK \
    --create2-factory-addr=$CREATE2_FACTORY \
    --l1-rpc-url=$L1_RPC_URL \
    --out=$CW/chain.gateway.init.json \
    -v

# ─── [4] Deploy second chain (ETH base token, chain-id=272) ──────────────────

pcli chain init \
    --ctm-proxy=$CTM_PROXY \
    --l1-da-validator=$L1_DA_VALIDATOR \
    --chain-id=$CHAIN1_CHAIN_ID \
    --owner=$CHAIN1_OWNER \
    --commit-operator=$CHAIN1_COMMIT_OPERATOR \
    --prove-operator=$CHAIN1_PROVE_OPERATOR \
    --execute-operator=$CHAIN1_EXECUTE_OPERATOR \
    --vm-type=zksyncos \
    --private-key=$DEPLOYER_PK \
    --owner-pk=$CHAIN1_OWNER_PK \
    --bridgehub-admin-pk=$ECOSYSTEM_OWNER_PK \
    --create2-factory-addr=$CREATE2_FACTORY \
    --l1-rpc-url=$L1_RPC_URL \
    --pause-deposits \
    --skip-priority-txs \
    --out=$CW/chain.chain1.init.json \
    -v

# ─── [5] Deploy third chain (ETH base token, chain-id=273) ───────────────────

pcli chain init \
    --ctm-proxy=$CTM_PROXY \
    --l1-da-validator=$L1_DA_VALIDATOR \
    --chain-id=$CHAIN2_CHAIN_ID \
    --owner=$CHAIN2_OWNER \
    --commit-operator=$CHAIN2_COMMIT_OPERATOR \
    --prove-operator=$CHAIN2_PROVE_OPERATOR \
    --execute-operator=$CHAIN2_EXECUTE_OPERATOR \
    --vm-type=zksyncos \
    --private-key=$DEPLOYER_PK \
    --owner-pk=$CHAIN2_OWNER_PK \
    --bridgehub-admin-pk=$ECOSYSTEM_OWNER_PK \
    --create2-factory-addr=$CREATE2_FACTORY \
    --l1-rpc-url=$L1_RPC_URL \
    --pause-deposits \
    --skip-priority-txs \
    --out=$CW/chain.chain2.init.json \
    -v

# ─── [6] Convert first chain (271) to be a gateway ───────────────────────────

# Deploy the transaction filterer on the gateway chain (required before whitelist grant)
pcli chain deploy-gateway-transaction-filterer \
    --bridgehub-proxy-address=$BRIDGEHUB \
    --chain-id=$GATEWAY_CHAIN_ID \
    --private-key=$ECOSYSTEM_OWNER_PK \
    --l1-rpc-url=$L1_RPC_URL

# Dump force_deployments_data for the gateway chain's CTM
pcli chain dump-gateway-force-deployments \
    --ctm-proxy=$CTM_PROXY \
    --dump-toml-rel=/script-out/force-dep-dump.toml \
    --l1-rpc-url=$L1_RPC_URL

export FORCE_DEPLOYMENTS_DATA=$(grep 'force_deployments_data' $WORK_DIR/script-out/force-dep-dump.toml | sed 's/force_deployments_data = //')

# Create the vote preparation input config
mkdir -p "$WORK_DIR/script-config"
cat > "$WORK_DIR/script-config/gateway-vote-preparation.toml" <<EOF
owner_address = "$ECOSYSTEM_OWNER"
testnet_verifier = true
support_l2_legacy_shared_bridge_test = false
is_zk_sync_os = true
zk_token_asset_id = "$ZK_TOKEN_ID"
refund_recipient = "$DEPLOYER"
gateway_chain_id = $GATEWAY_CHAIN_ID
gateway_settlement_fee = 500000000000000000
force_deployments_data = $FORCE_DEPLOYMENTS_DATA

[contracts]
governance_security_council_address = "$DEPLOYER"
governance_min_delay = 0
validator_timelock_execution_delay = 0
EOF

# Mount the script-config directory into the container for vote-prepare
export EXTRA_MOUNTS="$WORK_DIR/script-config:/contracts/l1-contracts/script-config"

# Extract the CTM deployment tracker address (needed for whitelist)
export STM_TRACKER=$(jq -r '.output.deployed_addresses.bridgehub.ctm_deployment_tracker_proxy_addr' $WORK_DIR/hub.init.json)

# Step 6a: Grant whitelist to deployer, governance, and STM tracker
pcli chain convert-to-gateway \
    --stage=grant-whitelist \
    --bridgehub-proxy-address=$BRIDGEHUB \
    --gateway-chain-id=$GATEWAY_CHAIN_ID \
    --whitelist-grantees=$GOVERNANCE \
    --whitelist-grantees=$DEPLOYER \
    --whitelist-grantees=$STM_TRACKER \
    --private-key=$ECOSYSTEM_OWNER_PK \
    --l1-rpc-url=$L1_RPC_URL

# Step 6b: Deploy gateway CTM contracts and prepare governance calls
#   ctm-representative-chain-id: any chain already registered on the CTM (use chain1=272)
pcli chain convert-to-gateway \
    --stage=vote-prepare \
    --bridgehub-proxy-address=$BRIDGEHUB \
    --gateway-chain-id=$GATEWAY_CHAIN_ID \
    --ctm-representative-chain-id=$GATEWAY_CHAIN_ID \
    --private-key=$DEPLOYER_PK \
    --l1-rpc-url=$L1_RPC_URL

# Step 6c: Execute governance calls
pcli chain convert-to-gateway \
    --stage=governance-execute \
    --bridgehub-proxy-address=$BRIDGEHUB \
    --gateway-chain-id=$GATEWAY_CHAIN_ID \
    --governance-address=$GOVERNANCE \
    --private-key=$ECOSYSTEM_OWNER_PK \
    --l1-rpc-url=$L1_RPC_URL

# Step 6d: Revoke deployer whitelist
pcli chain convert-to-gateway \
    --stage=revoke-whitelist \
    --bridgehub-proxy-address=$BRIDGEHUB \
    --gateway-chain-id=$GATEWAY_CHAIN_ID \
    --revoke-address=$DEPLOYER \
    --private-key=$ECOSYSTEM_OWNER_PK \
    --l1-rpc-url=$L1_RPC_URL

# ─── Fund chain admins with ZK tokens for migration L1→L2 priority tx gas ────
# The gateway uses ZK base token, so migrating a chain requires paying for the
# L1→L2 priority tx in ZK tokens. The forge script routes through the ChainAdmin
# contract (multicall), so the ChainAdmin itself needs ZK token balance.
# The approval to shared bridge happens inside the forge script's multicall.
export CHAIN1_DIAMOND=$(jq -r '.output.diamond_proxy_addr' $WORK_DIR/chain.chain1.init.json)
export CHAIN2_DIAMOND=$(jq -r '.output.diamond_proxy_addr' $WORK_DIR/chain.chain2.init.json)
export CHAIN1_ADMIN=$(dcast call $CHAIN1_DIAMOND "getAdmin()(address)" --rpc-url=$L1_RPC_URL)
export CHAIN2_ADMIN=$(dcast call $CHAIN2_DIAMOND "getAdmin()(address)" --rpc-url=$L1_RPC_URL)

echo "Chain1 admin: $CHAIN1_ADMIN, Chain2 admin: $CHAIN2_ADMIN"
dcast send $ZK_TOKEN_ADDRESS "mint(address,uint256)" $CHAIN1_ADMIN 1000000000000000000000000 --private-key=$DEPLOYER_PK --rpc-url=$L1_RPC_URL
dcast send $ZK_TOKEN_ADDRESS "mint(address,uint256)" $CHAIN2_ADMIN 1000000000000000000000000 --private-key=$DEPLOYER_PK --rpc-url=$L1_RPC_URL

# ─── [7] Migrate second chain (272) to gateway ───────────────────────────────

# Step 7a: Skip pause-deposits (already paused via --pause-deposits in chain init)

# Step 7b: Submit migration transaction (L1 → gateway L2)
pcli chain migrate-to-gateway \
    --stage=migrate \
    --bridgehub-proxy-address=$BRIDGEHUB \
    --chain-id=$CHAIN1_CHAIN_ID \
    --gateway-chain-id=$GATEWAY_CHAIN_ID \
    --l1-gas-price=1000000000 \
    --refund-recipient=$DEPLOYER \
    --private-key=$CHAIN1_OWNER_PK \
    --l1-rpc-url=$L1_RPC_URL

# Step 7c: Notify server about migration
pcli chain migrate-to-gateway \
    --stage=notify-server \
    --bridgehub-proxy-address=$BRIDGEHUB \
    --chain-id=$CHAIN1_CHAIN_ID \
    --private-key=$CHAIN1_OWNER_PK \
    --l1-rpc-url=$L1_RPC_URL

# ─── [8] Migrate third chain (273) to gateway ────────────────────────────────

# Step 8a: Skip pause-deposits (already paused via --pause-deposits in chain init)

# Step 8b: Submit migration transaction (L1 → gateway L2)
pcli chain migrate-to-gateway \
    --stage=migrate \
    --bridgehub-proxy-address=$BRIDGEHUB \
    --chain-id=$CHAIN2_CHAIN_ID \
    --gateway-chain-id=$GATEWAY_CHAIN_ID \
    --l1-gas-price=1000000000 \
    --refund-recipient=$DEPLOYER \
    --private-key=$CHAIN2_OWNER_PK \
    --l1-rpc-url=$L1_RPC_URL

# Step 8c: Notify server about migration
pcli chain migrate-to-gateway \
    --stage=notify-server \
    --bridgehub-proxy-address=$BRIDGEHUB \
    --chain-id=$CHAIN2_CHAIN_ID \
    --private-key=$CHAIN2_OWNER_PK \
    --l1-rpc-url=$L1_RPC_URL

# ─── private keys reference ───────────────────────────────────────────────────
# 0x5927c313861c01b82a026e35d93cc787e5356c0f 0xf00bf4165f9e1a67841b981949033c06c1423dab34c33d6d1237ae14d85bd729
# 0xff2087417fe10bc436b972aa0460fb5ec1024109 0x7940c26ae2aca94c65de3a75f977c96fc87930fa9c036bc63c57815ca4c5290f
# 0xfe81be741590259904834c895667d7b85e248191 0x8692b109efe6d78fce8f6290f7b22f9cb64464604bd1f0a06d4060d2f9083b50
# 0x76936581521a1bC051882BaE54b0A0fB62eDd3BE 0xcd7fb571363e68a72a7c7ba7464403bee437e9a91763a70d5f7bf9f5b514571b
# 0x2552675d2DE8155d4713374A9cc240E5bB3a3c0c 0x78400d6d6554c7b8df7868ef9c79eb4f9fd4bdd3e72db017f6cb8118d62c47cd
# 0x50235771961984B0eE83a00db9afFf61D23f3460 0xd2486e5caa009a62d609e348b1f9b54c5fdc374ca29e5b74d7a6e12b4b81fea2
# 0xDE8675d9758CfDf2A78A2CD9775a6bB9BE6C4C14 0xe730d449afa608b52cc7af0983d438a47c5e1379b9865277677bf94646181264
# 0x17B4E8d4211a1d734ff485046a84f79CAD7Ee372 0x21f72b201ca1722acb44fbcbd125c91a54c04b086d21f2c142b64e669858d5e0
# 0x13bb1fE4A82F01706732fb3dAAF1768d5E48cCdE 0xf5e174adffac202eb71379332fd22be3754f851be5aba9bf3f49ce17805f5983
