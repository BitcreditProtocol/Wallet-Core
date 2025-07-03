// ----- standard library imports
use std::collections::HashMap;
// ----- extra library imports
use anyhow::Error as AnyError;
use async_trait::async_trait;
use bcr_wallet_lib::wallet::Token;
use bitcoin::bip32 as btc32;
use cashu::{
    Amount, KeySet, KeySetInfo, amount::SplitTarget, nut00 as cdk00, nut02 as cdk02, nut03 as cdk03,
};
use cdk::wallet::MintConnector;
// ----- local imports
use crate::{
    error::{Error, Result},
    wallet::{CreditPocket, DebitPocket, Pocket},
};

// ----- end imports

#[async_trait(?Send)]
pub trait PocketRepository {
    async fn store_new(&self, proof: cdk00::Proof) -> Result<()>;
    async fn list_unspent(&self) -> Result<Vec<cdk00::Proof>>;

    async fn counter(&self, kid: cdk02::Id) -> Result<u32>;
    async fn increment_counter(&self, kid: cdk02::Id, old: u32, increment: u32) -> Result<()>;
}

///////////////////////////////////////////// credit pocket
pub struct CrPocket<Repo> {
    pub unit: cashu::CurrencyUnit,
    pub db: Repo,
    pub xpriv: btc32::Xpriv,
}

#[async_trait(?Send)]
impl<Repo> Pocket for CrPocket<Repo>
where
    Repo: PocketRepository,
{
    async fn balance(&self) -> Result<Amount> {
        let proofs: Vec<cdk00::Proof> = self.db.list_unspent().await?;
        let total = proofs
            .into_iter()
            .fold(Amount::ZERO, |acc, proof| acc + proof.amount);
        Ok(total)
    }

    fn is_mine(&self, token: &Token) -> bool {
        matches!(token, Token::BitcrV4(..)) && token.unit().as_ref() == Some(&self.unit)
    }

    async fn receive(
        &self,
        client: &dyn MintConnector,
        k_infos: &[KeySetInfo],
        token: Token,
    ) -> Result<cashu::Amount> {
        let proofs = token.proofs(k_infos)?;
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
            let keyset = client.get_mint_keyset(*kid).await?;
            if keyset.unit != self.unit {
                return Err(Error::CurrencyUnitMismatch(self.unit.clone(), keyset.unit));
            }
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
}

impl<Repo> CreditPocket for CrPocket<Repo> where Repo: PocketRepository {}

///////////////////////////////////////////// debit pocket
pub struct DbPocket<Repo> {
    pub unit: cashu::CurrencyUnit,
    pub db: Repo,
    pub xpriv: btc32::Xpriv,
}

#[async_trait(?Send)]
impl<Repo> Pocket for DbPocket<Repo>
where
    Repo: PocketRepository,
{
    async fn balance(&self) -> Result<cashu::Amount> {
        let proofs: Vec<cdk00::Proof> = self.db.list_unspent().await?;
        let total = proofs
            .into_iter()
            .fold(Amount::ZERO, |acc, proof| acc + proof.amount);
        Ok(total)
    }

    fn is_mine(&self, token: &Token) -> bool {
        matches!(token, Token::CashuV4(..)) && token.unit().as_ref() == Some(&self.unit)
    }

    async fn receive(
        &self,
        client: &dyn MintConnector,
        k_infos: &[KeySetInfo],
        token: Token,
    ) -> Result<cashu::Amount> {
        let proofs = token.proofs(k_infos)?;
        let total_amount = proofs.iter().fold(Amount::ZERO, |acc, p| acc + p.amount);
        let kinfos = client.get_mint_keysets().await?.keysets;
        let active = kinfos
            .into_iter()
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
}

impl<Repo> DebitPocket for DbPocket<Repo> where Repo: PocketRepository {}

pub struct DummyPocket {}

#[async_trait(?Send)]
impl Pocket for DummyPocket {
    async fn balance(&self) -> Result<cashu::Amount> {
        Ok(cashu::Amount::ZERO)
    }
    fn is_mine(&self, _token: &Token) -> bool {
        false
    }
    async fn receive(
        &self,
        _client: &dyn MintConnector,
        _k_infos: &[KeySetInfo],
        _token: Token,
    ) -> Result<cashu::Amount> {
        Err(Error::Any(AnyError::msg("DymmyPocket is dummy")))
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

#[cfg(test)]
mod tests {
    use super::*;
    use bcr_wdc_utils::{
        keys::{self as keys_utils, test_utils as keys_test},
        signatures::test_utils as signatures_test,
    };

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
}
