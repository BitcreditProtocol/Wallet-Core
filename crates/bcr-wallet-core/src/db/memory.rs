// ----- standard library imports
// ----- extra library imports
use async_trait::async_trait;
use cashu::Proof;
// ----- local modules
use crate::db::WalletDatabase;
// ----- end imports

pub struct MemoryDatabase {
    proofs: Vec<Proof>,
}

impl Default for MemoryDatabase {
    fn default() -> Self {
        Self { proofs: Vec::new() }
    }
}

#[async_trait]
impl WalletDatabase for MemoryDatabase {
    async fn get_proofs(&self) -> Vec<Proof> {
        self.proofs.clone()
    }

    async fn set_proofs(&mut self, proofs: Vec<Proof>) {
        self.proofs = proofs;
    }

    async fn add_proof(&mut self, proof: Proof) {
        self.proofs.push(proof);
    }
}
