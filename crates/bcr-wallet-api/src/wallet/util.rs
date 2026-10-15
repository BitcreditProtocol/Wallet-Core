use std::collections::{HashMap, HashSet};

use crate::{
    ClowderMintConnector,
    error::{Error, Result},
};
use bcr_common::{
    cashu::{self, Proof, amount::SplitTarget},
    cdk_common,
    core::{
        htlc::{self, wallet_htlc},
        signature::verify_spending_conditions,
        swap::wallet::prepare_swap,
    },
    ecash,
    ecash::KeySet,
    wire::{
        exchange::{self, OfflineExchangeRequest},
        keys::ProofFingerprint,
    },
};
use bcr_wallet_core::types::Transaction;
use bitcoin::{
    hashes::{Hash, sha256::Hash as Sha256},
    secp256k1,
};

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
            keyset_id: p.keyset_id.into(),
            c: *p.c,
            dleq: Some(dleq),
            y: *p.y()?,
        });
    }

    Ok((fingerprints, secrets))
}

pub fn build_offline_exchange_request(
    alpha_id: &secp256k1::PublicKey,
    evidence_digest: &[u8; 32],
    proofs: Vec<Proof>,
    wallet_sk: &cashu::SecretKey,
) -> Result<OfflineExchangeRequest> {
    let (fingerprints, secrets) = proofs_to_fingerprints(proofs)?;
    let hashes: Vec<Sha256> = secrets
        .iter()
        .map(|secret| Sha256::hash(&secret.to_bytes()))
        .collect();
    let wallet_pk = wallet_sk.public_key();
    let exchange_digest = exchange::exchange_digest(
        alpha_id,
        evidence_digest,
        &fingerprints,
        &hashes,
        &wallet_pk,
    );
    let keypair = secp256k1::Keypair::from_secret_key(secp256k1::global::SECP256K1, wallet_sk);
    let wallet_signature = secp256k1::global::SECP256K1
        .sign_schnorr_no_aux_rand(&exchange::exchange_message(&exchange_digest), &keypair);
    Ok(OfflineExchangeRequest {
        fingerprints,
        hashes,
        wallet_pk,
        wallet_signature,
    })
}

pub async fn keyset_infos(
    client: &dyn ClowderMintConnector,
) -> Result<HashMap<ecash::Id, ecash::KeySetInfo>> {
    Ok(client
        .get_mint_keysets()
        .await?
        .into_iter()
        .map(|k| (k.id, k))
        .collect())
}

pub async fn post_offline_exchange(
    substitute_client: &dyn ClowderMintConnector,
    substitute_clowder_id: secp256k1::PublicKey,
    alpha_id: &secp256k1::PublicKey,
    evidence_digest: &[u8; 32],
    proofs: Vec<Proof>,
    wallet_sk: &cashu::SecretKey,
) -> Result<Vec<Proof>> {
    let request = build_offline_exchange_request(alpha_id, evidence_digest, proofs, wallet_sk)?;
    let beta_proofs = substitute_client
        .post_offline_exchange(
            request.fingerprints,
            request.hashes,
            *request.wallet_pk,
            request.wallet_signature,
            substitute_clowder_id,
        )
        .await?;
    Ok(beta_proofs)
}

pub fn unlock_offline_exchanged(
    mut beta_proofs: Vec<Proof>,
    secrets: &[cashu::secret::Secret],
    wallet_sk: &cashu::SecretKey,
    issuer: &secp256k1::PublicKey,
    margin: u64,
) -> Result<Vec<Proof>> {
    let by_hash_lock: HashMap<Sha256, &cashu::secret::Secret> = secrets
        .iter()
        .map(|s| (Sha256::hash(&s.to_bytes()), s))
        .collect();
    let now = time::OffsetDateTime::now_utc().unix_timestamp() as u64;
    for p in beta_proofs.iter_mut() {
        let (hash_lock, conditions) = htlc::htlc_lock(p)
            .map_err(|_| Error::Swap("issued proof is not HTLC locked".into()))?;
        htlc::validate_exchange_conditions(
            &wallet_sk.public_key(),
            &(*issuer).into(),
            &conditions,
            margin,
            now,
        )
        .map_err(|e| Error::Swap(format!("issued proof has an unusable lock: {e}")))?;
        let secret = by_hash_lock
            .get(&hash_lock)
            .ok_or_else(|| Error::Swap("issued proof carries an unknown hash lock".into()))?;
        htlc::sign_htlc(p, &secret.to_string(), wallet_sk)?;
        verify_spending_conditions(p)
            .map_err(|e| Error::Swap(format!("issued proof can't be unlocked: {e}")))?;
    }
    Ok(beta_proofs)
}

