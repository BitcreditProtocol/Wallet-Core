// ----- standard library imports
use std::{
    collections::{HashMap, HashSet},
    sync::Mutex,
};
// ----- extra library imports
use anyhow::Error as AnyError;
use async_trait::async_trait;
use bitcoin::bip32 as btc32;
use cashu::{
    Amount, CurrencyUnit, KeySet, KeySetInfo, amount::SplitTarget, nut00 as cdk00, nut01 as cdk01,
    nut03 as cdk03, nut07 as cdk07,
};
use cdk::wallet::MintConnector;
use uuid::Uuid;
// ----- local imports
use crate::{
    error::{Error, Result},
    types::PocketSendSummary,
    wallet::{CreditPocket, DebitPocket, Pocket},
};

// ----- end imports

#[derive(Default, Clone)]
struct SendReference {
    rid: Uuid,
    target: Amount,
    send_proofs: Vec<cdk01::PublicKey>,
    swap_proof: Option<cdk01::PublicKey>,
}

#[cfg_attr(test, mockall::automock)]
#[async_trait(?Send)]
pub trait PocketRepository {
    async fn store_new(&self, proof: cdk00::Proof) -> Result<cdk01::PublicKey>;
    async fn store_pendingspent(&self, proof: cdk00::Proof) -> Result<cdk01::PublicKey>;
    async fn load_proof(&self, y: cdk01::PublicKey) -> Result<(cdk00::Proof, cdk07::State)>;
    async fn delete_proof(&self, y: cdk01::PublicKey) -> Result<()>;
    async fn list_unspent(&self) -> Result<HashMap<cdk01::PublicKey, cdk00::Proof>>;
    async fn list_pending(&self) -> Result<HashMap<cdk01::PublicKey, cdk00::Proof>>;
    async fn list_reserved(&self) -> Result<HashMap<cdk01::PublicKey, cdk00::Proof>>;
    async fn list_all(&self) -> Result<Vec<cdk01::PublicKey>>;

    async fn mark_as_pendingspent(&self, y: cdk01::PublicKey) -> Result<cdk00::Proof>;

    async fn counter(&self, kid: cashu::Id) -> Result<u32>;
    async fn increment_counter(&self, kid: cashu::Id, old: u32, increment: u32) -> Result<()>;
}

///////////////////////////////////////////// credit pocket
pub struct CrPocket<Repo> {
    pub unit: cashu::CurrencyUnit,
    pub db: Repo,
    pub xpriv: btc32::Xpriv,

    current_send: Mutex<Option<SendReference>>,
}
impl<Repo> CrPocket<Repo> {
    pub fn new(unit: CurrencyUnit, db: Repo, xpriv: btc32::Xpriv) -> Self {
        Self {
            unit,
            db,
            xpriv,
            current_send: Mutex::new(None),
        }
    }
}

impl<Repo> CrPocket<Repo>
where
    Repo: PocketRepository,
{
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
            self.unit(),
            proofs_in_request,
            premints,
            keysets,
            client,
            &self.db,
        )
        .await?;
        Ok((cashed_in, ys))
    }
}

#[async_trait(?Send)]
impl<Repo> Pocket for CrPocket<Repo>
where
    Repo: PocketRepository,
{
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
            target,
            ..Default::default()
        };
        for kid in kids {
            let kid_ys = ys.get(&kid);
            for y in kid_ys.unwrap_or(&Vec::new()) {
                let proof = proofs.get(y).expect("proof should be here");
                if current_amount + proof.amount > target {
                    send_ref.swap_proof = Some(*y);
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
                return Err(Error::NoPrepareSendRef(rid));
            }
            if locked.as_ref().unwrap().rid != rid {
                return Err(Error::NoPrepareSendRef(rid));
            }
            locked.take().unwrap()
        };
        let mut sending_proofs: HashMap<cdk01::PublicKey, cdk00::Proof> = HashMap::new();
        let mut current_amount = Amount::ZERO;
        for y in send_ref.send_proofs {
            let proof = self.db.mark_as_pendingspent(y).await?;
            current_amount += proof.amount;
            sending_proofs.insert(y, proof);
        }

        if let Some(swap_y) = send_ref.swap_proof {
            let swap_proof = self.db.mark_as_pendingspent(swap_y).await?;
            let swap_proof_keyset = client.get_mint_keyset(swap_proof.keyset_id).await?;
            let target_swapped = swap_proof_to_target(
                swap_proof,
                &swap_proof_keyset,
                send_ref.target - current_amount,
                self.xpriv,
                &self.db,
                client,
            )
            .await?;
            for y in target_swapped.keys() {
                let proof = self.db.mark_as_pendingspent(*y).await?;
                current_amount += proof.amount;
                sending_proofs.insert(*y, proof);
            }
        }
        // this will go once tested thoroughly
        assert_eq!(current_amount, send_ref.target, "amount should match");
        Ok(sending_proofs)
    }

    async fn clean_local_proofs(
        &self,
        client: &dyn MintConnector,
    ) -> Result<Vec<cdk01::PublicKey>> {
        let cleaned_ys = clean_local_proofs(&self.db, client).await?;
        Ok(cleaned_ys)
    }
}

