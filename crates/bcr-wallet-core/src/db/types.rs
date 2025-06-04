// ----- standard library imports
// ----- extra library imports
use cashu::{Id, Proof, PublicKey};
use serde::{Deserialize, Serialize};
use thiserror::Error;
// ----- local modules
// ----- end imports

#[derive(Debug, Error)]
pub enum DatabaseError {
    #[error("Database operation failed: {0}")]
    DatabaseError(String),
    #[error("Serialization failed: {0}")]
    SerializationError(String),
    #[error("Keyset not found")]
    KeysetNotFound,
}

#[derive(Debug, Serialize, PartialEq, Eq, Deserialize)]
pub enum ProofStatus {
    Unspent,
    Spent,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct WalletProof {
    pub(crate) proof: Proof,
    pub(crate) status: ProofStatus,
    pub(crate) id: PublicKey, // This might change
}

pub trait WalletDatabase {
    async fn get_active_proofs(&self) -> Result<Vec<Proof>, DatabaseError>;
    /// Mark a proof as spent so it won't get used by subsequent transfers
    async fn deactivate_proof(&self, proof: Proof) -> Result<(), DatabaseError>;
    async fn add_proof(&self, proof: Proof) -> Result<(), DatabaseError>;
}

pub trait KeysetDatabase {
    async fn get_count(&self, keyset_id: Id) -> Result<u32, DatabaseError>;
    async fn increase_count(&self, keyset_id: Id, addition: u32) -> Result<u32, DatabaseError>;
}
