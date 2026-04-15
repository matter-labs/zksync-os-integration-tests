#!/usr/bin/env bash
#
# Orchestrator: run integration tests preset by preset.
#
# Tests within a single preset share the same generated l1-state and run in
# parallel via cargo-nextest (cargo test by itself does not parallelise across
# integration-test binaries). Presets run sequentially so the l1-state
# generation step does not have to contend with concurrent tests.
#
# Requires `cargo-nextest`: `cargo install cargo-nextest --locked`.
#
# Usage:
#   ./run-tests.sh                              # run all presets from presets.yaml
#   ./run-tests.sh --presets custom.yaml         # use a different presets file
#   ./run-tests.sh --preset v31_draft            # run a single preset
#   ./run-tests.sh --test l1_settling_test       # run a single test across all presets that include it
#   ./run-tests.sh --preset v31_draft --test l1_settling_test
#   ./run-tests.sh --skip-generate               # skip l1-state generation, tests find cached state
#   ./run-tests.sh --rebuild-cache               # delete cached l1-state before generating
#
set -euo pipefail

if ! cargo nextest --version >/dev/null 2>&1; then
  echo "ERROR: cargo-nextest not found. Install it with:"
  echo "    cargo install cargo-nextest --locked"
  exit 1
fi

PRESETS_FILE="presets.yaml"
FILTER_PRESET=""
FILTER_TEST=""
SKIP_GENERATE=false
REBUILD_CACHE=false

while [[ $# -gt 0 ]]; do
  case "$1" in
    --presets)         PRESETS_FILE="$2"; shift 2 ;;
    --preset)          FILTER_PRESET="$2"; shift 2 ;;
    --test)            FILTER_TEST="$2";   shift 2 ;;
    --skip-generate)   SKIP_GENERATE=true; shift ;;
    --rebuild-cache)   REBUILD_CACHE=true; shift ;;
    -h|--help)
      sed -n '3,18p' "$0"
      exit 0
      ;;
    *) echo "Unknown flag: $1"; exit 1 ;;
  esac
done

if [[ ! -f "$PRESETS_FILE" ]]; then
  echo "ERROR: presets file not found: $PRESETS_FILE"
  exit 1
fi

# ---------------------------------------------------------------------------
# Parse presets YAML (just preset names + test lists)
# ---------------------------------------------------------------------------
PAIRS=""
current_preset=""
in_tests=false

