//! Integration tests — require `anvil` binary in PATH.

use alloy_primitives::{Address, U256};
use lib_anvil::{Anvil, AnvilConfig};

#[tokio::test]
async fn test_anvil_spawns_and_rpc_responds() {
    let anvil = Anvil::spawn(AnvilConfig::default()).await.unwrap();
    let url = anvil.rpc_url().to_string();

    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1,
            "method": "eth_chainId", "params": []
        }))
        .send()
        .await
        .unwrap();
    let json: serde_json::Value = resp.json().await.unwrap();
    assert!(json["result"].is_string());

    anvil.stop().await.unwrap();
}

#[tokio::test]
async fn test_set_balance() {
    let anvil = Anvil::spawn(AnvilConfig::default()).await.unwrap();
    let addr: Address = "0x1111111111111111111111111111111111111111"
        .parse()
        .unwrap();
    let expected = U256::from(99_000_000_000_000_000_000u128); // 99 ETH

    anvil.set_balance(addr, expected).await.unwrap();

    let client = reqwest::Client::new();
    let resp = client
        .post(anvil.rpc_url())
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1,
            "method": "eth_getBalance",
            "params": [format!("{addr:#x}"), "latest"]
        }))
        .send()
        .await
        .unwrap();
    let json: serde_json::Value = resp.json().await.unwrap();
    let hex = json["result"].as_str().unwrap();
    let balance = U256::from_str_radix(hex.trim_start_matches("0x"), 16).unwrap();
    assert_eq!(balance, expected);

    anvil.stop().await.unwrap();
}

#[tokio::test]
async fn test_dump_and_load_state() {
    let dir = tempfile::tempdir().unwrap();
    let state_path = dir.path().join("state.json");

    // Start with dump enabled, set a balance, then stop (triggers dump).
    let config = AnvilConfig {
        dump_state: Some(state_path.clone()),
        ..Default::default()
    };
    let anvil = Anvil::spawn(config).await.unwrap();
    let addr: Address = "0x1111111111111111111111111111111111111111"
        .parse()
        .unwrap();
    anvil
        .set_balance(addr, U256::from(12345678u64))
        .await
        .unwrap();
    let returned = anvil.stop().await.unwrap();
    assert_eq!(returned, Some(state_path.clone()));
    assert!(state_path.exists(), "state file must exist after stop");

    // Reload state and verify balance persisted.
    let config2 = AnvilConfig {
        load_state: Some(state_path),
        ..Default::default()
    };
    let anvil2 = Anvil::spawn(config2).await.unwrap();
    let client = reqwest::Client::new();
    let resp = client
        .post(anvil2.rpc_url())
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1,
            "method": "eth_getBalance",
            "params": [format!("{addr:#x}"), "latest"]
        }))
        .send()
        .await
        .unwrap();
    let json: serde_json::Value = resp.json().await.unwrap();
    let hex = json["result"].as_str().unwrap();
    let balance = U256::from_str_radix(hex.trim_start_matches("0x"), 16).unwrap();
    assert_eq!(balance, U256::from(12345678u64));
    anvil2.stop().await.unwrap();
}
