// ----- standard library imports
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
// ----- extra library imports
use async_trait::async_trait;
use bitcoin::bip32 as btc32;
use cashu::{
    Amount, CurrencyUnit, KeySet, KeySetInfo, amount::SplitTarget, nut00 as cdk00, nut01 as cdk01,
};
use cdk::wallet::MintConnector;
use futures::future::JoinAll;
use uuid::Uuid;
// ----- local imports
use crate::{
    error::{Error, Result},
    pocket::*,
    restore,
    types::PocketSendSummary,
    wallet,
};

// ----- end imports

///////////////////////////////////////////// debit pocket
pub struct Pocket {
    pub unit: cashu::CurrencyUnit,
    pub db: Arc<dyn PocketRepository>,
    pub xpriv: btc32::Xpriv,

    current_send: Mutex<Option<SendReference>>,
}

impl Pocket {
    pub fn new(unit: CurrencyUnit, db: Arc<dyn PocketRepository>, xpriv: btc32::Xpriv) -> Self {
        Self {
            unit,
            db,
            xpriv,
            current_send: Mutex::new(None),
        }
    }

    fn find_active_keysetid(&self, keysets_info: &[KeySetInfo]) -> Result<cashu::KeySetInfo> {
        let active_info = keysets_info
            .iter()
            .find(|info| info.unit == self.unit && info.active && info.input_fee_ppk == 0);
        let Some(active_info) = active_info else {
            return Err(Error::NoActiveKeyset);
        };
        Ok(active_info.clone())
    }
    async fn find_active_keyset(
        &self,
        keysets_info: &[KeySetInfo],
        client: &dyn MintConnector,
    ) -> Result<(KeySetInfo, KeySet)> {
        let active_info = self.find_active_keysetid(keysets_info)?;
        let active_keyset = client.get_mint_keyset(active_info.id).await?;
        Ok((active_info, active_keyset))
    }

    async fn digest_proofs(
        &self,
        client: &dyn MintConnector,
        keysets_info: &[KeySetInfo],
        inputs: HashMap<cdk01::PublicKey, cdk00::Proof>,
    ) -> Result<(Amount, Vec<cdk01::PublicKey>)> {
        if inputs.is_empty() {
            tracing::warn!("DbPocket::digest_proofs: empty inputs");
            return Ok((Amount::ZERO, Vec::new()));
        }
        let (ys, proofs): (Vec<cdk01::PublicKey>, Vec<cdk00::Proof>) = inputs.into_iter().unzip();
        let (active_info, active_keyset) = self.find_active_keyset(keysets_info, client).await?;
        let counter = self.db.counter(active_info.id).await?;
        let total_amount = proofs.iter().fold(Amount::ZERO, |acc, p| acc + p.amount);
        let premint_secrets = cdk00::PreMintSecrets::from_xpriv(
            active_info.id,
            counter,
            self.xpriv,
            total_amount,
            &SplitTarget::None,
        )?;
        self.db
            .increment_counter(active_info.id, counter, premint_secrets.len() as u32)
            .await?;
        let premints = HashMap::from([(active_info.id, premint_secrets)]);
        let keysets = HashMap::from([(active_info.id, active_keyset)]);
        let cashed_in = swap(
            self.unit.clone(),
            proofs,
            premints,
            keysets,
            client,
            self.db.as_ref(),
        )
        .await?;
        Ok((cashed_in, ys))
    }

    async fn compute_send_costs(
        &self,
        target: Amount,
        keysets_info: &[KeySetInfo],
    ) -> Result<(PocketSendSummary, SendReference)> {
        let proofs = self.db.list_unspent().await?;
        let infos = collect_keyset_infos_from_proofs(proofs.values(), keysets_info)?;
        let ys = group_ys_by_keyset_id(proofs.iter());
        let mut kids: Vec<cashu::Id> = Vec::with_capacity(infos.len());
        for (kid, info) in infos.iter() {
            if info.unit == self.unit && info.input_fee_ppk == 0 {
                kids.push(*kid);
            }
        }
        let mut current_amount = Amount::ZERO;
        let pocket_summary = PocketSendSummary::new();
        let mut send_ref = SendReference {
            rid: pocket_summary.request_id,
            ..Default::default()
        };
        for kid in kids {
            let kid_ys = ys.get(&kid).cloned().unwrap_or_default();
            for y in kid_ys {
                let proof = proofs.get(&y).expect("proof should be here");
                if current_amount + proof.amount > target {
                    send_ref.swap_proof = Some((target - current_amount, y));
                    return Ok((pocket_summary, send_ref));
                } else if current_amount + proof.amount == target {
                    send_ref.send_proofs.push(y);
                    return Ok((pocket_summary, send_ref));
                } else {
                    send_ref.send_proofs.push(y);
                    current_amount += proof.amount;
                }
            }
        }
        Err(Error::InsufficientFunds)
    }
}

#[async_trait(?Send)]
impl wallet::Pocket for Pocket {
    fn unit(&self) -> CurrencyUnit {
        self.unit.clone()
    }

    async fn balance(&self) -> Result<cashu::Amount> {
        let proofs = self.db.list_unspent().await?;
        let total = proofs
            .into_iter()
            .fold(Amount::ZERO, |acc, (_, proof)| acc + proof.amount);
        Ok(total)
    }

