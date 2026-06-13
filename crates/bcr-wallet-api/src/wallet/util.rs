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

pub fn tx_can_be_refreshed(tx: &cdk_common::wallet::Transaction) -> bool {
    // Only refresh outgoing transactions
    if matches!(
        tx.direction,
        cdk_common::wallet::TransactionDirection::Incoming
    ) {
        return false;
    }

    // Only refresh pending transactions
    let p_status = crate::types::get_transaction_status(&tx.metadata);
    if !matches!(p_status, crate::types::TransactionStatus::Pending) {
        return false;
    }
    true
}
