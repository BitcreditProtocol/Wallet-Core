use std::collections::{HashMap, HashSet};

use crate::{
    ClowderMintConnector,
    error::{Error, Result},
    pocket::unblind_proofs,
    wallet::types::SwapConfig,
};
use bcr_common::{
    cashu::{self, HTLCWitness, KeySet, Proof, amount::SplitTarget},
    cdk_common,
    core::swap::wallet::prepare_swap,
    wire::keys::ProofFingerprint,
};
use bcr_wallet_core::types::Transaction;
use bitcoin::{hashes::sha256::Hash as Sha256, secp256k1};
use secp256k1::schnorr::Signature;

//////////////////////////////////// utils
pub fn proofs_to_fingerprints(
    proofs: Vec<Proof>,
) -> Result<(Vec<ProofFingerprint>, Vec<cashu::secret::Secret>)> {
    let mut secrets = Vec::with_capacity(proofs.len());
    let mut fingerprints = Vec::with_capacity(proofs.len());

    for p in proofs.iter() {
        let dleq = p.dleq.clone().ok_or(Error::MissingDleq)?;
        secrets.push(p.secret.clone());

        fingerprints.push(ProofFingerprint {
            amount: p.amount.into(),
            keyset_id: p.keyset_id,
            c: p.c,
            dleq: Some(dleq),
            y: p.y()?,
        });
    }

    Ok((fingerprints, secrets))
}

pub fn remove_dleq_from_proofs(mut proofs: Vec<Proof>) -> Vec<Proof> {
    for proof in proofs.iter_mut() {
        proof.dleq = None;
    }
    proofs
}

pub fn sign_htlc_proof(
    proof: &mut Proof,
    preimage: &str,
    wallet_secret: &cashu::SecretKey,
) -> Result<()> {
    let msg: Vec<u8> = proof.secret.to_bytes();
    let signature: Signature = wallet_secret
        .sign(&msg)
        .map_err(|err| Error::SchnorrSignature(format!("signing error: {err}")))?;

    let signatures = vec![signature.to_string()];

    proof.witness = Some(cashu::Witness::HTLCWitness(HTLCWitness {
        preimage: preimage.to_string(),
        signatures: Some(signatures),
    }));

    Ok(())
}

/// The hash lock a proof's HTLC spending condition names, if it has one.
pub fn htlc_hash_lock(proof: &Proof) -> Option<Sha256> {
    match (&proof.secret).try_into().ok()? {
        cashu::SpendingConditions::HTLCConditions { data, .. } => Some(data),
        cashu::SpendingConditions::P2PKConditions { .. } => None,
    }
}

pub async fn htlc_lock(
    tstamp: u64,
    client: &dyn ClowderMintConnector,
    proofs: Vec<cashu::Proof>,
    hash_lock: Sha256,
    key_locks: Vec<secp256k1::PublicKey>,
    wallet_pubkey: secp256k1::PublicKey,
    swap_config: SwapConfig,
    beta: &dyn crate::pocket::BetaProvider,
) -> Result<Vec<cashu::Proof>> {
    tracing::debug!("HTLC-locking proofs");
    let key_locks: Vec<cashu::PublicKey> = key_locks.into_iter().map(|k| k.into()).collect();

    // total hops * time per hop + 2 hops buffer
    let lock_time =
        tstamp + (key_locks.len() as u64 + 2) * crate::config::LOCK_REDUCTION_SECONDS_PER_HOP;

    // fetch keysets infos for the given client
    let infos: HashMap<cashu::Id, cashu::KeySetInfo> = client
        .get_mint_keysets()
        .await?
        .into_iter()
        .map(|k| (k.id, k))
        .collect();

    let swap_plan = prepare_swap(&proofs, &infos)?;

    let kids: HashSet<cashu::Id> = proofs.iter().map(|p| p.keyset_id).collect();
    let mut keysets: HashMap<cashu::Id, KeySet> = HashMap::new();
    for kid in kids.iter() {
        let keyset = client.get_mint_keyset(*kid).await?;
        keysets.insert(*kid, keyset);
    }

    let n = key_locks.len() as u64;
    let p2pk = cashu::Conditions::new(
        Some(lock_time),
        Some(key_locks),
        Some(vec![wallet_pubkey.into()]),
        Some(n),
        None,
        Some(1),
    )?;
    let htlc = cashu::SpendingConditions::new_htlc_hash(&hash_lock.to_string(), Some(p2pk))?;

    // prepare the premints
    let mut premints: HashMap<cashu::Id, cashu::PreMintSecrets> = HashMap::new();
    for (kid, amount) in swap_plan {
        let premint = cashu::PreMintSecrets::with_conditions(
            kid,
            amount,
            &SplitTarget::None,
            &htlc,
            &bcr_wallet_core::util::to_fee_and_amounts(&keysets[&kid]),
        )?;
        premints.insert(kid, premint);
    }

    let blinds: Vec<cashu::BlindedMessage> = premints
        .values()
        .flat_map(|premint| premint.blinded_messages())
        .collect();
    let attestation = beta.attest(&proofs).await?;
    let signatures = crate::pocket::committed_swap(
        client,
        None,
        proofs,
        blinds,
        &swap_config,
        std::collections::HashMap::new(),
        attestation,
    )
    .await?;

    let mut result_proofs = Vec::new();
    let mut sigs_by_kid: HashMap<cashu::Id, Vec<cashu::BlindSignature>> = HashMap::new();
    for signature in signatures {
        sigs_by_kid
            .entry(signature.keyset_id)
            .or_default()
            .push(signature);
    }

    for (kid, sigs) in sigs_by_kid.into_iter() {
        let premint = premints.remove(&kid).expect("premint should be here");
        let keyset = keysets.get(&kid).expect("keyset should be here");
        let proofs = unblind_proofs(keyset, sigs, premint);

        result_proofs.extend(proofs);
    }

    Ok(result_proofs)
}

