// ----- standard library imports
use std::{
    collections::{HashMap, HashSet},
    sync::Mutex,
};
// ----- extra library imports
use anyhow::Error as AnyError;
use async_trait::async_trait;
use bcr_wallet_lib::wallet::Token;
use bitcoin::bip32 as btc32;
use cashu::{
    Amount, CurrencyUnit, KeySet, KeySetInfo, MintUrl, PublicKey, amount::SplitTarget,
    nut00 as cdk00, nut01 as cdk01, nut02 as cdk02, nut03 as cdk03, nut07 as cdk07,
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
    send_proofs: Vec<PublicKey>,
    swap_proof: Option<PublicKey>,
}

#[cfg_attr(test, mockall::automock)]
#[async_trait(?Send)]
pub trait PocketRepository {
    async fn store_new(&self, proof: cdk00::Proof) -> Result<cdk01::PublicKey>;
    async fn load_proof(&self, y: cdk01::PublicKey)
    -> Result<Option<(cdk00::Proof, cdk07::State)>>;
    async fn list_unspent(&self) -> Result<HashMap<cdk01::PublicKey, cdk00::Proof>>;
    async fn mark_as_pending(&self, y: cdk01::PublicKey) -> Result<cdk00::Proof>;

    async fn counter(&self, kid: cdk02::Id) -> Result<u32>;
    async fn increment_counter(&self, kid: cdk02::Id, old: u32, increment: u32) -> Result<()>;
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

#[async_trait(?Send)]
impl<Repo> Pocket for CrPocket<Repo>
where
    Repo: PocketRepository,
{
    fn is_mine(&self, token: &Token) -> bool {
        matches!(token, Token::BitcrV4(..)) && token.unit().as_ref() == Some(&self.unit)
    }

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

    async fn receive(
        &self,
        client: &dyn MintConnector,
        k_infos: &[KeySetInfo],
        token: Token,
    ) -> Result<Amount> {
        let proofs = token.proofs(k_infos)?;
        if proofs.is_empty() {
            tracing::warn!("token with no proofs");
            return Ok(Amount::ZERO);
        }
        let mut grouped_proofs: HashMap<cdk02::Id, Vec<cdk00::Proof>> = HashMap::new();
        for proof in proofs.iter() {
            grouped_proofs
                .entry(proof.keyset_id)
                .and_modify(|v| v.push(proof.clone()))
                .or_insert_with(|| vec![proof.clone()]);
        }

        let mut grouped_premints: HashMap<cdk02::Id, cdk00::PreMintSecrets> = HashMap::new();
        let mut grouped_keysets: HashMap<cdk02::Id, cdk02::KeySet> = HashMap::new();
        let mut blinds = Vec::new();
        for (kid, proofs) in grouped_proofs.iter() {
            let info = k_infos
                .iter()
                .find(|info| info.id == *kid)
                .unwrap_or_else(|| panic!("keyset id {kid} not found"));
            if !info.active {
                return Err(Error::ExpiredKeyset(*kid));
            }
            if info.unit != self.unit {
                return Err(Error::CurrencyUnitMismatch(
                    self.unit.clone(),
                    info.unit.clone(),
                ));
            }
            if info.input_fee_ppk != 0 {
                return Err(Error::Any(AnyError::msg(
                    "mint with fees not supported yet",
                )));
            }
            let keyset = client.get_mint_keyset(*kid).await?;
            grouped_keysets.insert(keyset.id, keyset);

            let total = proofs.iter().fold(Amount::ZERO, |acc, p| acc + p.amount);
            let counter = self.db.counter(*kid).await?;
            let premint = cdk00::PreMintSecrets::from_xpriv(
                *kid,
                counter,
                self.xpriv,
                total,
                &SplitTarget::None,
            )?;
            self.db
                .increment_counter(*kid, counter, premint.secrets().len() as u32)
                .await?;
            blinds.extend(premint.blinded_messages());
            grouped_premints.insert(*kid, premint);
        }
        let request = cdk03::SwapRequest::new(proofs, blinds);
        let signatures = client.post_swap(request).await?.signatures;
        let grouped_signatures: HashMap<cdk02::Id, Vec<cdk00::BlindSignature>> =
            signatures.into_iter().fold(HashMap::new(), |mut acc, sig| {
                acc.entry(sig.keyset_id)
                    .and_modify(|v| v.push(sig.clone()))
                    .or_insert_with(|| vec![sig]);
                acc
            });
        let mut total_cashed_in = Amount::ZERO;
        // we assume the order of signatures and premints, grouped by keyset_id, is preserved
        for (kid, signatures) in grouped_signatures.iter() {
            let Some(premint) = grouped_premints.get(kid) else {
                tracing::error!("No premint found for kid: {kid}");
                continue;
            };
            let keyset = grouped_keysets
                .get(kid)
                .expect("should have been collected already");
            let proofs = unblind_proofs(keyset, signatures, premint);
            for proof in proofs {
                let amount = proof.amount;
                let response = self.db.store_new(proof).await;
                if let Err(e) = response {
                    tracing::error!("fail in storing new proof: {kid}, {}, {e}", amount);
                    continue;
                }
                total_cashed_in += amount;
            }
        }
        Ok(total_cashed_in)
    }

    async fn prepare_send(
        &self,
        target: Amount,
        infos: &[KeySetInfo],
    ) -> Result<PocketSendSummary> {
        let (kids, proofs, infos, ys) = prepare_send_prepare_maps(infos, &self.db).await?;
        let mut current_amount = Amount::ZERO;
        let summary = PocketSendSummary::new();
        let mut send_ref = SendReference {
            rid: summary.request_id,
            target,
            ..Default::default()
        };
        for kid in kids {
            let info = infos.get(&kid).expect("keyset id should be here by now");
            if !info.active {
                continue;
            }
            if info.unit != self.unit() {
                tracing::warn!("proof signed with {kid} different unit in storage");
                continue;
            }
            if info.input_fee_ppk != 0 {
                tracing::warn!("current version does not support mint with fees");
                continue;
            };
            let kid_ys = ys.get(&kid).cloned().unwrap_or_default();
            for y in kid_ys {
                let proof = proofs.get(&y).expect("proof should be here");
                if current_amount + proof.amount > target {
                    send_ref.swap_proof = Some(y);
                    *self.current_send.lock().unwrap() = Some(send_ref.clone());
                    return Ok(summary);
                } else if current_amount + proof.amount == target {
                    send_ref.send_proofs.push(y);
                    *self.current_send.lock().unwrap() = Some(send_ref);
                    return Ok(summary);
                } else {
                    send_ref.send_proofs.push(y);
                    current_amount += proof.amount;
                }
            }
        }
        Err(Error::InsufficientFunds)
    }

    async fn send(
        &self,
        rid: Uuid,
        _: &[KeySetInfo],
        client: &dyn MintConnector,
        mint_url: MintUrl,
        memo: Option<String>,
    ) -> Result<Token> {
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
        let mut proofs: Vec<cdk00::Proof> = Vec::new();
        let mut current_amount = Amount::ZERO;
        for y in send_ref.send_proofs {
            let proof = self.db.mark_as_pending(y).await?;
            current_amount += proof.amount;
            proofs.push(proof);
        }

        if let Some(swap_y) = send_ref.swap_proof {
            let swap_proof = self.db.mark_as_pending(swap_y).await?;
            let swap_proof_keyset = client.get_mint_keyset(swap_proof.keyset_id).await?;
            let target_swapped_ys = swap_proof_to_target(
                swap_proof,
                &swap_proof_keyset,
                send_ref.target - current_amount,
                self.xpriv,
                &self.db,
                client,
            )
            .await?;
            for y in target_swapped_ys {
                let proof = self.db.mark_as_pending(y).await?;
                current_amount += proof.amount;
                proofs.push(proof);
            }
        }
        // this will go once tested thoroughly
        assert_eq!(current_amount, send_ref.target, "amount should match");

        let token = Token::new_bitcr(mint_url, proofs, memo, self.unit.clone());
        Ok(token)
    }
}

impl<Repo> CreditPocket for CrPocket<Repo> where Repo: PocketRepository {}

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

#[async_trait(?Send)]
impl<Repo> Pocket for DbPocket<Repo>
where
    Repo: PocketRepository,
{
    fn is_mine(&self, token: &Token) -> bool {
        matches!(token, Token::CashuV4(..)) && token.unit().as_ref() == Some(&self.unit)
    }

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

    async fn receive(
        &self,
        client: &dyn MintConnector,
        k_infos: &[KeySetInfo],
        token: Token,
    ) -> Result<Amount> {
        let proofs = token.proofs(k_infos)?;
        if proofs.is_empty() {
            tracing::warn!("token with no proofs");
            return Ok(Amount::ZERO);
        }
        let total_amount = proofs.iter().fold(Amount::ZERO, |acc, p| acc + p.amount);
        let active = k_infos
            .iter()
            .find(|info| info.active && info.unit == self.unit)
            .ok_or(Error::NoActiveKeyset)?;
        let keyset = client.get_mint_keyset(active.id).await?;
        let counter = self.db.counter(active.id).await?;
        let premint_secrets = cdk00::PreMintSecrets::from_xpriv(
            active.id,
            counter,
            self.xpriv,
            total_amount,
            &SplitTarget::None,
        )?;
        let request = cdk03::SwapRequest::new(proofs, premint_secrets.blinded_messages());
        let signatures = client.post_swap(request).await?.signatures;
        let proofs = unblind_proofs(&keyset, &signatures, &premint_secrets);
        let mut total_cashed_in = Amount::ZERO;
        for proof in proofs {
            let amount = proof.amount;
            let response = self.db.store_new(proof).await;
            if let Err(e) = response {
                tracing::error!("fail in storing new proof: {}, {}, {e}", active.id, amount);
                continue;
            }
            total_cashed_in += amount;
        }
        Ok(total_cashed_in)
    }

    async fn prepare_send(
        &self,
        target: Amount,
        infos: &[KeySetInfo],
    ) -> Result<PocketSendSummary> {
        let (kids, proofs, infos, ys) = prepare_send_prepare_maps(infos, &self.db).await?;
        let mut current_amount = Amount::ZERO;
        let pocket_summary = PocketSendSummary::new();
        let mut send_ref = SendReference {
            rid: pocket_summary.request_id,
            target,
            ..Default::default()
        };
        for kid in kids {
            let info = infos.get(&kid).expect("keyset id should be here by now");
            if info.unit != self.unit() {
                tracing::warn!("proof signed with {kid} different unit in storage");
                continue;
            }
            if info.input_fee_ppk != 0 {
                tracing::warn!("current version does not support mint with fees");
                continue;
            };
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

    async fn send(
        &self,
        rid: Uuid,
        keyset_infos: &[KeySetInfo],
        client: &dyn MintConnector,
        mint_url: MintUrl,
        memo: Option<String>,
    ) -> Result<Token> {
        let active_info = keyset_infos
            .iter()
            .find(|info| info.active && info.unit == self.unit)
            .ok_or(Error::NoActiveKeyset)?;
        let active_keyset = client.get_mint_keyset(active_info.id).await?;

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
        let mut proofs: Vec<cdk00::Proof> = Vec::new();
        let mut current_amount = Amount::ZERO;
        for y in send_ref.send_proofs {
            let proof = self.db.mark_as_pending(y).await?;
            current_amount += proof.amount;
            proofs.push(proof);
        }
        if let Some(swap_y) = send_ref.swap_proof {
            let proof = self.db.mark_as_pending(swap_y).await?;
            let target_swapped_ys = swap_proof_to_target(
                proof,
                &active_keyset,
                send_ref.target - current_amount,
                self.xpriv,
                &self.db,
                client,
            )
            .await?;
            for y in target_swapped_ys {
                let proof = self.db.mark_as_pending(y).await?;
                current_amount += proof.amount;
                proofs.push(proof);
            }
        }
        // this will go once tested thoroughly
        assert_eq!(current_amount, send_ref.target, "amount should match");

        let token = Token::new_cashu(mint_url, proofs, memo, self.unit.clone());
        Ok(token)
    }
}

impl<Repo> DebitPocket for DbPocket<Repo> where Repo: PocketRepository {}

pub struct DummyPocket {}

#[async_trait(?Send)]
impl Pocket for DummyPocket {
    fn is_mine(&self, _token: &Token) -> bool {
        false
    }
    fn unit(&self) -> CurrencyUnit {
        CurrencyUnit::Custom(String::from("dummy"))
    }
    async fn balance(&self) -> Result<cashu::Amount> {
        Ok(cashu::Amount::ZERO)
    }
    async fn receive(
        &self,
        _client: &dyn MintConnector,
        _k_infos: &[KeySetInfo],
        _token: Token,
    ) -> Result<cashu::Amount> {
        Err(Error::Any(AnyError::msg("DummyPocket is dummy")))
    }
    async fn prepare_send(&self, _: Amount, _: &[KeySetInfo]) -> Result<PocketSendSummary> {
        Err(Error::Any(AnyError::msg("DummyPocket is dummy")))
    }
    async fn send(
        &self,
        _: Uuid,
        _: &[KeySetInfo],
        _: &dyn MintConnector,
        _: MintUrl,
        _: Option<String>,
    ) -> Result<Token> {
        Err(Error::Any(AnyError::msg("DummyPocket is dummy")))
    }
}
impl CreditPocket for DummyPocket {}
impl DebitPocket for DummyPocket {}

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

async fn swap_proof_to_target(
    proof: cdk00::Proof,
    target_keyset: &KeySet,
    target_amount: Amount,
    xpriv: btc32::Xpriv,
    db: &dyn PocketRepository,
    client: &dyn MintConnector,
) -> Result<Vec<cdk01::PublicKey>> {
    let target = SplitTarget::Value(target_amount);
    let counter = db.counter(target_keyset.id).await?;
    let premint =
        cdk00::PreMintSecrets::from_xpriv(target_keyset.id, counter, xpriv, proof.amount, &target)?;
    let blinds = premint.blinded_messages();
    let request = cdk03::SwapRequest::new(vec![proof], blinds);
    db.increment_counter(target_keyset.id, counter, premint.len() as u32)
        .await?;
    let signatures = client.post_swap(request).await?.signatures;
    let mut on_target: Vec<cdk01::PublicKey> = Vec::new();
    let mut proofs = unblind_proofs(target_keyset, &signatures, &premint);
    proofs.sort_by_key(|proof| std::cmp::Reverse(proof.amount));
    let mut current_amount = Amount::ZERO;
    for proof in proofs.into_iter() {
        let result = db.store_new(proof.clone()).await;
        match result {
            Ok(y) => {
                if current_amount + proof.amount <= target_amount {
                    current_amount += proof.amount;
                    on_target.push(y);
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

async fn prepare_send_prepare_maps(
    infos: &[KeySetInfo],
    db: &dyn PocketRepository,
) -> Result<(
    Vec<cashu::Id>,
    HashMap<PublicKey, cdk00::Proof>,
    HashMap<cashu::Id, KeySetInfo>,
    HashMap<cashu::Id, Vec<PublicKey>>,
)> {
    let proofs: HashMap<PublicKey, cashu::Proof> = db.list_unspent().await?;
    let mut kids: Vec<cashu::Id> = proofs
        .values()
        .map(|p| p.keyset_id)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let infos: HashMap<cashu::Id, KeySetInfo> = infos
        .iter()
        .filter_map(|info| {
            if kids.contains(&info.id) {
                Some((info.id, info.clone()))
            } else {
                None
            }
        })
        .collect();
    kids.retain(|kid| {
        let info = infos.get(kid);
        if info.is_none() {
            tracing::warn!("keyset {kid} not found in keyset_infos");
            return false;
        }
        true
    });
    kids.sort_by_key(|kid| {
        let info = infos
            .get(kid)
            .unwrap_or_else(|| panic!("keyset id {kid} not found"));
        info.final_expiry
    });
    let mut grouped_ys: HashMap<cashu::Id, Vec<PublicKey>> = HashMap::new();
    for kid in &kids {
        let ys = proofs
            .iter()
            .filter_map(|(y, p)| if p.keyset_id == *kid { Some(*y) } else { None })
            .collect();
        grouped_ys.insert(*kid, ys);
    }

    Ok((kids, proofs, infos, grouped_ys))
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use crate::utils::tests::MockMintConnector;
    use bcr_wdc_utils::{
        keys::{self as keys_utils, test_utils as keys_test},
        signatures::test_utils as signatures_test,
    };
    use mockall::predicate::*;
    use std::str::FromStr;

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

    #[tokio::test]
    async fn credit_receive() {
        let mint_url = MintUrl::from_str("https://test.com/mint").unwrap();
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
            let y = cashu::dhke::hash_to_curve(p.secret.as_bytes())
                .expect("Hash to curve should not fail");
            Ok(y)
        });

        let crpocket = crpocket(db);

        let token = Token::new_bitcr(mint_url, proofs, None, crpocket.unit());
        let cashed = crpocket.receive(&connector, &k_infos, token).await.unwrap();
        assert_eq!(cashed, Amount::from(24u64));
    }

    #[tokio::test]
    async fn credit_receive_inactive_keyset() {
        let mint_url = MintUrl::from_str("https://test.com/mint").unwrap();
        let (mut info, keyset) = keys_test::generate_keyset();
        info.active = false;
        let k_infos = vec![KeySetInfo::from(info)];
        let amounts = [Amount::from(8u64), Amount::from(16u64)];
        let proofs = signatures_test::generate_proofs(&keyset, &amounts);

        let db = MockPocketRepository::new();
        let connector = MockMintConnector::new();

        let crpocket = crpocket(db);

        let token = Token::new_bitcr(mint_url, proofs, None, crpocket.unit());
        let result = crpocket.receive(&connector, &k_infos, token).await;
        assert!(matches!(result, Err(Error::ExpiredKeyset(_))));
    }

    #[tokio::test]
    async fn credit_receive_currency_mismatch() {
        let mint_url = MintUrl::from_str("https://test.com/mint").unwrap();
        let (mut info, keyset) = keys_test::generate_keyset();
        info.unit = CurrencyUnit::Usd;
        let k_infos = vec![KeySetInfo::from(info)];
        let amounts = [Amount::from(8u64), Amount::from(16u64)];
        let proofs = signatures_test::generate_proofs(&keyset, &amounts);

        let db = MockPocketRepository::new();
        let connector = MockMintConnector::new();

        let crpocket = crpocket(db);

        let token = Token::new_bitcr(mint_url, proofs, None, crpocket.unit());
        let result = crpocket.receive(&connector, &k_infos, token).await;
        assert!(matches!(result, Err(Error::CurrencyUnitMismatch(_, _))));
    }

    #[tokio::test]
    async fn credit_prepare_send() {
        let (info, keyset) = keys_test::generate_keyset();
        let k_infos = vec![KeySetInfo::from(info)];
        let amount = Amount::from(16u64);
        let amounts = [Amount::from(32u64), Amount::from(16u64)];
        let proofs = signatures_test::generate_proofs(&keyset, &amounts);
        let proofs_map: HashMap<PublicKey, cdk00::Proof> =
            HashMap::from_iter(proofs.into_iter().map(|p| {
                let y = cashu::dhke::hash_to_curve(p.secret.as_bytes())
                    .expect("Hash to curve should not fail");
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
        let proofs_map: HashMap<PublicKey, cdk00::Proof> =
            HashMap::from_iter(proofs.into_iter().map(|p| {
                let y = cashu::dhke::hash_to_curve(p.secret.as_bytes())
                    .expect("Hash to curve should not fail");
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
        let proofs_map: HashMap<PublicKey, cdk00::Proof> =
            HashMap::from_iter(proofs.into_iter().map(|p| {
                let y = cashu::dhke::hash_to_curve(p.secret.as_bytes())
                    .expect("Hash to curve should not fail");
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
}