pub fn remove_dleq_from_proofs(mut proofs: Vec<Proof>) -> Vec<Proof> {
    for proof in proofs.iter_mut() {
        proof.dleq = None;
    }
    proofs
}

/// What locking proofs into an HTLC at their mint needs, prepared before anything is swapped.
pub struct HtlcLock {
    /// Unix timestamp in seconds after which the wallet key alone unlocks the outputs
    pub lock_time: u64,
    pub keysets: HashMap<ecash::Id, KeySet>,
    pub premints: HashMap<ecash::Id, cashu::PreMintSecrets>,
}

pub async fn prepare_htlc_lock(
    tstamp: u64,
    client: &dyn ClowderMintConnector,
    proofs: &[cashu::Proof],
    hash_lock: Sha256,
    key_locks: Vec<secp256k1::PublicKey>,
    wallet_pubkey: secp256k1::PublicKey,
) -> Result<HtlcLock> {
    tracing::debug!("preparing HTLC lock");
    let key_locks: Vec<cashu::PublicKey> = key_locks.into_iter().map(|k| k.into()).collect();

    // total hops * time per hop + 2 hops buffer
    let lock_time =
        tstamp + (key_locks.len() as u64 + 2) * crate::config::LOCK_REDUCTION_SECONDS_PER_HOP;

    // fetch keysets infos for the given client
    let infos: HashMap<cashu::Id, ecash::KeySetInfo> = client
        .get_mint_keysets()
        .await?
        .into_iter()
        .map(|k| (k.id.into(), k))
        .collect();

    let swap_plan = prepare_swap(proofs, &infos)?;

    let kids: HashSet<ecash::Id> = proofs.iter().map(|p| p.keyset_id.into()).collect();
    let mut keysets: HashMap<ecash::Id, KeySet> = HashMap::new();
    for kid in kids.iter() {
        let keyset = client.get_mint_keyset(*kid).await?;
        keysets.insert(*kid, keyset);
    }

    let htlc = wallet_htlc(hash_lock, lock_time, key_locks, wallet_pubkey.into())?;

    // prepare the premints
    let mut premints: HashMap<ecash::Id, cashu::PreMintSecrets> = HashMap::new();
    for (kid, amount) in swap_plan {
        let premint = cashu::PreMintSecrets::with_conditions(
            kid,
            amount,
            &SplitTarget::None,
            &htlc,
            &bcr_wallet_core::util::to_fee_and_amounts(&keysets[&kid.into()]),
        )?;
        premints.insert(kid.into(), premint);
    }

    Ok(HtlcLock {
        lock_time,
        keysets,
        premints,
    })
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
    use bitcoin::hex::FromHex;
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
    fn offline_exchange_request_matches_the_mint_retry_fixture() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/offline_exchange_retry.json"
        ))
        .expect("fixture parses");
        let field = |key: &str| fixture[key].clone();
        let request = build_offline_exchange_request(
            &serde_json::from_value(field("alpha_id")).unwrap(),
            &<[u8; 32]>::from_hex(field("evidence_digest").as_str().unwrap()).unwrap(),
            vec![serde_json::from_value(field("proof")).unwrap()],
            &serde_json::from_value(field("wallet_sk")).unwrap(),
        )
        .expect("builds");
        assert_eq!(serde_json::to_value(&request).unwrap(), field("request"));
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
            assert_eq!(fp.keyset_id, expected_keyset_ids[idx].into());
            assert_eq!(fp.c, *expected_cs[idx]);
            assert_eq!(fp.dleq, expected_dleqs[idx]);
            assert_eq!(fp.y, *expected_ys[idx]);
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
