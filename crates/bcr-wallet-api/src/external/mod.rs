pub mod mint;
#[cfg(test)]
pub mod test_utils;

use thiserror::Error;

/// Generic error type
#[derive(Debug, Error)]
pub enum Error {
    /// all errors originating from the external mint API
    #[error("Mint API error: {0}")]
    MintApi(#[from] bcr_common::cdk::Error),
}
