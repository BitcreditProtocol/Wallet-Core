use anyhow::Error as AnyError;
use bcr_common::{
    cashu::{self, MintUrl, nut02 as cdk02},
    cdk,
};
use bitcoin::hashes::sha256::Hash as Sha256;
use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;
#[derive(Debug, Error)]
pub enum Error {
    #[error("BorshSignature: {0}")]
    BorshSignature(#[from] bcr_common::core::signature::BorshMsgSignatureError),
    #[error("Borsh: {0}")]
    Borsh(#[from] borsh::io::Error),
    #[error("cashu::mint_url::Error: {0}")]
    CashuMintUrl(#[from] cashu::mint_url::Error),
    #[error("cdk::Error: {0}")]
    Cdk(#[from] cdk::Error),
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
    #[error("cashu::amount: {0}")]
    CdkAmount(#[from] cashu::amount::Error),
    #[error("cashu::dhke: {0}")]
    CdkDhke(#[from] cashu::dhke::Error),
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
    #[error("deserialize ciborium: {0}")]
    CiboriumDe(#[from] ciborium::de::Error<std::io::Error>),
    #[error("serialize ciborium: {0}")]
    CiboriumSer(#[from] ciborium::ser::Error<std::io::Error>),
    #[error("reqwest::Url {0}")]
    Url(#[from] url::ParseError),
    #[error("reqwest::Client {0}")]
    ReqwestClient(#[from] reqwest::Error),
    #[error("insufficient funds")]
    InsufficientFunds,
    #[error("Database operation error: {0}")]
    Redb(#[from] redb::Error),
    #[error("Database error: {0}")]
    RedbDatabase(#[from] redb::DatabaseError),
    #[error("Database Transaction error: {0}")]
    RedbTransaction(#[from] redb::TransactionError),
    #[error("Database Commit error: {0}")]
    RedbCommit(#[from] redb::CommitError),
    #[error("Database Table error: {0}")]
    RedbTable(#[from] redb::TableError),
    #[error("Database Storage error: {0}")]
    RedbStorage(#[from] redb::StorageError),
    #[error("Database Join error: {0}")]
    RedbTokioSpawn(#[from] tokio::task::JoinError),
    #[error("proof in local DB not found: {0}")]
    ProofNotFound(cashu::PublicKey),
    #[error("proof not in desired state: {0}")]
    InvalidProofState(cashu::PublicKey),
    #[error("counter kid mismatch")]
    CounterKidMismatch,
    #[error("counter in local DB not found: {0}")]
    CounterNotFound(cdk02::Id),
    #[error("There already exists a wallet - delete it to create a new one")]
    WalletAlreadyExists,
    #[error("wallet id {0} not found")]
    WalletIdNotFound(String),
    #[error("wallet at idx {0} not found")]
    WalletNotFound(usize),
    #[error("empty token: {0}")]
    EmptyToken(String),
    #[error("invalid token: {0}")]
    InvalidToken(String),
    #[error("invalid bitcoin address: {0}")]
    InvalidBitcoinAddress(String),
    #[error("Invalid Hash Lock on Beta Proofs, expected {0} got {1}")]
    InvalidHashLock(Sha256, String),
    #[error("no active keyset")]
    NoActiveKeyset,
    #[error("unknown keyset ID")]
    UnknownKeysetId(cashu::Id),
    #[error("invalid currency unit: {0}")]
    InvalidCurrencyUnit(String),
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
    #[error("transaction can't be reclaimed - not outgoing or pending {0}")]
    TransactionCantBeReclaimed(cdk::wallet::types::TransactionId),
    #[error("Mint not supporting debit currency")]
    NoDebitCurrencyInMint(Vec<cashu::CurrencyUnit>),
    #[error("network mismatch, ours: {0}, theirs: {1}")]
    InvalidNetwork(bitcoin::Network, bitcoin::Network),
    #[error("mnemonic mismatch")]
    InvalidMnemonic,
    #[error("mint url mismatch, ours: {0}, theirs: {1}")]
    InvalidMintUrl(MintUrl, MintUrl),
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
    #[error("mint op not found: {0}")]
    MintNotFound(String),
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
    #[error("Beta not found")]
    BetaNotFound(cashu::MintUrl),
    #[error("No Substitute could be determined")]
    NoSubstitute,
    #[error("Unsupported: {0}")]
    Unsupported(String),
    #[error("insufficient amount for melting {0}")]
    InsufficientOnChainMeltAmount(u64),
    #[error("insufficient amount for minting {0}")]
    InsufficientOnChainMintAmount(u64),

    #[error("internal error: {0}")]
    Internal(String),
    #[error("internal, generic: {0}")]
    Any(AnyError),
}
