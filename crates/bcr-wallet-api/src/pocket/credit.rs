use crate::{
    ClowderMintConnector,
    error::{Error, Result},
    pocket::*,
    wallet::types::SwapConfig,
};
use async_trait::async_trait;
use bcr_common::cashu::{
    self, Amount, CurrencyUnit, KeySet, KeySetInfo, Proof, ProofsMethods, amount::SplitTarget,
    nut00 as cdk00, nut01 as cdk01, nut07 as cdk07,
};
use bcr_wallet_core::types::{RedemptionSummary, Seed, SendSummary};
use bcr_wallet_persistence::PocketRepository;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use uuid::Uuid;

#[async_trait]
pub trait CreditPocketApi: super::PocketApi {
    /// Reclaims the proofs for the given ys
    /// returns the amount reclaimed and the proofs that can be redeemed (i.e. unspent proofs with
    /// inactive keysets)
    async fn reclaim_proofs(
        &self,
        ys: &[cashu::PublicKey],
        keysets_info: &[KeySetInfo],
        client: Arc<dyn ClowderMintConnector>,
        swap_config: SwapConfig,
    ) -> Result<(Amount, Vec<cashu::Proof>)>;
    async fn get_redeemable_proofs(&self, keysets_info: &[KeySetInfo])
    -> Result<Vec<cashu::Proof>>;
    async fn list_redemptions(
        &self,
        keysets_info: &[KeySetInfo],
        payment_window: std::time::Duration,
    ) -> Result<Vec<RedemptionSummary>>;
}

///////////////////////////////////////////// credit pocket
pub struct Pocket {
    pub unit: cashu::CurrencyUnit,
    pub db: Arc<dyn PocketRepository>,
    seed: Seed,
    current_send: Mutex<Option<SendReference>>,
}

impl Pocket {
    pub fn new(unit: CurrencyUnit, db: Arc<dyn PocketRepository>, seed: Seed) -> Self {
        Self {
            unit,
            db,
            seed,
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
                return Err(Error::Unsupported(
                    "mint with fees not supported yet".to_string(),
                ));
            }
        }
        Ok(infos)
    }

    async fn digest_proofs(
        &self,
        client: Arc<dyn ClowderMintConnector>,
        inputs: HashMap<cdk01::PublicKey, cdk00::Proof>,
        swap_config: SwapConfig,
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
        for kid in old_proofs.keys() {
            let keyset = client.get_mint_keyset(*kid).await?;
            keysets.insert(*kid, keyset);
        }
        // preparing the premints
        let mut premints: HashMap<cashu::Id, cdk00::PreMintSecrets> = HashMap::new();
        for (kid, proofs) in old_proofs.iter() {
            let total = proofs.total_amount()?;
            let counter = self.db.counter(*kid).await?;
            let premint = cdk00::PreMintSecrets::from_seed(
                *kid,
                counter,
                &self.seed,
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
            swap_config,
        )
        .await?;
        Ok((cashed_in, ys))
    }
}

#[async_trait]
impl super::PocketApi for Pocket {
    fn unit(&self) -> CurrencyUnit {
        self.unit.clone()
    }

    async fn balance(&self) -> Result<Amount> {
        let proofs: Vec<Proof> = self.db.list_unspent().await?.into_values().collect();
        let total = proofs.total_amount()?;
        Ok(total)
    }

    async fn receive_proofs(
        &self,
        client: Arc<dyn ClowderMintConnector>,
        keysets_info: &[KeySetInfo],
        inputs: Vec<cdk00::Proof>,
        swap_config: SwapConfig,
    ) -> Result<(Amount, Vec<cdk01::PublicKey>)> {
        tracing::info!(
            "Credit receive proofs keyset {:?} proofs {:?}",
            keysets_info,
            inputs
        );
        self.validate_keysets(keysets_info, &inputs)?;
        // storing proofs in pending state
        let mut proofs: HashMap<cdk01::PublicKey, cdk00::Proof> =
            HashMap::with_capacity(inputs.len());
        for input in inputs.into_iter() {
            let y = input.y()?;
            proofs.insert(y, input);
        }
        tracing::info!("credit digest proofs");
        self.digest_proofs(client, proofs, swap_config).await
    }

