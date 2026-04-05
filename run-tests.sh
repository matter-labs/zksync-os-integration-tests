#!/usr/bin/env bash
#
# Orchestrator: run integration tests preset by preset.
#
# Usage:
#   ./run-tests.sh                              # run all presets from presets.yaml
#   ./run-tests.sh --presets custom.yaml         # use a different presets file
#   ./run-tests.sh --preset v31_draft            # run a single preset
#   ./run-tests.sh --test l1_settling_test       # run a single test across all presets that include it
#   ./run-tests.sh --preset v31_draft --test l1_settling_test
#   ./run-tests.sh --skip-generate               # skip l1-state generation, tests find cached state
#
set -euo pipefail

PRESETS_FILE="presets.yaml"
FILTER_PRESET=""
FILTER_TEST=""
SKIP_GENERATE=false

while [[ $# -gt 0 ]]; do
  case "$1" in
    --presets)         PRESETS_FILE="$2"; shift 2 ;;
    --preset)          FILTER_PRESET="$2"; shift 2 ;;
    --test)            FILTER_TEST="$2";   shift 2 ;;
    --skip-generate)   SKIP_GENERATE=true; shift ;;
    -h|--help)
      sed -n '3,12p' "$0"
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
# Test name -> cargo command
# ---------------------------------------------------------------------------
test_command() {
  echo "cargo test --package integration-tests --test $1 -- --nocapture"
}

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
# Run
# ---------------------------------------------------------------------------
total_pass=0
total_fail=0
failed_list=""

for preset in $all_presets; do
  tests=$(echo "$PAIRS" | awk -v p="$preset" '$1 == p {print $2}')

  echo ""
  echo "========================================"
  echo "Preset: $preset"
  echo "========================================"

  # Generate ecosystem (tests resolve the cache dir themselves via preset)
  if ! $SKIP_GENERATE; then
    echo "--- [$preset] Generating ecosystem ---"
    if ! cargo run --release -p generate-l1-state -- "$preset"; then
      echo "ERROR: generate-l1-state failed for preset '$preset'"
      exit 1
    fi
  fi

  for test_name in $tests; do
    [[ -n "$FILTER_TEST" && "$test_name" != "$FILTER_TEST" ]] && continue

    cmd=$(test_command "$test_name")
    echo ""
    echo "--- [$preset] $test_name ---"
    echo "  $cmd"

    if eval "PRESET_NAME=$preset PRESETS_FILE=$PRESETS_FILE $cmd"; then
      echo "  PASS: $test_name"
      total_pass=$((total_pass + 1))
    else
      echo "  FAIL: $test_name"
      total_fail=$((total_fail + 1))
      failed_list="${failed_list}  - ${preset}/${test_name}"$'\n'
    fi
  done
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
