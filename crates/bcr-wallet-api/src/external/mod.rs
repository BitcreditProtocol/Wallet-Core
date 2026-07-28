pub mod bitcoin;
pub mod mint;

use thiserror::Error;

/// Generic error type
#[derive(Debug, Error)]
pub enum Error {
    /// all errors originating from the external mint API
    #[error("Mint API error: {0}")]
    MintApi(#[from] bcr_common::client::mint::Error),
    /// all errors originating from the external bitcoin API
    #[error("Bitcoin API error: {0}")]
    BitcoinApi(#[from] bitcoin::Error),
}
