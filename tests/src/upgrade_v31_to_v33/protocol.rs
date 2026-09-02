//! v31→v33 protocol upgrade steps.
//!
//! Each function here is a *real* step of the upgrade runbook (the fixture is
//! restored as-is; no state mending is needed). The flow, in order:
//!
//! 1. `ecosystem upgrade-prepare-all` (deployer) — deploy the new ecosystem
//!    contracts, among them the `PriorityOpLowerBound` registry and the
//!    per-chain upgrade contract the CTM stores as its default upgrade
//! 2. `ecosystem upgrade-governance` (governor) — governance stages 0+1+2
//! 3. [`record_priority_op_lower_bound`] — pin the chain's priority-op count,
//!    then [`wait_for_priority_ops_processed`]; the upgrade rejects the
//!    diamond cut until every op below the pin has been processed on L2
//! 4. [`schedule_upgrade_timestamp`] — notify ChainAdmin + ServerNotifier; the
//!    server then injects the L2 upgrade tx and its upgrade_gatekeeper holds
//!    v33 batches until the L1 chain upgrade lands
//! 5. [`run_chain_upgrade`] — diamond cut, L1 protocolVersion → v33
//!
//! Steps 1, 2, 4 and 5 go through protocol-ops commands and [`apply`]; step 3
//! goes through `chain record-priority-op-lower-bound`, which broadcasts rather
//! than emitting a bundle (see [`record_priority_op_lower_bound`]).
//!
//! The target version is not spelled anywhere in this test: the upgrade scripts
//! read it out of the contracts' own genesis config
//! (`DefaultCoreUpgrade.loadProtocolVersionFromGenesis`), so the pinned
//! era-contracts revision decides it — v33 on `release/v0.33.0-atomic-interop`.
//! The v31-named scripts and the `V32UpgradeZKsyncOS` contract are the release's
//! current upgrade tooling, not leftovers from an older target.
//!
//! Unlike v30→v31 there is no stage-3 token migration and no base-token supply
//! backfill: a chain created on v31 gets `baseTokenHasTotalSupply` from
//! `DiamondInit`, which is what the upgrade checks.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use alloy::primitives::Address;
use anyhow::{Context, Result};
use protocol_ops::commands::chain;
use protocol_ops::commands::dev::execute_manifest::apply_manifest;
use protocol_ops::common::abi::{IChainTypeManagerAbi, ZkChainAbi};
use protocol_ops::common::forge::ForgeScriptArgs;
use protocol_ops::common::{EcosystemArgs, EcosystemChainArgs, SharedRunArgs};
use serde::Deserialize;

use crate::eth::{call, provider};

alloy::sol! {
    /// Read-only view of the registry, used to poll the bound the upgrade waits on.
    /// Recording it goes through `chain record-priority-op-lower-bound`.
    interface IPriorityOpLowerBound {
        function lowerBound(address chain) external view returns (uint256);
    }
}

// ---------------------------------------------------------------------------
// protocol-ops invocation glue
// ---------------------------------------------------------------------------

pub fn shared_args(l1_rpc: &str, out_dir: &Path) -> SharedRunArgs {
    SharedRunArgs {
        l1_rpc_url: l1_rpc.to_string(),
        out: Some(out_dir.to_path_buf()),
        // The upgrade scripts (CoreUpgrade_v31 etc.) don't have path-taking
        // entrypoints yet, so per-run IO scoping isn't available on the
        // upgrade path.
        subdir: None,
        forge_args: ForgeScriptArgs::default(),
    }
}

pub fn ecosystem_args(bridgehub: Address) -> EcosystemArgs {
    EcosystemArgs {
        bridgehub: Some(bridgehub),
        env: None,
    }
}

pub fn chain_args(bridgehub: Address, chain_id: u64) -> EcosystemChainArgs {
    EcosystemChainArgs {
        ecosystem: ecosystem_args(bridgehub),
        chain_id,
    }
}

/// Apply the manifest a protocol-ops command wrote to `out_dir` with `keys`.
pub async fn apply(out_dir: &Path, keys: &[&str], l1_rpc: &str) -> Result<()> {
    let keys: Vec<String> = keys.iter().map(|k| k.to_string()).collect();
    apply_manifest(&out_dir.join("manifest.json"), &keys, None, l1_rpc, true)
        .await
        .with_context(|| format!("apply manifest from {}", out_dir.display()))
}

// ---------------------------------------------------------------------------
// Priority-op lower bound (the one prerequisite only this upgrade has)
// ---------------------------------------------------------------------------

/// The slice of the CTM-side prepare output this test reads. `DefaultCTMUpgrade`
/// serializes the deployed registry under `[state_transition]`.
#[derive(Deserialize)]
struct CtmUpgradeOutput {
    state_transition: CtmStateTransition,
}