#[async_trait(?Send)]
impl<Repo> CreditPocket for CrPocket<Repo>
where
    Repo: PocketRepository,
{
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
}

///////////////////////////////////////////// debit pocket
pub struct DbPocket<Repo> {
    pub unit: cashu::CurrencyUnit,
    pub db: Repo,
    pub xpriv: btc32::Xpriv,

    current_send: Mutex<Option<SendReference>>,
}
impl<Repo> DbPocket<Repo> {
    pub fn new(unit: CurrencyUnit, db: Repo, xpriv: btc32::Xpriv) -> Self {
        Self {
            unit,
            db,
            xpriv,
            current_send: Mutex::new(None),
        }
    }
}

impl<Repo> DbPocket<Repo>
where
    Repo: PocketRepository,
{
    async fn find_active_keyset(
        &self,
        keysets_info: &[KeySetInfo],
        client: &dyn MintConnector,
    ) -> Result<(KeySetInfo, KeySet)> {
        let active_info = keysets_info
            .iter()
            .find(|info| info.unit == self.unit && info.active && info.input_fee_ppk == 0);
        let Some(active_info) = active_info else {
            return Err(Error::NoActiveKeyset);
        };
        let active_keyset = client.get_mint_keyset(active_info.id).await?;
        Ok((active_info.clone(), active_keyset))
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
        let cashed_in = swap(self.unit(), proofs, premints, keysets, client, &self.db).await?;
        Ok((cashed_in, ys))
    }
}

#[async_trait(?Send)]
impl<Repo> Pocket for DbPocket<Repo>
where
    Repo: PocketRepository,
{
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
            target,
            ..Default::default()
        };
        for kid in kids {
            let kid_ys = ys.get(&kid).cloned().unwrap_or_default();
            for y in kid_ys {
                let proof = proofs.get(&y).expect("proof should be here");
                if current_amount + proof.amount > target {
                    send_ref.swap_proof = Some(y);
                    *self.current_send.lock().unwrap() = Some(send_ref);
                    return Ok(pocket_summary);
                } else if current_amount + proof.amount == target {
                    send_ref.send_proofs.push(y);
                    *self.current_send.lock().unwrap() = Some(send_ref);
                    return Ok(pocket_summary);
                } else {
                    send_ref.send_proofs.push(y);
                    current_amount += proof.amount;
                }
            }
        }
        Err(Error::InsufficientFunds)
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
        let (_, active_keyset) = self.find_active_keyset(keysets_info, client).await?;
        let mut sending_proofs: HashMap<cdk01::PublicKey, cdk00::Proof> = HashMap::new();
        let mut current_amount = Amount::ZERO;
        for y in send_ref.send_proofs {
            let proof = self.db.mark_as_pendingspent(y).await?;
            current_amount += proof.amount;
            sending_proofs.insert(y, proof);
        }
        if let Some(swap_y) = send_ref.swap_proof {
            let proof = self.db.mark_as_pendingspent(swap_y).await?;
            let target_swapped = swap_proof_to_target(
                proof,
                &active_keyset,
                send_ref.target - current_amount,
                self.xpriv,
                &self.db,
                client,
            )
            .await?;
            for y in target_swapped.keys() {
                let proof = self.db.mark_as_pendingspent(*y).await?;
                current_amount += proof.amount;
                sending_proofs.insert(*y, proof);
            }
        }
        // this will go once tested thoroughly
        assert_eq!(current_amount, send_ref.target, "amount should match");
        Ok(sending_proofs)
    }

    async fn clean_local_proofs(
        &self,
        client: &dyn MintConnector,
    ) -> Result<Vec<cdk01::PublicKey>> {
        let cleaned_ys = clean_local_proofs(&self.db, client).await?;
        Ok(cleaned_ys)
    }
}

