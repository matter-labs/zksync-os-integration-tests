//! End-to-end atomic-interop swap between two L1-settling chains (no gateway in the path), driven
//! natively in Rust.
//!
//! The `ecosystem` fixture with `#[with(vec![6565, 6566])]` brings up two L1-settling ZKsync OS
//! chains on one Anvil L1, each with its own in-process server. The full bundle-model atomic swap is
//! implemented once in [`tests::atomic_swap::run_atomic_swap`] and shared with the `atomic-swap-demo`
//! binary; this test just wires the fixture into it and lets the engine assert every side effect
//! (burns, leg-committed state, real L1-settled proofs, destination mints, FullyExecuted bundles).
//!
//! Requirements:
//! - `PROTOCOL_CONTRACTS_ROOT` must point at an era-contracts checkout carrying the atomic-interop
//!   contracts (atomic genesis contracts + interop enabled without a gateway) — the
//!   `atomic-imt-interop-release` line this workspace pins. The atomic-interop contract bindings are
//!   artifact-sourced at build time from that checkout's committed `zkstack-out` ABIs (see
//!   `tests/build.rs`), so a contract-shape change fails this test to compile.
//! - The zksync-os-server build must carry the L1 aggregation-hop proof and the `zks_getImt*` RPCs
//!   (zksync-os-server#1413, on `main` since 0.21).

use alloy::primitives::U256;
use anyhow::Result;
use rstest::rstest;

use tests::atomic_swap::{run_atomic_swap, token_unit, AtomicSwapParams, Layout, DEFAULT_DEADLINE};
use tests::fixtures::ecosystem;
use tests::Ecosystem;

#[rstest]
#[tokio::test(flavor = "multi_thread")]
async fn atomic_swap_l1_settled(
    #[future]
    #[with(vec![6565, 6566])]
    ecosystem: Ecosystem,
) -> Result<()> {
    let eco = ecosystem.await;
    let chains: Vec<_> = eco.chains().collect();
    let (ca, cb) = (chains[0], chains[1]);
    // The rich wallet (#0) is funded on both chains; use it as the depositor/recipient.
    let signer = ca.wallet(0).clone();

    let unit = token_unit();
    run_atomic_swap(AtomicSwapParams {
        rpc_a: ca.l2_rpc_url().to_string(),
        rpc_b: cb.l2_rpc_url().to_string(),
        l1_rpc: ca.l1_rpc_url().to_string(),
        signer,
        a_amount: unit * U256::from(10u64),
        b_amount: unit * U256::from(7u64),
        supply: unit * U256::from(1_000_000u64),
        deadline: DEFAULT_DEADLINE,
        // The `ecosystem` fixture forge-builds this same checkout, so `out/TestnetERC20Token` exists.
        era_root: protocol_ops::common::paths::contracts_root(),
        // Fresh fixture chains are not registered with each other yet; the engine registers them
        // permissionlessly via the L1 bridgehub.
        bridgehub: Some(ca.bridgehub_addr()),
        layout: Layout::default(),
    })
    .await
}