#[derive(Deserialize)]
struct CtmStateTransition {
    priority_op_lower_bound_addr: Address,
}

/// Where `ecosystem upgrade-prepare-all` writes the per-CTM output
/// (`upgrade_inner.rs` builds the same path).
fn ctm_upgrade_output_path(ctm: Address) -> PathBuf {
    protocol_ops::common::paths::contracts_root()
        .join("l1-contracts")
        .join("script-out")
        .join(format!("v33-upgrade-ctm-{ctm:#x}.toml"))
}

/// Read the `PriorityOpLowerBound` registry address out of the CTM prepare
/// output.
pub fn priority_op_lower_bound_registry(ctm: Address) -> Result<Address> {
    let path = ctm_upgrade_output_path(ctm);
    let output: CtmUpgradeOutput = protocol_ops::common::files::read_toml_file(&path)
        .with_context(|| format!("read CTM upgrade output {}", path.display()))?;
    Ok(output.state_transition.priority_op_lower_bound_addr)
}

/// Pin the chain's priority-op count in the registry, via
/// `protocol-ops chain record-priority-op-lower-bound`.
///
/// `V32UpgradeZKsyncOS` — the per-chain upgrade this release stores as the CTM's
/// default upgrade — requires a recorded bound plus every priority op below it
/// processed, which together prove the v31 base-token supply backfill executed on
/// L2 before this release removed its entry point.
///
/// Like every protocol-ops forge command this only *prepares* — `ForgeRunner` runs
/// the script against a throwaway anvil fork — so the emitted bundle is applied here.
/// It stays a bundle of its own rather than joining the governance one: the call is
/// permissionless and must land in a separate, earlier transaction, since the upgrade
/// reads the bound before the chain's facets are replaced. Idempotent — the script
/// no-ops when a bound is already recorded.
pub async fn record_priority_op_lower_bound(
    l1_rpc: &str,
    workdir: &Path,
    bridgehub: Address,
    chain_id: u64,
    ctm: Address,
    sender_key: &str,
) -> Result<()> {
    let registry = priority_op_lower_bound_registry(ctm)?;
    let out_dir = workdir.join(format!("record_priority_op_lower_bound_{chain_id}"));
    std::fs::create_dir_all(&out_dir).context("create out dir")?;

    chain::record_priority_op_lower_bound::run(
        chain::record_priority_op_lower_bound::ChainRecordPriorityOpLowerBoundArgs {
            topology: chain_args(bridgehub, chain_id),
            priority_op_lower_bound: registry,
            sender: sender_key
                .parse::<alloy::signers::local::PrivateKeySigner>()
                .context("parse sender key")?
                .address(),
            shared: shared_args(l1_rpc, &out_dir),
        },
    )
    .await
    .context("chain record-priority-op-lower-bound")?;
    apply(&out_dir, &[sender_key], l1_rpc)
        .await
        .context("apply record-priority-op-lower-bound")?;

    Ok(())
}

