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
    #[error("bitcoin::bip32 {0}")]
    BtcBip32(#[from] bitcoin::bip32::Error),

    #[error("local proof DB not initialized correctly")]
    BadProofDB,
    #[error("proof in local DB not found: {0}")]
    ProofNotFound(cashu::PublicKey),
    #[error("internal, generic: {0}")]
    Any(AnyError),
    #[error("wallet at idx {0} not found")]
    WalletNotFound(usize),
    #[error("invalid token: {0}")]
    InvalidToken(String),
    #[error("no active keyset")]
    NoActiveKeyset,
    #[error("currency unit mismatch: mine {0}, his {1}")]
    CurrencyUnitMismatch(cashu::CurrencyUnit, cashu::CurrencyUnit),
}
