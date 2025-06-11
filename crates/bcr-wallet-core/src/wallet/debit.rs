// ----- standard library imports
// ----- extra library imports
use anyhow::{Result, anyhow};
use cashu::{Amount, Proof};
use tracing::debug;
// ----- local modules
use super::types::SwapProofs;
use super::wallet::*;
use crate::db::{KeysetDatabase, WalletDatabase};
use crate::mint::MintConnector;
use bcr_wallet_lib::wallet::Token;
// ----- end imports

impl<DB: WalletDatabase + KeysetDatabase> SwapProofs for Wallet<DebitWallet, DB> {
    fn proofs_to_token(&self, proofs: Vec<Proof>, memo: Option<String>) -> Token {
        Token::new_debit(self.mint_url.clone(), self.unit.clone(), memo, proofs)
    }
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
        let mut total = Amount::from(0);
        for p in &proofs {
            total = total.checked_add(p.amount).ok_or(anyhow!("Overflow"))?;
        }

        if let Ok(new_proofs) = self.swap_proofs_amount(proofs, total.split()).await {
            for p in new_proofs {
                self.db.add_proof(p).await?;
            }
        } else {
            tracing::error!(amounts=?total.split(), "Failed to swap debit proofs");
            return Err(anyhow::anyhow!("Failed to swap proofs"));
        }
        Ok(())
    }
}
