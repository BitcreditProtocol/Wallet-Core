// ----- standard library imports
// ----- extra library imports
use anyhow::Result;
use cashu::{Amount, Proof, amount};
use tracing::{error, info, warn};
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

        // proofs[0].keyset_id
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

        return Ok(token.to_v3_string());
    }
}

impl<DB: WalletDatabase + KeysetDatabase> SwapProofs for Wallet<CreditWallet, DB> {
    async fn swap_proofs_amount(
        &self,
        proofs: Vec<Proof>,
        amounts: Vec<Amount>,
    ) -> Result<Vec<Proof>> {
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

        let counter = self.db.get_count(keyset_id).await.unwrap_or(0);
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

        let response = wdc.swap(swap_request).await?;

        let secrets = premint_secrets
            .iter()
            .map(|b| b.secret.clone())
            .collect::<Vec<_>>();
        let rs = premint_secrets
            .iter()
            .map(|b| b.r.clone())
            .collect::<Vec<_>>();

        let proofs = cashu::dhke::construct_proofs(response.signatures, rs, secrets, &keys.keys)?;

        let _ = self
            .db
            .increase_count(keyset_id, proofs.len() as u32)
            .await?;

        Ok(proofs)
    }
}
