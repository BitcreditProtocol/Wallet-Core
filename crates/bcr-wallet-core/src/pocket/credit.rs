// ----- standard library imports
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
// ----- extra library imports
use anyhow::Error as AnyError;
use async_trait::async_trait;
use bitcoin::bip32 as btc32;
use cashu::{
    Amount, CurrencyUnit, KeySet, KeySetInfo, amount::SplitTarget, nut00 as cdk00, nut01 as cdk01,
    nut07 as cdk07,
};
use cdk::wallet::MintConnector;
use futures::future::JoinAll;
use uuid::Uuid;
// ----- local imports
use crate::{
    error::{Error, Result},
    pocket::*,
    restore,
    types::{PocketSendSummary, RedemptionSummary},
    wallet,
};

// ----- end imports

///////////////////////////////////////////// credit pocket
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
}

impl Pocket {
    fn validate_keysets<'inf>(
        &self,
        keysets_info: &'inf [KeySetInfo],
        inputs: &[cdk00::Proof],
    ) -> Result<HashMap<cashu::Id, &'inf KeySetInfo>> {
        let infos = collect_keyset_infos_from_proofs(inputs.iter(), keysets_info)?;
        for info in infos.values() {
            if info.unit != self.unit {
                return Err(Error::CurrencyUnitMismatch(
                    info.unit.clone(),
                    self.unit.clone(),
                ));
            }
            if !info.active {
                return Err(Error::InactiveKeyset(info.id));
            }
            if info.input_fee_ppk != 0 {
                return Err(Error::Any(AnyError::msg(
                    "mint with fees not supported yet",
                )));
            }
        }
        Ok(infos)
    }

    async fn digest_proofs(
        &self,
        client: &dyn MintConnector,
        infos: HashMap<cashu::Id, &KeySetInfo>,
        inputs: HashMap<cdk01::PublicKey, cdk00::Proof>,
    ) -> Result<(Amount, Vec<cdk01::PublicKey>)> {
        if inputs.is_empty() {
            tracing::warn!("CrPocket::digest_proofs: empty inputs");
            return Ok((Amount::ZERO, Vec::new()));
        }
        let ys = inputs.keys().cloned().collect();
        // reshaping inputs into keyset_id -> proofs
        let mut old_proofs: HashMap<cashu::Id, Vec<cdk00::Proof>> = HashMap::new();
        for (_, proof) in inputs.into_iter() {
            old_proofs
                .entry(proof.keyset_id)
                .and_modify(|v| v.push(proof.clone()))
                .or_insert_with(|| vec![proof]);
        }
        // collecting the keysets first as we dont't want any failure once the swap request
        // has been made
        let mut keysets: HashMap<cashu::Id, KeySet> = HashMap::new();
        for kid in infos.keys() {
            let keyset = client.get_mint_keyset(*kid).await?;
            keysets.insert(*kid, keyset);
        }
        // preparing the premints
        let mut premints: HashMap<cashu::Id, cdk00::PreMintSecrets> = HashMap::new();
        for (kid, proofs) in old_proofs.iter() {
            let total = proofs.iter().fold(Amount::ZERO, |acc, p| acc + p.amount);
            let counter = self.db.counter(*kid).await?;
            let premint = cdk00::PreMintSecrets::from_xpriv(
                *kid,
                counter,
                self.xpriv,
                total,
                &SplitTarget::None,
            )?;
            let increment = premint.len() as u32;
            premints.insert(*kid, premint);
            self.db.increment_counter(*kid, counter, increment).await?;
        }
        let mut proofs_in_request: Vec<cdk00::Proof> = Vec::new();
        for (_, proofs) in old_proofs.into_iter() {
            proofs_in_request.extend(proofs);
        }
        let cashed_in = swap(
            self.unit.clone(),
            proofs_in_request,
            premints,
            keysets,
            client,
            self.db.as_ref(),
        )
        .await?;
        Ok((cashed_in, ys))
    }
}

#[async_trait(?Send)]
impl wallet::Pocket for Pocket {
    fn unit(&self) -> CurrencyUnit {
        self.unit.clone()
    }

