#!/usr/bin/env bash
#
# Wrapper that runs protocol_ops (and forge/cast) commands inside the
# ghcr.io/matter-labs/protocol-ops Docker image.
#
# Usage:
#   ./protocol-ops.sh <image-tag> protocol_ops ecosystem init --l1-rpc-url http://localhost:8545 ...
#   ./protocol-ops.sh <image-tag> forge script deploy-scripts/Foo.s.sol --rpc-url http://localhost:8545 ...
#   ./protocol-ops.sh <image-tag> cast call 0x... "foo()(uint256)" --rpc-url http://localhost:8545
#
# The first argument is the Docker image tag (e.g. "latest", "gateway-commands-in-protocol-ops").
# Everything after it is the command to run inside the container.
#
# Environment:
#   WORK_DIR       — host directory mounted into the container for output files (default: ./protocol-ops-workdir)
#   EXTRA_MOUNTS   — space-separated host:container mount pairs (e.g. "/tmp/cfg:/contracts/cfg")
#
set -euo pipefail

IMAGE_REPO="ghcr.io/matter-labs/protocol-ops"

if [[ $# -lt 2 ]]; then
  echo "Usage: $0 <image-tag> <command> [args...]"
  echo ""
  echo "Examples:"
  echo "  $0 latest protocol_ops ecosystem init --l1-rpc-url http://localhost:8545 --private-key 0xac0974..."
  echo "  $0 latest forge script deploy-scripts/Foo.s.sol --rpc-url http://localhost:8545 --broadcast"
  echo "  $0 latest cast call 0xaddr 'foo()(uint256)' --rpc-url http://localhost:8545"
  exit 1
fi

TAG="$1"; shift
IMAGE="${IMAGE_REPO}:${TAG}"

WORK_DIR="${WORK_DIR:-$(pwd)/protocol-ops-workdir}"
mkdir -p "${WORK_DIR}/script-out"
WORK_DIR="$(cd "${WORK_DIR}" && pwd)"   # absolute path

CONTAINER_WORK="/contracts/work/session"

# ── Rewrite localhost URLs to host.docker.internal ──────────────────────
# The container cannot reach the host via localhost; Docker provides
# host.docker.internal as the DNS name for the host network.
rewrite_args=()
for arg in "$@"; do
  arg="${arg//:\/\/localhost:/:\/\/host.docker.internal:}"
  arg="${arg//:\/\/127.0.0.1:/:\/\/host.docker.internal:}"
  rewrite_args+=("$arg")
done

# ── Determine working directory inside the container ──────────────────
# forge must run from /contracts/l1-contracts so it can find
# the Solidity sources and foundry.toml.
container_workdir=""
case "${rewrite_args[0]}" in
  forge)
    container_workdir="/contracts/l1-contracts"
    ;;
esac

# ── Build docker run command ────────────────────────────────────────────
docker_args=(
  run --rm
  --platform=linux/amd64
  --add-host=host.docker.internal:host-gateway
  -e FOUNDRY_DISABLE_NIGHTLY_WARNING=1
  -e ETH_RPC_URL="${ETH_RPC_URL:-http://host.docker.internal:8545}"
  -v "${WORK_DIR}:${CONTAINER_WORK}"
  -v "${WORK_DIR}/script-out:/contracts/l1-contracts/script-out"
)

# Extra mounts from env (space-separated "host:container" pairs)
for mount in ${EXTRA_MOUNTS:-}; do
  docker_args+=(-v "$mount")
done

if [[ -n "${container_workdir}" ]]; then
  docker_args+=(-w "${container_workdir}")
fi

docker_args+=("${IMAGE}")

exec docker "${docker_args[@]}" "${rewrite_args[@]}"
