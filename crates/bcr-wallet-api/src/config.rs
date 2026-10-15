use crate::external::mint::MintTimeouts;
use nostr::types::RelayUrl;
use std::{collections::HashMap, path::PathBuf, sync::atomic::AtomicBool, time::Duration};

pub const LOCK_REDUCTION_SECONDS_PER_HOP: u64 = 600;
pub const MAX_INTERMINT_ATTEMPTS: u64 = 3;
pub const MAX_ATTESTATION_ATTEMPTS: usize = 3;

#[derive(Debug)]
pub struct AppStateConfig {
    pub db_path: PathBuf,
    pub mnemonics: HashMap<String, bip39::Mnemonic>,
    pub swap_expiry: time::Duration,
    /// List of Esplora API base URLs (in order of priority).
    /// The first URL is used for API requests with fallback to subsequent URLs on failure.
    pub esplora_base_urls: Vec<url::Url>,
    pub dev_mode: AtomicBool,
    pub mint_connect_timeout: Duration,
    pub mint_request_timeout: Duration,
}

impl AppStateConfig {
    pub const DEFAULT_MINT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
    pub const DEFAULT_MINT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

    pub(crate) fn mint_timeouts(&self) -> MintTimeouts {
        MintTimeouts {
            connect: self.mint_connect_timeout,
            request: self.mint_request_timeout,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CreateWalletConfig {
    pub name: String,
    pub network: bitcoin::Network,
    pub nostr_relays: Vec<RelayUrl>,
    pub mnemonic: bip39::Mnemonic,
    pub default_mint_url: url::Url,
}