    async fn receive_proofs(
        &self,
        client: &dyn MintConnector,
        keysets_info: &[KeySetInfo],
        inputs: Vec<cdk00::Proof>,
    ) -> Result<(Amount, Vec<cdk01::PublicKey>)> {
        // storing proofs in pending state
        let mut proofs: HashMap<cdk01::PublicKey, cdk00::Proof> = HashMap::new();
        for input in inputs.into_iter() {
            let y = self.db.store_pendingspent(input.clone()).await?;
            proofs.insert(y, input);
        }
        self.digest_proofs(client, keysets_info, proofs).await
    }

    async fn prepare_send(
        &self,
        target: Amount,
        keysets_info: &[KeySetInfo],
    ) -> Result<PocketSendSummary> {
        let (summary, send_ref) = self.compute_send_costs(target, keysets_info).await?;
        *self.current_send.lock().unwrap() = Some(send_ref);
        Ok(summary)
    }

    async fn send_proofs(
        &self,
        rid: Uuid,
        keysets_info: &[KeySetInfo],
        client: &dyn MintConnector,
    ) -> Result<HashMap<cdk01::PublicKey, cdk00::Proof>> {
        let send_ref = {
            let mut locked = self.current_send.lock().unwrap();
            if locked.is_none() {
                return Err(Error::NoPrepareSendRef(rid));
            }
            if locked.as_ref().unwrap().rid != rid {
                return Err(Error::NoPrepareSendRef(rid));
            }
            locked.take().unwrap()
        };
        let info = self.find_active_keysetid(keysets_info)?;
        let sending_proofs = send_proofs(
            send_ref,
            self.xpriv,
            self.db.as_ref(),
            client,
            Some(info.id),
        )
        .await?;

        Ok(sending_proofs)
    }

    async fn clean_local_proofs(
        &self,
        client: &dyn MintConnector,
    ) -> Result<Vec<cdk01::PublicKey>> {
        let cleaned_ys = clean_local_proofs(self.db.as_ref(), client).await?;
        Ok(cleaned_ys)
    }

    async fn restore_local_proofs(
        &self,
        keysets_info: &[KeySetInfo],
        client: &dyn MintConnector,
    ) -> Result<usize> {
        let kids = keysets_info.iter().filter_map(|info| {
            if info.unit == self.unit {
                Some(info.id)
            } else {
                None
            }
        });
        let joined: JoinAll<_> = kids
            .into_iter()
            .map(|kid| restore::restore_keysetid(self.xpriv, kid, client, self.db.as_ref()))
            .collect();
        let mut total_recovered = 0;
        for task in joined.await {
            total_recovered += task?;
        }
        Ok(total_recovered)
    }
}

#[async_trait(?Send)]
impl wallet::DebitPocket for Pocket {
    async fn reclaim_proofs(
        &self,
        keysets_info: &[KeySetInfo],
        client: &dyn MintConnector,
    ) -> Result<Amount> {
        let pendings = self.db.list_pending().await?;
        let pendings_len = pendings.len();
        let (reclaimed, _) = self.digest_proofs(client, keysets_info, pendings).await?;
        tracing::debug!(
            "DbPocket::reclaim_proofs: pendings: {pendings_len} reclaimed: {reclaimed}"
        );
        Ok(reclaimed)
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use crate::{utils::tests::MockMintConnector, wallet::Pocket};
    use bcr_wdc_utils::{keys::test_utils as keys_test, signatures::test_utils as signatures_test};
    use mockall::predicate::*;

    fn pocket(db: Arc<dyn PocketRepository>) -> super::Pocket {
        let unit = CurrencyUnit::Sat;
        let seed = [0u8; 32];
        let xpriv = btc32::Xpriv::new_master(bitcoin::Network::Regtest, &seed).unwrap();
        super::Pocket::new(unit, db, xpriv)
    }
    #[tokio::test]
    async fn debit_receive_proofs() {
        let (info, keyset) = keys_test::generate_keyset();
        let kid = info.id;
        let k_infos = vec![KeySetInfo::from(info)];
        let amounts = [Amount::from(8u64), Amount::from(16u64)];
        let proofs = signatures_test::generate_proofs(&keyset, &amounts);

        let mut db = MockPocketRepository::new();
        let mut connector = MockMintConnector::new();
        let cloned_keyset = keyset.clone();
        connector
            .expect_get_mint_keyset()
            .times(1)
            .with(eq(kid))
            .returning(move |_| Ok(KeySet::from(cloned_keyset.clone())));
        db.expect_store_pendingspent().times(2).returning(|p| {
            let y = p.y().expect("Hash to curve should not fail");
            Ok(y)
        });
        db.expect_counter()
            .times(1)
            .with(eq(kid))
            .returning(|_| Ok(0));
        db.expect_increment_counter()
            .times(1)
            .with(eq(kid), eq(0), eq(2))
            .returning(|_, _, _| Ok(()));
        connector
            .expect_post_swap()
            .times(1)
            .returning(move |request| {
                let amounts = request
                    .outputs()
                    .iter()
                    .map(|b| b.amount)
                    .collect::<Vec<_>>();
                let signatures = signatures_test::generate_signatures(&keyset, &amounts);
                let response = cdk03::SwapResponse { signatures };
                Ok(response)
            });
        db.expect_store_new().times(2).returning(|p| {
            let y = p.y().expect("Hash to curve should not fail");
            Ok(y)
        });

        let pocket = pocket(Arc::new(db));

        let (cashed, _) = pocket
            .receive_proofs(&connector, &k_infos, proofs)
            .await
            .unwrap();
        assert_eq!(cashed, Amount::from(24u64));
    }
}
