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

    #[error("insufficient funds")]
    InsufficientFunds,
    #[error("local pocket DB not initialized correctly")]
    BadPocketDB,
    #[error("local transaction DB not initialized correctly")]
    BadTransactionDB,
    #[error("proof in local DB not found: {0}")]
    ProofNotFound(cashu::PublicKey),
    #[error("proof not in desired state: {0}")]
    InvalidProofState(cashu::PublicKey),
    #[error("internal, generic: {0}")]
    Any(AnyError),
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
    #[error("currency unit mismatch: mine {0}, his {1}")]
    CurrencyUnitMismatch(cashu::CurrencyUnit, cashu::CurrencyUnit),
    #[error("no reference to prepare_send request_id: {0}")]
    NoPrepareSendRef(uuid::Uuid),
    #[error("inactive keyset {0}")]
    InactiveKeyset(cashu::Id),
    #[error("transaction not found {0}")]
    TransactionNotFound(cdk::wallet::types::TransactionId),

    #[error("internal error: {0}")]
    Internal(String),
}
