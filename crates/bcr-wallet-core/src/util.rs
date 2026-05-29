use std::str::FromStr;

use bcr_common::cashu;
use bitcoin::{
    hashes::{Hash, HashEngine, sha256},
    hex::DisplayHex,
    secp256k1::{self, Keypair, SECP256K1},
};

use crate::types::Seed;

pub fn fee_and_amounts(max: cashu::Amount) -> cashu::amount::FeeAndAmounts {
    let max = max.to_u64();
    let mut amounts = Vec::new();
    let mut d = 1u64;
    while d <= max {
        amounts.push(d);
        d <<= 1;
    }
    (0u64, amounts).into()
}

// Build a public KeySet from a MintKeySet, the From impl was removed in cashu
pub fn to_keyset(keyset: &cashu::MintKeySet, active: Option<bool>) -> cashu::KeySet {
    cashu::KeySet {
        id: keyset.id,
        unit: keyset.unit.clone(),
        active,
        keys: keyset.keys.clone().into(),
        input_fee_ppk: keyset.input_fee_ppk,
        final_expiry: keyset.final_expiry,
    }
}

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

pub fn to_mint_url(url: &url::Url) -> cashu::MintUrl {
    cashu::MintUrl::from_str(url.as_ref()).expect("valid urls are valid mint urls")
}

pub fn from_mint_url(mint_url: &cashu::MintUrl) -> url::Url {
    url::Url::from_str(&mint_url.to_string()).expect("valid mint urls are valid urls")
}
