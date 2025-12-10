use crate::error::Result;
use nostr_sdk::{Keys, RelayUrl, nips::nip06::FromMnemonic, nips::nip19::Nip19Profile};

pub const LOCK_REDUCTION_SECONDS_PER_HOP: u64 = 600;
pub const MAX_INTERMINT_ATTEMPTS: u64 = 3;

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub enum SameMintSafeMode {
    Enabled { expiration: chrono::TimeDelta },
    Disabled,
}
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Settings {
    pub network: bitcoin::Network,
    pub mnemonic: bip39::Mnemonic,
    pub nostr_relays: Vec<RelayUrl>,
    pub same_mint_safe_mode: SameMintSafeMode,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            network: bitcoin::Network::Testnet,
            mnemonic: bip39::Mnemonic::generate(12).expect("Failed to generate default mnemonic"),
            nostr_relays: vec![
                RelayUrl::parse("wss://bcr-relay-dev.minibill.tech")
                    .expect("Invalid default relay URL"),
            ],
            same_mint_safe_mode: SameMintSafeMode::Disabled,
            // Disabled for now until Clowder stabilizes more
            // same_mint_safe_mode: SameMintSafeMode::Enabled {
            //     expiration: chrono::TimeDelta::minutes(15),
            // },
        }
    }
}

pub struct Config {
    pub nprofile: Nip19Profile,
    pub nostr_signer: Keys,
    pub relays: Vec<RelayUrl>,
}

impl Config {
    pub fn new(settings: Settings) -> Result<Self> {
        let keys = Keys::from_mnemonic(settings.mnemonic.to_string(), None)?;

        Ok(Self {
            nprofile: Nip19Profile::new(keys.public_key, settings.nostr_relays.clone()),
            nostr_signer: keys,
            relays: settings.nostr_relays,
        })
    }
}
