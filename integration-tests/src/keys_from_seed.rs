//! Generate secp256k1 private keys from short seed phrases for tests.
//! Keys are deterministic: same seed always yields the same key.

use sha2::{Digest, Sha256};

const HEX: &[u8; 16] = b"0123456789abcdef";

/// Produces a 32-byte secp256k1 private key (as hex with 0x prefix) from a seed string.
/// Deterministic: same seed always returns the same key.
pub fn private_key_from_seed(seed: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(seed.as_bytes());
    let bytes: [u8; 32] = hasher.finalize().into();
    let mut out = String::with_capacity(66);
    out.push_str("0x");
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 15) as usize] as char);
    }
    out
}

/// Operator keys for protocol_ops_init (commit / prove / execute).
pub fn operator_commit_private_key() -> String {
    private_key_from_seed("operator commit pk")
}

pub fn operator_prove_private_key() -> String {
    private_key_from_seed("operator prove pk")
}

pub fn operator_execute_private_key() -> String {
    private_key_from_seed("operator execute pk")
}
