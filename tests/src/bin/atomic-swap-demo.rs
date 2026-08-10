//! Atomic-interop demo: a cross-chain ATOMIC SWAP across a running two-chain (or multi-chain) stack,
//! driven natively in Rust. Chain A sends token X to a user on chain B, and chain B sends token Y to
//! the same user on chain A; both legs are bound into ONE atomic flow (the IMT bundle model) — either
//! both execute or neither does.
//!
//! This is a thin CLI wrapper: it reads RPCs/keys from the environment, gates on the target chains
//! being atomic-capable, and then runs the exact same [`tests::atomic_swap::run_atomic_swap`] engine
//! the `atomic_swap_l1_settled` integration test uses. One implementation, two callers.
//!
//! ── Requirements ──────────────────────────────────────────────────────────────────────────────
//! Needs a zksync-os-server that predeploys the atomic built-ins (`L2InteropCommitmentTree` @0x10012,
//! `AtomicFlowManager` @0x10014) with the atomic protocol layout (`InteropCenter` @0x1000d,
//! `InteropHandler` @0x1000e): a server carrying the L1 aggregation-hop proof
//! (zksync-os-server#1413) plus an atomic-interop era-contracts genesis
//! (`atomic-imt-interop-release`). The two chains must be registered with each other for
//! interop — either set `ATOMIC_BRIDGEHUB` so this demo registers them (permissionless L1 call), or
//! register them beforehand once per anvil session.
//!
//! Also needs the era-contracts checkout (resolved via `PROTOCOL_CONTRACTS_ROOT`, else the git
//! checkout) to be forge-built: the throwaway `TestnetERC20Token` creation bytecode is read from its
//! `l1-contracts/out/`.
//!
//! Usage (defaults A @ :3050, B @ :3051, L1 @ :8545):
//!   PRIVATE_KEY=0x... \
//!   L2_RPC_URL=http://127.0.0.1:3050 L2_RPC_URL_SECOND=http://127.0.0.1:3051 \
//!   cargo run -p tests --bin atomic-swap-demo
//!
//! Optional env: L1_RPC_URL, ATOMIC_DEADLINE (SL timestamp, default 10_000_000_000),
//!   ATOMIC_BRIDGEHUB (L1 bridgehub — if set, the demo registers the two chains for interop),
//!   ATOMIC_INTEROP_CENTER / ATOMIC_INTEROP_HANDLER / ATOMIC_COMMITMENT_TREE / ATOMIC_FLOW_MANAGER
//!   (layout overrides if a published image differs).

use alloy::primitives::{Address, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::signers::local::PrivateKeySigner;
use anyhow::{Context, Result};

use tests::atomic_swap::{
    assert_atomic_capable, run_atomic_swap, token_unit, AtomicSwapParams, Layout, DEFAULT_DEADLINE,
};

fn require_env(name: &str) -> Result<String> {
    std::env::var(name).with_context(|| format!("missing env var {name}"))
}

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

fn env_addr(name: &str, default: Address) -> Result<Address> {
    match std::env::var(name) {
        Ok(v) => v
            .parse()
            .with_context(|| format!("parse {name} as address")),
        Err(_) => Ok(default),
    }
}

fn env_addr_opt(name: &str) -> Result<Option<Address>> {
    match std::env::var(name) {
        Ok(v) => Ok(Some(
            v.parse()
                .with_context(|| format!("parse {name} as address"))?,
        )),
        Err(_) => Ok(None),
    }
}

fn resolve_layout() -> Result<Layout> {
    let d = Layout::default();
    Ok(Layout {
        interop_center: env_addr("ATOMIC_INTEROP_CENTER", d.interop_center)?,
        interop_handler: env_addr("ATOMIC_INTEROP_HANDLER", d.interop_handler)?,
        commitment_tree: env_addr("ATOMIC_COMMITMENT_TREE", d.commitment_tree)?,
        flow_manager: env_addr("ATOMIC_FLOW_MANAGER", d.flow_manager)?,
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    let private_key = require_env("PRIVATE_KEY")?;
    let rpc_a = env_or("L2_RPC_URL", "http://127.0.0.1:3050");
    let rpc_b = env_or("L2_RPC_URL_SECOND", "http://127.0.0.1:3051");
    let l1_rpc = env_or("L1_RPC_URL", "http://127.0.0.1:8545");
    let deadline: u64 = std::env::var("ATOMIC_DEADLINE")
        .ok()
        .map(|v| v.parse())
        .transpose()
        .context("parse ATOMIC_DEADLINE")?
        .unwrap_or(DEFAULT_DEADLINE);
    let layout = resolve_layout()?;
    // If set, the demo permissionlessly registers the two chains with each other via this L1
    // bridgehub (like the integration test). If unset, the chains must already be registered.
    let bridgehub = env_addr_opt("ATOMIC_BRIDGEHUB")?;
    let signer: PrivateKeySigner = private_key.parse().context("parse PRIVATE_KEY")?;

    println!("=== ATOMIC SWAP DEMO (IMT bundle model) ===");
    println!(
        "layout: interopCenter={} interopHandler={} commitmentTree={} flowManager={}",
        layout.interop_center, layout.interop_handler, layout.commitment_tree, layout.flow_manager
    );

    // ── Capability gate: fail with a precise message rather than obscurely mid-flow ──
    for (name, rpc) in [("A", &rpc_a), ("B", &rpc_b)] {
        let ro = ProviderBuilder::new().connect(rpc).await?.erased();
        assert_atomic_capable(name, &ro, &layout).await?;
    }
    println!("atomic built-ins detected on both chains; proceeding.");

    let unit = token_unit();
    run_atomic_swap(AtomicSwapParams {
        rpc_a,
        rpc_b,
        l1_rpc,
        signer,
        a_amount: unit * U256::from(100u64),
        b_amount: unit * U256::from(100u64),
        supply: unit * U256::from(1_000_000u64),
        deadline,
        // TestnetERC20Token creation bytecode is read from this checkout's forge `out/`; the era
        // checkout must be forge-built (set PROTOCOL_CONTRACTS_ROOT, else the resolved git checkout).
        era_root: protocol_ops::common::paths::contracts_root(),
        // Auto-register via ATOMIC_BRIDGEHUB if provided, else assume the chains are already
        // registered (e.g. registered manually once per anvil session).
        bridgehub,
        layout,
    })
    .await?;

    println!("\nSUCCESS: atomic swap completed end-to-end (both legs executed atomically).");
    Ok(())
}
