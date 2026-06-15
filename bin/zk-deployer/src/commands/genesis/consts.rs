use alloy::primitives::{Address, B256};
use hex_literal::hex;

use super::types::ContractDeployment;

macro_rules! addr {
    ($hex:literal) => {
        Address::new(hex!($hex))
    };
}

macro_rules! b256 {
    ($hex:literal) => {
        B256::new(hex!($hex))
    };
}

pub const L2_COMPLEX_UPGRADER_ADDR: Address = addr!("000000000000000000000000000000000000800f");
pub const L2_GENESIS_UPGRADE: Address = addr!("0000000000000000000000000000000000010001");
pub const L2_WRAPPED_BASE_TOKEN: Address = addr!("0000000000000000000000000000000000010007");
pub const SYSTEM_CONTRACT_PROXY_ADMIN: Address = addr!("000000000000000000000000000000000001000c");
pub const L2_MESSAGE_ROOT_ADDR: Address = addr!("0000000000000000000000000000000000010005");
pub const L2_BRIDGEHUB_ADDR: Address = addr!("0000000000000000000000000000000000010002");
pub const L2_ASSET_ROUTER_ADDR: Address = addr!("0000000000000000000000000000000000010003");
pub const L2_NATIVE_TOKEN_VAULT_ADDR: Address = addr!("0000000000000000000000000000000000010004");
pub const L2_NTV_BEACON_DEPLOYER_ADDR: Address = addr!("000000000000000000000000000000000001000b");
pub const L2_CHAIN_ASSET_HANDLER_ADDR: Address = addr!("000000000000000000000000000000000001000a");
pub const L2_INTEROP_CENTER_ADDR: Address = addr!("000000000000000000000000000000000001000d");
pub const L2_INTEROP_HANDLER_ADDR: Address = addr!("000000000000000000000000000000000001000e");
pub const L2_ASSET_TRACKER_ADDR: Address = addr!("000000000000000000000000000000000001000f");
pub const GW_ASSET_TRACKER_ADDR: Address = addr!("0000000000000000000000000000000000010010");
pub const L2_BASE_TOKEN_HOLDER_ADDR: Address = addr!("0000000000000000000000000000000000010011");

pub const DETERMINISTIC_CREATE2_ADDRESS: Address =
    addr!("4e59b44847b379578588920cA78FbF26c0B4956C");
pub const CREATE2_FACTORY_RUNTIME_BYTECODE: &[u8] = &hex!(
    "7fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffe03601600081602082378035828234f58015156039578182fd5b8082525050506014600cf3"
);

pub const L2_DEPLOYER_SYSTEM_CONTRACT_ADDR: Address =
    addr!("0000000000000000000000000000000000008006");
pub const L2_TO_L1_MESSENGER_SYSTEM_CONTRACT_ADDR: Address =
    addr!("0000000000000000000000000000000000008008");
pub const L2_BASE_TOKEN_SYSTEM_CONTRACT_ADDR: Address =
    addr!("000000000000000000000000000000000000800a");
pub const L2_SYSTEM_CONTEXT_ADDR: Address = addr!("000000000000000000000000000000000000800b");

const L2_INTEROP_ROOT_STORAGE: Address = addr!("0000000000000000000000000000000000010008");
const L2_MESSAGE_VERIFICATION: Address = addr!("0000000000000000000000000000000000010009");

pub const SYSTEM_PROXY_ADMIN_OWNER_SLOT: B256 = B256::ZERO;
pub const EIP1967_IMPLEMENTATION_SLOT: B256 =
    b256!("360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc");
pub const EIP1967_ADMIN_SLOT: B256 =
    b256!("b53127684a568b3173ae13b9f8a6016e243e63b6e8ee1178d6a717850b5d6103");

pub const INITIAL_CONTRACTS: [(Address, ContractDeployment); 22] = [
    (
        L2_COMPLEX_UPGRADER_ADDR,
        ContractDeployment::SystemProxy("L2ComplexUpgrader"),
    ),
    (
        L2_GENESIS_UPGRADE,
        ContractDeployment::Direct("L2GenesisUpgrade"),
    ),
    (
        L2_WRAPPED_BASE_TOKEN,
        ContractDeployment::Direct("L2WrappedBaseToken"),
    ),
    (
        SYSTEM_CONTRACT_PROXY_ADMIN,
        ContractDeployment::Direct("SystemContractProxyAdmin"),
    ),
    (
        L2_MESSAGE_ROOT_ADDR,
        ContractDeployment::SystemProxy("L2MessageRoot"),
    ),
    (
        L2_BRIDGEHUB_ADDR,
        ContractDeployment::SystemProxy("L2Bridgehub"),
    ),
    (
        L2_ASSET_ROUTER_ADDR,
        ContractDeployment::SystemProxy("L2AssetRouter"),
    ),
    (
        L2_NATIVE_TOKEN_VAULT_ADDR,
        ContractDeployment::SystemProxy("L2NativeTokenVaultZKOS"),
    ),
    (
        L2_NTV_BEACON_DEPLOYER_ADDR,
        ContractDeployment::SystemProxy("UpgradeableBeaconDeployer"),
    ),
    (
        L2_CHAIN_ASSET_HANDLER_ADDR,
        ContractDeployment::SystemProxy("L2ChainAssetHandler"),
    ),
    (
        L2_ASSET_TRACKER_ADDR,
        ContractDeployment::SystemProxy("L2AssetTracker"),
    ),
    (
        GW_ASSET_TRACKER_ADDR,
        ContractDeployment::SystemProxy("GWAssetTracker"),
    ),
    (
        L2_INTEROP_CENTER_ADDR,
        ContractDeployment::SystemProxy("InteropCenter"),
    ),
    (
        L2_INTEROP_HANDLER_ADDR,
        ContractDeployment::SystemProxy("InteropHandler"),
    ),
    (
        L2_BASE_TOKEN_HOLDER_ADDR,
        ContractDeployment::SystemProxy("BaseTokenHolder"),
    ),
    (
        L2_DEPLOYER_SYSTEM_CONTRACT_ADDR,
        ContractDeployment::SystemProxy("ZKOSContractDeployer"),
    ),
    (
        L2_TO_L1_MESSENGER_SYSTEM_CONTRACT_ADDR,
        ContractDeployment::SystemProxy("L1MessengerZKOS"),
    ),
    (
        L2_BASE_TOKEN_SYSTEM_CONTRACT_ADDR,
        ContractDeployment::SystemProxy("L2BaseTokenZKOS"),
    ),
    (
        L2_SYSTEM_CONTEXT_ADDR,
        ContractDeployment::SystemProxy("SystemContext"),
    ),
    (
        DETERMINISTIC_CREATE2_ADDRESS,
        ContractDeployment::Bytecode(CREATE2_FACTORY_RUNTIME_BYTECODE),
    ),
    (
        L2_INTEROP_ROOT_STORAGE,
        ContractDeployment::SystemProxy("L2InteropRootStorage"),
    ),
    (
        L2_MESSAGE_VERIFICATION,
        ContractDeployment::SystemProxy("L2MessageVerification"),
    ),
];