    async fn prepare_send(
        &self,
        target: Amount,
        keysets_info: &[KeySetInfo],
    ) -> Result<SendSummary> {
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
        let mut summary = SendSummary::new();
        summary.unit = self.unit.clone();
        summary.amount = target;
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
        client: Arc<dyn ClowderMintConnector>,
        swap_config: SwapConfig,
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
            &self.seed,
            self.db.as_ref(),
            &client,
            None,
            swap_config,
        )
        .await?;
        Ok(sending_proofs)
    }

    async fn cleanup_local_proofs(
        &self,
        client: Arc<dyn ClowderMintConnector>,
    ) -> Result<Vec<cdk01::PublicKey>> {
        let cleaned_ys = cleanup_local_proofs(self.db.as_ref(), client).await?;
        Ok(cleaned_ys)
    }

    async fn restore_local_proofs(
        &self,
        keysets_info: &[KeySetInfo],
        client: Arc<dyn ClowderMintConnector>,
    ) -> Result<usize> {
        let kids = keysets_info.iter().filter_map(|info| {
            if info.unit == self.unit {
                Some(info.id)
            } else {
                None
            }
        });
        let mut total_recovered = 0;
        for kid in kids.into_iter() {
            total_recovered +=
                restore::restore_keysetid(&self.seed, kid, &client, self.db.as_ref()).await?;
        }
        Ok(total_recovered)
    }
    async fn delete_proofs(&self) -> Result<HashMap<cashu::Id, Vec<cdk00::Proof>>> {
        let proofs = self.db.list_all().await?;

        let mut proofs_by_keyset = HashMap::<cashu::Id, Vec<cdk00::Proof>>::new();

        for y in proofs.iter() {
            if let Some(proof) = self.db.delete_proof(*y).await? {
                proofs_by_keyset
                    .entry(proof.keyset_id)
                    .or_default()
                    .push(proof);
            }
        }

        Ok(proofs_by_keyset)
    }

    async fn return_proofs_to_send_for_offline_payment(
        &self,
        rid: Uuid,
    ) -> Result<(Amount, HashMap<cdk01::PublicKey, cdk00::Proof>)> {
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
        let proofs_to_send = return_proofs_to_send_for_offline_payment(
            send_ref.send_proofs,
            send_ref.swap_proof,
            self.db.as_ref(),
        )
        .await?;
        Ok(proofs_to_send)
    }

    async fn swap_to_unlocked_substitute_proofs(
        &self,
        proofs: Vec<cdk00::Proof>,
        keysets_info: &[KeySetInfo],
        client: Arc<dyn ClowderMintConnector>,
        send_amount: Amount,
        swap_config: SwapConfig,
    ) -> Result<Vec<cashu::Proof>> {
        let mut swapped_proofs = Vec::new();
        let total_amount = proofs.total_amount()?;
        let change_amount = total_amount - send_amount;
        tracing::debug!(
            "Swapping to unlocked substitute credit proofs - {change_amount} will be lost."
        );
        // handle keyset
        let active_info = keysets_info
            .iter()
            .find(|info| info.unit == self.unit && info.active && info.input_fee_ppk == 0);
        let Some(active_info) = active_info else {
            return Err(Error::NoActiveKeyset);
        };

        let active_keyset = client.get_mint_keyset(active_info.id).await?;
        // calculate splits
        let send_splits = send_amount.split();
        let send_splits_len = send_splits.len();
        let change_splits = change_amount.split();
        let mut splits: Vec<Amount> = Vec::with_capacity(send_splits.len() + change_splits.len());
        splits.extend(send_splits);
        splits.extend(change_splits);
        // create premints - no counter etc., since we're not persisting them anyway
        let premint_secrets = cashu::PreMintSecrets::random(
            active_info.id,
            total_amount,
            &SplitTarget::Values(splits),
        )?;
        let mut premints = HashMap::from([(active_info.id, premint_secrets)]);
        let keysets = HashMap::from([(active_info.id, active_keyset)]);

        let blinds: Vec<cdk00::BlindedMessage> = premints
            .values()
            .flat_map(|premint| premint.blinded_messages())
            .collect();

        let all_signatures = super::committed_swap(
            client.as_ref(),
            None,
            proofs,
            blinds,
            &swap_config,
            HashMap::new(),
        )
        .await?;

        // We only take the send_splits signatures, they add up to our send_amount
        let mut send_signatures = all_signatures;
        send_signatures.truncate(send_splits_len);

        let mut signatures: HashMap<cashu::Id, Vec<cdk00::BlindSignature>> = HashMap::new();
        for signature in send_signatures {
            signatures
                .entry(signature.keyset_id)
                .and_modify(|v| v.push(signature.clone()))
                .or_insert_with(|| vec![signature]);
        }

        let mut current_amount = Amount::ZERO;
        // only collect sending proofs, so we can take all - change is discarded for now
        for (kid, signatures) in signatures.into_iter() {
            let premint = premints.remove(&kid).expect("premint should be here");
            let keyset = keysets.get(&kid).expect("keyset should be here");
            let unblinded_proofs = unblind_proofs(keyset, signatures, premint);
            for proof in unblinded_proofs.into_iter() {
                current_amount += proof.amount;
                swapped_proofs.push(proof);
            }
        }

        if current_amount != send_amount {
            tracing::warn!(
                "Mismatch between target {send_amount} and amount from proofs {current_amount}"
            );
        }

        Ok(swapped_proofs)
    }
}

