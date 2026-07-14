//! v30→v31 protocol upgrade steps.
//!
//! Each function here is a *real* step of the upgrade runbook (unlike the
//! fixture-mending hacks in [`super::fixture`]). The flow, in order:
//!
//! 1. `ecosystem upgrade-prepare` (deployer) — deploy new ecosystem contracts
//! 2. `ecosystem upgrade-governance` (governor) — governance stages 0+1+2
//! 3. [`run_stage3`] — legacy-token migration into the L1AssetTracker
//! 4. [`schedule_upgrade_timestamp`] — notify ChainAdmin + ServerNotifier; the
//!    server then injects the L2 upgrade tx and its upgrade_gatekeeper holds
//!    v31 batches until the L1 chain upgrade lands
//! 5. [`run_chain_upgrade`] — diamond cut, L1 protocolVersion → v31
//! 6. [`set_zkos_pre_v31_total_supply`] — ZKOS-only base-token supply backfill
//!
//! Steps 1-5 go through protocol-ops commands and [`apply`]. Step 6 is a
//! direct L1 call: TODO(protocol-ops): replace once a
//! `chain set-zkos-pre-v31-total-supply` command exists.

use std::path::Path;

use alloy::primitives::{Address, FixedBytes, U256};
use anyhow::{Context, Result};
use protocol_ops::commands::chain;
use protocol_ops::commands::dev::execute_manifest::apply_manifest;
use protocol_ops::common::forge::ForgeScriptArgs;
use protocol_ops::common::{EcosystemArgs, EcosystemChainArgs, SharedRunArgs};

use protocol_ops::common::abi::{
    BridgehubAbi, IChainAdminAbi, IL1AssetRouterAbi, IL1NativeTokenVaultAbi, ZkChainAbi,
};

use crate::eth::{call, provider, send_as_signer};

// ---------------------------------------------------------------------------
// protocol-ops invocation glue
// ---------------------------------------------------------------------------

