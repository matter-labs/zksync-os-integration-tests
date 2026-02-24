use integration_tests::anvil::Anvil;
use integration_tests::presets::load_default_presets;

/// Default Anvil account #0 private key (Foundry default).
const DEFAULT_ANVIL_PRIVATE_KEY: &str =
    "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

#[tokio::test]
#[ignore]
async fn test_protocol_ops_ecosystem_init_on_fresh_l1() {
    let presets = load_default_presets().expect("Failed to load presets");
    let mut names: Vec<String> = presets.keys().cloned().collect();
    names.sort();
    let name = names.first().expect("No presets found").clone();
    let preset = presets.get(&name).expect("Preset disappeared");

    let anvil = Anvil::spawn_fresh()
        .await
        .expect("Failed to spawn fresh Anvil");
    let l1_rpc_url = anvil.rpc_url_for(&preset.era_contracts);

    let args = [
        "ecosystem",
        "init",
        "--l1-rpc-url",
        l1_rpc_url.as_str(),
        "--private-key",
        DEFAULT_ANVIL_PRIVATE_KEY,
    ];
    integration_tests::protocol_ops::run_protocol_ops_for_preset(preset, &args)
        .expect("Failed to run protocol_ops ecosystem init");
}