#[async_trait(?Send)]
impl<Repo> DebitPocket for DbPocket<Repo>
where
    Repo: PocketRepository,
{
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

///////////////////////////////////////////// dummy pocket
pub struct DummyPocket {}

#[async_trait(?Send)]
impl Pocket for DummyPocket {
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
}
#[async_trait(?Send)]
impl CreditPocket for DummyPocket {
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
}

///////////////////////////////////////////// clean_local_proofs
async fn clean_local_proofs(
    db: &dyn PocketRepository,
    client: &dyn MintConnector,
) -> Result<Vec<cdk01::PublicKey>> {
    let ys = db.list_all().await?;
    let request = cdk07::CheckStateRequest { ys };
    let response = client.post_check_state(request).await?;
    let mut cleaned_ys: Vec<cdk01::PublicKey> = Vec::with_capacity(response.states.len());
    for proofstate in response.states {
        if proofstate.state == cdk07::State::Spent {
            db.delete_proof(proofstate.y).await?;
            cleaned_ys.push(proofstate.y);
        }
    }
    Ok(cleaned_ys)
}

///////////////////////////////////////////// unblind_proofs
fn unblind_proofs(
    keyset: &KeySet,
    signatures: &[cdk00::BlindSignature],
    premint: &cdk00::PreMintSecrets,
) -> Vec<cdk00::Proof> {
    let mut proofs: Vec<cdk00::Proof> = Vec::new();
    if signatures.len() != premint.len() {
        tracing::error!(
            "signatures and premint len mismatch: {} != {}",
            signatures.len(),
            premint.len()
        )
    }
    for (signature, secret) in signatures.iter().zip(premint.iter()) {
        if signature.keyset_id != keyset.id || signature.keyset_id != premint.keyset_id {
            tracing::error!(
                "kid mismatch in signature: {}, {}, {}",
                signature.keyset_id,
                keyset.id,
                premint.keyset_id,
            );
            continue;
        }
        if signature.amount != secret.amount {
            tracing::error!(
                "amount mismatch in signature: {} != {}",
                signature.amount,
                secret.amount
            );
            continue;
        }
        let Some(key) = keyset.keys.get(&signature.amount) else {
            tracing::error!(
                "No mint key for amount: {} in kid: {}",
                keyset.id,
                signature.amount,
            );
            continue;
        };
        let result = cashu::dhke::unblind_message(&signature.c, &secret.r, key);
        let Ok(c) = result else {
            tracing::error!(
                "unblind_message fail: kid: {}, amount {}",
                keyset.id,
                signature.amount,
            );
            continue;
        };
        let proof = cdk00::Proof::new(
            signature.amount,
            signature.keyset_id,
            secret.secret.clone(),
            c,
        );
        proofs.push(proof);
    }
    proofs
}

///////////////////////////////////////////// swap
async fn swap(
    output_unit: CurrencyUnit,
    inputs: Vec<cdk00::Proof>,
    premints: HashMap<cashu::Id, cdk00::PreMintSecrets>,
    keysets: HashMap<cashu::Id, KeySet>,
    client: &dyn MintConnector,
    db: &dyn PocketRepository,
) -> Result<Amount> {
    let total_input = inputs.iter().fold(Amount::ZERO, |acc, p| acc + p.amount);
    let input_len = inputs.len();
    let mut blinds: Vec<cdk00::BlindedMessage> = Vec::new();
    for premint in premints.values() {
        blinds.extend(premint.blinded_messages());
    }
    let request = cdk03::SwapRequest::new(inputs, blinds);
    // sending the swap request
    let response = client.post_swap(request).await?;
    let output_len = response.signatures.len();
    let total_output = response
        .signatures
        .iter()
        .fold(Amount::ZERO, |acc, sig| acc + sig.amount);
    tracing::debug!(
        "swap to {output_unit}: inputs: {input_len} {total_input}, outputs: {output_len} {total_output}",
    );
    let mut signatures: HashMap<cashu::Id, Vec<cdk00::BlindSignature>> = HashMap::new();
    for signature in response.signatures {
        signatures
            .entry(signature.keyset_id)
            .and_modify(|v| v.push(signature.clone()))
            .or_insert_with(|| vec![signature]);
    }
    let mut total_cashed_in = Amount::ZERO;
    for (kid, signatures) in signatures.iter() {
        let premint = premints.get(kid).expect("premint should be here");
        let keyset = keysets.get(kid).expect("keyset should be here");
        let proofs = unblind_proofs(keyset, signatures, premint);
        for proof in proofs {
            let amount = proof.amount;
            let response = db.store_new(proof).await;
            if let Err(e) = response {
                tracing::error!("failed at storing new proof: {kid}, {amount}, {e}");
                continue;
            }
            total_cashed_in += amount;
        }
    }
    Ok(total_cashed_in)
}

///////////////////////////////////////////// swap_proof_to_target
async fn swap_proof_to_target(
    proof: cdk00::Proof,
    target_keyset: &KeySet,
    target_amount: Amount,
    xpriv: btc32::Xpriv,
    db: &dyn PocketRepository,
    client: &dyn MintConnector,
) -> Result<HashMap<cdk01::PublicKey, cdk00::Proof>> {
    let target = SplitTarget::Value(target_amount);
    let counter = db.counter(target_keyset.id).await?;
    let premint =
        cdk00::PreMintSecrets::from_xpriv(target_keyset.id, counter, xpriv, proof.amount, &target)?;
    let blinds = premint.blinded_messages();
    let request = cdk03::SwapRequest::new(vec![proof], blinds);
    db.increment_counter(target_keyset.id, counter, premint.len() as u32)
        .await?;
    let signatures = client.post_swap(request).await?.signatures;
    let mut on_target: HashMap<cdk01::PublicKey, cdk00::Proof> = HashMap::new();
    let mut proofs = unblind_proofs(target_keyset, &signatures, &premint);
    proofs.sort_by_key(|proof| std::cmp::Reverse(proof.amount));
    let mut current_amount = Amount::ZERO;
    for proof in proofs.into_iter() {
        let result = db.store_new(proof.clone()).await;
        match result {
            Ok(y) => {
                if current_amount + proof.amount <= target_amount {
                    current_amount += proof.amount;
                    on_target.insert(y, proof);
                }
            }
            Err(e) => {
                tracing::error!(
                    "error in storing proof {}, {}: {e}",
                    target_keyset.id,
                    proof.amount
                );
            }
        }
    }
    Ok(on_target)
}

///////////////////////////////////////////// collect_keyset_infos_from_proofs
fn collect_keyset_infos_from_proofs<'it, 'inf>(
    proofs: impl Iterator<Item = &'it cdk00::Proof>,
    keysets_info: &'inf [KeySetInfo],
) -> Result<HashMap<cashu::Id, &'inf KeySetInfo>> {
    let kids = proofs.map(|p| p.keyset_id).collect::<HashSet<_>>();
    let mut infos: HashMap<cashu::Id, &'inf KeySetInfo> = HashMap::new();
    for kid in kids {
        let info = keysets_info.iter().find(|info| info.id == kid);
        if let Some(info) = info {
            infos.insert(kid, info);
        } else {
            return Err(Error::UnknownKeysetId(kid));
        }
    }
    Ok(infos)
}

