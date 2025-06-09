// ----- standard library imports
// ----- extra library imports
use cashu::{CheckStateRequest, ProofsMethods};
use tracing::warn;
// ----- local modules
use super::types::SwapProofs;
use super::{utils, wallet::*};
use crate::db::{KeysetDatabase, WalletDatabase};
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
                    self.db.mark_spent(p.clone()).await?;
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

    /// Scan the wallet database
    /// update whether proofs are spent or not by asking the mint
    /// update whether pending proofs are spent and mark as unspent
    pub async fn recheck(&self) -> anyhow::Result<()> {
        let proofs = self.db.get_active_proofs().await?;
        let ys: Vec<cashu::PublicKey> = proofs.iter().map(|p| p.y().unwrap()).collect();
        let states = self
            .connector
            .checkstate(CheckStateRequest { ys })
            .await?
            .states;

        for (state, proof) in states.iter().zip(proofs.iter()) {
            if state.state != cashu::nut07::State::Unspent {
                let _ = self.db.mark_spent(proof.clone()).await;
            }
        }

        let proofs = self.db.get_pending_proofs().await?;
        let ys: Vec<cashu::PublicKey> = proofs.iter().map(|p| p.y().unwrap()).collect();
        let states = self
            .connector
            .checkstate(CheckStateRequest { ys })
            .await?
            .states;

        for (state, proof) in states.iter().zip(proofs.iter()) {
            if state.state == cashu::nut07::State::Unspent {
                if let Err(e) = self.db.mark_unspent(proof.clone()).await {
                    warn!("Failed to mark proof as unspent: {}", e);
                }
            }
        }

        Ok(())
    }

    pub async fn restore(&self) -> anyhow::Result<()> {
        let keysets = self.connector.list_keysets().await?;

        let keyset_ids: Vec<cashu::Id> = keysets.keysets.iter().map(|ks| ks.id).collect();

        let mut restored_value = cashu::Amount::ZERO;

        for kid in keyset_ids {
            tracing::debug!(kid=?kid,"Restore");
            let resp = self.connector.list_keys(kid).await?;
            let keys = resp.keysets.first();
            if keys.is_none() {
                warn!("No keys found for keyset {}", kid);
                continue;
            }
            let keys = keys.unwrap().keys.clone();
            let mut fruitless_attempts = 0;
            let mut key_counter = 0;

            // Nut13 describes restoring proofs in batches of 100,
            // If there are 3 batches where nothing is restored, we stop.
            // We use 2 as Wildcat mints have many more keysets
            while fruitless_attempts < 2 {
                let premint_secrets = cashu::PreMintSecrets::restore_batch(
                    kid,
                    self.xpriv,
                    key_counter,
                    key_counter + 100,
                )?;

                let restore_request = cashu::RestoreRequest {
                    outputs: premint_secrets.blinded_messages(),
                };

                let response = self.connector.restore(restore_request).await?;

                tracing::info!(sigs=?response.signatures,"Restored signatures");

                if response.signatures.is_empty() {
                    fruitless_attempts += 1;
                    key_counter += 100;
                    continue;
                }

                let premint_secrets: Vec<&cashu::PreMint> = premint_secrets
                    .secrets
                    .iter()
                    .filter(|p| response.outputs.contains(&p.blinded_message))
                    .collect();

                if response.outputs.len() != premint_secrets.len() {
                    warn!(
                        "Mismatch between response outputs ({}) and filtered premint secrets ({})",
                        response.outputs.len(),
                        premint_secrets.len()
                    );
                    continue;
                }

                let proofs = cashu::dhke::construct_proofs(
                    response.signatures,
                    premint_secrets.iter().map(|p| p.r.clone()).collect(),
                    premint_secrets.iter().map(|p| p.secret.clone()).collect(),
                    &keys,
                )?;

                self.db.increase_count(kid, proofs.len() as u32).await?;

                let ys = proofs
                    .iter()
                    .map(|p| p.y())
                    .collect::<Result<Vec<_>, _>>()?;

                let states = self
                    .connector
                    .checkstate(CheckStateRequest { ys: ys.clone() })
                    .await?
                    .states;

                let unspent_proofs: Vec<cashu::Proof> = proofs
                    .iter()
                    .zip(states)
                    .filter(|(_, state)| !state.state.eq(&cashu::State::Spent))
                    .map(|(p, _)| p)
                    .cloned()
                    .collect();

                for p in unspent_proofs.iter() {
                    let _ = self.db.add_proof(p.clone()).await;
                }

                restored_value += unspent_proofs.total_amount()?;

                fruitless_attempts = 0;
                key_counter += 100;
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
                self.db.mark_pending(p.clone()).await?;
            }

            return Ok(token.to_v3_string());
        }
        warn!("Could not select subset of proofs to send");
        Err(anyhow::anyhow!("Could not select subset of proofs to send"))
    }
}
