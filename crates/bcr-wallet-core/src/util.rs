use bitcoin::{
    hashes::{Hash, HashEngine, sha256},
    hex::DisplayHex,
    secp256k1::{self, Keypair, SECP256K1},
};

pub fn build_wallet_id(seed: &[u8; 64]) -> String {
    let mut hasher = sha256::HashEngine::default();
    hasher.input(seed);
    sha256::Hash::from_engine(hasher)
        .as_byte_array()
        .as_hex()
        .to_string()
}

pub fn seed_from_mnemonic(mnemonic: &bip39::Mnemonic) -> [u8; 64] {
    mnemonic.to_seed("")
}

pub fn keypair_from_seed(seed: [u8; 64]) -> Keypair {
    let (key, _) = seed.split_at(secp256k1::constants::SECRET_KEY_SIZE);
    Keypair::from_seckey_slice(SECP256K1, key).expect("key to be correct size")
}

pub fn keypair_from_mnemonic(mnemonic: &bip39::Mnemonic) -> Keypair {
    let seed = seed_from_mnemonic(mnemonic);
    keypair_from_seed(seed)
}
