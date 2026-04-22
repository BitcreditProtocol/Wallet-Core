use crate::{
    ClowderMintConnector,
    error::{Error, Result},
    wallet::types::SwapConfig,
};
use bcr_common::cdk::{self};
use bcr_common::{
    cashu::{self, HTLCWitness, Proof, ProofsMethods},
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
            witness: None,
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
    unit: cashu::CurrencyUnit,
    tstamp: u64,
    client: &dyn ClowderMintConnector,
    proofs: Vec<cashu::Proof>,
    hash_lock: Sha256,
    key_locks: Vec<secp256k1::PublicKey>,
    wallet_pubkey: secp256k1::PublicKey,
    swap_config: SwapConfig,
) -> Result<Vec<cashu::Proof>> {
    tracing::debug!("HTLC-locking proofs");
    let amount = proofs.total_amount()?;

    let key_locks: Vec<cashu::PublicKey> = key_locks.into_iter().map(|k| k.into()).collect();

    // total hops * time per hop + 2 hops buffer
    let lock_time =
        tstamp + (key_locks.len() as u64 + 2) * crate::config::LOCK_REDUCTION_SECONDS_PER_HOP;

    // fetch keysets infos for the given client
    let infos = client.get_mint_keysets().await?;

    let active_keyset_id = infos
        .iter()
        .find(|info| info.active && info.unit == unit)
        .ok_or(Error::NoActiveKeyset)?
        .id;

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
    let split_target = cashu::amount::SplitTarget::None;
    let premints =
        cashu::PreMintSecrets::with_conditions(active_keyset_id, amount, &split_target, &htlc)?;

    let signatures = crate::pocket::committed_swap(
        client,
        None,
        proofs,
        premints.blinded_messages(),
        &swap_config,
        std::collections::HashMap::new(),
    )
    .await?;

    let keyset = client.get_mint_keyset(active_keyset_id).await?;
    let proofs = crate::pocket::unblind_proofs(&keyset, signatures, premints);

    Ok(proofs)
}

pub fn tx_can_be_refreshed(tx: &cdk::wallet::types::Transaction) -> bool {
    // Only refresh outgoing transactions
    if matches!(
        tx.direction,
        cdk::wallet::types::TransactionDirection::Incoming
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
