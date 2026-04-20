#!/usr/bin/env bash
#
# Wrapper that runs protocol_ops (and forge / cast) against a specific version
# of the matter-labs/era-contracts codebase. The version is selected by the
# first positional argument, which can be either:
#
#   * a Docker image tag of ghcr.io/matter-labs/protocol-ops, e.g. "latest"
#     or "gateway-commands-in-protocol-ops" — the command runs inside the
#     corresponding container;
#
#   * a path to a local era-contracts checkout (anything containing "/",
#     ".", or ".."), e.g. "../era-contracts" or "/abs/path/era-contracts" —
#     the command runs directly on the host against that checkout.
#
# Usage:
#   ./protocol-ops.sh <tag-or-path> protocol_ops ecosystem init --l1-rpc-url http://localhost:8545 ...
#   ./protocol-ops.sh <tag-or-path> forge script deploy-scripts/Foo.s.sol --rpc-url http://localhost:8545 ...
#   ./protocol-ops.sh <tag-or-path> cast call 0x... "foo()(uint256)" --rpc-url http://localhost:8545
#
# Environment:
#   WORK_DIR         — Docker mode: host directory mounted into the container
#                      for output files (default: ./protocol-ops-workdir).
#                      Ignored in local mode.
#   EXTRA_MOUNTS     — Docker mode: space-separated host:container mount pairs,
#                      e.g. "/tmp/cfg:/contracts/cfg".
#   PROTOCOL_OPS_BIN — Local mode only: path to a pre-built protocol_ops
#                      binary. If unset, the wrapper runs `cargo build
#                      --release` inside <era-path>/protocol-ops and uses the
#                      resulting target/release binary.
#
set -euo pipefail

IMAGE_REPO="ghcr.io/matter-labs/protocol-ops"

usage() {
  cat >&2 <<'EOF'
Usage: protocol-ops.sh <tag-or-era-path> <command> [args...]

<tag-or-era-path>:
  - anything with "/", "." or ".." → local era-contracts checkout
  - anything else                   → Docker image tag

<command> is one of: protocol_ops, forge, cast.

Examples:
  protocol-ops.sh latest  protocol_ops ecosystem init --l1-rpc-url …
  protocol-ops.sh ../era-contracts  forge script deploy-scripts/Foo.s.sol --rpc-url …
  protocol-ops.sh /abs/era  cast call 0xaddr 'foo()(uint256)' --rpc-url …
EOF
  exit 1
}

[[ $# -lt 2 ]] && usage

SOURCE="$1"; shift
CMD="$1"

# ──────────────────────────────────────────────────────────────────────
# Mode detection
#   Docker tags are short identifiers without path characters
#   (e.g. "latest", "v1.2.3", "gateway-commands-in-protocol-ops").
#   Anything containing "/", or that equals "." / "..", is treated as
#   a path to an era-contracts checkout.
# ──────────────────────────────────────────────────────────────────────
is_local_path() {
  case "$1" in
    */*|.|..) return 0 ;;
    *)        return 1 ;;
  esac
}

# ──────────────────────────────────────────────────────────────────────
# Local mode — run against an on-disk era-contracts checkout.
# ──────────────────────────────────────────────────────────────────────
if is_local_path "$SOURCE"; then
  if [[ ! -d "$SOURCE" ]]; then
    echo "error: '$SOURCE' is not a directory (expected an era-contracts checkout)" >&2
    exit 1
  fi
  ERA_PATH="$(cd "$SOURCE" && pwd)"
  if [[ ! -d "$ERA_PATH/protocol-ops" || ! -d "$ERA_PATH/l1-contracts" ]]; then
    echo "error: '$ERA_PATH' does not look like an era-contracts checkout" >&2
    echo "       (missing protocol-ops/ or l1-contracts/ subdirectory)" >&2
    exit 1
  fi

  case "$CMD" in
    protocol_ops)
      shift
      BIN="${PROTOCOL_OPS_BIN:-}"
      if [[ -z "$BIN" ]]; then
        BIN="$ERA_PATH/protocol-ops/target/release/protocol_ops"
        # Always rebuild — cargo is a no-op when nothing changed.
        (cd "$ERA_PATH/protocol-ops" && cargo build --release --quiet)
      fi
      exec env PROTOCOL_CONTRACTS_ROOT="$ERA_PATH" "$BIN" "$@"
      ;;
    forge)
      shift
      # forge must run from the l1-contracts directory so it can find
      # foundry.toml and the Solidity sources.
      cd "$ERA_PATH/l1-contracts"
      exec env PROTOCOL_CONTRACTS_ROOT="$ERA_PATH" forge "$@"
      ;;
    cast)
      shift
      exec cast "$@"
      ;;
    *)
      echo "error: unsupported command '$CMD' in local mode" >&2
      echo "       expected one of: protocol_ops, forge, cast" >&2
      exit 1
      ;;
  esac
fi

# ──────────────────────────────────────────────────────────────────────
# Docker mode — run inside the matter-labs/protocol-ops image.
# ──────────────────────────────────────────────────────────────────────
TAG="$SOURCE"
IMAGE="${IMAGE_REPO}:${TAG}"

WORK_DIR="${WORK_DIR:-$(pwd)/protocol-ops-workdir}"
mkdir -p "${WORK_DIR}/script-out"
WORK_DIR="$(cd "${WORK_DIR}" && pwd)"   # absolute path

CONTAINER_WORK="/contracts/work/session"

# Container-side working directory. forge needs /contracts/l1-contracts so it
# can find foundry.toml; protocol_ops and cast don't care.
container_workdir=""
if [[ "$CMD" == "forge" ]]; then
  container_workdir="/contracts/l1-contracts"
fi

# ── Platform-specific networking ──────────────────────────────────────
# Linux: --network=host lets the container reach localhost directly.
# macOS (Docker Desktop): host network mode doesn't work; use
#   host.docker.internal and rewrite localhost / 127.0.0.1 URLs in args.
docker_args=(
  run --rm
  --platform=linux/amd64
  -e FOUNDRY_DISABLE_NIGHTLY_WARNING=1
  -v "${WORK_DIR}:${CONTAINER_WORK}"
  -v "${WORK_DIR}/script-out:/contracts/l1-contracts/script-out"
)

run_args=("$@")

if [[ "$(uname -s)" == "Linux" ]]; then
  docker_args+=(--network=host)
  docker_args+=(-e ETH_RPC_URL="${ETH_RPC_URL:-http://localhost:8545}")
else
  # macOS / Docker Desktop: rewrite localhost → host.docker.internal in args.
  docker_args+=(--add-host=host.docker.internal:host-gateway)
  docker_args+=(-e ETH_RPC_URL="${ETH_RPC_URL:-http://host.docker.internal:8545}")
  rewritten=()
  for arg in "$@"; do
    arg="${arg//:\/\/localhost:/:\/\/host.docker.internal:}"
    arg="${arg//:\/\/127.0.0.1:/:\/\/host.docker.internal:}"
    rewritten+=("$arg")
  done
  run_args=("${rewritten[@]}")
fi

# Extra mounts from env (space-separated "host:container" pairs).
for mount in ${EXTRA_MOUNTS:-}; do
  docker_args+=(-v "$mount")
done

if [[ -n "${container_workdir}" ]]; then
  docker_args+=(-w "${container_workdir}")
fi

docker_args+=("${IMAGE}")

exec docker "${docker_args[@]}" "${run_args[@]}"
