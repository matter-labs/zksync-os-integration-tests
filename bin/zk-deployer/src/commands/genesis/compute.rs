use std::collections::BTreeMap;
use std::path::Path;

use alloy::consensus::{Header, EMPTY_OMMER_ROOT_HASH};
use alloy::eips::eip1559::INITIAL_BASE_FEE;
use alloy::primitives::{keccak256, Address, Bloom, B256, B64, U256};
use blake2::{Blake2s256, Digest};
use zk_os_api::helpers::{set_properties_code, set_properties_nonce};
use zk_os_basic_system::system_implementation::flat_storage_model::{
    AccountProperties, ACCOUNT_PROPERTIES_STORAGE_ADDRESS,
};

use super::consts::{
    EIP1967_ADMIN_SLOT, EIP1967_IMPLEMENTATION_SLOT, INITIAL_CONTRACTS, L2_COMPLEX_UPGRADER_ADDR,
    SYSTEM_CONTRACT_PROXY_ADMIN, SYSTEM_PROXY_ADMIN_OWNER_SLOT,
};
use super::types::{
    ContractDeployment, InitialGenesisInput, LeafInfo, MAX_B256_VALUE, MERKLE_TREE_DEPTH,
};

/// Builds the `InitialGenesisInput` from the current l1-contracts artifacts on disk.
pub fn build_initial_genesis_input(l1_contracts_out: &Path) -> anyhow::Result<InitialGenesisInput> {
    let system_proxy_bytecode = load_l1_contract(l1_contracts_out, "SystemContractProxy")?;

    let mut initial_contracts: Vec<(Address, alloy::primitives::Bytes)> = Vec::new();
    let mut proxy_impls: Vec<(Address, Address)> = Vec::new();

    for (addr, deployment) in INITIAL_CONTRACTS.iter() {
        match deployment {
            ContractDeployment::Direct(name) => {
                let code = load_l1_contract(l1_contracts_out, name)?;
                initial_contracts.push((*addr, alloy::primitives::Bytes::from(code)));
            }
            ContractDeployment::SystemProxy(name) => {
                let impl_bytecode = load_l1_contract(l1_contracts_out, name)?;
                let impl_addr = generate_random_address(&impl_bytecode);
                initial_contracts.push((
                    *addr,
                    alloy::primitives::Bytes::from(system_proxy_bytecode.clone()),
                ));
                initial_contracts.push((impl_addr, alloy::primitives::Bytes::from(impl_bytecode)));
                proxy_impls.push((*addr, impl_addr));
            }
            ContractDeployment::Bytecode(bytecode) => {
                initial_contracts.push((*addr, alloy::primitives::Bytes::from(bytecode.to_vec())));
            }
        }
    }

    Ok(InitialGenesisInput {
        initial_contracts,
        additional_storage: construct_additional_storage(&proxy_impls),
        additional_storage_raw: Default::default(),
    })
}

/// Computes the genesis root hash from a `InitialGenesisInput`.
pub fn build_genesis_root_hash(genesis_input: &InitialGenesisInput) -> anyhow::Result<B256> {
    let mut storage_logs: BTreeMap<B256, B256> = BTreeMap::new();

    for (address, deployed_code) in genesis_input.initial_contracts.iter() {
        let mut account_properties = AccountProperties::default();
        set_properties_nonce(&mut account_properties, 1);
        set_properties_code(&mut account_properties, deployed_code);

        let flat_key = account_properties_flat_key(*address);
        storage_logs.insert(
            flat_key,
            account_properties.compute_hash().as_u8_array().into(),
        );
    }

    for (key, value) in genesis_input.additional_storage_raw.iter() {
        if storage_logs.insert(*key, *value).is_some() {
            anyhow::bail!("duplicate key in additional_storage_raw: {key:?}");
        }
    }

    for (address, slots) in genesis_input.additional_storage.iter() {
        for (slot_key, value) in slots {
            let flat_key = flat_storage_key_for_contract(*address, *slot_key);
            if storage_logs.insert(flat_key, *value).is_some() {
                anyhow::bail!("duplicate flattened key from address {address:?} slot {slot_key:?}");
            }
        }
    }

    let header = genesis_header();
    build_initial_genesis_commitment(storage_logs, header)
}