///////////////////////////////////////////// group_ys_by_keyset_id
fn group_ys_by_keyset_id<'a>(
    proofs: impl Iterator<Item = (&'a cdk01::PublicKey, &'a cdk00::Proof)>,
) -> HashMap<cashu::Id, Vec<cdk01::PublicKey>> {
    let mut ys: HashMap<cashu::Id, Vec<cdk01::PublicKey>> = HashMap::new();
    for (y, proof) in proofs {
        ys.entry(proof.keyset_id)
            .and_modify(|v| v.push(*y))
            .or_insert(vec![*y]);
    }
    ys
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use crate::utils::tests::MockMintConnector;
    use bcr_wdc_utils::{
        keys::{self as keys_utils, test_utils as keys_test},
        signatures::test_utils as signatures_test,
    };
    use cashu::nut02 as cdk02;
    use mockall::predicate::*;

    #[test]
    fn unblind_proofs() {
        let amounts = [Amount::from(8u64)];
        let (_, mintkeyset) = keys_test::generate_keyset();

        let keyset = cdk02::KeySet::from(mintkeyset.clone());
        let premint =
            cdk00::PreMintSecrets::random(keyset.id, amounts[0], &SplitTarget::None).unwrap();
        assert!(premint.blinded_messages().len() == 1);
        let blind = premint.blinded_messages()[0].clone();
        let signature = keys_utils::sign_with_keys(&mintkeyset, &blind).unwrap();
        let proofs = super::unblind_proofs(&keyset, &[signature], &premint);
        assert_eq!(proofs.len(), 1);
        keys_utils::verify_with_keys(&mintkeyset, &proofs[0]).unwrap();
    }

    #[test]
    fn unblind_proofs_len_mismatch() {
        let (_, mintkeyset) = keys_test::generate_keyset();

        let keyset = cdk02::KeySet::from(mintkeyset.clone());
        let premint =
            cdk00::PreMintSecrets::random(keyset.id, Amount::from(8u64), &SplitTarget::None)
                .unwrap();
        assert_eq!(premint.blinded_messages().len(), 1);
        let signatures = signatures_test::generate_signatures(
            &mintkeyset,
            &[Amount::from(8u64), Amount::from(32u64)],
        );
        let proofs = super::unblind_proofs(&keyset, &signatures, &premint);
        assert_eq!(proofs.len(), 1);
    }

    #[test]
    fn unblind_proofs_amount_mismatch() {
        let (_, mintkeyset) = keys_test::generate_keyset();

        let keyset = cdk02::KeySet::from(mintkeyset.clone());
        let premint =
            cdk00::PreMintSecrets::random(keyset.id, Amount::from(40u64), &SplitTarget::None)
                .unwrap();
        assert_eq!(premint.blinded_messages().len(), 2);
        let signatures = signatures_test::generate_signatures(
            &mintkeyset,
            &[Amount::from(16u64), Amount::from(4u64)],
        );
        let proofs = super::unblind_proofs(&keyset, &signatures, &premint);
        assert_eq!(proofs.len(), 0);
    }

    #[test]
    fn unblind_proofs_kid_mismatch() {
        let (_, mintkeyset) = keys_test::generate_keyset();

        let keyset = cdk02::KeySet::from(mintkeyset.clone());
        let kid2 = keys_test::generate_random_keysetid();
        let premint =
            cdk00::PreMintSecrets::random(kid2, Amount::from(16u64), &SplitTarget::None).unwrap();
        assert_eq!(premint.blinded_messages().len(), 1);
        let signatures = signatures_test::generate_signatures(&mintkeyset, &[Amount::from(16u64)]);
        let proofs = super::unblind_proofs(&keyset, &signatures, &premint);
        assert_eq!(proofs.len(), 0);
    }

    fn crpocket<Repo>(db: Repo) -> CrPocket<Repo> {
        let unit = CurrencyUnit::Sat;
        let seed = [0u8; 32];
        let xpriv = btc32::Xpriv::new_master(bitcoin::Network::Regtest, &seed).unwrap();
        CrPocket::new(unit, db, xpriv)
    }

    fn dbpocket<Repo>(db: Repo) -> DbPocket<Repo> {
        let unit = CurrencyUnit::Sat;
        let seed = [0u8; 32];
        let xpriv = btc32::Xpriv::new_master(bitcoin::Network::Regtest, &seed).unwrap();
        DbPocket::new(unit, db, xpriv)
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

        let crpocket = crpocket(db);

        let (cashed, _) = crpocket
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

        let crpocket = crpocket(db);

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

        let crpocket = crpocket(db);

        let result = crpocket.receive_proofs(&connector, &k_infos, proofs).await;
        assert!(matches!(result, Err(Error::CurrencyUnitMismatch(_, _))));
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

        let dbpocket = dbpocket(db);

        let (cashed, _) = dbpocket
            .receive_proofs(&connector, &k_infos, proofs)
            .await
            .unwrap();
        assert_eq!(cashed, Amount::from(24u64));
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

        let crpocket = crpocket(db);

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

        let crpocket = crpocket(db);

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

        let crpocket = crpocket(db);

        let response = crpocket.prepare_send(amount, &k_infos).await;
        assert!(matches!(response, Err(Error::InsufficientFunds)));
    }

    #[tokio::test]
    async fn swap_proof_to_target() {
        let (_, keyset) = keys_test::generate_keyset();
        // 16 --> 13 ==> ( 8 + 4 + 1 ) + 2 + 1
        let amount = Amount::from(16u64);
        let target = Amount::from(13u64);
        let proof = signatures_test::generate_proofs(&keyset, &[amount])[0].clone();

        let seed = [0u8; 32];
        let xpriv = btc32::Xpriv::new_master(bitcoin::Network::Regtest, &seed).unwrap();
        let mut mockdb = MockPocketRepository::new();
        let mut mockclient = MockMintConnector::new();

        mockdb
            .expect_counter()
            .times(1)
            .with(eq(keyset.id))
            .returning(|_| Ok(0));
        mockdb
            .expect_increment_counter()
            .times(1)
            .with(eq(keyset.id), eq(0), eq(5))
            .returning(|_, _, _| Ok(()));
        let cloned_keyset = keyset.clone();
        mockclient
            .expect_post_swap()
            .times(1)
            .returning(move |request| {
                let amounts = request
                    .outputs()
                    .iter()
                    .map(|b| b.amount)
                    .collect::<Vec<_>>();
                let mock_signatures =
                    signatures_test::generate_signatures(&cloned_keyset, &amounts);
                Ok(cdk03::SwapResponse {
                    signatures: mock_signatures,
                })
            });
        mockdb.expect_store_new().times(5).returning(|p| {
            let y = p.y().expect("Hash to curve should not fail");
            Ok(y)
        });

        let proofs = super::swap_proof_to_target(
            proof,
            &KeySet::from(keyset),
            target,
            xpriv,
            &mockdb,
            &mockclient,
        )
        .await
        .unwrap();

        assert_eq!(proofs.len(), 3);
        let total = proofs
            .iter()
            .fold(Amount::ZERO, |acc, (_, p)| acc + p.amount);
        assert_eq!(total, target);
    }

    #[tokio::test]
    async fn swap() {
        let (info, keyset) = keys_test::generate_random_keyset();
        let amounts = [Amount::from(8u64), Amount::from(16u64)];
        let unit = CurrencyUnit::Sat;
        let inputs = signatures_test::generate_proofs(&keyset, &amounts);
        let premints = HashMap::from_iter([(
            info.id,
            cdk00::PreMintSecrets::random(info.id, Amount::from(24u64), &SplitTarget::None)
                .unwrap(),
        )]);
        let keysets = HashMap::from([(info.id, KeySet::from(keyset.clone()))]);

        let mut mockclient = MockMintConnector::new();
        let mut mockdb = MockPocketRepository::new();
        mockclient
            .expect_post_swap()
            .times(1)
            .returning(move |request| {
                let amounts = request
                    .outputs()
                    .iter()
                    .map(|b| b.amount)
                    .collect::<Vec<_>>();
                let signatures = signatures_test::generate_signatures(&keyset, &amounts);
                Ok(cdk03::SwapResponse { signatures })
            });
        mockdb.expect_store_new().times(2).returning(|p| {
            let y = p.y().expect("Hash to curve should not fail");
            Ok(y)
        });

        let amount = super::swap(unit, inputs, premints, keysets, &mockclient, &mockdb)
            .await
            .unwrap();
        assert_eq!(amount, Amount::from(24u64));
    }
}