    async fn balance(&self) -> Result<Amount> {
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
        let infos = self.validate_keysets(keysets_info, &inputs)?;
        // storing proofs in pending state
        let mut proofs: HashMap<cdk01::PublicKey, cdk00::Proof> = HashMap::new();
        for input in inputs.into_iter() {
            let y = self.db.store_pendingspent(input.clone()).await?;
            proofs.insert(y, input);
        }
        self.digest_proofs(client, infos, proofs).await
    }

    async fn prepare_send(
        &self,
        target: Amount,
        keysets_info: &[KeySetInfo],
    ) -> Result<PocketSendSummary> {
        let proofs = self.db.list_unspent().await?;
        let infos = collect_keyset_infos_from_proofs(proofs.values(), keysets_info)?;
        let ys = group_ys_by_keyset_id(proofs.iter());
        // selecting keysets
        let mut kids: Vec<cashu::Id> = Vec::with_capacity(infos.len());
        for (kid, info) in infos.iter() {
            if info.unit != self.unit || !info.active || info.input_fee_ppk != 0 {
                tracing::warn!(
                    "CrPocket::prepare_send: {kid} discarded {}, {}, {}",
                    info.unit,
                    info.active,
                    info.input_fee_ppk
                );
                continue;
            }
            match kids.binary_search_by_key(&info.final_expiry, |kid| {
                infos
                    .get(kid)
                    .expect("kids is a subset of info.keys()")
                    .final_expiry
            }) {
                Ok(pos) => kids.insert(pos, *kid),
                Err(pos) => kids.insert(pos, *kid),
            }
        }
        let mut current_amount = Amount::ZERO;
        let summary = PocketSendSummary::new();
        let mut send_ref = SendReference {
            rid: summary.request_id,
            ..Default::default()
        };
        for kid in kids {
            let kid_ys = ys.get(&kid);
            for y in kid_ys.unwrap_or(&Vec::new()) {
                let proof = proofs.get(y).expect("proof should be here");
                if current_amount + proof.amount > target {
                    send_ref.swap_proof = Some((target - current_amount, *y));
                    *self.current_send.lock().unwrap() = Some(send_ref.clone());
                    return Ok(summary);
                } else if current_amount + proof.amount == target {
                    send_ref.send_proofs.push(*y);
                    *self.current_send.lock().unwrap() = Some(send_ref);
                    return Ok(summary);
                } else {
                    send_ref.send_proofs.push(*y);
                    current_amount += proof.amount;
                }
            }
        }
        Err(Error::InsufficientFunds)
    }

