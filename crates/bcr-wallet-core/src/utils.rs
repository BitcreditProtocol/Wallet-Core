use bitcoin::{
    hashes::{Hash, HashEngine, sha256},
    hex::DisplayHex,
};

pub fn build_wallet_id(seed: &[u8; 64]) -> String {
    let mut hasher = sha256::HashEngine::default();
    hasher.input(seed);
    sha256::Hash::from_engine(hasher)
        .as_byte_array()
        .as_hex()
        .to_string()
}
