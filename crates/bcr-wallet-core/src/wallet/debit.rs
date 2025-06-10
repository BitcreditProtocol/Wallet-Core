// ----- standard library imports
// ----- extra library imports
use anyhow::Result;
use cashu::{Amount, Proof};
use tracing::debug;
// ----- local modules
use super::types::SwapProofs;
use super::wallet::*;
use crate::db::{KeysetDatabase, WalletDatabase};
use crate::mint::MintConnector;
// ----- end imports

impl<DB: WalletDatabase + KeysetDatabase> SwapProofs for Wallet<DebitWallet, DB> {
    async fn swap_proofs_amount(
        &self,
        proofs: Vec<Proof>,
        amounts: Vec<Amount>,
    ) -> Result<Vec<Proof>> {
        let keysets = self.connector.list_keysets().await?;
        let unit = self.unit.clone();
        // Swap to an active keyset
        let keyset = keysets
            .keysets
            .iter()
            .find(|k| k.unit == unit && k.active)
            .ok_or(anyhow::anyhow!("No active keyset found"))?;

        debug!(keyset_id = ?keyset.id, amounts=?amounts,"Swapping debit proofs");
        self.perform_swap(proofs, amounts, keyset.id).await
    }

    async fn import_proofs(&self, proofs: Vec<Proof>) -> anyhow::Result<()> {
        let amounts = proofs
            .iter()
            .map(|x| x.amount)
            .collect::<Vec<cashu::Amount>>();

        if let Ok(new_proofs) = self.swap_proofs_amount(proofs, amounts).await {
            for p in new_proofs {
                self.db.add_proof(p).await?;
            }
        };
        Ok(())
    }
}
