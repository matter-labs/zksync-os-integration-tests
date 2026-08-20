---
name: upgrade-contracts
description: Use when bumping a preset's era-contracts (or zksync-os-server) ref to a new branch/SHA, OR when generate-l1-state fails with `Failed to pull protocol-ops image ... manifest unknown` / `not found`. Covers updating presets.yaml, ensuring the GHCR image is actually published (PR builds DO NOT push), and verifying the resolver picks up the new ref.
---

# Upgrading contracts in a preset

This skill applies when changing what `era_contracts` (or `zksync_os_server`) a preset in `presets.yaml` points at — typically pointing at a new branch or commit so integration tests exercise newer contract code. Also use it when `generate-l1-state` fails because the resolved Docker image isn't in GHCR.

## Background: how preset refs become images

Resolution chain (`integration-tests/src/presets.rs:300`):
1. Resolve the ref to a full commit SHA via `git ls-remote`.
2. Look for `ghcr.io/matter-labs/protocol-ops:{sha}` (full SHA tag).
3. If missing, walk back up to 10 ancestor commits looking for any published image.
4. If still nothing, emit a warning and use the SHA as the tag anyway → `docker pull` fails with `manifest unknown`.

**The gotcha:** PR-triggered runs of `era-contracts/.github/workflows/build-docker.yaml` build the image but **do not push it** (`push: ${{ github.event_name == 'workflow_dispatch' }}`). A green CI checkmark on a PR does NOT mean the image is in GHCR. Only `workflow_dispatch` runs publish, and they only tag the full SHA when invoked with `image_tag_override`.

## Steps

### 1. Update the preset

Edit `presets.yaml` and change the `era_contracts` (or `zksync_os_server`) value to the new branch/tag/SHA. Use a branch name during active development; the resolver will follow the tip.

```yaml
v30_to_v31_upgrade:
  era_contracts: kl/anvil-hardening   # was: main
```

### 2. Publish a Docker image at the full-SHA tag

For each ref you bumped, dispatch the build-docker workflow with the full SHA as `image_tag_override` so the resolver finds it:

```bash
SHA=$(gh api repos/matter-labs/era-contracts/branches/<BRANCH> --jq '.commit.sha')
gh workflow run build-docker.yaml \
  --repo matter-labs/era-contracts \
  --ref <BRANCH> \
  -f ref=<BRANCH> \
  -f image_tag_override=$SHA
```

Same pattern for `zksync-os-server` if its workflow has the same PR-doesn't-push behavior — verify before assuming. Quick check that the image exists:

```bash
docker manifest inspect ghcr.io/matter-labs/protocol-ops:$SHA
docker manifest inspect ghcr.io/matter-labs/zksync-os-server:$SHA
```

(Expect `manifest unknown` until the dispatched build finishes.)

### 3. Wait for the build, then re-run

Wait for the dispatched workflow to finish:

```bash
gh run list --repo matter-labs/era-contracts --workflow build-docker.yaml --limit 3
```

Once the image is published, re-run tests:

```bash
./run-tests.sh --preset <preset_name> --rebuild-cache
```

`--rebuild-cache` is required when changing refs — the cached `l1-state-cache/<preset>/...` directory is keyed by the old image SHAs and won't be invalidated automatically.

### 4. Verify the resolved SHAs

After the run starts (or after it completes), check that the resolver actually picked up the new commit:

```bash
cat test-run-logs/<preset>/resolved-refs.json
```

If `used_fallback: true`, the tip image still wasn't found and the resolver walked back to an ancestor — usually means the workflow_dispatch hasn't completed yet, or it built but didn't push (re-check the workflow run logs).

## Common pitfalls

- **"CI on the era-contracts PR is green, why does the image not exist?"** — Because PR builds don't push. See background section. Run the `workflow_dispatch` step.
- **Branch tip moves while you're working.** The SHA you dispatched is now an ancestor; the resolver finds it via the 10-commit fallback and emits a warning. Either dispatch again for the new tip, or accept the fallback (it's load-bearing for exactly this case).
- **Forgot `--rebuild-cache`.** Tests will silently run against the OLD contracts because `l1-state-cache/` is reused. Symptom: the new contract behavior you expected to see isn't there.
- **Don't add local-development presets to `presets.yaml`.** Use a gitignored `local_presets.yaml` and `--presets local_presets.yaml` (this file is committed and shared).
