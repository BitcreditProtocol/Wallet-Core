// ----- standard library imports
// ----- extra library imports
use anyhow::Result;
use bcr_wallet_lib::wallet::Token;
use cashu::{Amount, Proof};
// ----- local modules
// ----- end imports

// Sending and receiving proof interface
pub trait SwapProofs {
    async fn swap_proofs_amount(
        &self,
        proofs: Vec<Proof>,
        amounts: Vec<Amount>,
    ) -> Result<Vec<Proof>>;
    async fn import_proofs(&self, proofs: Vec<Proof>) -> Result<()>;
    fn proofs_to_token(&self, proofs: Vec<Proof>, memo: Option<String>) -> Token;
}
