use bcr_common::{
    cashu::{self, nut02 as cdk02},
    cdk_common,
};
use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("deserialize ciborium: {0}")]
    CiboriumDe(#[from] ciborium::de::Error<std::io::Error>),
    #[error("serialize ciborium: {0}")]
    CiboriumSer(#[from] ciborium::ser::Error<std::io::Error>),
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
    #[error("cdk_common::Error: {0}")]
    Cdk(#[from] cdk_common::Error),
    #[error("wallet id {0} not found")]
    WalletIdNotFound(String),
    #[error("transaction not found {0}")]
    TransactionNotFound(cdk_common::wallet::TransactionId),
    #[error("proof not in desired state: {0}")]
    InvalidProofState(cashu::PublicKey),
    #[error("proof in local DB not found: {0}")]
    ProofNotFound(cashu::PublicKey),
    #[error("mint op not found: {0}")]
    MintNotFound(String),
    #[error("melt op not found: {0}")]
    MeltNotFound(String),
    #[error("melt commitment not found: {0}")]
    MeltCommitmentNotFound(String),
    #[error("counter kid mismatch")]
    CounterKidMismatch,
    #[error("counter in local DB not found: {0}")]
    CounterNotFound(cdk02::Id),
    #[error("{0}")]
    Custom(String),
}
