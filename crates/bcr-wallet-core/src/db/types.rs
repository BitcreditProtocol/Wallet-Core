// ----- standard library imports
// ----- extra library imports
use cashu::Proof;
use thiserror::Error;
// ----- local modules
// ----- end imports

#[derive(Debug, Error)]
pub enum DatabaseError {
    #[error("Database operation failed: {0}")]
    DatabaseError(String),
    #[error("Serialization failed: {0}")]
    SerializationError(String),
}

pub trait WalletDatabase {
    async fn get_proofs(&self) -> Result<Vec<Proof>, DatabaseError>;
    async fn set_proofs(&self, proofs: Vec<Proof>) -> Result<(), DatabaseError>;
    async fn add_proof(&self, proof: Proof) -> Result<(), DatabaseError>;
}
