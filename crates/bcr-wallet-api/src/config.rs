use std::path::PathBuf;

use crate::error::Result;
use bcr_common::cashu::MintUrl;
use nostr_sdk::{Keys, RelayUrl, nips::nip06::FromMnemonic, nips::nip19::Nip19Profile};

pub const LOCK_REDUCTION_SECONDS_PER_HOP: u64 = 600;
pub const MAX_INTERMINT_ATTEMPTS: u64 = 3;

#[derive(Debug, Clone)]
pub struct AppStateConfig {
    pub db_path: PathBuf,
    pub network: bitcoin::Network,
    pub nostr_relays: Vec<RelayUrl>,
    pub mnemonic: bip39::Mnemonic,
    pub swap_expiry: chrono::TimeDelta,
    pub default_mint_url: MintUrl,
}

#[derive(Debug, Clone)]
pub struct NostrConfig {
    pub nprofile: Nip19Profile,
    pub nostr_signer: Keys,
    pub relays: Vec<RelayUrl>,
}

impl NostrConfig {
    pub fn new(mnemonic: bip39::Mnemonic, nostr_relays: Vec<RelayUrl>) -> Result<Self> {
        let keys = Keys::from_mnemonic(mnemonic.to_string(), None)?;

        Ok(Self {
            nprofile: Nip19Profile::new(keys.public_key, nostr_relays.clone()),
            nostr_signer: keys,
            relays: nostr_relays,
        })
    }
}
