use async_trait::async_trait;
use cashu::Proof;

#[async_trait]
pub trait WalletDatabase {
    async fn get_proofs(&self) -> Vec<Proof>;
    async fn set_proofs(&mut self, proofs: Vec<Proof>);
    async fn add_proof(&mut self, proof: Proof);
}