/// Wait until the chain has processed every priority op below the recorded
/// bound — the second half of the upgrade's precondition. The counter only
/// advances as the batches holding those ops are executed on L1.
pub async fn wait_for_priority_ops_processed(
    l1_rpc: &str,
    bridgehub: Address,
    chain_id: u64,
    ctm: Address,
    timeout: Duration,
) -> Result<()> {
    let registry = priority_op_lower_bound_registry(ctm)?;
    let diamond = protocol_ops::common::l1_contracts::resolve_zk_chain(l1_rpc, bridgehub, chain_id)
        .await
        .context("resolve diamond")?;
    let provider = provider(l1_rpc).await?;

    let bound = call(
        &provider,
        registry,
        IPriorityOpLowerBound::lowerBoundCall { chain: diamond },
    )
    .await?;

    let deadline = Instant::now() + timeout;
    loop {
        let processed = call(
            &provider,
            diamond,
            ZkChainAbi::getFirstUnprocessedPriorityTxCall {},
        )
        .await?;
        if processed >= bound {
            return Ok(());
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "priority queue did not drain to the recorded bound in time \
             (processed {processed}, bound {bound})"
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

// ---------------------------------------------------------------------------
// Upgrade steps
// ---------------------------------------------------------------------------

/// Schedule the upgrade timestamp via `chain set-upgrade-timestamp`. The
/// command's AdminFunctions script notifies both ChainAdmin and the
/// ServerNotifier; the latter's UpgradeTimestampUpdated event is what the
/// server's L1UpgradeTxWatcher reacts to. The timestamp is set in the past
/// so the watcher fires immediately.
/// `keys` must cover every signer the emitted bundle targets — the ChainAdmin
/// owner, which on some ecosystems differs from the governance owner.
pub async fn schedule_upgrade_timestamp(
    l1_rpc: &str,
    workdir: &Path,
    keys: &[&str],
    bridgehub: Address,
    chain_id: u64,
) -> Result<()> {
    let provider = provider(l1_rpc).await?;

    let upgrade_timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .saturating_sub(60);

    let ctm = protocol_ops::common::l1_contracts::resolve_ctm_proxy(l1_rpc, bridgehub, chain_id)
        .await
        .context("resolve CTM")?;
    // Liveness check that the CTM proxy resolves and governance already moved it
    // to the new version; `set-upgrade-timestamp` derives the target internally.
    let _target_pv = call(&provider, ctm, IChainTypeManagerAbi::protocolVersionCall {}).await?;

    let out_dir = workdir.join(format!("schedule_upgrade_{chain_id}"));
    std::fs::create_dir_all(&out_dir).context("create out dir")?;

    // AdminFunctions.s.sol::adminScheduleUpgrade multicalls BOTH
    // ChainAdmin.setUpgradeTimestamp and ServerNotifier.setUpgradeTimestamp,
    // so this single command is what triggers the server's L1UpgradeTxWatcher.
    chain::set_upgrade_timestamp::run(chain::set_upgrade_timestamp::ChainSetUpgradeTimestampArgs {
        topology: chain_args(bridgehub, chain_id),
        access_control_restriction: Address::ZERO,
        upgrade_timestamp: upgrade_timestamp.to_string(),
        shared: shared_args(l1_rpc, &out_dir),
    })
    .await
    .context("chain set-upgrade-timestamp")?;
    apply(&out_dir, keys, l1_rpc)
        .await
        .context("apply set-upgrade-timestamp")?;

    Ok(())
}

/// Run the L1 chain upgrade (diamond cut via `chain upgrade` + apply). This
/// bumps the chain diamond's protocolVersion to the CTM's current version,
/// unblocking the server's upgrade_gatekeeper.
/// `keys` must cover every signer the emitted bundle targets (see
/// [`schedule_upgrade_timestamp`]).
/// `da_move` is `Some((validator, mode))` for a chain whose DA setup the upgrade has to move —
/// the DA validator pair and the pubdata content then go out in the same `ChainAdmin.multicall`
/// as the diamond cut.
pub async fn run_chain_upgrade(
    l1_rpc: &str,
    workdir: &Path,
    keys: &[&str],
    bridgehub: Address,
    chain_id: u64,
    da_move: Option<(Address, protocol_ops::types::DAValidatorType)>,
) -> Result<()> {
    let out_dir = workdir.join(format!("chain_upgrade_{chain_id}"));
    std::fs::create_dir_all(&out_dir).context("create out dir")?;

    chain::upgrade::run(chain::upgrade::ChainUpgradeArgs {
        topology: ecosystem_args(bridgehub),
        chain_id: Some(chain_id),
        access_control_restriction: Address::ZERO,
        l1_da_validator: da_move.map(|(validator, _)| validator),
        da_mode: da_move.map(|(_, mode)| mode),
        // Both follow from the mode and the chain's VM; the overrides exist for gateway-settling
        // chains, which this fixture has none of.
        l2_da_commitment_scheme: None,
        pubdata_content: None,
        keep_da_setup: false,
        shared: shared_args(l1_rpc, &out_dir),
    })
    .await
    .context("chain upgrade")?;
    apply(&out_dir, keys, l1_rpc)
        .await
        .context("apply chain upgrade")?;

    Ok(())
}

/// Assert the chain diamond's protocol version, as the full semver triple it reports — a chain
/// left on the wrong patch is as wrong as one left on the wrong minor, and the packed value the
/// other getter returns hides the patch.
pub async fn assert_protocol_version(
    l1_rpc: &str,
    bridgehub: Address,
    chain_id: u64,
    expected: (u32, u32, u32),
) -> Result<()> {
    let diamond = protocol_ops::common::l1_contracts::resolve_zk_chain(l1_rpc, bridgehub, chain_id)
        .await
        .context("resolve diamond")?;
    let provider = provider(l1_rpc).await?;
    let semver = call(
        &provider,
        diamond,
        ZkChainAbi::getSemverProtocolVersionCall {},
    )
    .await?;
    let actual = (semver._0, semver._1, semver._2);
    anyhow::ensure!(
        actual == expected,
        "expected protocol version v{}.{}.{} on chain {chain_id}, got v{}.{}.{}",
        expected.0,
        expected.1,
        expected.2,
        actual.0,
        actual.1,
        actual.2
    );
    Ok(())
}