while IFS= read -r line; do
  [[ -z "$line" || "$line" =~ ^[[:space:]]*# ]] && continue

  if [[ "$line" =~ ^([a-zA-Z0-9_-]+): ]]; then
    current_preset="${BASH_REMATCH[1]}"
    in_tests=false
    continue
  fi

  if [[ "$line" =~ ^[[:space:]]+tests: ]]; then
    in_tests=true
    continue
  fi

  if $in_tests && [[ "$line" =~ ^[[:space:]]+-[[:space:]]+(.+) ]]; then
    PAIRS="${PAIRS}${current_preset} ${BASH_REMATCH[1]}"$'\n'
    continue
  fi

  if [[ "$line" =~ ^[[:space:]]+[a-zA-Z] ]]; then
    in_tests=false
  fi
done < "$PRESETS_FILE"

all_presets=$(echo "$PAIRS" | awk 'NF {print $1}' | sort -u)

# ---------------------------------------------------------------------------
# Validate --preset filter
# ---------------------------------------------------------------------------
if [[ -n "$FILTER_PRESET" ]]; then
  if ! echo "$all_presets" | grep -qx "$FILTER_PRESET"; then
    echo "ERROR: preset '$FILTER_PRESET' not found in $PRESETS_FILE"
    echo "Available presets:"
    echo "$all_presets" | sed 's/^/  /'
    exit 1
  fi
  all_presets="$FILTER_PRESET"
fi

# ---------------------------------------------------------------------------
# Clean up stragglers from any previous run before we start.
#
# If a previous invocation crashed (test panic, CI SIGKILL, ctrl-C between
# presets), child zksync-os-server / anvil processes and their Docker
# containers can survive. They don't clash on ports (we pick random unused
# ports) but they:
#   - hold open file descriptors on RocksDB LOCK files, so the next
#     `rm -rf test-run-logs/.../db_*` below "succeeds" but the directory
#     stays on disk via the leaked fd, and the next server spawns against
#     a phantom-empty DB that the old process is still writing into
#     (corrupt state, server-unable-to-connect symptoms on the next run).
#   - accumulate and eat memory/CPU on CI boxes.
# ---------------------------------------------------------------------------
pkill -f 'target/(release|debug)/zksync-os-server' 2>/dev/null || true
pkill -f 'anvil.*--(load|dump)-state'             2>/dev/null || true
if command -v docker >/dev/null 2>&1; then
  docker ps -aq --filter "name=integration-tests-zksync-os-server-" \
    | xargs -r docker rm -f >/dev/null 2>&1 || true
fi

# ---------------------------------------------------------------------------
# Run
# ---------------------------------------------------------------------------
total_pass=0
total_fail=0
failed_list=""

mkdir -p test-run-logs

for preset in $all_presets; do
  tests=$(echo "$PAIRS" | awk -v p="$preset" '$1 == p {print $2}')

  echo ""
  echo "========================================"
  echo "Preset: $preset"
  echo "========================================"

  # Clean stale test-run-logs (server logs, rocksdb) but keep contracts_artifacts/.
  # We must preserve contracts_artifacts/ because of a macOS Docker/VirtioFS bug:
  # newly created host directories are invisible to the container VM, so the
  # Docker session bind-mounts the stable parent (test-run-logs/) and creates
  # sub-directories inside the container. Deleting contracts_artifacts/ would
  # force a new mkdir that the running container cannot see.
  if [[ -d test-run-logs ]]; then
    find test-run-logs -mindepth 2 -maxdepth 2 ! -name contracts_artifacts -exec rm -rf {} + 2>/dev/null || true
  fi

  # Generate ecosystem (tests resolve the cache dir themselves via preset)
  if ! $SKIP_GENERATE; then
    if $REBUILD_CACHE; then
      echo "--- [$preset] Clearing l1-state cache ---"
      rm -rf l1-state-cache
    fi
    echo "--- [$preset] Generating ecosystem ---"
    if ! cargo run --release -p generate-l1-state -- "$preset"; then
      echo "ERROR: generate-l1-state failed for preset '$preset'"
      exit 1
    fi
  fi

  # Gather tests for this preset, honouring --test filter.
  selected_tests=()
  for test_name in $tests; do
    [[ -n "$FILTER_TEST" && "$test_name" != "$FILTER_TEST" ]] && continue
    selected_tests+=("$test_name")
  done

  if [[ ${#selected_tests[@]} -eq 0 ]]; then
    continue
  fi

  # Build a nextest filterset expression: binary(a) + binary(b) + ...
  # Each preset-entry maps to a test file (one #[tokio::test] per binary),
  # so filtering by binary name is exact.
  filterset=""
  for test_name in "${selected_tests[@]}"; do
    if [[ -z "$filterset" ]]; then
      filterset="binary(${test_name})"
    else
      filterset="${filterset} + binary(${test_name})"
    fi
  done

  echo ""
  echo "--- [$preset] Running ${#selected_tests[@]} tests in parallel via nextest ---"
  echo "  filter: $filterset"

  # Capture stderr so we can parse per-test pass/fail after the run.
  # Nextest emits lines like:
  #   PASS [   0.234s] (1/3) integration-tests::<binary> <test_fn>
  #   FAIL [   1.234s] (2/3) integration-tests::<binary> <test_fn>
  # to stderr for every test, and exits non-zero if any failed.
  nextest_stderr="test-run-logs/nextest-${preset}.stderr"
  mkdir -p "$(dirname "$nextest_stderr")"

  set +e
  # NO_COLOR=1 so our grep sees plain text (nextest otherwise wraps
  # PASS/FAIL tokens in ANSI escape codes when writing to a tty-like sink).
  NO_COLOR=1 PRESET_NAME="$preset" PRESETS_FILE="$PRESETS_FILE" \
    cargo nextest run \
      --package integration-tests \
      --no-fail-fast \
      -E "$filterset" \
      2> >(tee "$nextest_stderr" >&2)
  nextest_exit=$?
  set -e

  # Parse pass/fail per test binary. Scraping nextest's human output
  # with regex is admittedly a bit overkill — the simpler alternative is
  # to trust nextest's own exit code and treat the whole preset as one
  # pass/fail. We do it this way because it keeps the per-test
  # granularity (and the `failed_list` summary) that the script had
  # before nextest, which is much more useful for diagnosing CI runs
  # with many passing tests and a single failure.
  #
  # The pattern accounts for nextest's `(N/M)` progress counter between
  # the timing block and the binary name; `[[:space:]]+` handles any
  # inter-token whitespace. Sample lines that must match:
  #
  #   PASS [   0.234s] integration-tests::<binary> <test_fn>
  #   PASS [  41.301s] (1/3) integration-tests::<binary> <test_fn>
  #   FAIL [   1.234s] (2/3) integration-tests::<binary> <test_fn>
  for test_name in "${selected_tests[@]}"; do
    if grep -Eq "PASS[[:space:]]+\[[^]]*\][[:space:]]+(\([0-9]+/[0-9]+\)[[:space:]]+)?integration-tests::${test_name}[[:space:]]" "$nextest_stderr"; then
      echo "  PASS: $test_name"
      total_pass=$((total_pass + 1))
    elif grep -Eq "FAIL[[:space:]]+\[[^]]*\][[:space:]]+(\([0-9]+/[0-9]+\)[[:space:]]+)?integration-tests::${test_name}[[:space:]]" "$nextest_stderr"; then
      echo "  FAIL: $test_name"
      total_fail=$((total_fail + 1))
      failed_list="${failed_list}  - ${preset}/${test_name}"$'\n'
    else
      # Neither PASS nor FAIL observed — treat as failure (build error,
      # nextest crash, filter mismatch, etc).
      echo "  FAIL: $test_name (no nextest status — see $nextest_stderr)"
      total_fail=$((total_fail + 1))
      failed_list="${failed_list}  - ${preset}/${test_name}"$'\n'
    fi
  done

  if [[ $nextest_exit -ne 0 ]]; then
    echo "  (nextest exited with $nextest_exit)"
  fi
done

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo ""
echo "========================================"
echo "Summary: $total_pass passed, $total_fail failed"
if [[ $total_fail -gt 0 ]]; then
  echo "Failed:"
  echo -n "$failed_list"
fi
echo "========================================"
[[ $total_fail -eq 0 ]]
