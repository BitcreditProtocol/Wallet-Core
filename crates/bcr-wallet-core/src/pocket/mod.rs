// ----- standard library imports
use std::collections::{HashMap, HashSet};
// ----- extra library imports
use async_trait::async_trait;
use bitcoin::bip32 as btc32;
use cashu::{
    Amount, CurrencyUnit, KeySet, KeySetInfo, amount::SplitTarget, nut00 as cdk00, nut01 as cdk01,
    nut03 as cdk03, nut07 as cdk07,
};
use uuid::Uuid;
// ----- local imports
use crate::{
    MintConnector,
    error::{Error, Result},
    sync,
};
// ----- local modules
pub mod credit;
pub mod debit;

// ----- end imports

///////////////////////////////////////////// SendReference
#[derive(Default, Clone)]
struct SendReference {
    rid: Uuid,
    send_proofs: Vec<cdk01::PublicKey>,
    swap_proof: Option<(Amount, cdk01::PublicKey)>,
}

///////////////////////////////////////////// PocketRepository
#[cfg_attr(test, mockall::automock)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
pub trait PocketRepository: sync::SendSync {
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
    if signatures.len() > premint.len() {
        tracing::error!(
            "signatures and premint len mismatch: {} > {}",
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
        if secret.amount != Amount::ZERO && signature.amount != secret.amount {
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

///////////////////////////////////////////// send_proofs
async fn send_proofs(
    send_proofs: Vec<cdk01::PublicKey>,
    swap_proof: Option<(Amount, cdk01::PublicKey)>,
    xpriv: btc32::Xpriv,
    db: &dyn PocketRepository,
    client: &dyn MintConnector,
    target_swap_keysetid: Option<cashu::Id>,
) -> Result<HashMap<cdk01::PublicKey, cdk00::Proof>> {
    let mut current_amount = Amount::ZERO;
    let mut sending_proofs: HashMap<cdk01::PublicKey, cdk00::Proof> = HashMap::new();
    for y in send_proofs {
        let proof = db.mark_as_pendingspent(y).await?;
        current_amount += proof.amount;
        sending_proofs.insert(y, proof);
    }
    let swapped_to_target = if let Some((swap_target, swap_y)) = swap_proof {
        let swap_proof = db.mark_as_pendingspent(swap_y).await?;
        let target_kid = target_swap_keysetid.unwrap_or(swap_proof.keyset_id);
        let swap_proof_keyset = client.get_mint_keyset(target_kid).await?;
        swap_proof_to_target(
            swap_proof,
            &swap_proof_keyset,
            swap_target,
            xpriv,
            db,
            client,
        )
        .await?
    } else {
        HashMap::new()
    };
    for y in swapped_to_target.keys() {
        let proof = db.mark_as_pendingspent(*y).await?;
        current_amount += proof.amount;
        sending_proofs.insert(*y, proof);
    }
    Ok(sending_proofs)
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
