// ----- standard library imports
// ----- extra library imports
use anyhow::Result;
use cashu::{Amount, amount};
use tracing::{error, info, warn};
// ----- local modules
use super::utils;
use super::wallet::*;
use crate::db::KeysetDatabase;
use crate::db::WalletDatabase;
use crate::mint::{Connector, MintConnector};
// ----- end imports

// TODO async trait
pub trait SwapProofs {
    async fn swap_proofs_amount(
        &self,
        proofs: Vec<cashu::Proof>,
        amounts: Vec<cashu::Amount>,
    ) -> Result<Vec<cashu::Proof>>;
}

impl<DB: WalletDatabase + KeysetDatabase> SwapProofs for Wallet<CreditWallet, DB> {
    async fn swap_proofs_amount(
        &self,
        proofs: Vec<cashu::Proof>,
        amounts: Vec<cashu::Amount>,
    ) -> Result<Vec<cashu::Proof>> {
        let wdc = &self.connector;
        if proofs.is_empty() {
            warn!("No proofs provided");
            return Err(anyhow::anyhow!("No proofs provided"));
        };
        let mut total_proofs = 0u64;
        let mut total_amounts = 0u64;
        for p in &proofs {
            total_proofs += u64::from(p.amount);
        }
        for a in &amounts {
            total_amounts += u64::from(*a);
        }
        if total_proofs != total_amounts {
            error!("Proofs and amounts do not match");
            return Err(anyhow::anyhow!("Proofs and amounts do not match"));
        }

        let keyset_id = proofs[0].keyset_id;
        info!(keyset_id=?keyset_id, "Swap");

        let keys = wdc.list_keys(keyset_id).await?;

        let keys = keys
            .keysets
            .first()
            .ok_or(anyhow::anyhow!("No keys found"))?;

        info!("Pre counter");
        let counter = self.db.get_count(keyset_id).await.unwrap_or(0);
        info!("Counter: {}", counter);
        let target = amount::SplitTarget::Values(amounts);
        let premint_secrets = cashu::PreMintSecrets::from_xpriv(
            keyset_id,
            counter,
            self.xpriv,
            Amount::from(total_proofs),
            &target,
        )?
        .secrets;

        let bs = premint_secrets
            .iter()
            .map(|b| b.blinded_message.clone())
            .collect::<Vec<_>>();
        let swap_request = cashu::nut03::SwapRequest::new(proofs, bs);

        info!(amount = total_proofs, "Swapping");
        let response = wdc.swap(swap_request).await?;

        let secrets = premint_secrets
            .iter()
            .map(|b| b.secret.clone())
            .collect::<Vec<_>>();
        let rs = premint_secrets
            .iter()
            .map(|b| b.r.clone())
            .collect::<Vec<_>>();

        info!("Building proofs");
        let proofs = cashu::dhke::construct_proofs(response.signatures, rs, secrets, &keys.keys)?;

        let _ = self
            .db
            .increase_count(keyset_id, proofs.len() as u32)
            .await?;

        info!("Returning Proofs");
        Ok(proofs)
    }
}

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
