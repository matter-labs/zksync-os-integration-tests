//! This build script used to invoke `yarn build-all-contracts` and
//! `cargo build --release` in each local era-contracts / zksync-os-server
//! path referenced by `presets.yaml`. It has been stripped to a no-op.
//!
//! Why: cargo only re-runs build scripts when something declared via
//! `cargo:rerun-if-changed` actually changes. Declaring every external
//! source tree would either be incomplete (stale artifacts sneak into
//! tests) or wildly over-broad (rebuild on every sub-file touch). Neither
//! is honest.
//!
//! Instead, `run-tests.sh` invokes the dedicated `build-artifacts` tool
//! (`tools/build-artifacts`) before each preset's test run. That tool
//! always calls yarn + cargo unconditionally; yarn/forge/cargo handle
//! their own incremental checks, so the no-op cost is small and stale
//! artifacts can't hide.
//!
//! If you are running `cargo test -p integration-tests` directly (without
//! run-tests.sh), invoke the tool first:
//!
//!     cargo run --release -p build-artifacts
//!
//! Opt out of even this no-op run with `INTEGRATION_TESTS_SKIP_BUILD=1`
//! (kept for backwards compatibility; there is no build to skip).
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=INTEGRATION_TESTS_SKIP_BUILD");
}
