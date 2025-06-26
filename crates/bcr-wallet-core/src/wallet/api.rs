// ----- standard library imports
use std::str::FromStr;
// ----- extra library imports
use cashu::{Amount, CheckStateRequest, CurrencyUnit, Proof, ProofsMethods, amount};
use cdk::wallet::MintConnector;
use tracing::{error, warn};
// ----- local modules
use super::types::SwapProofs;
use super::{utils, wallet::*};
use crate::db::{KeysetDatabase, WalletDatabase};
use bcr_wallet_lib::wallet::{Token, TokenOperations};

// ----- end imports

impl<T, DB, C> Wallet<T, DB, C>
where
    T: WalletType,
    DB: WalletDatabase + KeysetDatabase,
    C: MintConnector,
    Wallet<T, DB, C>: SwapProofs,
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

    // Invalidates DB proof status and overrides with mint status
    // Need to swap afterward as pending proof can be marked as active
    // TODO, improve criteria for updating pending proofs (older than 1day?)
    pub async fn recheck(&self) -> anyhow::Result<()> {
        let active_proofs = self.db.get_active_proofs().await?;
        let pending_proofs = self.db.get_pending_proofs().await?;
        let non_spent: Vec<Proof> = active_proofs
            .into_iter()
            .chain(pending_proofs.into_iter())
            .collect();

        let ys: Vec<cashu::PublicKey> = non_spent.iter().map(|p| p.y().unwrap()).collect();
        let states = self
            .connector
            .post_check_state(CheckStateRequest { ys })
            .await?
            .states;

        for (state, proof) in states.iter().zip(non_spent.iter()) {
            if state.state != cashu::nut07::State::Unspent {
                if let Err(e) = self.db.mark_spent(proof.clone()).await {
                    warn!(c=?proof.c, amount=?proof.amount, "Failed to mark proof as spent: {}", e);
                }
            }
            if state.state == cashu::nut07::State::Unspent {
                if let Err(e) = self.db.mark_unspent(proof.clone()).await {
                    warn!(c=?proof.c, amount=?proof.amount, "Failed to mark proof as unspent: {}", e);
                }
            }
        }

        Ok(())
    }

    pub async fn restore(&self) -> anyhow::Result<()> {
        let keysets = self.connector.get_mint_keysets().await?;

        let keyset_ids: Vec<cashu::Id> = keysets.keysets.iter().map(|ks| ks.id).collect();

        let mut restored_value = cashu::Amount::ZERO;

        for kid in keyset_ids {
            tracing::debug!(kid=?kid,"Restore");
            let keyset = self.connector.get_mint_keyset(kid).await?;
            let keys = keyset.keys.clone();
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

                let response = self.connector.post_restore(restore_request).await?;

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
                    .post_check_state(CheckStateRequest { ys: ys.clone() })
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

    pub async fn import_token(&self, token: String) -> anyhow::Result<()> {
        let token =
            Token::from_str(&token).map_err(|e| anyhow::anyhow!("Failed to parse token: {}", e))?;

        if token.mint_url() == self.mint_url
            && token.unit().to_string().to_lowercase() == "crsat"
            && self.unit == CurrencyUnit::Sat
        {
            // TODO Improve rules
            // Allow CRSAT -> SAT
        } else if token.mint_url() != self.mint_url || token.unit() != self.unit {
            tracing::error!( token_mint = ?token.mint_url(), token_unit = ?token.unit(),
                            wallet_mint = ?self.mint_url,
                            wallet_unit = ?self.unit,
                            "Token mint_url or unit does not match wallet" );
            return Err(anyhow::anyhow!(
                "Token mint_url or unit does not match wallet"
            ));
        }

        self.import_proofs(token.proofs()).await?;
        Ok(())
    }

    pub async fn send_proofs_for(&self, amount: u64) -> anyhow::Result<String> {
        let proofs = self.db.get_active_proofs().await?;

        if let Some(selected_proofs) = utils::select_proofs_for_amount(&proofs, amount) {
            let mut selected_cs = std::collections::HashSet::new();
            for p in &selected_proofs {
                selected_cs.insert(p.c);
            }

            let token = self.proofs_to_token(selected_proofs.clone(), None);

            // Mark the proofs we send as a token as spent
            for p in &selected_proofs {
                self.db.mark_pending(p.clone()).await?;
            }

            return Ok(token.to_string());
        }
        warn!("Could not select subset of proofs to send");
        Err(anyhow::anyhow!("Could not select subset of proofs to send"))
    }

    pub async fn perform_swap(
        &self,
        proofs: Vec<Proof>,
        amounts: Vec<Amount>,
        keyset_id: cashu::Id,
    ) -> anyhow::Result<Vec<Proof>> {
        let wdc = &self.connector;

        if proofs.is_empty() {
            warn!("No proofs provided");
            return Err(anyhow::anyhow!("No proofs provided"));
        }

        let mut total_proofs = Amount::from(0);
        let mut total_amounts = Amount::from(0);
        for p in &proofs {
            total_proofs = total_proofs
                .checked_add(p.amount)
                .ok_or(anyhow::anyhow!("Overflow"))?;
        }
        for a in &amounts {
            total_amounts = total_amounts
                .checked_add(*a)
                .ok_or(anyhow::anyhow!("Overflow"))?;
        }
        if total_proofs != total_amounts {
            error!("Proofs and amounts do not match");
            return Err(anyhow::anyhow!("Proofs and amounts do not match"));
        }

        let keyset = wdc.get_mint_keyset(keyset_id).await?;

        let counter = self.db.get_count(keyset_id).await.unwrap_or(0);
        let target = amount::SplitTarget::Values(amounts);
        let premint_secrets = cashu::PreMintSecrets::from_xpriv(
            keyset_id,
            counter,
            self.xpriv,
            total_proofs,
            &target,
        )?
        .secrets;

        let bs = premint_secrets
            .iter()
            .map(|b| b.blinded_message.clone())
            .collect::<Vec<_>>();

        let _ = self.db.increase_count(keyset_id, bs.len() as u32).await?;

        let swap_request = cashu::nut03::SwapRequest::new(proofs, bs);

        let response = wdc.post_swap(swap_request).await?;

        let secrets = premint_secrets
            .iter()
            .map(|b| b.secret.clone())
            .collect::<Vec<_>>();
        let rs = premint_secrets
            .iter()
            .map(|b| b.r.clone())
            .collect::<Vec<_>>();

        let proofs = cashu::dhke::construct_proofs(response.signatures, rs, secrets, &keyset.keys)?;

        Ok(proofs)
    }
}
