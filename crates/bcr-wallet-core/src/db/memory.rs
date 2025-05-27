use crate::db::WalletDatabase;
use cashu::Proof;

pub struct MemoryDatabase {
    proofs: Vec<Proof>,
}

impl Default for MemoryDatabase {
    fn default() -> Self {
        Self { proofs: Vec::new() }
    }
}

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