pub fn tx_can_be_refreshed(tx: &Transaction) -> bool {
    // Only refresh outgoing transactions
    if matches!(
        tx.direction,
        cdk_common::wallet::TransactionDirection::Incoming
    ) {
        return false;
    }

    // Only refresh pending transactions
    if !matches!(tx.status, crate::types::TransactionStatus::Pending) {
        return false;
    }
    true
}

pub fn update_optional_field<T: Clone + PartialEq>(
    field_to_update: &mut Option<T>,
    field: &Option<T>,
    changed: &mut bool,
) {
    if field_to_update != field {
        *field_to_update = field.clone();
        *changed = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcr_common::{
        cashu::{self, Amount},
        core_tests,
    };
    use bcr_wallet_core::{
        name::Name,
        types::{PaymentType, TransactionFees, TransactionStatus},
        util::to_mint_url,
    };
    use std::str::FromStr;
    use uuid::Uuid;

    fn add_test_dleqs(proofs: &mut [cashu::Proof]) {
        for proof in proofs {
            proof.dleq = Some(cashu::nut12::ProofDleq {
                e: cashu::SecretKey::generate(),
                s: cashu::SecretKey::generate(),
                r: cashu::SecretKey::generate(),
            });
        }
    }

    fn test_proofs(amounts: &[Amount]) -> Vec<cashu::Proof> {
        let (_info, keyset) = core_tests::generate_random_ecash_keyset();
        let mut proofs = core_tests::generate_random_ecash_proofs(&keyset, amounts);
        add_test_dleqs(&mut proofs);
        proofs
    }

    #[test]
    fn test_proofs_to_fingerprints_success() {
        let proofs = test_proofs(&[Amount::from(8), Amount::from(4)]);
        let expected_secrets: Vec<_> = proofs.iter().map(|p| p.secret.clone()).collect();
        let expected_amounts: Vec<_> = proofs.iter().map(|p| p.amount).collect();
        let expected_keyset_ids: Vec<_> = proofs.iter().map(|p| p.keyset_id).collect();
        let expected_cs: Vec<_> = proofs.iter().map(|p| p.c).collect();
        let expected_dleqs: Vec<_> = proofs.iter().map(|p| p.dleq.clone()).collect();
        let expected_ys: Vec<_> = proofs.iter().map(|p| p.y().expect("works")).collect();
        let (fingerprints, secrets) = proofs_to_fingerprints(proofs).expect("works");
        assert_eq!(fingerprints.len(), 2);
        assert_eq!(secrets, expected_secrets);
        for (idx, fp) in fingerprints.iter().enumerate() {
            assert_eq!(fp.amount, expected_amounts[idx].to_u64());
            assert_eq!(fp.keyset_id, expected_keyset_ids[idx]);
            assert_eq!(fp.c, expected_cs[idx]);
            assert_eq!(fp.dleq, expected_dleqs[idx]);
            assert_eq!(fp.y, expected_ys[idx]);
        }
    }

    #[test]
    fn test_proofs_to_fingerprints_returns_missing_dleq_error() {
        let mut proofs = test_proofs(&[Amount::from(8)]);
        proofs[0].dleq = None;
        let res = proofs_to_fingerprints(proofs);
        assert!(matches!(res, Err(Error::MissingDleq)));
    }

    #[test]
    fn test_proofs_to_fingerprints_empty_vec() {
        let (fingerprints, secrets) = proofs_to_fingerprints(vec![]).expect("works");
        assert!(fingerprints.is_empty());
        assert!(secrets.is_empty());
    }

    #[test]
    fn test_remove_dleq_from_proofs_removes_all_dleqs() {
        let proofs = test_proofs(&[Amount::from(8), Amount::from(4), Amount::from(2)]);
        assert!(proofs.iter().all(|p| p.dleq.is_some()));
        let stripped = remove_dleq_from_proofs(proofs);
        assert_eq!(stripped.len(), 3);
        assert!(stripped.iter().all(|p| p.dleq.is_none()));
    }

    #[test]
    fn test_remove_dleq_from_proofs_preserves_other_fields() {
        let proofs = test_proofs(&[Amount::from(8)]);
        let original = proofs[0].clone();
        let stripped = remove_dleq_from_proofs(proofs).pop().expect("works");
        assert_eq!(stripped.amount, original.amount);
        assert_eq!(stripped.keyset_id, original.keyset_id);
        assert_eq!(stripped.secret, original.secret);
        assert_eq!(stripped.c, original.c);
        assert_eq!(stripped.witness, original.witness);
        assert!(stripped.dleq.is_none());
    }

    #[test]
    fn test_sign_htlc_proof_signature_verifies_against_secret_bytes() {
        let mut proofs = test_proofs(&[Amount::from(8)]);
        let proof = proofs.first_mut().expect("proof exists");
        let wallet_secret = cashu::SecretKey::generate();
        let wallet_public_key = wallet_secret.public_key();
        let message = proof.secret.to_bytes();
        let preimage = "test-preimage";
        sign_htlc_proof(proof, preimage, &wallet_secret).expect("works");
        let witness = proof.witness.as_ref().expect("witness exists");
        let signature = match witness {
            cashu::Witness::HTLCWitness(htlc_witness) => htlc_witness.signatures.as_ref().unwrap()
                [0]
            .parse::<secp256k1::schnorr::Signature>()
            .unwrap(),
            other => panic!("expected HTLC witness, got {other:?}"),
        };

        wallet_public_key
            .verify(&message, &signature)
            .expect("works");
    }

    #[test]
    fn test_tx_can_be_refreshed_false_for_incoming_pending() {
        let tx = test_tx(
            cdk_common::wallet::TransactionDirection::Incoming,
            TransactionStatus::Pending,
        );
        assert!(!tx_can_be_refreshed(&tx));
    }

    #[test]
    fn test_tx_can_be_refreshed_false_for_outgoing_canceled() {
        let tx = test_tx(
            cdk_common::wallet::TransactionDirection::Outgoing,
            TransactionStatus::Canceled,
        );
        assert!(!tx_can_be_refreshed(&tx));
    }

    #[test]
    fn test_tx_can_be_refreshed_true_for_outgoing_pending() {
        let tx = test_tx(
            cdk_common::wallet::TransactionDirection::Outgoing,
            TransactionStatus::Pending,
        );
        assert!(tx_can_be_refreshed(&tx));
    }

    fn test_tx(
        direction: cdk_common::wallet::TransactionDirection,
        status: TransactionStatus,
    ) -> Transaction {
        Transaction {
            id: Uuid::new_v4(),
            mint_url: to_mint_url(&url::Url::from_str("https://mint.example").unwrap()),
            fees: TransactionFees::default(),
            direction,
            memo: None,
            tstamp: 5,
            unit: cashu::CurrencyUnit::Sat,
            ys: vec![],
            amount: cashu::Amount::from(42),
            status,
            payment_type: PaymentType::Token,
            quote_id: None,
            payment_request_id: None,
            btc_tx_id: None,
            nostr_event_id: None,
            contact_node_id: None,
            linked_txs: vec![],
        }
    }

    #[test]
    fn update_optional_field_baseline() {
        let mut field_to_update = Some(String::from("hi"));
        let mut changed = false;
        update_optional_field(
            &mut field_to_update,
            &Some(String::from("hello")),
            &mut changed,
        );
        assert!(changed);
        assert_eq!(Some(String::from("hello")), field_to_update);
    }

    #[test]
    fn update_optional_field_same() {
        let mut field_to_update = Some(String::from("hello"));
        let mut changed = false;
        update_optional_field(
            &mut field_to_update,
            &Some(String::from("hello")),
            &mut changed,
        );
        assert!(!changed);
        assert_eq!(Some(String::from("hello")), field_to_update);
    }

    #[test]
    fn update_optional_field_none() {
        let mut field_to_update: Option<Name> = None;
        let mut changed = false;
        update_optional_field(&mut field_to_update, &None, &mut changed);
        assert!(!changed);
        assert_eq!(None, field_to_update);
    }

    #[test]
    fn update_optional_field_some_none() {
        let mut field_to_update = Some(String::from("hi"));
        let mut changed = false;
        update_optional_field(&mut field_to_update, &None, &mut changed);
        assert!(changed);
        assert_eq!(None, field_to_update);
    }
}
