//! Adding a chain to the ecosystem the v31 fixture restores, after it has been upgraded.
//!
//! The frozen fixture is a single-chain ecosystem, so anything that needs two interop-capable
//! chains — an atomic swap, say — has to create the second one. That is only possible once the
//! ecosystem has been upgraded: chain creation runs against the CTM's current chain-creation
//! params, which the v33 CTM upgrade installs.
//!
//! Everything here is the deployer's normal path (`adopt::seed_state` + `apply`), not a bespoke
//! registration: the fixture simply has no `state.json`, because it was never bootstrapped in this
//! workdir.

use std::path::PathBuf;

use alloy::primitives::Address;
use anyhow::{Context, Result};
use zk_deployer::adopt::{seed_state, ExistingEcosystem};
use zk_deployer::commands::apply::{self, ApplyArgs};
use zk_deployer::commands::genesis::{self, GenesisCommands, GenesisGenerateArgs};
use zk_deployer::commands::wallets::generate::{self as wallets_generate, WalletsGenerateArgs};
use zk_deployer::deployed::DeployedEcosystem;
use zk_deployer::intent::{ChainIntent, DaMode, IntentConfig, WalletsIntent};

use crate::chain::WALLET_KEYS;
use crate::ecosystem::Ecosystem;
use crate::eth::{call, provider};
use protocol_ops::common::abi::ZkChainAbi;

use super::fixture::DEPLOYER_KEY;

/// Create `chain_id` on the (already upgraded) ecosystem `eco` runs on and bring its server up.
///
/// The new chain reuses the L1 DA validator the fixture's chain is registered with — the upgrade
/// does not deploy a fresh set, and a chain created now has to speak the same DA the ecosystem
/// already validates.
pub async fn create_and_start(eco: &mut Ecosystem, chain_id: u64) -> Result<()> {
    let existing = eco.chain();
    let l1_rpc = existing.l1_rpc_url().to_string();
    let bridgehub = existing.bridgehub_addr();
    let existing_chain_id = existing.chain_id();
    let workdir = eco.workdir().to_path_buf();

    let dir = workdir.join(format!("chain-{chain_id}"));
    std::fs::create_dir_all(&dir).context("create chain workdir")?;

    // ── The ecosystem this chain joins ───────────────────────────────────────
    let ctm = protocol_ops::common::l1_contracts::resolve_ctm_proxy(
        &l1_rpc,
        bridgehub,
        existing_chain_id,
    )
    .await
    .context("resolve CTM")?;
    let diamond =
        protocol_ops::common::l1_contracts::resolve_zk_chain(&l1_rpc, bridgehub, existing_chain_id)
            .await
            .context("resolve diamond")?;
    let l1_provider = provider(&l1_rpc).await?;
    let da_pair = call(&l1_provider, diamond, ZkChainAbi::getDAValidatorPairCall {}).await?;
    let l1_da_validator = da_pair._0;
    let governance = call(&l1_provider, bridgehub, BridgehubOwner::ownerCall {}).await?;

    seed_state(
        &dir.join("state.json"),
        ExistingEcosystem {
            bridgehub,
            ctm,
            governance,
            bytecodes_supplier: None,
            rollup_l1_da_validator: l1_da_validator,
            no_da_l1_validator: Address::ZERO,
            avail_l1_da_validator: Address::ZERO,
            blobs_zksync_os_l1_da_validator: Some(l1_da_validator),
        },
    )
    .context("seed deployer state from the live ecosystem")?;

    // ── The chain itself ─────────────────────────────────────────────────────
    let intent = IntentConfig {
        schema_version: 1,
        l1_rpc_url: Some(l1_rpc.clone()),
        wallets: WalletsIntent {
            ecosystem_seed: Some(format!("upgraded-ecosystem-{chain_id}")),
            path: None,
        },
        chains: vec![ChainIntent {
            chain_id,
            base_token: None,
            da_mode: DaMode::Rollup,
        }],
    };
    std::fs::write(
        dir.join("intent.yaml"),
        serde_yaml::to_string(&intent).context("serialize intent")?,
    )
    .context("write intent.yaml")?;

    wallets_generate::run(WalletsGenerateArgs {
        chains: vec![chain_id],
        ecosystem_seed: format!("upgraded-ecosystem-{chain_id}"),
        output: dir.join("wallets.yaml"),
    })
    .await
    .context("generate wallets")?;

    let genesis_path = dir.join("genesis.json");
    genesis::run(GenesisCommands::Generate(GenesisGenerateArgs {
        genesis_config: protocol_ops::common::paths::path_from_root(
            "configs/genesis/zksync-os/latest.json",
        ),
        l1_contracts_out: protocol_ops::common::paths::resolve_l1_contracts_path()?.join("out"),
        output: genesis_path.clone(),
    }))
    .await
    .context("generate genesis for the new chain")?;

    apply::run(ApplyArgs {
        intent: dir.join("intent.yaml"),
        state: dir.join("state.json"),
        wallets: dir.join("wallets.yaml"),
        out: dir.join("out"),
        private_key: DEPLOYER_KEY.parse().context("deployer key")?,
        broadcast: true,
        l1_state: dir.join("l1-state.json"),
        subdir: Some(format!("upgraded-{chain_id}")),
        no_fund_l2: false,
    })
    .await
    .context("register and initialize the new chain")?;

    // ── Its server ───────────────────────────────────────────────────────────
    let deployed = DeployedEcosystem::load(&dir).context("load the new chain's deployment")?;
    let server_config: PathBuf = dir.join("server.yaml");
    std::fs::write(
        &server_config,
        deployed
            .server_config_yaml(chain_id)
            .context("render server config")?,
    )
    .context("write server.yaml")?;

    eco.add_chain(
        chain_id,
        bridgehub,
        server_config,
        genesis_path,
        WALLET_KEYS
            .iter()
            .map(|k| k.parse().expect("wallet key"))
            .collect(),
    )
    .await
}

alloy::sol! {
    #[sol(rpc)]
    interface BridgehubOwner {
        function owner() external view returns (address);
    }
}
