use cashu::{CurrencyUnit, MintUrl};

use crate::db::WalletDatabase;
use anyhow::{Result, anyhow};
use tracing::{info, warn};

use super::connector::{Connector, MintConnector};
use super::utils;
use super::wallet::*;

use std::str::FromStr;

pub trait SwapProofs {
    async fn swap_proofs_amount(
        &self,
        proofs: Vec<cashu::Proof>,
        amounts: Vec<cashu::Amount>,
    ) -> Result<Vec<cashu::Proof>>;
}

impl<DB: WalletDatabase> SwapProofs for Wallet<CreditWallet, DB> {
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
        assert_eq!(
            total_proofs, total_amounts,
            "Proofs and amounts do not match"
        );

        let keyset_id = proofs[0].keyset_id;

        // let mint = mint::Service::<mint::MintUserService>::new("http://localhost:4343".into());
        let keys = wdc.list_keys(keyset_id).await?;

        let keys = keys
            .keysets
            .first()
            .ok_or(anyhow::anyhow!("No keys found"))?;

        let new_blinds = utils::generate_blinds(keyset_id, &amounts);
        let bs = new_blinds.iter().map(|b| b.0.clone()).collect::<Vec<_>>();
        let swap_request = cashu::nut03::SwapRequest::new(proofs, bs);

        let response = wdc.swap(swap_request).await?;

        let secrets = new_blinds.iter().map(|b| b.1.clone()).collect::<Vec<_>>();
        let rs = new_blinds.iter().map(|b| b.2.clone()).collect::<Vec<_>>();
        let proofs = cashu::dhke::construct_proofs(response.signatures, rs, secrets, &keys.keys)?;

        Ok(proofs)
    }
}

impl<T: WalletType, DB: WalletDatabase> Wallet<T, DB>
where
    Connector<T>: MintConnector,
    Wallet<T, DB>: SwapProofs,
{
    pub async fn get_balance(&self) -> u64 {
        let proofs = self.db.get_proofs().await;

        let mut sum = 0_u64;
        for p in &proofs {
            sum += u64::from(p.amount);
        }
        sum
    }

    pub async fn split(&mut self, amount: u64) {
        let balance = self.get_balance().await;
        if amount > balance {
            warn!("Requested amount to split is more than balance, cancelling split");
            return;
        }
        let base_amounts = utils::get_amounts(amount);
        let change = utils::get_amounts(balance - amount);
        // merge two vecs into one
        let amounts: Vec<cashu::Amount> = base_amounts
            .into_iter()
            .chain(change.into_iter())
            .map(|x| cashu::Amount::from(x))
            .collect();

        let proofs = self.db.get_proofs().await;

        if let Ok(new_proofs) = self.swap_proofs_amount(proofs, amounts).await {
            self.db.set_proofs(Vec::new()).await; // clear

            for p in new_proofs {
                self.db.add_proof(p).await;
            }
        } else {
            warn!("Error ocurred when splitting");
        }
    }

    pub async fn import_token_v3(&mut self, token: String) {
        if let Ok(token) = token.parse::<cashu::nut00::TokenV3>() {
            let amounts = token
                .proofs()
                .iter()
                .map(|x| x.amount)
                .collect::<Vec<cashu::Amount>>();

            info!(amounts = ?amounts, "Swapping for new proofs");
            if let Ok(new_proofs) = self.swap_proofs_amount(token.proofs(), amounts).await {
                for p in new_proofs {
                    self.db.add_proof(p).await;
                }
            }
        }
    }
    pub async fn send_proofs_for(&mut self, amount: u64) -> String {
        let proofs = self.db.get_proofs().await;

        if let Some(selected_proofs) = utils::select_proofs_for_amount(&proofs, amount) {
            let mut selected_cs = std::collections::HashSet::new();
            for p in &selected_proofs {
                selected_cs.insert(p.c);
            }

            let mint_url = cashu::MintUrl::from_str("http://example.com".into()).unwrap();
            let token = cashu::nut00::Token::new(
                mint_url,
                selected_proofs,
                None,
                cashu::CurrencyUnit::Custom("crsat".into()),
            );

            let mut proofs = proofs;
            proofs.retain(|p| !selected_cs.contains(&p.c));
            self.db.set_proofs(proofs).await;

            return token.to_v3_string();
        } else {
            warn!("Could not select subset of proofs to send");
        }
        "".into()
    }
}
