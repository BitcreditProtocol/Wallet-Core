// ----- standard library imports
// ----- extra library imports
use anyhow::Result;
use cashu::{Amount, Proof};
// ----- local modules
// ----- end imports

pub trait SwapProofs {
    async fn swap_proofs_amount(
        &self,
        proofs: Vec<Proof>,
        amounts: Vec<Amount>,
    ) -> Result<Vec<Proof>>;
    async fn import_proofs(&self, proofs: Vec<Proof>) -> Result<()>;
}