#[async_trait]
impl CreditPocketApi for Pocket {
    async fn reclaim_proofs(
        &self,
        ys: &[cdk01::PublicKey],
        keysets_info: &[KeySetInfo],
        client: Arc<dyn ClowderMintConnector>,
        swap_config: SwapConfig,
    ) -> Result<(Amount, Vec<cdk00::Proof>)> {
        let mut pendings = self.db.load_proofs(ys).await?;
        let infos = collect_keyset_infos_from_proofs(pendings.values(), keysets_info)?;
        let request = cdk07::CheckStateRequest { ys: ys.to_owned() };
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
        // Separate reclaimable (active keyset) from redeemable (inactive keyset) proofs
        // since we can't reclaim proofs from an inactive keyset anymore
        let (reclaimable, redeemable): (HashMap<_, _>, HashMap<_, _>) =
            unspent_proofs.into_iter().partition(|(_, p)| {
                let info = infos
                    .get(&p.keyset_id)
                    .expect("infos map is built from unspent_proofs keyset_id");
                info.unit == self.unit && info.active
            });
        let (reclaimed, _) = self.digest_proofs(client, reclaimable, swap_config).await?;
        tracing::debug!(
            "CrPocket::reclaim_proofs: reclaimed: {reclaimed}, redeemable: {}",
            redeemable.len()
        );
        Ok((reclaimed, redeemable.into_values().collect()))
    }

    async fn get_redeemable_proofs(
        &self,
        keysets_info: &[KeySetInfo],
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        external::test_utils::tests::MockMintConnector,
        pocket::{PocketApi, credit::CreditPocketApi},
    };
    use bcr_common::core_tests;
    use bcr_wallet_persistence::{MockPocketRepository, test_utils::tests::zero_seed};
    use mockall::predicate::*;

    fn pocket(db: Arc<dyn PocketRepository>) -> super::Pocket {
        let unit = CurrencyUnit::Sat;
        let seed = zero_seed();
        super::Pocket::new(unit, db, seed)
    }

    use crate::pocket::test_utils::tests::{
        setup_commitment_mocks, test_swap_config,
    };