fn load_l1_contract(l1_contracts_out: &Path, name: &str) -> anyhow::Result<Vec<u8>> {
    let path = l1_contracts_out.join(format!("{name}.sol/{name}.json"));
    let content = std::fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("failed to read contract artifact {}: {e}", path.display()))?;
    let artifact: serde_json::Value = serde_json::from_str(&content)
        .map_err(|e| anyhow::anyhow!("failed to parse artifact {}: {e}", path.display()))?;
    let bytecode = artifact["deployedBytecode"]["object"]
        .as_str()
        .filter(|&b| b != "0x")
        .ok_or_else(|| anyhow::anyhow!("no deployed bytecode in artifact for {name}"))?;
    hex::decode(&bytecode[2..])
        .map_err(|e| anyhow::anyhow!("failed to decode bytecode for {name}: {e}"))
}

/// Mirrors Solidity `generateRandomAddress`: derives a deterministic impl address from bytecode.
fn generate_random_address(bytecode: &[u8]) -> Address {
    let blake_hash: [u8; 32] = Blake2s256::digest(bytecode).into();
    let keccak_hash = keccak256(bytecode);

    let mut bytecode_info = [0u8; 96];
    bytecode_info[0..32].copy_from_slice(&blake_hash);
    bytecode_info[60..64].copy_from_slice(&(bytecode.len() as u32).to_be_bytes());
    bytecode_info[64..96].copy_from_slice(keccak_hash.as_slice());

    let mut preimage = [0u8; 128];
    preimage[32..128].copy_from_slice(&bytecode_info);
    let hash = keccak256(preimage);
    Address::from_slice(&hash[12..])
}

fn flat_storage_key_for_contract(address: Address, key: B256) -> B256 {
    let mut bytes = [0u8; 64];
    bytes[12..32].copy_from_slice(address.as_slice());
    bytes[32..64].copy_from_slice(key.as_slice());
    B256::from_slice(&Blake2s256::digest(bytes))
}

fn account_properties_flat_key(address: Address) -> B256 {
    let mut bytes = [0u8; 32];
    bytes[12..32].copy_from_slice(address.as_slice());
    flat_storage_key_for_contract(
        ACCOUNT_PROPERTIES_STORAGE_ADDRESS.to_be_bytes().into(),
        bytes.into(),
    )
}

fn address_to_b256(addr: &Address) -> B256 {
    let mut bytes = [0u8; 32];
    bytes[12..].copy_from_slice(addr.0.as_slice());
    B256::from(bytes)
}

fn construct_additional_storage(
    proxy_impls: &[(Address, Address)],
) -> BTreeMap<Address, BTreeMap<B256, B256>> {
    let mut map: BTreeMap<Address, BTreeMap<B256, B256>> = BTreeMap::new();

    let mut admin_storage = BTreeMap::new();
    admin_storage.insert(
        SYSTEM_PROXY_ADMIN_OWNER_SLOT,
        address_to_b256(&L2_COMPLEX_UPGRADER_ADDR),
    );
    map.insert(SYSTEM_CONTRACT_PROXY_ADMIN, admin_storage);

    for (proxy_addr, impl_addr) in proxy_impls {
        let mut proxy_storage = BTreeMap::new();
        proxy_storage.insert(EIP1967_IMPLEMENTATION_SLOT, address_to_b256(impl_addr));
        proxy_storage.insert(
            EIP1967_ADMIN_SLOT,
            address_to_b256(&SYSTEM_CONTRACT_PROXY_ADMIN),
        );
        map.insert(*proxy_addr, proxy_storage);
    }

    map
}

