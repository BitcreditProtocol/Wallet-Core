// ----- standard library imports
// ----- extra library imports
use tracing::warn;
// ----- local modules
use super::types::SwapProofs;
use super::utils;
use super::wallet::*;
use crate::db::KeysetDatabase;
use crate::db::WalletDatabase;
use crate::mint::{Connector, MintConnector};
// ----- end imports

// TODO async trait

impl<T: WalletType, DB: WalletDatabase + KeysetDatabase> Wallet<T, DB>
where
    Connector<T>: MintConnector,
    Wallet<T, DB>: SwapProofs,
{
    pub async fn get_balance(&self) -> anyhow::Result<u64> {
        let proofs = self.db.get_active_proofs().await?;
        let sum = proofs.iter().map(|p| u64::from(p.amount)).sum();
        Ok(sum)
    }

    // This is currently inefficient as we always swap, which is okay for WDC as there are no fees
    pub async fn split(&self, amount: u64) -> anyhow::Result<()> {
        let balance = self.get_balance().await?;
        if amount > balance {
            warn!("Requested amount to split is more than balance, cancelling split");
            anyhow::bail!("Requested amount to split is more than balance");
        }
        let base_amounts = cashu::Amount::from(amount).split();
        let change = cashu::Amount::from(balance - amount).split();
        let amounts: Vec<cashu::Amount> =
            base_amounts.into_iter().chain(change.into_iter()).collect();

        if let Ok(proofs) = self.db.get_active_proofs().await {
            if let Ok(new_proofs) = self.swap_proofs_amount(proofs.clone(), amounts).await {
                // set old proofs as spent
                for p in &proofs {
                    self.db.deactivate_proof(p.clone()).await?;
                }

                for p in new_proofs {
                    self.db.add_proof(p).await?;
                }
            } else {
                warn!("Error ocurred when splitting");
            }
        }
        Ok(())
    }

    pub async fn import_token_v3(&self, token: String) -> anyhow::Result<()> {
        if let Ok(token) = token.parse::<cashu::nut00::TokenV3>() {
            let amounts = token
                .proofs()
                .iter()
                .map(|x| x.amount)
                .collect::<Vec<cashu::Amount>>();

            if let Ok(new_proofs) = self.swap_proofs_amount(token.proofs(), amounts).await {
                for p in new_proofs {
                    self.db.add_proof(p).await?;
                }
            }
        }
        Ok(())
    }
    pub async fn send_proofs_for(&self, amount: u64) -> anyhow::Result<String> {
        let proofs = self.db.get_active_proofs().await?;

        if let Some(selected_proofs) = utils::select_proofs_for_amount(&proofs, amount) {
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

            // Mark the proofs we send as a token as spent
            for p in &selected_proofs {
                self.db.deactivate_proof(p.clone()).await?;
            }

            return Ok(token.to_v3_string());
        }
        warn!("Could not select subset of proofs to send");
        Err(anyhow::anyhow!("Could not select subset of proofs to send"))
    }
}
