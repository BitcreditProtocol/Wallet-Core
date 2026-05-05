use bitcoin::{
    hashes::{Hash, HashEngine, sha256},
    hex::DisplayHex,
    secp256k1::{self, Keypair, SECP256K1},
};

use crate::types::Seed;

// Builds the wallet id, which is the hashed seed and bitcoin network, to ensure
// uniqueness of a keypair per bitcoin network
pub fn build_wallet_id(seed: &Seed, network: bitcoin::Network) -> String {
    let mut hasher = sha256::HashEngine::default();
    hasher.input(seed);
    hasher.input(network.magic().to_bytes().as_slice());
    sha256::Hash::from_engine(hasher)
        .as_byte_array()
        .as_hex()
        .to_string()
}

pub fn seed_from_mnemonic(mnemonic: &bip39::Mnemonic) -> Seed {
    mnemonic.to_seed("")
}

pub fn keypair_from_seed(seed: Seed) -> Keypair {
    let (key, _) = seed.split_at(secp256k1::constants::SECRET_KEY_SIZE);
    Keypair::from_seckey_slice(SECP256K1, key).expect("key to be correct size")
}

pub fn keypair_from_mnemonic(mnemonic: &bip39::Mnemonic) -> Keypair {
    let seed = seed_from_mnemonic(mnemonic);
    keypair_from_seed(seed)
}
