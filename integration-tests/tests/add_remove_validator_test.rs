//! Exercise the `chain add-validator` / `chain remove-validator` protocol-ops
//! commands against a live L1 anvil (prepare-only — forked anvil,
//! auto-impersonated chain admin). Each command emits a Gnosis Safe
//! Transaction Builder JSON bundle; the test then applies each bundle to
//! the real (non-forked) anvil via `dev execute-safe --private-key`,
//! mirroring the "prepare → Safe owner signs → execute" flow used in
//! production.

use anyhow::{Context, Result};
use integration_tests::anvil::Anvil;
use integration_tests::l1_state::{
    load_ecosystem, load_wallets, resolve_ecosystem_dir, resolve_l1_state,
};
use integration_tests::presets::load_current_preset;
use integration_tests::protocol_ops::EraContractsBackend;

/// A fixed address we add/remove as a validator. Chosen so it doesn't collide
/// with any pre-existing validator or operator seeded into the state.
const TEST_VALIDATOR_ADDRESS: &str = "0x0000000000000000000000000000000000badbad";

/// `keccak256("COMMITTER_ROLE")`. Precomputed so the test doesn't need to shell
/// out to `cast keccak`. The `addValidator` Solidity helper grants every
/// operator role (precommitter/committer/reverter/prover/executor/upgrader),
/// so checking one role is sufficient — if the tx landed, all six are granted.
const COMMITTER_ROLE: &str = "0x0b60b5d7f7e737e4561eecda7c6a01e19e626c495c26e6f45e5b255f76a20106";

/// Read a hex address from a cast call output, stripping ansi/whitespace.
fn address_from_cast(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    let addr = trimmed.split_whitespace().next().unwrap_or("");
    anyhow::ensure!(
        addr.starts_with("0x") && addr.len() == 42,
        "expected 0x-prefixed address, got {trimmed:?}"
    );
    Ok(addr.to_string())
}

/// Read a bool (`true`/`false`) from a cast call.
fn bool_from_cast(
    contracts_backend: &EraContractsBackend,
    l1_rpc_url: &str,
    contract: &str,
    sig: &str,
    fn_args: &[&str],
) -> Result<bool> {
    let mut args: Vec<&str> = vec!["call", contract, sig];
    args.extend_from_slice(fn_args);
    args.extend_from_slice(&["--rpc-url", l1_rpc_url]);
    let raw = contracts_backend
        .cast(&args)
        .with_context(|| format!("cast call {sig} on {contract}"))?;
    let first = raw.split_whitespace().next().unwrap_or("");
    match first {
        "true" => Ok(true),
        "false" => Ok(false),
        other => anyhow::bail!("expected true/false from {sig}, got {other:?}"),
    }
}

fn has_committer_role(
    contracts_backend: &EraContractsBackend,
    l1_rpc_url: &str,
    validator_timelock: &str,
    chain_id: u64,
    validator: &str,
) -> Result<bool> {
    let chain_id_str = chain_id.to_string();
    bool_from_cast(
        contracts_backend,
        l1_rpc_url,
        validator_timelock,
        "hasRoleForChainId(uint256,bytes32,address)(bool)",
        &[&chain_id_str, COMMITTER_ROLE, validator],
    )
}

/// Run `chain add-validator` or `chain remove-validator` in prepare mode
/// (forked anvil, auto-impersonated chain admin) and apply the resulting
/// Safe bundle against the live anvil.
fn prepare_and_execute_validator_change(
    contracts_backend: &EraContractsBackend,
    l1_rpc_url: &str,
    eco_path: &str,
    chain_name: &str,
    chain_owner_pk: &str,
    subcommand: &str, // "add-validator" or "remove-validator"
    out_subdir: &str,
) -> Result<()> {
    println!("\n  Preparing '{subcommand}' Safe bundle via `protocol_ops chain {subcommand}`…");
    let signers: &[&str] = &[chain_owner_pk];
    let safe_rel = format!("{out_subdir}/safe");
    let safe_abs = contracts_backend.work_path(&safe_rel);
    contracts_backend
        .protocol_ops(&[
            "chain",
            subcommand,
            "--l1-rpc-url",
            l1_rpc_url,
            "--ecosystem",
            eco_path,
            "--chain",
            chain_name,
            "--validator-address",
            TEST_VALIDATOR_ADDRESS,
            "--out",
            &safe_abs,
        ])
        .with_context(|| format!("chain {subcommand} prepare failed"))?;
    contracts_backend
        .parse_safe_bundles(&safe_rel, l1_rpc_url)?
        .apply(signers)
        .with_context(|| format!("apply chain {subcommand} Safe bundle failed"))?;
    Ok(())
}

