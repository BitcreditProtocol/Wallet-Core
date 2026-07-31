use bcr_common::{
    cashu::{self},
    cdk_common,
};
use thiserror::Error;
use uuid::Uuid;

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
    #[error("wallet with name {0} already exists for network {1}")]
    WalletUniqueName(String, bitcoin::Network),
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
    #[error("invalid bitcoin network: {0}")]
    InvalidBitcoinNetwork(String),
    #[error("invalid bitcoin tx id: {0}")]
    InvalidBitcoinTxId(String),
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
    TransactionCantBeReclaimed(uuid::Uuid),
    #[error("Mint not supporting debit currency")]
    NoDebitCurrencyInMint(Vec<cashu::CurrencyUnit>),
    #[error("network mismatch, ours: {0}, theirs: {1}")]
    InvalidNetwork(bitcoin::Network, bitcoin::Network),
    #[error("invalid name")]
    InvalidName,
    #[error("empty name")]
    EmptyName,
    #[error("invalid email")]
    InvalidEmail,
    #[error("empty email")]
    EmptyEmail,
    #[error("invalid contact - need one of name/company and one of node_id/email")]
    InvalidContact,
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
    ContactNotFound(String),
    #[error("Contact must have node id for payment request for contact {0}")]
    ContactMustHaveNodeId(String),
    #[error("Contact {0} already exists")]
    ContactAlreadyExists(Uuid),
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
    #[error("insufficient network fee for melting {0}")]
    InsufficientOnChainNetworkFee(u64),
    #[error("insufficient amount for minting {0}")]
    InsufficientOnChainMintAmount(u64),
    #[error("Database Error: {0}")]
    Database(#[from] bcr_wallet_persistence::error::Error),
    #[error("Transport Error: {0}")]
    Transport(#[from] bcr_wallet_transport::error::Error),
    #[error("Dev Mode is disabled")]
    NoDevMode,
    #[error("melt quote commitment does not match request")]
    MeltQuoteMismatch,
    #[error("swap commitment does not match request")]
    SwapCommitmentMismatch,
    #[error("No payment request found for {0}")]
    PaymentRequestNotFound(Uuid),
    #[error("Given Payment Request {0} was in the wrong state for this operation")]
    PaymentRequestInWrongState(Uuid),
    #[error("Bitcoin Client returned an Api Error: {0}")]
    BitcoinClient(String),
    #[error("Mint Client returned an internal Error: {0}")]
    MintClientInternal(String),
    #[error("{0}")]
    MintClientResourceNotFound(String),
    #[error("{0}")]
    MintClientServiceUnavailable(String),
    #[error("{0}")]
    MintClientBadRequest(String),
    #[error("{0}")]
    MintClientKeysetNotFound(String),
    #[error("{0}")]
    MintClientMeltOpSuspended(String),
    #[error("{0}")]
    MintClientCommitmentMismatch(String),
    #[error("invalid proof: {0}")]
    AttestationInvalidProof(String),
    #[error("fp_digest mismatch")]
    AttestationDigestMismatch,
    #[error("unknown beta: {0}")]
    AttestationUnknownBeta(String),
    #[error("{0}")]
    AttestationVerifyNotFound(String),
    #[error("{0}")]
    AttestationSignature(String),
}

impl From<crate::external::Error> for Error {
    fn from(value: crate::external::Error) -> Self {
        match value {
            crate::external::Error::MintApi(error) => match error {
                bcr_common::client::mint::Error::ResourceNotFound(rnferror) => match rnferror {
                    bcr_common::client::mint::RNFError::Unknown => {
                        Error::MintClientResourceNotFound(rnferror.to_string())
                    }
                    bcr_common::client::mint::RNFError::KeysetId(id) => {
                        Error::MintClientKeysetNotFound(id.to_string())
                    }
                    bcr_common::client::mint::RNFError::Generic(msg) => {
                        Error::MintClientResourceNotFound(msg)
                    }
                    bcr_common::client::mint::RNFError::Quote(value) => {
                        Error::MintClientResourceNotFound(value.to_string())
                    }
                    bcr_common::client::mint::RNFError::Treasury(value) => {
                        Error::MintClientResourceNotFound(value.to_string())
                    }
                    bcr_common::client::mint::RNFError::Clowder(value) => {
                        Error::MintClientResourceNotFound(value.to_string())
                    }
                },
                bcr_common::client::mint::Error::InvalidRequest(brerror) => match brerror {
                    bcr_common::client::mint::BRError::Unknown => {
                        Error::MintClientBadRequest(brerror.to_string())
                    }
                    bcr_common::client::mint::BRError::CommitmentMismatch => {
                        Error::MintClientCommitmentMismatch(brerror.to_string())
                    }
                    bcr_common::client::mint::BRError::Generic(msg) => {
                        Error::MintClientBadRequest(msg)
                    }
                    bcr_common::client::mint::BRError::Quote(value) => {
                        Error::MintClientBadRequest(value.to_string())
                    }
                    bcr_common::client::mint::BRError::Treasury(value) => {
                        Error::MintClientBadRequest(value.to_string())
                    }
                    bcr_common::client::mint::BRError::Clowder(value) => {
                        Error::MintClientBadRequest(value.to_string())
                    }
                },
                bcr_common::client::mint::Error::ServiceUnavailable(suerror) => match suerror {
                    bcr_common::client::mint::SUError::Unknown => {
                        Error::MintClientServiceUnavailable(suerror.to_string())
                    }
                    bcr_common::client::mint::SUError::Core(value) => {
                        Error::MintClientServiceUnavailable(value.to_string())
                    }
                    bcr_common::client::mint::SUError::Quote(value) => {
                        Error::MintClientServiceUnavailable(value.to_string())
                    }
                    bcr_common::client::mint::SUError::MeltOpSuspended(msg) => {
                        Error::MintClientMeltOpSuspended(msg)
                    }
                    bcr_common::client::mint::SUError::Clowder(value) => {
                        Error::MintClientServiceUnavailable(value.to_string())
                    }
                },
                bcr_common::client::mint::Error::Internal(err) => Error::MintClientInternal(err),
                bcr_common::client::mint::Error::Reqwest(error) => {
                    Error::MintClientInternal(error.to_string())
                }
                bcr_common::client::mint::Error::Cdk20(error) => {
                    Error::MintClientInternal(error.to_string())
                }
                bcr_common::client::mint::Error::BorshSign(error) => {
                    Error::MintClientInternal(error.to_string())
                }
            },
            crate::external::Error::BitcoinApi(error) => Error::BitcoinClient(error.to_string()),
        }
    }
}

impl From<bcr_common::wire::attestation::AttestationError> for Error {
    fn from(value: bcr_common::wire::attestation::AttestationError) -> Self {
        match value {
            bcr_common::wire::attestation::AttestationError::InvalidProof(error) => {
                Error::AttestationInvalidProof(error.to_string())
            }
            bcr_common::wire::attestation::AttestationError::DigestMismatch => {
                Error::AttestationDigestMismatch
            }
            bcr_common::wire::attestation::AttestationError::UnknownBeta(public_key) => {
                Error::AttestationUnknownBeta(public_key.to_string())
            }
            bcr_common::wire::attestation::AttestationError::VerifyNotFound => {
                Error::AttestationVerifyNotFound(value.to_string())
            }
            bcr_common::wire::attestation::AttestationError::Signature(_) => {
                Error::AttestationSignature(value.to_string())
            }
        }
    }
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
            bcr_wallet_core::ValidationError::EmptyEmail => Error::EmptyEmail,
            bcr_wallet_core::ValidationError::InvalidEmail => Error::InvalidEmail,
            bcr_wallet_core::ValidationError::InvalidContact(_) => Error::InvalidContact,
        }
    }
}
