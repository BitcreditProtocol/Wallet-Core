// ----- standard library imports
// ----- extra library imports
use anyhow::Result;
use cashu::{Amount, Proof};
use tracing::{debug, error};
// ----- local modules
use super::types::SwapProofs;
use super::wallet::*;
use crate::db::{KeysetDatabase, WalletDatabase};
use crate::mint::MintConnector;
// ----- end imports

impl<DB> Wallet<CreditWallet, DB>
where
    DB: WalletDatabase + KeysetDatabase,
{
    pub async fn redeem_first_inactive(&self) -> anyhow::Result<String> {
        let proofs = self.db.get_active_proofs().await?;

        let keysets = self.connector.list_keysets().await?;

        let inactive_keysets = keysets
            .keysets
            .into_iter()
            .filter(|ks| !ks.active)
            .map(|ks| ks.id)
            .collect::<std::collections::HashSet<_>>();

        let mut inactive_keyset = None;
        for p in &proofs {
            if inactive_keysets.contains(&p.keyset_id) {
                inactive_keyset = Some(p.keyset_id);
                break;
            }
        }
        if inactive_keyset.is_none() {
            return Err(anyhow::anyhow!("No inactive keyset found"));
        }
        let inactive_keyset = inactive_keyset.unwrap();

        // Get all proofs that belong to this keyset as selected proofs
        let mut selected_proofs = Vec::new();
        for p in &proofs {
            if p.keyset_id == inactive_keyset {
                selected_proofs.push(p.clone());
            }
        }
        if selected_proofs.is_empty() {
            return Err(anyhow::anyhow!("No proofs found for inactive keyset"));
        }

        let mut selected_cs = std::collections::HashSet::new();
        for p in &selected_proofs {
            selected_cs.insert(p.c);
        }
        let token = cashu::nut00::Token::new(
            self.mint_url.clone(),
            selected_proofs.clone(),
            None,
            self.unit.clone(),
        );

        for p in &selected_proofs {
            self.db.mark_pending(p.clone()).await?;
        }

        Ok(token.to_v3_string())
    }
}

impl<DB: WalletDatabase + KeysetDatabase> SwapProofs for Wallet<CreditWallet, DB> {
    async fn swap_proofs_amount(
        &self,
        proofs: Vec<Proof>,
        amounts: Vec<Amount>,
    ) -> Result<Vec<Proof>> {
        // Swap to the same keyset
        let keyset_id = proofs[0].keyset_id;

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
}
