use anyhow::Result;
use cashu::{Amount, Proof};

pub trait SwapProofs {
    async fn swap_proofs_amount(
        &self,
        proofs: Vec<Proof>,
        amounts: Vec<Amount>,
    ) -> Result<Vec<Proof>>;
}
