# Development Guide for AI Agents

## When to Comment

Write comments that remain valuable after the PR is merged. Future readers only see the current code — not the PR, not the history.

### Do: explain WHY and non-obvious behavior

```rust
// Keep tmp alive until after spawn completes (anvil reads the file at startup).

// Two formats are in use for .gz fixtures:
//   Format A: gzip(state_json)                    — plain JSON state, used by save_state
//   Format B: gzip("0x" + hex(gzip(state_json)))  — produced by cast rpc anvil_dumpState
// Detect by whether the decompressed content starts with `"0x`.

// Drop order matters: servers first, then Anvil, then workdir.

// Concurrent same-key builders may race; last rename wins, both valid.
```

### Don't: describe what the code does

```rust
// ❌ BAD — restates the code in English
// Query l2TransactionBaseCost to compute the required msg.value.

// ❌ BAD — labels a section instead of explaining it
// Main entry point
// ---------------------------------------------------------------------------

// ❌ BAD — navigation header with no explanatory content
// ── Wallets ──────────────────────────────────────────────────────────────
```

### The test: "Will this make sense in 6 months?"

Before adding a comment, ask: would someone reading just the current code (no PR, no history) find this helpful?

**Comment when:**
- Non-obvious behavior or edge cases
- Performance trade-offs
- Safety requirements (unsafe blocks must always be documented)
- Limitations or gotchas
- Why simpler alternatives don't work

**Don't comment when:**
- Code is self-explanatory
- Just restating the code in English
- Describing what changed in this PR
- Labeling a section of code (section headers add no value)
