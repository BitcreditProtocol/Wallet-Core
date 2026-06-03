use bcr_common::cashu;
use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("nostr::nip19 {0}")]
    Nip19(#[from] nostr_sdk::nips::nip19::Error),
    #[error("nostr::nip06 {0}")]
    Nip06(#[from] nostr_sdk::nips::nip06::Error),
    #[error("nostr-sdk::client {0}")]
    NostrClient(#[from] nostr_sdk::client::Error),
    #[error("nostr::event {0}")]
    NostrEvent(#[from] ::nostr::event::builder::Error),
    #[error("nostr::send_private_msg {0}")]
    NostrSendPrivateMsg(nostr::event::EventId),
    #[error("serde_json: {0}")]
    SerdeJson(#[from] serde_json::Error),
    #[error("cashu::nut00: {0}")]
    Cdk00(#[from] cashu::nut00::Error),
    #[error("Network error: {0}")]
    Network(String),
    #[error("Crypto error: {0}")]
    Crypto(String),
    #[error("Persistence error: {0}")]
    Persistence(#[from] bcr_wallet_persistence::error::Error),
}