pub fn shared_args(l1_rpc: &str, out_dir: &Path) -> SharedRunArgs {
    SharedRunArgs {
        l1_rpc_url: l1_rpc.to_string(),
        out: Some(out_dir.to_path_buf()),
        // The v31 upgrade scripts (CoreUpgrade_v31 etc.) don't have
        // path-taking entrypoints yet, so per-run IO scoping isn't available
        // on the upgrade path.
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
// Upgrade steps
// ---------------------------------------------------------------------------

/// Resolve the chain's base-token asset id plus the NTV and L1AssetTracker
/// behind the bridgehub's asset router.
async fn resolve_token_contracts(
    l1_rpc: &str,
    bridgehub: Address,
    chain_id: u64,
) -> Result<(FixedBytes<32>, Address)> {
    let provider = provider(l1_rpc).await?;
    let asset_id = call(
        &provider,
        bridgehub,
        BridgehubAbi::baseTokenAssetIdCall {
            chainId: U256::from(chain_id),
        },
    )
    .await?;
    let asset_router = call(&provider, bridgehub, BridgehubAbi::assetRouterCall {}).await?;
    let ntv = call(
        &provider,
        asset_router,
        IL1AssetRouterAbi::nativeTokenVaultCall {},
    )
    .await?;
    // NOTE: the L1AssetTracker was removed in era-contracts (#2269). This v30->v31 flow is
    // `#[ignore]`d on the atomic-interop branch (see `tests/upgrade_v30_to_v31.rs`); the tracker
    // lookup is dropped so the module still compiles against the asset-tracking-removed API.
    Ok((asset_id, ntv))
}

/// v31 stage3: register legacy tokens (ETH + the bridged-token list) in the
/// new L1AssetTracker via the `ecosystem stage3` command. Without it, every
/// post-upgrade deposit reverts with AssetIdNotRegistered(ethAssetId).
///
/// stage3 requires ETH to already have an asset id in the NTV (it reverts
/// rather than registering it), so a permissionless `registerEthToken()` is
/// sent first when the NTV doesn't know ETH yet.
pub async fn run_stage3(
    l1_rpc: &str,
    workdir: &Path,
    bridgehub: Address,
    chain_id: u64,
    sender_key: &str,
) -> Result<()> {
    let sender: alloy::signers::local::PrivateKeySigner =
        sender_key.parse().context("parse stage3 sender key")?;

    let (asset_id, ntv) = resolve_token_contracts(l1_rpc, bridgehub, chain_id).await?;
    let provider = provider(l1_rpc).await?;
    let origin = call(
        &provider,
        ntv,
        IL1NativeTokenVaultAbi::originChainIdCall { assetId: asset_id },
    )
    .await?;
    if origin.is_zero() {
        send_as_signer(
            l1_rpc,
            sender_key,
            ntv,
            IL1NativeTokenVaultAbi::registerEthTokenCall {},
        )
        .await
        .context("NTV.registerEthToken")?;
    }

    let out_dir = workdir.join("stage3");
    std::fs::create_dir_all(&out_dir).context("create out dir")?;
    protocol_ops::commands::ecosystem::stage3::run(
        protocol_ops::commands::ecosystem::stage3::Stage3Args {
            shared: shared_args(l1_rpc, &out_dir),
            topology: ecosystem_args(bridgehub),
            sender: Some(sender.address()),
        },
    )
    .await
    .context("ecosystem stage3")?;
    apply(&out_dir, &[sender_key], l1_rpc)
        .await
        .context("apply stage3")?;
    Ok(())
}

/// Schedule the upgrade timestamp via `chain set-upgrade-timestamp`. The
/// command's AdminFunctions script notifies both ChainAdmin and the
/// ServerNotifier; the latter's UpgradeTimestampUpdated event is what the
/// server's L1UpgradeTxWatcher reacts to. The timestamp is set in the past
/// so the watcher fires immediately.
pub async fn schedule_upgrade_timestamp(
    l1_rpc: &str,
    workdir: &Path,
    governor_key: &str,
    bridgehub: Address,
    chain_id: u64,
) -> Result<()> {
    let upgrade_timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .saturating_sub(60);

    let out_dir = workdir.join("schedule_upgrade");
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
    apply(&out_dir, &[governor_key], l1_rpc)
        .await
        .context("apply set-upgrade-timestamp")?;

    Ok(())
}

/// Run the L1 chain upgrade (diamond cut via `chain upgrade` + apply). This
/// bumps the chain diamond's protocolVersion to the CTM's current version,
/// unblocking the server's upgrade_gatekeeper.
pub async fn run_chain_upgrade(
    l1_rpc: &str,
    workdir: &Path,
    governor_key: &str,
    bridgehub: Address,
    chain_id: u64,
) -> Result<()> {
    let out_dir = workdir.join("chain_upgrade");
    std::fs::create_dir_all(&out_dir).context("create out dir")?;

    chain::upgrade::run(chain::upgrade::ChainUpgradeArgs {
        topology: ecosystem_args(bridgehub),
        chain_id: Some(chain_id),
        access_control_restriction: Address::ZERO,
        shared: shared_args(l1_rpc, &out_dir),
    })
    .await
    .context("chain upgrade")?;
    apply(&out_dir, &[governor_key], l1_rpc)
        .await
        .context("apply chain upgrade")?;

    Ok(())
}

/// Post-chain-upgrade step for ZKsync OS chains: backfill the base token's
/// pre-v31 total supply (mirrors SetZkosPreV31TotalSupply.s.sol). The v31 L2
/// upgrade sets needBaseTokenTotalSupplyBackfill=true on upgraded ZKOS chains,
/// and until ChainAdmin sends this service tx every base-token deposit makes
/// L2BaseTokenZKOS.totalSupply() revert, which kills the server's block
/// executor.
///
/// Sent as ChainAdmin.multicall signed by the chain admin's owner — the same
/// path SetZkosPreV31TotalSupply.s.sol uses (Utils.adminExecuteCalls).
///
/// TODO(protocol-ops): replace with a `chain set-zkos-pre-v31-total-supply`
/// command wrapping `SetZkosPreV31TotalSupply.s.sol`.
pub async fn set_zkos_pre_v31_total_supply(
    l1_rpc: &str,
    bridgehub: Address,
    chain_id: u64,
    chain_admin_owner_key: &str,
) -> Result<()> {
    use alloy::sol_types::SolCall as _;

    let diamond = protocol_ops::common::l1_contracts::resolve_zk_chain(l1_rpc, bridgehub, chain_id)
        .await
        .context("resolve diamond")?;
    let chain_admin =
        protocol_ops::common::l1_contracts::resolve_chain_admin(l1_rpc, bridgehub, chain_id)
            .await
            .context("resolve chain admin")?;

    // Pre-v31 total supply per L1 accounting was the balance `registerLegacyToken` migrated into the
    // L1AssetTracker for this chain. The asset-tracker subsystem was removed in era-contracts (#2269);
    // this v30->v31 flow is `#[ignore]`d on the atomic-interop branch, so the value is stubbed to keep
    // the module compiling. Re-derive it from the new accounting when re-enabling this test on main.
    let pre_v31_supply = U256::ZERO;

    // Owner → ChainAdmin.multicall → diamond.setZKsyncOSPreV31TotalSupply
    // → L2 service tx.
    let inner = ZkChainAbi::setZKsyncOSPreV31TotalSupplyCall {
        _totalSupply: pre_v31_supply,
    }
    .abi_encode();
    send_as_signer(
        l1_rpc,
        chain_admin_owner_key,
        chain_admin,
        IChainAdminAbi::multicallCall {
            _calls: vec![IChainAdminAbi::Call {
                target: diamond,
                value: U256::ZERO,
                data: inner.into(),
            }],
            _requireSuccess: true,
        },
    )
    .await
    .context("ChainAdmin.multicall(setZKsyncOSPreV31TotalSupply)")?;
    Ok(())
}

/// Assert the chain diamond's packed protocol version has the expected major.
pub async fn assert_protocol_version(
    l1_rpc: &str,
    bridgehub: Address,
    chain_id: u64,
    expected_major: u64,
) -> Result<()> {
    let diamond = protocol_ops::common::l1_contracts::resolve_zk_chain(l1_rpc, bridgehub, chain_id)
        .await
        .context("resolve diamond")?;
    let provider = provider(l1_rpc).await?;
    let packed = call(&provider, diamond, ZkChainAbi::getProtocolVersionCall {}).await?;
    let major = (packed.wrapping_to::<u64>() >> 32) & 0xFFFF;
    anyhow::ensure!(
        major == expected_major,
        "expected protocol version {expected_major}, got {major}"
    );
    Ok(())
}
