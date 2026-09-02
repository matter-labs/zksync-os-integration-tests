//! The v31 -> v33 upgrade, as one call.
//!
//! Both the upgrade test and the post-upgrade atomic-swap test need the same runbook; it lives
//! here so neither owns it. The individual steps are in [`super::protocol`].

use std::time::Duration;

use anyhow::{Context, Result};
use protocol_ops::commands::ecosystem::upgrade::{
    run_upgrade_governance, run_upgrade_prepare_all, UpgradeGovernanceArgs, UpgradePrepareAllArgs,
};
use protocol_ops::common::forge::scripts::{
    CORE_UPGRADE_V33_SCRIPT_PATH, CTM_UPGRADE_V33_SCRIPT_PATH, UPGRADE_V33_CORE_OUTPUT_PATH,
};

use super::da_switch;
use super::fixture::{self, CHAIN_SIGNING_KEYS, DEPLOYER_ADDR, DEPLOYER_KEY, GOVERNOR_KEY};
use super::protocol;
use crate::ecosystem::Ecosystem;

/// The upgrade waits on the chain executing its pre-upgrade priority ops; on a cold fixture that
/// is a full commit/prove/execute round per batch.
const PRIORITY_QUEUE_DRAIN_TIMEOUT: Duration = Duration::from_secs(300);

