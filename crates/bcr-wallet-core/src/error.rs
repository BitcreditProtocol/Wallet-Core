// ----- standard library imports
// ----- extra library imports
use anyhow::Error as AnyError;
use thiserror::Error;
// ----- local imports

pub type Result<T> = std::result::Result<T, Error>;
#[derive(Debug, Error)]
pub enum Error {
    #[error("Rexie error: {0}")]
    Rexie(#[from] rexie::Error),
    #[error("serde_wasm_bindgen error: {0}")]
    SerdeWasmBindgen(#[from] serde_wasm_bindgen::Error),
    #[error("cashu::mint_url::Error: {0}")]
    CashuMintUrl(#[from] cashu::mint_url::Error),
    #[error("cdk::Error: {0}")]
    Cdk(#[from] cdk::Error),
    #[error("bip39::Error: {0}")]
    Bip39(#[from] bip39::Error),
    #[error("cashu::nut00: {0}")]
    Cdk00(#[from] cashu::nut00::Error),
    #[error("cashu::nut13: {0}")]
    Cdk13(#[from] cashu::nut13::Error),
    #[error("cashu::amount: {0}")]
    CdkAmount(#[from] cashu::amount::Error),
    #[error("bitcoin::bip32 {0}")]
    BtcBip32(#[from] bitcoin::bip32::Error),
    #[error("uuid:: {0}")]
    Uuid(#[from] uuid::Error),
    #[error("nostr::nip19 {0}")]
    Nip19(#[from] nostr_sdk::nips::nip19::Error),
    #[error("nostr::nip06 {0}")]
    Nip06(#[from] nostr_sdk::nips::nip06::Error),
    #[error("nostr-sdk::client {0}")]
    NostrClient(#[from] nostr_sdk::client::Error),
    #[error("serde_json: {0}")]
    SerdeJson(#[from] serde_json::Error),
    #[error("reqwest::Url {0}")]
    Url(#[from] url::ParseError),
    #[error("reqwest::Client {0}")]
    ReqwestClient(#[from] reqwest::Error),

    #[error("insufficient funds")]
    InsufficientFunds,
    #[error("local pocket DB not initialized correctly")]
    BadPocketDB,
    #[error("local purse DB not initialized correctly")]
    BadPurseDB,
    #[error("local transaction DB not initialized correctly")]
    BadTransactionDB,
    #[error("local mint/melt DB not initialized correctly")]
    BadMintMeltDB,
    #[error("local settings DB not initialized correctly")]
    BadSettingsDB,
    #[error("proof in local DB not found: {0}")]
    ProofNotFound(cashu::PublicKey),
    #[error("proof not in desired state: {0}")]
    InvalidProofState(cashu::PublicKey),
    #[error("internal, generic: {0}")]
    Any(AnyError),
    #[error("wallet id {0} not found")]
    WalletIdNotFound(String),
    #[error("wallet at idx {0} not found")]
    WalletNotFound(usize),
    #[error("empty token: {0}")]
    EmptyToken(String),
    #[error("invalid token: {0}")]
    InvalidToken(String),
    #[error("no active keyset")]
    NoActiveKeyset,
    #[error("unknown keyset ID")]
    UnknownKeysetId(cashu::Id),
    #[error("unknown currency unit: {0}")]
    UnknownCurrencyUnit(cashu::CurrencyUnit),
    #[error("unknown mint: {0}")]
    UnknownMint(cashu::MintUrl),
    #[error("currency unit mismatch: mine {0}, his {1}")]
    CurrencyUnitMismatch(cashu::CurrencyUnit, cashu::CurrencyUnit),
    #[error("no reference to prepare request_id: {0}")]
    NoPrepareRef(uuid::Uuid),
    #[error("inactive keyset {0}")]
    InactiveKeyset(cashu::Id),
    #[error("transaction not found {0}")]
    TransactionNotFound(cdk::wallet::types::TransactionId),
    #[error("Mint not supporting debit currency")]
    NoDebitCurrencyInMint(Vec<cashu::CurrencyUnit>),
    #[error("network mismatch, ours: {0}, theirs: {1}")]
    InvalidNetwork(bitcoin::Network, bitcoin::Network),
    #[error("payment request, missing amount")]
    MissingAmount,
    #[error("payment request unknown {0}")]
    UnknownPaymentRequest(String),
    #[error("payment expired")]
    PaymentExpired,
    #[error("melt op unpaid")]
    MeltUnpaid(String),
    #[error("melt op not found: {0}")]
    MeltNotFound(String),
    #[error("missing initialization")]
    Initialization,
    #[error("inter-mint payment not supported yet")]
    InterMint,
    #[error("spending conditions not supported yet")]
    SpendingConditions,
    #[error("NUT-18 request has no transport")]
    NoTransport,

    #[error("internal error: {0}")]
    Internal(String),
}
