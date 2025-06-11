use std::collections::HashMap;

// ----- standard library imports
// ----- extra library imports
use anyhow::{Result, anyhow};
use cashu::{Amount, Proof};
use tracing::{debug, error};
// ----- local modules
use super::types::SwapProofs;
use super::wallet::*;
use crate::db::{KeysetDatabase, WalletDatabase};
use crate::mint::MintConnector;
use bcr_wallet_lib::wallet::Token;
// ----- end imports

impl<DB> Wallet<CreditWallet, DB>
where
    DB: WalletDatabase + KeysetDatabase,
{
    pub async fn redeem_inactive(&self) -> anyhow::Result<String> {
        let proofs = self.db.get_active_proofs().await?;

        let keysets = self.connector.list_keysets().await?;

        let inactive_keysets = keysets
            .keysets
            .into_iter()
            .filter(|ks| !ks.active)
            .map(|ks| ks.id)
            .collect::<std::collections::HashSet<_>>();

        let selected_proofs = proofs
            .into_iter()
            .filter(|p| inactive_keysets.contains(&p.keyset_id))
            .collect::<Vec<_>>();

        if selected_proofs.is_empty() {
            error!("No proofs with an inactive keyset");
            return Err(anyhow::anyhow!("No proofs found for inactive keyset"));
        }

        let token = self.proofs_to_token(selected_proofs.clone(), None);

        for p in &selected_proofs {
            self.db.mark_pending(p.clone()).await?;
        }

        Ok(token.to_string())
    }
}

impl<DB: WalletDatabase + KeysetDatabase> SwapProofs for Wallet<CreditWallet, DB> {
    fn proofs_to_token(&self, proofs: Vec<Proof>, memo: Option<String>) -> Token {
        Token::new_credit(self.mint_url.clone(), self.unit.clone(), memo, proofs)
    }
    async fn swap_proofs_amount(
        &self,
        proofs: Vec<Proof>,
        amounts: Vec<Amount>,
    ) -> Result<Vec<Proof>> {
        // Swap to the same keyset
        let keyset_id = proofs.first().ok_or(anyhow!("No proofs found"))?.keyset_id;

        // check that all proofs have the same keyset
        for p in &proofs {
            if p.keyset_id != keyset_id {
                error!("Proofs do not belong to the same keyset");
                return Err(anyhow::anyhow!("Proofs do not belong to the same keyset"));
            }
        }

        debug!(keyset_id = ?keyset_id, amounts=?amounts,"Swapping credit proofs");
        self.perform_swap(proofs, amounts, keyset_id).await
    }
    async fn import_proofs(&self, proofs: Vec<Proof>) -> anyhow::Result<()> {
        // Credit tokens need to handle the edge case where the token we receive
        // contains different keysets - we can only swap to the same keyset

        // Group proofs by keyset_id
        let mut keyset_proofs = HashMap::new();
        for p in proofs {
            keyset_proofs
                .entry(p.keyset_id)
                .or_insert_with(Vec::new)
                .push(p);
        }
        for (_, keyset_proofs) in keyset_proofs {
            let mut total = Amount::from(0);
            for p in &keyset_proofs {
                total = total.checked_add(p.amount).ok_or(anyhow!("Overflow"))?;
            }

            if let Ok(new_proofs) = self.swap_proofs_amount(keyset_proofs, total.split()).await {
                for p in new_proofs {
                    self.db.add_proof(p).await?;
                }
            } else {
                error!(amounts=?total.split(), "Failed to swap credit proofs");
                return Err(anyhow::anyhow!("Failed to swap proofs"));
            }
        }
        Ok(())
    }
}
