use std::collections::BTreeMap;

use alloy::primitives::{Address, FixedBytes, B256};
use blake2::{Blake2s256, Digest};

/// How a contract is deployed at genesis.
pub enum ContractDeployment {
    /// Deploy contract bytecode read from l1-contracts artifacts by name.
    Direct(&'static str),
    /// Deploy as EIP-1967 SystemContractProxy with the named implementation.
    SystemProxy(&'static str),
    /// Deploy raw bytecode directly.
    Bytecode(&'static [u8]),
}

pub const MERKLE_TREE_DEPTH: usize = 64;

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct InitialGenesisInput {
    pub initial_contracts: Vec<(Address, alloy::primitives::Bytes)>,
    pub additional_storage: BTreeMap<Address, BTreeMap<B256, B256>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub additional_storage_raw: Vec<(B256, B256)>,
}

#[derive(Debug)]
pub struct LeafInfo {
    pub key: B256,
    pub value: B256,
    pub next_index: u64,
}

impl LeafInfo {
    pub fn new(key: B256, value: B256, next_index: u64) -> Self {
        Self {
            key,
            value,
            next_index,
        }
    }

    pub fn hash_leaf(&self) -> B256 {
        let mut hashed_bytes = [0; 2 * 32 + 8];
        hashed_bytes[..32].copy_from_slice(self.key.as_slice());
        hashed_bytes[32..64].copy_from_slice(self.value.as_slice());
        hashed_bytes[64..].copy_from_slice(&self.next_index.to_le_bytes());
        B256::from_slice(&Blake2s256::digest(hashed_bytes))
    }
}

pub const MAX_B256_VALUE: B256 = FixedBytes::<32>([0xFF; 32]);

#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct Genesis {
    #[serde(flatten)]
    pub initial_genesis: InitialGenesisInput,
    pub genesis_root: B256,
    pub protocol_semantic_version: ProtocolVersion,
    #[serde(flatten)]
    pub other: serde_json::Value,
}

#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct ProtocolVersion {
    pub major: u16,
    pub minor: u16,
    pub patch: u16,
}
