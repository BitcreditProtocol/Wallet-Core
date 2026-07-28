use nostr::RelayUrl;
use std::{collections::HashMap, path::PathBuf, sync::atomic::AtomicBool};

pub const LOCK_REDUCTION_SECONDS_PER_HOP: u64 = 600;
pub const MAX_INTERMINT_ATTEMPTS: u64 = 3;
pub const MAX_ATTESTATION_ATTEMPTS: usize = 3;

#[derive(Debug)]
pub struct AppStateConfig {
    pub db_path: PathBuf,
    pub mnemonics: HashMap<String, bip39::Mnemonic>,
    pub swap_expiry: chrono::TimeDelta,
    /// List of Esplora API base URLs (in order of priority).
    /// The first URL is used for API requests with fallback to subsequent URLs on failure.
    pub esplora_base_urls: Vec<url::Url>,
    pub dev_mode: AtomicBool,
}

#[derive(Debug, Clone)]
pub struct CreateWalletConfig {
    pub name: String,
    pub network: bitcoin::Network,
    pub nostr_relays: Vec<RelayUrl>,
    pub mnemonic: bip39::Mnemonic,
    pub default_mint_url: url::Url,
}
