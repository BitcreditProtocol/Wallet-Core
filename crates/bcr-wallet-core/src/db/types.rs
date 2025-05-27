use cashu::Proof;

pub trait WalletDatabase {
    async fn get_proofs(&self) -> Vec<Proof>;
    async fn set_proofs(&mut self, proofs: Vec<Proof>);
    async fn add_proof(&mut self, proof: Proof);
}
