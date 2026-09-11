use std::path::{Path, PathBuf};
use std::time::Duration;

use alloy::network::{EthereumWallet, TransactionBuilder};
use alloy::primitives::{address, Address, Bytes, B256, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol_types::{SolCall, SolValue};
use anyhow::{Context, Result};
use protocol_ops::common::abi::{
    AdminFunctionsAbi, BridgehubAbi, IChainAssetHandlerAbi, IChainTypeManagerAbi, ZkChainAbi,
};
use protocol_ops::common::governance_calls::{encode_calls, GovernanceCall};

use tests::eth::{call, provider};
use tests::upgrade_v31_to_v33::fixture::DEPLOYER_KEY;
use tests::upgrade_v31_to_v33::protocol;

const OLD_VERSION: u64 = 33 << 32;
const NEW_VERSION: u64 = OLD_VERSION | 1;
const OLD_VK: &str = "0x29651d5f044e1671ff820f85018ed87b26f57402222eb31dd453206e2379bc9c";
const NEW_VK: &str = "0xf27b70fb2ac27736c92f8445fffe8ff9554ad5ea5fd9297a6f1fa2759848915f";

alloy::sol! {
    interface IVerifier {
        function verificationKeyHash() external view returns (bytes32);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_v33_0_to_v33_1_verifier_only_upgrade() -> Result<()> {
    tests::fixtures::ensure_contracts_built().await;
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("local-chains/v33.0");
    let eco = tests::fixtures::restore::restore(&fixture).await?;
    let chain = eco.chain();
    chain.wait_for_block_finalized(1).await?;
    let l1_rpc = chain.l1_rpc_url();
    let bridgehub = chain.bridgehub_addr();
    let chain_id = chain.chain_id();
    let l1 = provider(l1_rpc).await?;
    let ctm =
        protocol_ops::common::l1_contracts::resolve_ctm_proxy(l1_rpc, bridgehub, chain_id).await?;
    let diamond =
        protocol_ops::common::l1_contracts::resolve_zk_chain(l1_rpc, bridgehub, chain_id).await?;
    protocol::assert_protocol_version(l1_rpc, bridgehub, chain_id, (0, 33, 0)).await?;
    let old_verifier = call(&l1, diamond, ZkChainAbi::getVerifierCall {}).await?;
    assert_eq!(
        call(&l1, old_verifier, IVerifier::verificationKeyHashCall {}).await?,
        OLD_VK.parse::<B256>()?
    );
    let before = chain.ping().await?;
    chain.wait_for_tx_finalized(before).await?;

    let plonk = deploy(
        l1_rpc,
        "ZKsyncOSVerifierPlonk.sol/ZKsyncOSVerifierPlonk.json",
        &[],
    )
    .await?;
    let verifier = deploy(
        l1_rpc,
        "ZKsyncOSTestnetVerifier.sol/ZKsyncOSTestnetVerifier.json",
        &plonk.abi_encode(),
    )
    .await?;
    assert_ne!(verifier, old_verifier);
    assert_eq!(
        call(&l1, verifier, IVerifier::verificationKeyHashCall {}).await?,
        NEW_VK.parse::<B256>()?
    );

    let asset_handler = call(&l1, bridgehub, BridgehubAbi::chainAssetHandlerCall {}).await?;
    let was_paused = call(
        &l1,
        asset_handler,
        IChainAssetHandlerAbi::migrationPausedCall {},
    )
    .await?;
    let mut calls = Vec::new();
    if !was_paused {
        calls.push(governance_call(
            asset_handler,
            IChainAssetHandlerAbi::pauseMigrationCall {},
        ));
    }
    calls.push(governance_call(
        ctm,
        IChainTypeManagerAbi::createNewVerifierOnlyUpgradeCall {
            _oldProtocolVersion: U256::from(OLD_VERSION),
            _oldProtocolVersionDeadline: U256::MAX,
            _newProtocolVersion: U256::from(NEW_VERSION),
            _verifier: verifier,
        },
    ));
    execute_governance(
        l1_rpc,
        bridgehub,
        eco.workdir(),
        "register-verifier",
        &calls,
    )
    .await?;
    assert_eq!(
        call(&l1, ctm, IChainTypeManagerAbi::protocolVersionCall {}).await?,
        U256::from(NEW_VERSION)
    );
    assert_eq!(
        call(
            &l1,
            ctm,
            IChainTypeManagerAbi::protocolVersionVerifierCall {
                _protocolVersion: U256::from(OLD_VERSION)
            }
        )
        .await?,
        old_verifier
    );
    assert_eq!(
        call(
            &l1,
            ctm,
            IChainTypeManagerAbi::protocolVersionVerifierCall {
                _protocolVersion: U256::from(NEW_VERSION)
            }
        )
        .await?,
        verifier
    );
    // Registering the new CTM version must leave the chain on its existing verifier until its cut.
    protocol::assert_protocol_version(l1_rpc, bridgehub, chain_id, (0, 33, 0)).await?;
    assert_eq!(
        call(&l1, diamond, ZkChainAbi::getVerifierCall {}).await?,
        old_verifier
    );

    let wallets = protocol_ops::common::wallets::load_wallets(&fixture.join("wallets.yaml"))?;
    let admin_key = wallets.chains[&chain_id.to_string()]
        .owner
        .private_key_b256()
        .context("fixture chain admin key")?;
    let admin_key = format!("{admin_key:#x}");
    let keys = &[DEPLOYER_KEY, admin_key.as_str()];
    let last_old_block = chain.latest_block().await?;
    chain.wait_for_block_finalized(last_old_block).await?;
    protocol::schedule_upgrade_timestamp(l1_rpc, eco.workdir(), keys, bridgehub, chain_id, None)
        .await?;
    // A patch has no L2 upgrade transaction to wait on. With traffic quiesced and all old batches
    // executed, applying the cut is safe; the next deposit will exercise the watcher transition.
    protocol::run_chain_upgrade(l1_rpc, eco.workdir(), keys, bridgehub, chain_id, None).await?;
    protocol::assert_protocol_version(l1_rpc, bridgehub, chain_id, (0, 33, 1)).await?;
    assert_eq!(
        call(&l1, diamond, ZkChainAbi::getVerifierCall {}).await?,
        verifier
    );
    assert_eq!(
        call(
            &l1,
            diamond,
            ZkChainAbi::getL2SystemContractsUpgradeTxHashCall {}
        )
        .await?,
        B256::ZERO
    );
    if !was_paused {
        execute_governance(
            l1_rpc,
            bridgehub,
            eco.workdir(),
            "resume-migrations",
            &[governance_call(
                asset_handler,
                IChainAssetHandlerAbi::unpauseMigrationCall {},
            )],
        )
        .await?;
    }

    let recipient = address!("deadbeef00000000000000000000000000330001");
    let amount = U256::from(1_000_000_000_000_000_000u128);
    assert_eq!(chain.balance(recipient).await?, U256::ZERO);
    zk_deployer::l1_l2_deposit::deposit_base_token(
        l1_rpc,
        bridgehub,
        chain_id,
        recipient,
        amount,
        1_000_000_000,
        DEPLOYER_KEY,
        None,
    )
    .await?;
    zk_deployer::l1_l2_deposit::wait_for_l2_balance(chain.l2_rpc_url(), recipient, 120).await?;
    assert!(chain.balance(recipient).await? >= amount);
    let deposit_block = chain.latest_block().await?;
    assert!(deposit_block > last_old_block);
    chain.wait_for_block_finalized(deposit_block).await?;
    let after = chain.ping().await?;
    chain.wait_for_tx_finalized(after).await?;
    Ok(())
}

fn governance_call<C: SolCall>(target: Address, call: C) -> GovernanceCall {
    GovernanceCall {
        target,
        value: U256::ZERO,
        data: call.abi_encode(),
    }
}

async fn execute_governance(
    l1_rpc: &str,
    bridgehub: Address,
    workdir: &Path,
    name: &str,
    calls: &[GovernanceCall],
) -> Result<()> {
    let out = workdir.join(name);
    let shared = protocol::shared_args(l1_rpc, &out);
    let mut runner = protocol_ops::common::forge::ForgeRunner::new(&shared)?;
    let owner = runner.prepare_governance_owner(bridgehub).await?;
    let governance =
        protocol_ops::common::l1_contracts::resolve_governance(l1_rpc, bridgehub).await?;
    let calldata = AdminFunctionsAbi::governanceExecuteCallsCall {
        _callsToExecute: encode_calls(calls).into(),
        _governanceAddr: governance,
    }
    .abi_encode();
    let script = runner
        .script_with_calldata(
            &protocol_ops::common::forge::scripts::ADMIN_FUNCTIONS_INVOCATION,
            calldata,
        )
        .with_wallet(&owner);
    runner.run(script)?;
    protocol_ops::common::output::write_output_if_requested(
        name,
        &shared,
        &runner,
        &serde_json::json!({}),
        &serde_json::json!({}),
    )
    .await?;
    protocol::apply(&out, &[DEPLOYER_KEY], l1_rpc).await
}

async fn deploy(l1_rpc: &str, artifact: &str, constructor: &[u8]) -> Result<Address> {
    let path = protocol_ops::common::paths::contracts_root()
        .join("l1-contracts/out")
        .join(artifact);
    let artifact: serde_json::Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    let mut bytecode = hex::decode(
        artifact["bytecode"]["object"]
            .as_str()
            .context("artifact bytecode")?
            .trim_start_matches("0x"),
    )?;
    bytecode.extend_from_slice(constructor);
    let signer: PrivateKeySigner = DEPLOYER_KEY.parse()?;
    let provider = ProviderBuilder::new()
        .wallet(EthereumWallet::from(signer))
        .connect(l1_rpc)
        .await?;
    provider
        .client()
        .set_poll_interval(Duration::from_millis(100));
    let receipt = provider
        .send_transaction(TransactionRequest::default().with_deploy_code(Bytes::from(bytecode)))
        .await?
        .get_receipt()
        .await?;
    anyhow::ensure!(receipt.status(), "verifier deployment reverted");
    receipt
        .contract_address
        .context("deployed verifier address")
}
