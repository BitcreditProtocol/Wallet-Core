// ----- standard library imports
// ----- extra library imports
use nostr_sdk::{Keys, RelayUrl, nips::nip06::FromMnemonic, nips::nip19::Nip19Profile};
// ----- local imports
use crate::error::Result;

// ----- end imports

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Settings {
    pub network: bitcoin::Network,
    pub mnemonic: bip39::Mnemonic,
    pub nostr_relays: Vec<RelayUrl>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            network: bitcoin::Network::Testnet,
            mnemonic: bip39::Mnemonic::generate(12).expect("Failed to generate default mnemonic"),
            nostr_relays: vec![
                RelayUrl::parse("wss://bitcr-cloud-run-05-550030097098.europe-west1.run.app")
                    .expect("Invalid default relay URL"),
            ],
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