/// Drive the whole v31 -> v33 upgrade on every chain of `eco`: the ecosystem half once (prepare +
/// governance stages 0/1/2), then per chain the priority-op bound this release requires, the
/// scheduled upgrade timestamp the server reacts to, and the diamond cut.
///
/// Returns, per chain id, the L2 block its upgrade transaction was sealed in, so callers can wait
/// for those to finalize.
pub async fn run_upgrade(eco: &mut Ecosystem) -> Result<Vec<(u64, u64)>> {
    // Owned, not borrowed from a `Chain`: the per-chain loop below restarts servers, which needs
    // the ecosystem mutably.
    let (l1_rpc, bridgehub, first_chain_id) = {
        let first = eco.chain();
        (
            first.l1_rpc_url().to_string(),
            first.bridgehub_addr(),
            first.chain_id(),
        )
    };
    let l1_rpc = l1_rpc.as_str();

    // Every chain of the fixture shares one CTM — the upgrade is an ecosystem-level operation and
    // the per-chain cuts all come from it.
    let ctm =
        protocol_ops::common::l1_contracts::resolve_ctm_proxy(l1_rpc, bridgehub, first_chain_id)
            .await
            .context("resolve CTM")?;

    // ── ecosystem upgrade-prepare (deployer) ─────────────────────────────────
    //
    // Driven through the v33 scripts (`deploy-scripts/upgrade/v33/`), which extend the
    // Default* bases directly: the v31 flow's stage-2 legacy-Gateway decommission and
    // stage-3 token migration are one-time v30->v31 work and must not be replayed here.
    // The v31 CTM exposes the getters protocol-ops resolves the bytecodes supplier and
    // rollup DA manager from, so no pre-v31 override addresses are needed.
    let prepare_dir = eco.workdir().join("upgrade_prepare");
    let governance_toml = eco.workdir().join("ecosystem.toml");
    run_upgrade_prepare_all(UpgradePrepareAllArgs {
        shared: protocol::shared_args(l1_rpc, &prepare_dir),
        topology: protocol::ecosystem_args(bridgehub),
        deployer_address: Some(DEPLOYER_ADDR.parse().unwrap()),
        ctm_proxies: vec![ctm],
        ctm_config: None,
        bytecodes_supplier_address: None,
        rollup_da_manager_address: None,
        is_zk_sync_os: Some(true),
        create2_factory_salt: None,
        upgrade_input_path: fixture::UPGRADE_INPUT_PATH.to_string(),
        core_output_path: UPGRADE_V33_CORE_OUTPUT_PATH.to_string(),
        core_script_path: CORE_UPGRADE_V33_SCRIPT_PATH.to_string(),
        ctm_script_path: CTM_UPGRADE_V33_SCRIPT_PATH.to_string(),
    })
    .await
    .context("upgrade-prepare")?;
    protocol::apply(&prepare_dir, &[DEPLOYER_KEY, GOVERNOR_KEY], l1_rpc)
        .await
        .context("apply upgrade-prepare")?;

    // ── ecosystem upgrade-governance (stages 0+1+2, governor) ────────────────
    let gov_dir = eco.workdir().join("upgrade_governance");
    run_upgrade_governance(UpgradeGovernanceArgs {
        shared: protocol::shared_args(l1_rpc, &gov_dir),
        topology: protocol::ecosystem_args(bridgehub),
        governance_toml: vec![governance_toml],
    })
    .await
    .context("upgrade-governance")?;
    protocol::apply(&gov_dir, &[GOVERNOR_KEY], l1_rpc)
        .await
        .context("apply upgrade-governance")?;

    // ── Per chain: DA prep, bound, drain, schedule, cut ──────────────────────
    //
    // A validium-priced chain has to leave no-DA behind as part of this upgrade: its first v33
    // batch does not settle while it publishes nothing (`proveBatches` reverts `InvalidProof`),
    // and until it publishes its log region its interop (IMT) leaves are unreachable from L1.
    let validium_chains = validium_priced_chains(l1_rpc, bridgehub, eco).await?;
    let blobs_validator = if validium_chains.is_empty() {
        None
    } else {
        Some(da_switch::blobs_da_validator(l1_rpc, bridgehub, eco).await?)
    };

    let chain_ids: Vec<u64> = eco.chains().map(|c| c.chain_id()).collect();
    let mut upgrade_blocks = Vec::new();
    for chain_id in chain_ids {
        let da_move = if validium_chains.contains(&chain_id) {
            // Before the cut, not after: from v33 the server posts through blobs, and it has to
            // already be doing so when the chain gets there. Pre-v33 batches keep committing
            // no-DA regardless of this setting.
            da_switch::prepare_server_for_blobs(eco, chain_id).await?;
            Some((
                blobs_validator.expect("resolved for validium chains"),
                protocol_ops::types::DAValidatorType::LogsOnlyValidium,
            ))
        } else {
            None
        };
        let chain = eco
            .chains()
            .find(|c| c.chain_id() == chain_id)
            .expect("chain of this ecosystem");

        // The bound is recorded before the upgrade is scheduled so it lands in its own
        // transaction, as `V32UpgradeZKsyncOS` requires.
        protocol::record_priority_op_lower_bound(
            l1_rpc,
            eco.workdir(),
            bridgehub,
            chain_id,
            ctm,
            DEPLOYER_KEY,
        )
        .await
        .with_context(|| format!("record priority-op lower bound for chain {chain_id}"))?;
        protocol::wait_for_priority_ops_processed(
            l1_rpc,
            bridgehub,
            chain_id,
            ctm,
            PRIORITY_QUEUE_DRAIN_TIMEOUT,
        )
        .await
        .with_context(|| format!("drain priority ops below the bound on chain {chain_id}"))?;

        // Capture the latest L2 block BEFORE scheduling: the server's upgrade-tx watcher polls
        // every 100 ms, so the upgrade block is often sealed before schedule_upgrade_timestamp()
        // even returns. Capturing afterwards would make us wait for a block no traffic produces.
        let pre_upgrade_latest = chain.latest_block().await.unwrap_or(0);
        protocol::schedule_upgrade_timestamp(
            l1_rpc,
            eco.workdir(),
            CHAIN_SIGNING_KEYS,
            bridgehub,
            chain_id,
        )
        .await
        .with_context(|| format!("schedule the upgrade timestamp for chain {chain_id}"))?;

        // The upgrade_gatekeeper blocks the v33 batch until the diamond cut below; meanwhile the
        // server seals the L2 upgrade block.
        let upgrade_block = pre_upgrade_latest + 1;
        chain
            .wait_for_block_produced(upgrade_block)
            .await
            .with_context(|| format!("wait for chain {chain_id}'s upgrade block"))?;

        // Before bumping L1's protocolVersion, every pre-upgrade (v31) batch must be finalized —
        // otherwise the upgrade_gatekeeper sees contract(v33) > batch(v31) and hard-fails.
        chain
            .wait_for_block_finalized(pre_upgrade_latest)
            .await
            .with_context(|| format!("finalize chain {chain_id}'s pre-upgrade batches"))?;

        // The cut, the DA validator pair and the pubdata content go out as one
        // `ChainAdmin.multicall`: `setPubdataContent` exists only on the facets the cut installs,
        // and a chain left between the two would commit v33 batches under its old DA setup.
        protocol::run_chain_upgrade(
            l1_rpc,
            eco.workdir(),
            CHAIN_SIGNING_KEYS,
            bridgehub,
            chain_id,
            da_move,
        )
        .await
        .with_context(|| format!("upgrade chain {chain_id}"))?;

        upgrade_blocks.push((chain_id, upgrade_block));
    }

    Ok(upgrade_blocks)
}

/// The chains of `eco` whose on-chain pricing mode is `Validium` — the ones that were registered
/// with the no-DA validator and have to move off it.
async fn validium_priced_chains(
    l1_rpc: &str,
    bridgehub: alloy::primitives::Address,
    eco: &Ecosystem,
) -> Result<Vec<u64>> {
    const PRICING_VALIDIUM: u8 = 1;

    let provider = crate::eth::provider(l1_rpc).await?;
    let mut ids = Vec::new();
    for chain in eco.chains() {
        let diamond = protocol_ops::common::l1_contracts::resolve_zk_chain(
            l1_rpc,
            bridgehub,
            chain.chain_id(),
        )
        .await
        .context("resolve diamond")?;
        let pricing = crate::eth::call(
            &provider,
            diamond,
            protocol_ops::common::abi::ZkChainAbi::getPubdataPricingModeCall {},
        )
        .await?;
        if pricing == PRICING_VALIDIUM {
            ids.push(chain.chain_id());
        }
    }
    Ok(ids)
}