    #[tokio::test]
    async fn credit_receive_proofs() {
        let (info, keyset) = core_tests::generate_random_ecash_keyset();
        let kid = info.id;
        let k_infos = vec![KeySetInfo::from(info)];
        let amounts = [Amount::from(8u64), Amount::from(16u64)];
        let proofs = core_tests::generate_random_ecash_proofs(&keyset, &amounts);
        let mut db = MockPocketRepository::new();
        let mut connector = MockMintConnector::new();
        let cloned_keyset = keyset.clone();
        connector
            .expect_get_mint_keyset()
            .times(1)
            .with(eq(kid))
            .returning(move |_| Ok(KeySet::from(cloned_keyset.clone())));
        db.expect_counter()
            .times(1)
            .with(eq(kid))
            .returning(|_| Ok(0));
        db.expect_increment_counter()
            .times(1)
            .with(eq(kid), eq(0), eq(2))
            .returning(|_, _, _| Ok(()));
        setup_commitment_mocks(&mut connector, &mut db);
        connector
            .expect_post_swap_committed()
            .times(1)
            .returning(move |request| {
                let amounts = request
                    .outputs
                    .iter()
                    .map(|b| b.amount)
                    .collect::<Vec<_>>();
                let signatures = core_tests::generate_ecash_signatures(&keyset, &amounts);
                Ok(bcr_common::wire::swap::SwapResponse { signatures })
            });
        db.expect_store_new().times(2).returning(|p| {
            let y = p.y().expect("Hash to curve should not fail");
            Ok(y)
        });
        let pocket = pocket(Arc::new(db));
        let (cashed, _) = pocket
            .receive_proofs(Arc::new(connector), &k_infos, proofs, test_swap_config())
            .await
            .unwrap();
        assert_eq!(cashed, Amount::from(24u64));
    }

    #[tokio::test]
    async fn credit_receive_proofs_inactive_keyset() {
        let (mut info, keyset) = core_tests::generate_random_ecash_keyset();
        info.active = false;
        let k_infos = vec![KeySetInfo::from(info)];
        let amounts = [Amount::from(8u64), Amount::from(16u64)];
        let proofs = core_tests::generate_random_ecash_proofs(&keyset, &amounts);
        let db = MockPocketRepository::new();
        let connector = MockMintConnector::new();
        let crpocket = pocket(Arc::new(db));
        let result = crpocket
            .receive_proofs(Arc::new(connector), &k_infos, proofs, test_swap_config())
            .await;
        assert!(matches!(result, Err(Error::InactiveKeyset(_))));
    }

    #[tokio::test]
    async fn credit_receive_proofs_currency_mismatch() {
        let (mut info, keyset) = core_tests::generate_random_ecash_keyset();
        info.unit = CurrencyUnit::Usd;
        let k_infos = vec![KeySetInfo::from(info)];
        let amounts = [Amount::from(8u64), Amount::from(16u64)];
        let proofs = core_tests::generate_random_ecash_proofs(&keyset, &amounts);
        let db = MockPocketRepository::new();
        let connector = MockMintConnector::new();
        let crpocket = pocket(Arc::new(db));
        let result = crpocket
            .receive_proofs(Arc::new(connector), &k_infos, proofs, test_swap_config())
            .await;
        assert!(matches!(result, Err(Error::CurrencyUnitMismatch(_, _))));
    }

