use bcr_common::{
    cashu::{self},
    cdk_common,
    core::NodeId,
};
use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;
#[derive(Debug, Error)]
pub enum Error {
    #[error("BorshSignature: {0}")]
    BorshSignature(#[from] bcr_common::core::signature::BorshMsgSignatureError),
    #[error("SchnorrSignature: {0}")]
    SchnorrSignature(String),
    #[error("Borsh: {0}")]
    Borsh(#[from] borsh::io::Error),
    #[error("cashu::mint_url::Error: {0}")]
    CashuMintUrl(#[from] cashu::mint_url::Error),
    #[error("MintError: {0}")]
    Mint(#[from] bcr_common::client::mint::Error),
    #[error("cdk_common::Error: {0}")]
    Cdk(#[from] cdk_common::Error),
    #[error("bip39::Error: {0}")]
    Bip39(#[from] bip39::Error),
    #[error("cashu::nut00: {0}")]
    Cdk00(#[from] cashu::nut00::Error),
    #[error("cashu::nut01: {0}")]
    Cdk01(#[from] cashu::nut01::Error),
    #[error("cashu::nut13: {0}")]
    Cdk13(#[from] cashu::nut13::Error),
    #[error("cashu::nut11: {0}")]
    Cdk11(#[from] cashu::nut11::Error),
    #[error("cashu::nut10: {0}")]
    Cdk10(#[from] cashu::nut10::Error),
    #[error("cashu::nut14: {0}")]
    Cdk14(#[from] cashu::nut14::Error),
    #[error("cashu::amount: {0}")]
    CdkAmount(#[from] cashu::amount::Error),
    #[error("cashu::dhke: {0}")]
    CdkDhke(#[from] cashu::dhke::Error),
    #[error("Invalid Split Target - only Value supported")]
    InvalidSplitTarget,
    #[error("Error during Swap: {0}")]
    Swap(String),
    #[error("bitcoin::bip32 {0}")]
    BtcBip32(#[from] bitcoin::bip32::Error),
    #[error("uuid:: {0}")]
    Uuid(#[from] uuid::Error),
    #[error("serde_json: {0}")]
    SerdeJson(#[from] serde_json::Error),
    #[error("reqwest::Url {0}")]
    Url(#[from] url::ParseError),
    #[error("reqwest::Client {0}")]
    ReqwestClient(#[from] reqwest::Error),
    #[error("total balance {0} is less than target {1}")]
    InsufficientBalance(cashu::Amount, cashu::Amount),
    #[error("wallet with id {0} not found")]
    WalletNotFound(String),
    #[error("wallet with name {0} already exists")]
    WalletUniqueName(String),
    #[error("wallet with id {0} already exists")]
    WalletUniqueId(String),
    #[error("mnemonic for id {0} not found")]
    MnemonicNotFound(String),
    #[error("empty token: {0}")]
    EmptyToken(String),
    #[error("invalid token: {0}")]
    InvalidToken(String),
    #[error("invalid bitcoin address: {0}")]
    InvalidBitcoinAddress(String),
    #[error("no active keyset")]
    NoActiveKeyset,
    #[error("unknown keyset ID")]
    UnknownKeysetId(cashu::Id),
    #[error("inactive keyset {0}")]
    InactiveKeyset(cashu::Id),
    #[error("invalid currency unit: {0}")]
    InvalidCurrencyUnit(String),
    #[error("no reference to prepare request_id: {0}")]
    NoPrepareRef(uuid::Uuid),
    #[error("transaction can't be reclaimed - not outgoing or pending {0}")]
    TransactionCantBeReclaimed(cdk_common::wallet::TransactionId),
    #[error("Mint not supporting debit currency")]
    NoDebitCurrencyInMint(Vec<cashu::CurrencyUnit>),
    #[error("network mismatch, ours: {0}, theirs: {1}")]
    InvalidNetwork(bitcoin::Network, bitcoin::Network),
    #[error("invalid name")]
    InvalidName,
    #[error("empty name")]
    EmptyName,
    #[error("invalid node id")]
    InvalidNodeId,
    #[error("invalid bill id")]
    InvalidBillId,
    #[error("invalid transaction id")]
    InvalidTransactionId,
    #[error("invalid cursor")]
    InvalidCursor,
    #[error("sort has to match cursor sort")]
    SortMismatch,
    #[error("mnemonic mismatch")]
    InvalidMnemonic,
    #[error("payment request, missing amount")]
    MissingAmount,
    #[error("payment request unknown {0}")]
    UnknownPaymentRequest(String),
    #[error("mint op failed: {0}")]
    MintingError(String),
    #[error("inter-mint payment not supported yet")]
    InterMint,
    #[error("Missing DLEQ proof")]
    MissingDleq,
    #[error("intermint payment, but no clowder path")]
    InterMintButNoClowderPath,
    #[error("spending conditions not supported yet")]
    SpendingConditions,
    #[error("NUT-18 request has no transport")]
    NoTransport,
    #[error("Maximum Exchange attempts reached")]
    MaxExchangeAttempts,
    #[error("Invalid Clowder Path for foreign eCash")]
    InvalidClowderPath,
    #[error("No contact found for {0}")]
    ContactNotFound(NodeId),
    #[error("Contact {0} already exists")]
    ContactAlreadyExists(NodeId),
    #[error("Beta not found")]
    BetaNotFound(url::Url),
    #[error("No Substitute could be determined")]
    NoSubstitute,
    #[error("No beta mints available")]
    NoBetas,
    #[error("Unsupported: {0}")]
    Unsupported(String),
    #[error("insufficient amount for melting {0}")]
    InsufficientOnChainMeltAmount(u64),
    #[error("insufficient amount for minting {0}")]
    InsufficientOnChainMintAmount(u64),
    #[error("Database Error: {0}")]
    Database(#[from] bcr_wallet_persistence::error::Error),
    #[error("Transport Error: {0}")]
    Transport(#[from] bcr_wallet_transport::error::Error),
    #[error("External Error: {0}")]
    External(#[from] crate::external::Error),
    #[error("Dev Mode is disabled")]
    NoDevMode,
}

impl From<bcr_common::core::swap::wallet::Error> for Error {
    fn from(value: bcr_common::core::swap::wallet::Error) -> Self {
        match value {
            bcr_common::core::swap::wallet::Error::UnknownKeyset(id) => Error::UnknownKeysetId(id),
            bcr_common::core::swap::wallet::Error::InsufficientBalance(amount, other_amount) => {
                Error::InsufficientBalance(amount, other_amount)
            }
        }
    }
}

impl From<bcr_common::core::Error> for Error {
    fn from(err: bcr_common::core::Error) -> Self {
        match err {
            bcr_common::core::Error::InvalidNodeId => Error::InvalidNodeId,
            bcr_common::core::Error::InvalidBillId => Error::InvalidBillId,
        }
    }
}

impl From<bcr_wallet_core::ValidationError> for Error {
    fn from(err: bcr_wallet_core::ValidationError) -> Self {
        match err {
            bcr_wallet_core::ValidationError::EmptyName => Error::EmptyName,
            bcr_wallet_core::ValidationError::InvalidName => Error::InvalidName,
        }
    }
}