async fn run_add_remove_validator_test() -> Result<()> {
    integration_tests::server::get_or_create_run_id("add_remove_validator");
    let preset = load_current_preset()?;
    let eco = load_ecosystem(&preset)?;

    println!("\n=== Loading l1-state.json into Anvil ===");
    let state_path = resolve_l1_state(&preset)?;
    let anvil = Anvil::spawn_with_state(&state_path).await?;
    let l1_rpc_url = anvil.rpc_url().to_string();
    println!("Anvil ready at {l1_rpc_url}");

    let (chain_name, chain_id) = eco.l1_settling();
    println!("Using L1-settling chain {chain_id} ({chain_name})");

    let wallets = load_wallets(&preset).context("load wallets.yaml")?;
    let chain_wallets = wallets
        .chains
        .get(chain_name)
        .ok_or_else(|| anyhow::anyhow!("wallets.yaml missing entry for chain '{}'", chain_name))?;
    let chain_owner_pk = chain_wallets.owner.private_key.clone();

    let contracts_backend = EraContractsBackend::from_preset(&preset, "add_remove_validator", &[])?;

    // Resolve diamond proxy from L1 bridgehub.
    let diamond_proxy = contracts_backend
        .cast(&[
            "call",
            &eco.bridgehub,
            "getZKChain(uint256)(address)",
            &chain_id.to_string(),
            "--rpc-url",
            &l1_rpc_url,
        ])
        .context("bridgehub.getZKChain()")?
        .trim()
        .to_string();
    println!("  diamond_proxy = {diamond_proxy}");

    // Resolve addresses from on-chain state.
    println!("\n=== Resolving ChainAdmin / CTM / ValidatorTimelock ===");
    let chain_admin = address_from_cast(
        &contracts_backend
            .cast(&[
                "call",
                &diamond_proxy,
                "getAdmin()(address)",
                "--rpc-url",
                &l1_rpc_url,
            ])
            .context("getAdmin()")?,
    )?;
    let ctm = address_from_cast(
        &contracts_backend
            .cast(&[
                "call",
                &diamond_proxy,
                "getChainTypeManager()(address)",
                "--rpc-url",
                &l1_rpc_url,
            ])
            .context("getChainTypeManager()")?,
    )?;
    let validator_timelock = address_from_cast(
        &contracts_backend
            .cast(&[
                "call",
                &ctm,
                "validatorTimelockPostV29()(address)",
                "--rpc-url",
                &l1_rpc_url,
            ])
            .context("validatorTimelockPostV29()")?,
    )?;
    println!("  chain_admin        = {chain_admin}");
    println!("  ctm                = {ctm}");
    println!("  validator_timelock = {validator_timelock}");

    // Sanity check: the test validator must not already have the role —
    // otherwise the "add" step would be a no-op and we'd be asserting on
    // stale state.
    let before = has_committer_role(
        &contracts_backend,
        &l1_rpc_url,
        &validator_timelock,
        chain_id,
        TEST_VALIDATOR_ADDRESS,
    )?;
    anyhow::ensure!(
        !before,
        "test validator {TEST_VALIDATOR_ADDRESS} already has COMMITTER_ROLE before test"
    );

    // Each phase writes its Safe bundle to a dedicated sub-dir under
    // `test-run-logs/add_remove_validator/<subdir>/safe`. A per-invocation
    // UUID keeps concurrent test runs isolated and prevents stale bundles
    // from a prior run leaking into the current `manifest.json`.
    let eco_dir = resolve_ecosystem_dir(&preset)?;
    let eco_path = eco_dir.join("ecosystem.yaml").to_string_lossy().to_string();
    let run_tag = uuid::Uuid::new_v4();
    let add_subdir = format!("add_remove_validator_{run_tag}/validator_add");
    let remove_subdir = format!("add_remove_validator_{run_tag}/validator_remove");

    println!("\n=== chain add-validator (prepare + execute) ===");
    prepare_and_execute_validator_change(
        &contracts_backend,
        &l1_rpc_url,
        &eco_path,
        chain_name,
        &chain_owner_pk,
        "add-validator",
        &add_subdir,
    )
    .context("chain add-validator prepare+execute failed")?;

    let after_add = has_committer_role(
        &contracts_backend,
        &l1_rpc_url,
        &validator_timelock,
        chain_id,
        TEST_VALIDATOR_ADDRESS,
    )?;
    anyhow::ensure!(
        after_add,
        "validator {TEST_VALIDATOR_ADDRESS} did not receive COMMITTER_ROLE after add-validator"
    );
    println!("✓ Validator has COMMITTER_ROLE after add-validator");

    println!("\n=== chain remove-validator (prepare + execute) ===");
    prepare_and_execute_validator_change(
        &contracts_backend,
        &l1_rpc_url,
        &eco_path,
        chain_name,
        &chain_owner_pk,
        "remove-validator",
        &remove_subdir,
    )
    .context("chain remove-validator prepare+execute failed")?;

    let after_remove = has_committer_role(
        &contracts_backend,
        &l1_rpc_url,
        &validator_timelock,
        chain_id,
        TEST_VALIDATOR_ADDRESS,
    )?;
    anyhow::ensure!(
        !after_remove,
        "validator {TEST_VALIDATOR_ADDRESS} still has COMMITTER_ROLE after remove-validator"
    );
    println!("✓ Validator lost COMMITTER_ROLE after remove-validator");

    let _ = anvil.kill();
    println!("\nTest passed!");
    Ok(())
}

#[tokio::test]
async fn test_add_remove_validator() {
    run_add_remove_validator_test()
        .await
        .expect("add_remove_validator_test failed");
}