    async fn send_proofs(
        &self,
        rid: Uuid,
        _: &[KeySetInfo],
        client: &dyn MintConnector,
    ) -> Result<HashMap<cdk01::PublicKey, cdk00::Proof>> {
        let send_ref = {
            let mut locked = self.current_send.lock().unwrap();
            if locked.is_none() {
                return Err(Error::NoPrepareRef(rid));
            }
            if locked.as_ref().unwrap().rid != rid {
                return Err(Error::NoPrepareRef(rid));
            }
            locked.take().unwrap()
        };
        let sending_proofs = send_proofs(
            send_ref.send_proofs,
            send_ref.swap_proof,
            self.xpriv,
            self.db.as_ref(),
            client,
            None,
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
impl wallet::CreditPocket for Pocket {
    async fn reclaim_proofs(
        &self,
        keysets_info: &[KeySetInfo],
        client: &dyn MintConnector,
    ) -> Result<(Amount, Vec<cdk00::Proof>)> {
        let mut pendings = self.db.list_pending().await?;
        let infos = collect_keyset_infos_from_proofs(pendings.values(), keysets_info)?;
        let ys = pendings.keys().cloned().collect::<Vec<_>>();
        let request = cdk07::CheckStateRequest { ys };
        let response = client.post_check_state(request).await?;
        let unspent_proofs: HashMap<cdk01::PublicKey, cdk00::Proof> = response
            .states
            .iter()
            .filter_map(|state| {
                if state.state == cdk07::State::Unspent {
                    let proof = pendings
                        .remove(&state.y)
                        .expect("response built from pendings");
                    Some((state.y, proof))
                } else {
                    None
                }
            })
            .collect();
        let (reclaimable, redeemable): (HashMap<_, _>, HashMap<_, _>) =
            unspent_proofs.into_iter().partition(|(_, p)| {
                let info = infos
                    .get(&p.keyset_id)
                    .expect("infos map is built from unspent_proofs keyset_id");
                info.unit == self.unit && info.active
            });
        let (reclaimed, _) = self.digest_proofs(client, infos, reclaimable).await?;
        tracing::debug!(
            "CrPocket::reclaim_proofs: reclaimed: {reclaimed}, redeemable: {}",
            redeemable.len()
        );
        Ok((reclaimed, redeemable.into_values().collect()))
    }

    async fn get_redeemable_proofs(
        &self,
        keysets_info: &[KeySetInfo],
        _client: &dyn MintConnector,
    ) -> Result<Vec<cdk00::Proof>> {
        let unspent = self.db.list_unspent().await?;
        let infos = collect_keyset_infos_from_proofs(unspent.values(), keysets_info)?;
        let mut redeemable: Vec<cdk00::Proof> = Vec::with_capacity(unspent.len());
        for (y, proof) in unspent.into_iter() {
            let info = infos
                .get(&proof.keyset_id)
                .expect("infos map is built from unspent proofs keyset_id");
            if info.active {
                continue;
            }
            self.db.mark_as_pendingspent(y).await?;
            redeemable.push(proof);
        }
        Ok(redeemable)
    }

    async fn list_redemptions(
        &self,
        keysets_info: &[KeySetInfo],
        payment_window: std::time::Duration,
    ) -> Result<Vec<RedemptionSummary>> {
        let proofs = self.db.list_unspent().await?;
        let infos = collect_keyset_infos_from_proofs(proofs.values(), keysets_info)?;
        let ys_by_kid = group_ys_by_keyset_id(proofs.iter());
        let mut redemptions: Vec<RedemptionSummary> = Vec::with_capacity(infos.len());
        for (kid, ys) in ys_by_kid.iter() {
            let info = infos
                .get(kid)
                .expect("infos map is built from proofs keyset_id");
            if info.final_expiry.is_none() {
                tracing::warn!(
                    "CrPocket::list_redemptions: keyset {kid} has no final_expiry, skipping"
                );
                continue;
            }
            let expiry = info.final_expiry.unwrap() + payment_window.as_secs();
            let mut amount = Amount::ZERO;
            for y in ys {
                let proof = proofs.get(y).expect("proof should be here");
                amount += proof.amount;
            }
            redemptions.push(RedemptionSummary {
                tstamp: expiry,
                amount,
            })
        }
        redemptions.sort_by_key(|r| r.tstamp);
        Ok(redemptions)
    }
}

///////////////////////////////////////////// dummy pocket
pub struct DummyPocket {}

#[async_trait(?Send)]
impl wallet::Pocket for DummyPocket {
    fn unit(&self) -> CurrencyUnit {
        CurrencyUnit::Custom(String::from("dummy"))
    }
    async fn balance(&self) -> Result<cashu::Amount> {
        Ok(cashu::Amount::ZERO)
    }
    async fn receive_proofs(
        &self,
        _client: &dyn MintConnector,
        _keysets_info: &[KeySetInfo],
        _proofs: Vec<cdk00::Proof>,
    ) -> Result<(Amount, Vec<cdk01::PublicKey>)> {
        Ok((Amount::ZERO, Vec::new()))
    }
    async fn prepare_send(&self, _: Amount, _: &[KeySetInfo]) -> Result<PocketSendSummary> {
        Err(Error::Any(AnyError::msg("DummyPocket is dummy")))
    }
    async fn send_proofs(
        &self,
        _: Uuid,
        _: &[KeySetInfo],
        _: &dyn MintConnector,
    ) -> Result<HashMap<cdk01::PublicKey, cdk00::Proof>> {
        Err(Error::Any(AnyError::msg("DummyPocket is dummy")))
    }
    async fn clean_local_proofs(
        &self,
        _client: &dyn MintConnector,
    ) -> Result<Vec<cdk01::PublicKey>> {
        Ok(Vec::new())
    }

    async fn restore_local_proofs(
        &self,
        _keysets_info: &[KeySetInfo],
        _client: &dyn MintConnector,
    ) -> Result<usize> {
        Ok(0)
    }
}
#[async_trait(?Send)]
impl wallet::CreditPocket for DummyPocket {
    async fn reclaim_proofs(
        &self,
        _keysets_info: &[KeySetInfo],
        _client: &dyn MintConnector,
    ) -> Result<(Amount, Vec<cdk00::Proof>)> {
        Ok((Amount::ZERO, Vec::new()))
    }
    async fn get_redeemable_proofs(
        &self,
        _keysets_info: &[KeySetInfo],
        _client: &dyn MintConnector,
    ) -> Result<Vec<cdk00::Proof>> {
        Ok(Vec::new())
    }
    async fn list_redemptions(
        &self,
        _keysets_info: &[KeySetInfo],
        _payment_window: std::time::Duration,
    ) -> Result<Vec<RedemptionSummary>> {
        Ok(Vec::new())
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use crate::{
        utils::tests::MockMintConnector,
        wallet::{CreditPocket, Pocket},
    };
    use bcr_wdc_utils::{keys::test_utils as keys_test, signatures::test_utils as signatures_test};
    use mockall::predicate::*;

    fn pocket(db: Arc<dyn PocketRepository>) -> super::Pocket {
        let unit = CurrencyUnit::Sat;
        let seed = [0u8; 32];
        let xpriv = btc32::Xpriv::new_master(bitcoin::Network::Regtest, &seed).unwrap();
        super::Pocket::new(unit, db, xpriv)
    }

    #[tokio::test]
    async fn credit_receive_proofs() {
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
            let y = cashu::dhke::hash_to_curve(p.secret.as_bytes())
                .expect("hash_to_curve should not fail");
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

    #[tokio::test]
    async fn credit_receive_proofs_inactive_keyset() {
        let (mut info, keyset) = keys_test::generate_keyset();
        info.active = false;
        let k_infos = vec![KeySetInfo::from(info)];
        let amounts = [Amount::from(8u64), Amount::from(16u64)];
        let proofs = signatures_test::generate_proofs(&keyset, &amounts);
        let db = MockPocketRepository::new();
        let connector = MockMintConnector::new();
        let crpocket = pocket(Arc::new(db));
        let result = crpocket.receive_proofs(&connector, &k_infos, proofs).await;
        assert!(matches!(result, Err(Error::InactiveKeyset(_))));
    }

    #[tokio::test]
    async fn credit_receive_proofs_currency_mismatch() {
        let (mut info, keyset) = keys_test::generate_keyset();
        info.unit = CurrencyUnit::Usd;
        let k_infos = vec![KeySetInfo::from(info)];
        let amounts = [Amount::from(8u64), Amount::from(16u64)];
        let proofs = signatures_test::generate_proofs(&keyset, &amounts);
        let db = MockPocketRepository::new();
        let connector = MockMintConnector::new();
        let crpocket = pocket(Arc::new(db));
        let result = crpocket.receive_proofs(&connector, &k_infos, proofs).await;
        assert!(matches!(result, Err(Error::CurrencyUnitMismatch(_, _))));
    }

    #[tokio::test]
    async fn credit_prepare_send() {
        let (info, keyset) = keys_test::generate_keyset();
        let k_infos = vec![KeySetInfo::from(info)];
        let amount = Amount::from(16u64);
        let amounts = [Amount::from(32u64), Amount::from(16u64)];
        let proofs = signatures_test::generate_proofs(&keyset, &amounts);
        let proofs_map: HashMap<cdk01::PublicKey, cdk00::Proof> =
            HashMap::from_iter(proofs.into_iter().map(|p| {
                let y = p.y().expect("Hash to curve should not fail");
                (y, p)
            }));
        let mut db = MockPocketRepository::new();
        db.expect_list_unspent()
            .times(1)
            .returning(move || Ok(proofs_map.clone()));
        let crpocket = pocket(Arc::new(db));
        let summary = crpocket.prepare_send(amount, &k_infos).await.unwrap();
        assert_eq!(summary.swap_fees, Amount::ZERO);
        assert_eq!(summary.send_fees, Amount::ZERO);
    }

    #[tokio::test]
    async fn credit_prepare_send_no_funds() {
        let (info, keyset) = keys_test::generate_keyset();
        let k_infos = vec![KeySetInfo::from(info)];
        let amount = Amount::from(16u64);
        let amounts = [Amount::from(8u64), Amount::from(4u64)];
        let proofs = signatures_test::generate_proofs(&keyset, &amounts);
        let proofs_map: HashMap<cdk01::PublicKey, cdk00::Proof> =
            HashMap::from_iter(proofs.into_iter().map(|p| {
                let y = p.y().expect("Hash to curve should not fail");
                (y, p)
            }));
        let mut db = MockPocketRepository::new();
        db.expect_list_unspent()
            .times(1)
            .returning(move || Ok(proofs_map.clone()));
        let crpocket = pocket(Arc::new(db));
        let response = crpocket.prepare_send(amount, &k_infos).await;
        assert!(matches!(response, Err(Error::InsufficientFunds)));
    }

    #[tokio::test]
    async fn credit_prepare_send_inactive_keyset() {
        let (mut info, keyset) = keys_test::generate_keyset();
        info.active = false;
        let k_infos = vec![KeySetInfo::from(info)];
        let amount = Amount::from(16u64);
        let amounts = [Amount::from(32u64), Amount::from(4u64)];
        let proofs = signatures_test::generate_proofs(&keyset, &amounts);
        let proofs_map: HashMap<cdk01::PublicKey, cdk00::Proof> =
            HashMap::from_iter(proofs.into_iter().map(|p| {
                let y = p.y().expect("Hash to curve should not fail");
                (y, p)
            }));
        let mut db = MockPocketRepository::new();
        db.expect_list_unspent()
            .times(1)
            .returning(move || Ok(proofs_map.clone()));
        let crpocket = pocket(Arc::new(db));
        let response = crpocket.prepare_send(amount, &k_infos).await;
        assert!(matches!(response, Err(Error::InsufficientFunds)));
    }

    #[tokio::test]
    async fn credit_list_redemptions() {
        let mut proofs_map: HashMap<cdk01::PublicKey, cdk00::Proof> = HashMap::new();
        let mut k_infos: Vec<KeySetInfo> = vec![];
        // keyset 1
        let (mut info, keyset) = keys_test::generate_random_keyset();
        info.final_expiry = Some(100);
        let amounts = [Amount::from(32u64), Amount::from(4u64)];
        let proofs = signatures_test::generate_proofs(&keyset, &amounts);
        proofs_map.extend(proofs.into_iter().map(|p| {
            let y = p.y().expect("Hash to curve should not fail");
            (y, p)
        }));
        k_infos.push(KeySetInfo::from(info));
        // keyset 2
        let (mut info, keyset) = keys_test::generate_random_keyset();
        info.final_expiry = Some(10);
        let amounts = [Amount::from(128u64), Amount::from(16u64)];
        let proofs = signatures_test::generate_proofs(&keyset, &amounts);
        proofs_map.extend(proofs.into_iter().map(|p| {
            let y = p.y().expect("Hash to curve should not fail");
            (y, p)
        }));
        k_infos.push(KeySetInfo::from(info));
        // keyset 3
        let (mut info, keyset) = keys_test::generate_keyset();
        info.final_expiry = None;
        let amounts = [Amount::from(128u64), Amount::from(16u64)];
        let proofs = signatures_test::generate_proofs(&keyset, &amounts);
        proofs_map.extend(proofs.into_iter().map(|p| {
            let y = p.y().expect("Hash to curve should not fail");
            (y, p)
        }));
        k_infos.push(KeySetInfo::from(info));
        let mut db = MockPocketRepository::new();
        db.expect_list_unspent()
            .times(1)
            .returning(move || Ok(proofs_map.clone()));
        let pocket = pocket(Arc::new(db));
        let list = pocket
            .list_redemptions(&k_infos, std::time::Duration::from_secs(10))
            .await
            .unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].tstamp, 20);
        assert_eq!(list[1].tstamp, 110);
    }
}