fn genesis_header() -> Header {
    Header {
        parent_hash: B256::ZERO,
        ommers_hash: EMPTY_OMMER_ROOT_HASH,
        beneficiary: Address::ZERO,
        state_root: B256::ZERO,
        transactions_root: B256::ZERO,
        receipts_root: B256::ZERO,
        logs_bloom: Bloom::ZERO,
        difficulty: U256::ZERO,
        number: 0,
        gas_limit: 5_000,
        gas_used: 0,
        timestamp: 0,
        extra_data: Default::default(),
        mix_hash: B256::ZERO,
        nonce: B64::ZERO,
        base_fee_per_gas: Some(INITIAL_BASE_FEE),
        withdrawals_root: None,
        blob_gas_used: None,
        excess_blob_gas: None,
        parent_beacon_block_root: None,
        requests_hash: None,
        block_access_list_hash: None,
        slot_number: None,
    }
}

fn build_initial_genesis_commitment(
    storage_logs: BTreeMap<B256, B256>,
    genesis_block: Header,
) -> anyhow::Result<B256> {
    let (genesis_root, leaves_count) = build_initial_genesis_root(storage_logs)?;
    let last_256_block_hashes_blake = {
        let mut h = Blake2s256::new();
        for _ in 0..255 {
            h.update([0u8; 32]);
        }
        h.update(genesis_block.hash_slow());
        h.finalize()
    };
    let mut hasher = Blake2s256::new();
    hasher.update(genesis_root.as_slice());
    hasher.update(leaves_count.to_be_bytes());
    hasher.update(0u64.to_be_bytes()); // block number
    hasher.update(last_256_block_hashes_blake);
    hasher.update(0u64.to_be_bytes()); // timestamp
    Ok(B256::from_slice(&hasher.finalize()))
}

fn build_initial_genesis_root(storage_logs: BTreeMap<B256, B256>) -> anyhow::Result<(B256, u64)> {
    let total = storage_logs.len();
    let provided: Vec<LeafInfo> = storage_logs
        .into_iter()
        .enumerate()
        .map(|(i, (k, v))| {
            let next = if i == total - 1 { 1 } else { i as u64 + 3 };
            LeafInfo::new(k, v, next)
        })
        .collect();

    let mut leaves = vec![
        LeafInfo::new(B256::ZERO, B256::ZERO, 2),
        LeafInfo::new(MAX_B256_VALUE, B256::ZERO, 1),
    ];
    leaves.extend(provided);
    let total_leaves = leaves.len() as u64;
    Ok((
        calculate_merkle_root(MERKLE_TREE_DEPTH, &leaves)?,
        total_leaves,
    ))
}

fn calculate_merkle_root(depth: usize, leaves: &[LeafInfo]) -> anyhow::Result<B256> {
    let mut nodes: Vec<B256> = leaves.iter().map(LeafInfo::hash_leaf).collect();
    let mut empty = LeafInfo::new(B256::ZERO, B256::ZERO, 0).hash_leaf();

    for _ in 0..depth {
        nodes = nodes
            .chunks(2)
            .map(|chunk| {
                let lhs = chunk[0];
                let rhs = if chunk.len() > 1 { chunk[1] } else { empty };
                let mut branch = [0u8; 64];
                branch[..32].copy_from_slice(lhs.as_slice());
                branch[32..].copy_from_slice(rhs.as_slice());
                B256::from_slice(&Blake2s256::digest(branch))
            })
            .collect();

        let mut branch = [0u8; 64];
        branch[..32].copy_from_slice(empty.as_slice());
        branch[32..].copy_from_slice(empty.as_slice());
        empty = B256::from_slice(&Blake2s256::digest(branch));
    }

    anyhow::ensure!(
        nodes.len() == 1,
        "merkle reduction did not collapse to single root"
    );
    Ok(nodes[0])
}
