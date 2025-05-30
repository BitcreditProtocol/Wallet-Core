// ----- standard library imports
// ----- extra library imports
use cashu::{Proof, PublicKey};
// ----- local modules
use crate::db::WalletDatabase;
use crate::db::types::DatabaseError;
// ----- end imports

pub struct MemoryDatabase {
    proofs: Vec<Proof>,
}

impl Default for MemoryDatabase {
    fn default() -> Self {
        Self { proofs: Vec::new() }
    }
}

// impl WalletDatabase for MemoryDatabase {
//     async fn get_proofs(&self) -> Result<Vec<Proof>, DatabaseError> {
//         Ok(self.proofs.clone())
//     }

//     async fn set_proofs(&self, proofs: Vec<Proof>) -> Result<(), DatabaseError> {
//         // self.proofs = proofs;
//         Ok(())
//     }

//     async fn add_proof(&self, proof: Proof) -> Result<(), DatabaseError> {
//         // self.proofs.push(proof);
//         Ok(())
//     }
// }