    #[tokio::test]
    async fn credit_prepare_send() {
        let (info, keyset) = core_tests::generate_random_ecash_keyset();
        let k_infos = vec![KeySetInfo::from(info)];
        let amount = Amount::from(16u64);
        let amounts = [Amount::from(32u64), Amount::from(16u64)];
        let proofs = core_tests::generate_random_ecash_proofs(&keyset, &amounts);
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
        let (info, keyset) = core_tests::generate_random_ecash_keyset();
        let k_infos = vec![KeySetInfo::from(info)];
        let amount = Amount::from(16u64);
        let amounts = [Amount::from(8u64), Amount::from(4u64)];
        let proofs = core_tests::generate_random_ecash_proofs(&keyset, &amounts);
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
        let (mut info, keyset) = core_tests::generate_random_ecash_keyset();
        info.active = false;
        let k_infos = vec![KeySetInfo::from(info)];
        let amount = Amount::from(16u64);
        let amounts = [Amount::from(32u64), Amount::from(4u64)];
        let proofs = core_tests::generate_random_ecash_proofs(&keyset, &amounts);
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
        let (mut info, keyset) = core_tests::generate_random_ecash_keyset();
        info.final_expiry = Some(100);
        let amounts = [Amount::from(32u64), Amount::from(4u64)];
        let proofs = core_tests::generate_random_ecash_proofs(&keyset, &amounts);
        proofs_map.extend(proofs.into_iter().map(|p| {
            let y = p.y().expect("Hash to curve should not fail");
            (y, p)
        }));
        k_infos.push(KeySetInfo::from(info));
        // keyset 2
        let (mut info, keyset) = core_tests::generate_random_ecash_keyset();
        info.final_expiry = Some(10);
        let amounts = [Amount::from(128u64), Amount::from(16u64)];
        let proofs = core_tests::generate_random_ecash_proofs(&keyset, &amounts);
        proofs_map.extend(proofs.into_iter().map(|p| {
            let y = p.y().expect("Hash to curve should not fail");
            (y, p)
        }));
        k_infos.push(KeySetInfo::from(info));
        // keyset 3
        let (mut info, keyset) = core_tests::generate_random_ecash_keyset();
        info.final_expiry = None;
        let amounts = [Amount::from(128u64), Amount::from(16u64)];
        let proofs = core_tests::generate_random_ecash_proofs(&keyset, &amounts);
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

    #[tokio::test]
    async fn credit_reclaim_proofs() {
        let (info, keyset) = core_tests::generate_random_ecash_keyset();
        let kid = info.id;
        let k_infos = vec![KeySetInfo::from(info)];
        let amounts = [Amount::from(8u64), Amount::from(16u64)];
        let proofs = core_tests::generate_random_ecash_proofs(&keyset, &amounts);

        let ys: Vec<cdk01::PublicKey> = proofs.iter().map(|p| p.y().expect("valid y")).collect();

        let mut pdb = MockPocketRepository::new();
        let mut connector = MockMintConnector::new();
        let cloned_keyset = keyset.clone();

        connector
            .expect_get_mint_keyset()
            .times(1)
            .with(eq(kid))
            .returning(move |_| Ok(KeySet::from(cloned_keyset.clone())));
        let state_request = cdk07::CheckStateRequest { ys: ys.clone() };
        let state_proofs_clone = proofs.clone();
        connector
            .expect_post_check_state()
            .times(1)
            .with(eq(state_request))
            .returning(move |_| {
                let states: Vec<cashu::ProofState> = state_proofs_clone
                    .iter()
                    .map(|p| cashu::ProofState {
                        y: p.y().unwrap(),
                        state: cashu::State::Unspent,
                        witness: None,
                    })
                    .collect();
                Ok(cashu::CheckStateResponse { states })
            });
        let proofs_clone = proofs.clone();
        let ys_clone = ys.clone();
        pdb.expect_load_proofs()
            .times(1)
            .with(eq(ys_clone))
            .returning(move |_| {
                let mut map = HashMap::new();
                map.insert(proofs_clone[0].y().unwrap(), proofs_clone[0].clone());
                map.insert(proofs_clone[1].y().unwrap(), proofs_clone[1].clone());
                Ok(map)
            });
        pdb.expect_counter()
            .times(1)
            .with(eq(kid))
            .returning(|_| Ok(0));
        pdb.expect_increment_counter()
            .times(1)
            .with(eq(kid), eq(0), eq(2))
            .returning(|_, _, _| Ok(()));
        setup_commitment_mocks(&mut connector, &mut pdb);
        connector
            .expect_post_swap_committed()
            .times(1)
            .returning(move |request| {
                let amounts = request
                    .outputs
                    .iter()
                    .map(|b| b.amount)
                    .collect::<Vec<_>>();
                let signatures = core_tests::generate_ecash_signatures(&keyset, &amounts);
                Ok(bcr_common::wire::swap::SwapResponse { signatures })
            });
        pdb.expect_store_new().times(2).returning(|p| {
            let y = p.y().expect("Hash to curve should not fail");
            Ok(y)
        });

        let pocket = pocket(Arc::new(pdb));

        let arc_client: Arc<dyn ClowderMintConnector> = Arc::new(connector);
        let (reclaimed, _) = pocket
            .reclaim_proofs(&ys, &k_infos, arc_client, test_swap_config())
            .await
            .expect("reclaim works");
        assert_eq!(reclaimed, Amount::from(24u64));
    }
}
