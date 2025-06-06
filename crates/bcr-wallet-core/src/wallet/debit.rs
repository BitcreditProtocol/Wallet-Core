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

// TODO there is a lot of similarity with the credit swap that can be refactored into a shared function
// Here the difference is we swap to an active keyset of the mint and not necessarily the same one
impl<DB: WalletDatabase + KeysetDatabase> SwapProofs for Wallet<DebitWallet, DB> {
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

        // Find active keyset for our unit
        let keysets = self.connector.list_keysets().await?;
        let unit = self.unit.clone();
        let keyset = keysets
            .keysets
            .iter()
            .find(|k| k.unit == unit && k.active)
            .ok_or(anyhow::anyhow!("No active keyset found"))?;
        let keyset_id = keyset.id;

        info!(keyset_id=?keyset_id, "Debit Swap");

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
