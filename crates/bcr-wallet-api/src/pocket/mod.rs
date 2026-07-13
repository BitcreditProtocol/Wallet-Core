use crate::{
    ClowderMintConnector,
    error::{Error, Result},
    wallet::types::SwapConfig,
};
use async_trait::async_trait;
use bcr_common::{
    cashu::{
        self, Amount, CurrencyUnit, KeySet, KeySetInfo, ProofsMethods, amount::SplitTarget,
        nut00 as cdk00, nut01 as cdk01, nut07 as cdk07,
    },
    core::swap::wallet::prepare_swap,
    wire::{attestation as wire_attestation, keys as wire_keys},
};
use bcr_wallet_core::{
    SendSync,
    types::{Seed, SendSummary},
};
use bcr_wallet_persistence::PocketRepository;
use rand::seq::IndexedRandom;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use uuid::Uuid;

pub mod debit;
mod restore;
#[cfg(test)]
pub mod test_utils;

/// trait that represents a single compartment in our wallet where we store proofs/tokens of the
/// same currency emitted by the same mint
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait PocketApi: SendSync {
    fn unit(&self) -> CurrencyUnit;
    fn set_beta_provider(&mut self, beta_provider: Arc<dyn BetaProvider>);
    async fn balance(&self, keysets_info: &HashMap<cashu::Id, KeySetInfo>)
    -> Result<PocketBalance>;
    async fn receive_proofs(
        &self,
        client: Arc<dyn ClowderMintConnector>,
        keysets_info: &HashMap<cashu::Id, KeySetInfo>,
        proofs: Vec<cashu::Proof>,
        swap_config: SwapConfig,
    ) -> Result<(Amount, Vec<cashu::PublicKey>)>;
    async fn prepare_send(
        &self,
        amount: Amount,
        keysets_info: &HashMap<cashu::Id, KeySetInfo>,
    ) -> Result<SendSummary>;
    async fn send_proofs(
        &self,
        rid: Uuid,
        keysets_info: &HashMap<cashu::Id, KeySetInfo>,
        client: Arc<dyn ClowderMintConnector>,
        swap_config: SwapConfig,
    ) -> Result<HashMap<cashu::PublicKey, cashu::Proof>>;
    async fn restore_local_proofs(
        &self,
        keysets_info: &HashMap<cashu::Id, KeySetInfo>,
        client: Arc<dyn ClowderMintConnector>,
    ) -> Result<usize>;
    async fn delete_proofs(&self) -> Result<HashMap<cashu::Id, Vec<cashu::Proof>>>;
    async fn return_proofs_to_send_for_offline_payment(
        &self,
        rid: Uuid,
    ) -> Result<(Amount, HashMap<cashu::PublicKey, cashu::Proof>)>;
    /// WARN: Only used for hacky offline pay by token - will be removed
    async fn swap_to_unlocked_substitute_proofs(
        &self,
        proofs: Vec<cashu::Proof>,
        keysets_info: &HashMap<cashu::Id, KeySetInfo>,
        keysets: HashMap<cashu::Id, KeySet>,
        client: Arc<dyn ClowderMintConnector>,
        beta_provider: RandomBetaProvider,
        send_amount: Amount,
        swap_config: SwapConfig,
    ) -> Result<Vec<cashu::Proof>>;
    async fn dev_mode_detailed_balance(
        &self,
        keysets_info: &HashMap<cashu::Id, KeySetInfo>,
    ) -> Result<HashMap<cashu::Id, (Option<u64>, Amount)>>;
    async fn delete(&self) -> Result<()>;
}

#[derive(Default, Debug, Clone)]
pub struct PocketBalance {
    pub debit: Amount,
    pub credit: Amount,
}

///////////////////////////////////////////// BetaProvider
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub(crate) trait BetaProvider: SendSync {
    async fn attest(
        &self,
        proofs: &[cdk00::Proof],
    ) -> Result<wire_attestation::IssuanceAttestation>;
    fn random_client(&self) -> Arc<dyn ClowderMintConnector>;
    fn alpha_id(&self) -> bitcoin::secp256k1::PublicKey;
}

pub(crate) struct RandomBetaProvider {
    betas: Vec<Arc<dyn ClowderMintConnector>>,
    alpha_id: bitcoin::secp256k1::PublicKey,
}

impl RandomBetaProvider {
    pub fn new(
        betas: Vec<Arc<dyn ClowderMintConnector>>,
        alpha_id: bitcoin::secp256k1::PublicKey,
    ) -> Result<Self> {
        if betas.is_empty() {
            return Err(Error::NoBetas);
        }
        Ok(Self { betas, alpha_id })
    }
}

#[async_trait]
impl BetaProvider for RandomBetaProvider {
    async fn attest(
        &self,
        proofs: &[cdk00::Proof],
    ) -> Result<wire_attestation::IssuanceAttestation> {
        let max = self
            .betas
            .len()
            .min(crate::config::MAX_ATTESTATION_ATTEMPTS);
        let selected: Vec<_> = self
            .betas
            .choose_multiple(&mut rand::rng(), max)
            .cloned()
            .collect();
        let fingerprints = wire_attestation::project_to_fingerprints(proofs)?;
        let mut last_err = None;
        for beta in &selected {
            match fetch_attestation(beta.as_ref(), self.alpha_id, proofs).await {
                Ok(att) => {
                    match wire_attestation::authenticate_attestation_fingerprints(
                        &self.alpha_id,
                        &fingerprints,
                        &att,
                        |_| true,
                    ) {
                        Ok(()) => return Ok(att),
                        Err(e) => {
                            tracing::warn!("Beta returned invalid attestation: {e}");
                            last_err = Some(e.into());
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("Beta attestation attempt failed: {e}");
                    last_err = Some(e);
                }
            }
        }
        Err(last_err.expect("betas is non-empty, so at least one attempt was made"))
    }

    fn random_client(&self) -> Arc<dyn ClowderMintConnector> {
        self.betas
            .choose(&mut rand::rng())
            .expect("betas is non-empty")
            .clone()
    }

    fn alpha_id(&self) -> bitcoin::secp256k1::PublicKey {
        self.alpha_id
    }
}

///////////////////////////////////////////// SendReference
#[derive(Debug, Clone)]
struct SendReference {
    rid: Uuid,
    target_amount: Amount,
    plan: SendPlan,
}

#[derive(Debug, Clone)]
enum SendPlan {
    Ready {
        proofs: Vec<cdk01::PublicKey>,
    },
    NeedSwap {
        inputs: Vec<cdk01::PublicKey>,
        target: Amount,
        estimated_fee: Amount,
    },
}

///////////////////////////////////////////// unblind_proofs
pub(crate) fn unblind_proofs(
    keyset: &KeySet,
    signatures: Vec<cdk00::BlindSignature>,
    premint: cdk00::PreMintSecrets,
) -> Vec<cdk00::Proof> {
    let mut proofs: Vec<cdk00::Proof> = Vec::new();
    if signatures.len() > premint.len() {
        tracing::error!(
            "signatures and premint len mismatch: {} > {}",
            signatures.len(),
            premint.len()
        )
    }
    for (signature, secret) in signatures.into_iter().zip(premint.iter()) {
        let kid = signature.keyset_id;
        let amount = signature.amount;
        // WARNING: due to a bug in `into_iter()` in cashu 0.13.1 we need to `iter()` and clone the secret
        // fixed in 0.14.0
        match bcr_common::core::signature::unblind_ecash_signature(
            keyset,
            secret.clone(),
            signature,
        ) {
            Ok(proof) => proofs.push(proof),
            Err(e) => {
                tracing::error!(
                    "unblind_ecash_signature failed: kid: {kid}, amount: {amount}, error: {e}",
                );
            }
        }
    }
    proofs
}

///////////////////////////////////////////// fetch_attestation
pub(crate) async fn fetch_attestation(
    beta_client: &dyn ClowderMintConnector,
    alpha_id: bitcoin::secp256k1::PublicKey,
    inputs: &[cdk00::Proof],
) -> Result<wire_attestation::IssuanceAttestation> {
    let fingerprints: Vec<_> = inputs
        .iter()
        .cloned()
        .map(wire_keys::ProofFingerprint::try_from)
        .collect::<std::result::Result<_, cashu::nut00::Error>>()?;
    let request = wire_attestation::IssuanceAttestationRequest {
        alpha_id,
        inputs: fingerprints,
    };
    beta_client.post_attest_issuance(request).await
}

///////////////////////////////////////////// committed_swap
/// Commit → optionally store → swap → optionally delete.
/// Returns the blind signatures from the swap response.
pub(crate) async fn committed_swap(
    client: &dyn ClowderMintConnector,
    db: Option<&dyn PocketRepository>,
    inputs: Vec<cdk00::Proof>,
    outputs: Vec<cdk00::BlindedMessage>,
    swap_config: &SwapConfig,
    premints: HashMap<cashu::Id, cdk00::PreMintSecrets>,
    attestation: wire_attestation::IssuanceAttestation,
) -> Result<Vec<cdk00::BlindSignature>> {
    // Remove dleqs from same-mint swaps to not give up a blinding factor
    let inputs = crate::wallet::util::remove_dleq_from_proofs(inputs);
    let commit_result = client
        .post_swap_commitment(
            inputs.clone(),
            outputs.clone(),
            swap_config.expiry,
            swap_config.alpha_pk,
            attestation,
        )
        .await?;
    let commitment_sig = commit_result.commitment;

    if let Some(db) = db {
        db.store_commitment(bcr_wallet_persistence::SwapCommitmentRecord {
            inputs: commit_result.inputs_ys,
            outputs: commit_result.outputs,
            expiry: commit_result.expiry,
            commitment: commitment_sig,
            ephemeral_secret: commit_result.ephemeral_secret,
            body_content: commit_result.body_content,
            wallet_key: commit_result.wallet_key,
            premints,
        })
        .await?;
    }

    let signatures = client
        .post_swap_committed(inputs, outputs, commitment_sig)
        .await?;

    if let Some(db) = db
        && let Err(e) = db.delete_commitment(commitment_sig).await
    {
        tracing::warn!("Failed to delete commitment after swap: {e}");
    }

    Ok(signatures)
}

///////////////////////////////////////////// swap
async fn swap(
    output_unit: CurrencyUnit,
    inputs: Vec<cdk00::Proof>,
    mut premints: HashMap<cashu::Id, cdk00::PreMintSecrets>,
    keysets: HashMap<cashu::Id, KeySet>,
    client: Arc<dyn ClowderMintConnector>,
    db: &dyn PocketRepository,
    swap_config: SwapConfig,
    beta: &dyn BetaProvider,
) -> Result<Amount> {
    let total_input = inputs.total_amount()?;
    let input_len = inputs.len();
    let blinds: Vec<cdk00::BlindedMessage> = premints
        .values()
        .flat_map(|premint| premint.blinded_messages())
        .collect();

    let attestation = beta.attest(&inputs).await?;
    let signatures = committed_swap(
        client.as_ref(),
        Some(db),
        inputs,
        blinds,
        &swap_config,
        premints.iter().map(|(k, v)| (*k, v.clone())).collect(),
        attestation,
    )
    .await?;

    let output_len = signatures.len();
    let total_output = signatures
        .iter()
        .fold(Amount::ZERO, |acc, sig| acc + sig.amount);
    tracing::debug!(
        "swap to {output_unit}: inputs: {input_len} {total_input}, outputs: {output_len} {total_output}",
    );
    let mut sigs_by_kid: HashMap<cashu::Id, Vec<cdk00::BlindSignature>> = HashMap::new();
    for signature in signatures {
        sigs_by_kid
            .entry(signature.keyset_id)
            .or_default()
            .push(signature);
    }
    let mut total_cashed_in = Amount::ZERO;
    for (kid, sigs) in sigs_by_kid.into_iter() {
        let premint = premints.remove(&kid).expect("premint should be here");
        let keyset = keysets.get(&kid).expect("keyset should be here");
        let proofs = unblind_proofs(keyset, sigs, premint);

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

///////////////////////////////////////////// swap_proofs_to_target
async fn swap_proofs_to_target(
    swap_proofs: Vec<cdk00::Proof>,
    keysets_info: &HashMap<cashu::Id, KeySetInfo>,
    keysets: HashMap<cashu::Id, KeySet>,
    target_amount: Amount,
    seed: &Seed,
    db: &dyn PocketRepository,
    client: &Arc<dyn ClowderMintConnector>,
    swap_config: SwapConfig,
    beta: &dyn BetaProvider,
) -> Result<HashMap<cdk01::PublicKey, cdk00::Proof>> {
    let swap_plan: Vec<_> = prepare_swap(&swap_proofs, keysets_info)?
        .into_iter()
        .collect();
    tracing::debug!("Swapping Proof to Target {target_amount}, {swap_plan:?}");

    // prepare the premints
    let mut premints: HashMap<cashu::Id, cdk00::PreMintSecrets> = HashMap::new();
    let mut remaining_payment = target_amount;
    // collect payments by kid, so we can reconstruct it after the swap
    let mut payment_targets_by_kid: HashMap<cashu::Id, Amount> = HashMap::new();

    for (kid, amount) in swap_plan {
        let keyset_payment_target = std::cmp::min(amount, remaining_payment);

        let target = if keyset_payment_target > Amount::ZERO {
            // used for our payment - needs to add up to our payment amount
            payment_targets_by_kid.insert(kid, keyset_payment_target);
            remaining_payment -= keyset_payment_target;
            SplitTarget::Value(keyset_payment_target)
        } else {
            // change - doesn't matter how we get it
            SplitTarget::default()
        };

        let counter = db.counter(kid).await?;
        let premint = cdk00::PreMintSecrets::from_seed(
            kid,
            counter,
            seed,
            amount,
            &target,
            &bcr_wallet_core::util::to_fee_and_amounts(&keysets[&kid]),
        )?;
        let increment = premint.len() as u32;
        premints.insert(kid, premint);
        db.increment_counter(kid, counter, increment).await?;
    }

    if remaining_payment != Amount::ZERO {
        return Err(Error::Swap(format!(
            "swap plan cannot fund payment target {target_amount}, missing {remaining_payment}"
        )));
    }

    let blinds: Vec<cdk00::BlindedMessage> = premints
        .values()
        .flat_map(|premint| premint.blinded_messages())
        .collect();

    let attestation = beta.attest(&swap_proofs).await?;
    let signatures = committed_swap(
        client.as_ref(),
        Some(db),
        swap_proofs,
        blinds,
        &swap_config,
        premints.iter().map(|(k, v)| (*k, v.clone())).collect(),
        attestation,
    )
    .await?;
    let mut on_target: HashMap<cdk01::PublicKey, cdk00::Proof> = HashMap::new();

    let mut sigs_by_kid: HashMap<cashu::Id, Vec<cdk00::BlindSignature>> = HashMap::new();
    for signature in signatures {
        sigs_by_kid
            .entry(signature.keyset_id)
            .and_modify(|v| v.push(signature.clone()))
            .or_insert_with(|| vec![signature]);
    }

    let mut selected_amount = Amount::ZERO;
    for (kid, sigs) in sigs_by_kid.into_iter() {
        let premint = premints.remove(&kid).expect("premint should be here");
        let keyset = keysets.get(&kid).expect("keyset should be here");
        let mut proofs = unblind_proofs(keyset, sigs, premint);

        // get payment amount for this keyset
        let keyset_target_amount = payment_targets_by_kid.remove(&kid).unwrap_or(Amount::ZERO);
        let mut selected_amount_per_keyset = Amount::ZERO;

        proofs.sort_by_key(|proof| std::cmp::Reverse(proof.amount));
        for proof in proofs {
            let amount = proof.amount;
            let result = db.store_new(proof.clone()).await;
            match result {
                Ok(y) => {
                    if selected_amount_per_keyset + amount <= keyset_target_amount {
                        selected_amount_per_keyset += amount;
                        selected_amount += amount;
                        on_target.insert(y, proof);
                    }
                }
                Err(e) => {
                    tracing::error!("error in storing proof {}, {}: {e}", kid, amount);
                }
            }
        }

        if selected_amount_per_keyset != keyset_target_amount {
            return Err(Error::Swap(format!(
                "did not select exact payment proofs for keyset {kid}: {selected_amount_per_keyset} / {keyset_target_amount}"
            )));
        }
    }

    if selected_amount != target_amount {
        return Err(Error::Swap(format!(
            "did not select exact payment proofs for total amount: {selected_amount} / {target_amount}"
        )));
    }

    Ok(on_target)
}

///////////////////////////////////////////// collect_keyset_infos_from_proofs
fn collect_keyset_infos_from_proofs<'it, 'a>(
    proofs: impl Iterator<Item = &'it cdk00::Proof>,
    keysets_info: &'a HashMap<cashu::Id, KeySetInfo>,
) -> Result<HashMap<cashu::Id, &'a KeySetInfo>> {
    let kids = proofs.map(|p| p.keyset_id).collect::<HashSet<_>>();
    let mut infos = HashMap::with_capacity(kids.len());
    for kid in kids {
        let info = keysets_info.get(&kid).ok_or(Error::UnknownKeysetId(kid))?;
        infos.insert(kid, info);
    }
    Ok(infos)
}

///////////////////////////////////////////// sign_content_b64
/// Sign the preimage of a base64-encoded content string with a keypair.
/// Used for protest request wallet_signatures.
fn sign_content_b64(
    content_b64: &str,
    keypair: &bitcoin::secp256k1::Keypair,
) -> Result<bitcoin::secp256k1::schnorr::Signature> {
    use bitcoin::base64::{Engine, engine::general_purpose::STANDARD};
    use bitcoin::hashes::{Hash, sha256};
    let content_bytes = STANDARD
        .decode(content_b64)
        .map_err(|e| Error::MintingError(format!("invalid base64 content: {e}")))?;
    let digest = sha256::Hash::hash(&content_bytes);
    let msg = bitcoin::secp256k1::Message::from_digest(digest.to_byte_array());
    Ok(bitcoin::secp256k1::SECP256K1.sign_schnorr(&msg, keypair))
}

///////////////////////////////////////////// send_proofs
async fn send_proofs(
    plan: SendPlan,
    keysets_info: &HashMap<cashu::Id, KeySetInfo>,
    target_amount: Amount,
    seed: &Seed,
    db: &dyn PocketRepository,
    client: &Arc<dyn ClowderMintConnector>,
    swap_config: SwapConfig,
    beta: &dyn BetaProvider,
) -> Result<HashMap<cdk01::PublicKey, cdk00::Proof>> {
    let mut current_amount = Amount::ZERO;
    let mut sending_proofs: HashMap<cdk01::PublicKey, cdk00::Proof> = HashMap::new();

    match plan {
        SendPlan::Ready { proofs } => {
            for y in proofs {
                let proof = db.mark_as_pendingspent(y).await?;
                current_amount += proof.amount;
                sending_proofs.insert(y, proof);
            }
        }
        SendPlan::NeedSwap {
            inputs,
            target,
            estimated_fee,
        } => {
            tracing::debug!(
                "Send Proof for {target_amount} - swapping with target {target} and {estimated_fee} fee"
            );

            let swap_proofs = db.load_proofs(&inputs).await?;
            let kids: HashSet<cashu::Id> = swap_proofs.values().map(|p| p.keyset_id).collect();
            let mut keysets: HashMap<cashu::Id, KeySet> = HashMap::new();
            for kid in kids.iter() {
                let keyset = client.get_mint_keyset(*kid).await?;
                keysets.insert(*kid, keyset);
            }

            for y in swap_proofs.keys() {
                let _ = db.mark_as_pendingspent(*y).await?;
            }

            let swapped_to_target_proofs = swap_proofs_to_target(
                swap_proofs.into_values().collect(),
                keysets_info,
                keysets,
                target,
                seed,
                db,
                client,
                swap_config,
                beta,
            )
            .await?;

            for (y, proof) in swapped_to_target_proofs.iter() {
                let _ = db.mark_as_pendingspent(*y).await?;
                current_amount += proof.amount;
                sending_proofs.insert(*y, proof.clone());
            }
        }
    };

    if current_amount < target_amount {
        tracing::warn!("Send Proofs: Target was {target_amount}, sending only {current_amount}");
    }

    Ok(sending_proofs)
}

///////////////////////////////////////////// return proofs to send for offline payment
// WARN: This does not swap to target and is suited only for the current temporary offline pay by token flow
// This just sets the proofs to pending-spent and returns them
async fn return_proofs_to_send_for_offline_payment(
    plan: SendPlan,
    db: &dyn PocketRepository,
) -> Result<(Amount, HashMap<cdk01::PublicKey, cdk00::Proof>)> {
    let mut send_amount = Amount::ZERO;
    let mut sending_proofs: HashMap<cdk01::PublicKey, cdk00::Proof> = HashMap::new();
    match plan {
        SendPlan::Ready { proofs } => {
            for y in proofs {
                let proof = db.mark_as_pendingspent(y).await?;
                send_amount += proof.amount;
                sending_proofs.insert(y, proof);
            }
        }
        SendPlan::NeedSwap { inputs, target, .. } => {
            for proof in inputs {
                let swap_proof = db.mark_as_pendingspent(proof).await?;
                sending_proofs.insert(proof, swap_proof);
            }
            send_amount += target;
        }
    };

    Ok((send_amount, sending_proofs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{external::mint::MockClowderMintConnector, pocket::test_utils::tests::test_kinfos};
    use bcr_common::{cashu::Proof, core::signature, core_tests};
    use bcr_wallet_persistence::{MockPocketRepository, test_utils::tests::zero_seed};
    use mockall::predicate::*;

    #[test]
    fn unblind_proofs() {
        let amounts = [Amount::from(8)];
        let (_, mintkeyset) = core_tests::generate_random_ecash_keyset();
        let keyset = bcr_wallet_core::util::to_keyset(&mintkeyset, None);
        let premint = cdk00::PreMintSecrets::random(
            keyset.id,
            amounts[0],
            &SplitTarget::None,
            &bcr_wallet_core::util::to_fee_and_amounts(&keyset),
        )
        .unwrap();
        assert!(premint.blinded_messages().len() == 1);
        let blind = premint.blinded_messages()[0].clone();
        let signature = signature::sign_ecash(&mintkeyset, &blind).unwrap();
        let proofs = super::unblind_proofs(&keyset, vec![signature], premint);
        assert_eq!(proofs.len(), 1);
        signature::verify_ecash_proof(&mintkeyset, &proofs[0]).unwrap();
    }

    #[test]
    fn unblind_proofs_len_mismatch() {
        let (_, mintkeyset) = core_tests::generate_random_ecash_keyset();
        let keyset = bcr_wallet_core::util::to_keyset(&mintkeyset, None);
        let premint = cdk00::PreMintSecrets::random(
            keyset.id,
            Amount::from(8),
            &SplitTarget::None,
            &bcr_wallet_core::util::to_fee_and_amounts(&keyset),
        )
        .unwrap();
        assert_eq!(premint.blinded_messages().len(), 1);
        let signatures = core_tests::generate_ecash_signatures(
            &mintkeyset,
            &[Amount::from(8), Amount::from(32)],
        );
        let proofs = super::unblind_proofs(&keyset, signatures, premint);
        assert_eq!(proofs.len(), 1);
    }

    #[test]
    fn unblind_proofs_amount_mismatch() {
        let (_, mintkeyset) = core_tests::generate_random_ecash_keyset();
        let keyset = bcr_wallet_core::util::to_keyset(&mintkeyset, None);
        let premint = cdk00::PreMintSecrets::random(
            keyset.id,
            Amount::from(40),
            &SplitTarget::None,
            &bcr_wallet_core::util::to_fee_and_amounts(&keyset),
        )
        .unwrap();
        assert_eq!(premint.blinded_messages().len(), 2);
        let signatures = core_tests::generate_ecash_signatures(
            &mintkeyset,
            &[Amount::from(16), Amount::from(4)],
        );
        let proofs = super::unblind_proofs(&keyset, signatures, premint);
        assert_eq!(proofs.len(), 0);
    }

    #[test]
    fn unblind_proofs_kid_mismatch() {
        let (_, mintkeyset) = core_tests::generate_random_ecash_keyset();
        let keyset = bcr_wallet_core::util::to_keyset(&mintkeyset, None);
        let kid2 = core_tests::generate_random_ecash_keyset().0.id;
        let premint = cdk00::PreMintSecrets::random(
            kid2,
            Amount::from(16),
            &SplitTarget::None,
            &bcr_wallet_core::util::to_fee_and_amounts(&keyset),
        )
        .unwrap();
        assert_eq!(premint.blinded_messages().len(), 1);
        let signatures = core_tests::generate_ecash_signatures(&mintkeyset, &[Amount::from(16)]);
        let proofs = super::unblind_proofs(&keyset, signatures, premint);
        assert_eq!(proofs.len(), 0);
    }

    #[tokio::test]
    async fn fetch_attestation_sends_correct_fingerprints() {
        let (_, keyset) = core_tests::generate_random_ecash_keyset();
        let amounts = [Amount::from(8), Amount::from(16)];
        let proofs = core_tests::generate_random_ecash_proofs(&keyset, &amounts);

        let alpha_id = bitcoin::secp256k1::PublicKey::from_keypair(
            &bitcoin::secp256k1::Keypair::new_global(&mut bitcoin::secp256k1::rand::thread_rng()),
        );

        let expected_ys: Vec<cashu::PublicKey> = proofs.iter().map(|p| p.y().unwrap()).collect();

        let mut beta_mock = MockClowderMintConnector::new();
        beta_mock
            .expect_post_attest_issuance()
            .times(1)
            .withf(move |req| {
                req.alpha_id == alpha_id
                    && req.inputs.len() == 2
                    && req.inputs.iter().map(|fp| fp.y).collect::<Vec<_>>() == expected_ys
            })
            .returning(|_| Ok(crate::pocket::test_utils::tests::mock_attestation()));

        let result = super::fetch_attestation(&beta_mock, alpha_id, &proofs).await;
        assert!(result.is_ok());
    }

    use crate::pocket::test_utils::tests::{
        setup_commitment_mocks, test_beta_provider, test_swap_config,
    };

    #[tokio::test]
    async fn swap_proofs_to_target() {
        let (info, keyset) = core_tests::generate_random_ecash_keyset();
        let k_infos = test_kinfos(info);
        let amount = Amount::from(16);
        let target = Amount::from(13);
        let proof = core_tests::generate_random_ecash_proofs(&keyset, &[amount])[0].clone();
        let seed = zero_seed();
        let mut mockdb = MockPocketRepository::new();
        let mut mockclient = MockClowderMintConnector::new();
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
        setup_commitment_mocks(&mut mockclient, &mut mockdb);
        mockclient
            .expect_post_swap_committed()
            .times(1)
            .returning(move |_, outp, _| {
                let amounts = outp.iter().map(|b| b.amount).collect::<Vec<_>>();
                let mock_signatures =
                    core_tests::generate_ecash_signatures(&cloned_keyset, &amounts);
                Ok(mock_signatures)
            });
        mockdb.expect_store_new().times(5).returning(|p| {
            let y = p.y().expect("Hash to curve should not fail");
            Ok(y)
        });

        let arc_client: Arc<dyn ClowderMintConnector> = Arc::new(mockclient);
        let beta = test_beta_provider();
        let mut keysets = HashMap::new();
        keysets.insert(keyset.id, bcr_wallet_core::util::to_keyset(&keyset, None));
        let proofs = super::swap_proofs_to_target(
            vec![proof],
            &k_infos,
            keysets,
            target,
            &seed,
            &mockdb,
            &arc_client,
            test_swap_config(),
            &beta,
        )
        .await
        .unwrap();
        assert_eq!(proofs.len(), 3);
        let p: Vec<Proof> = proofs.values().cloned().collect();
        let total = p.total_amount().unwrap();
        assert_eq!(total, target);
    }

    #[tokio::test]
    async fn swap() {
        let (info, keyset) = core_tests::generate_random_ecash_keyset();
        let amounts = [Amount::from(8), Amount::from(16)];
        let unit = CurrencyUnit::Sat;
        let inputs = core_tests::generate_random_ecash_proofs(&keyset, &amounts);
        let premints = HashMap::from_iter([(
            info.id,
            cdk00::PreMintSecrets::random(
                info.id,
                Amount::from(24),
                &SplitTarget::None,
                &bcr_wallet_core::util::to_fee_and_amounts(&bcr_wallet_core::util::to_keyset(
                    &keyset, None,
                )),
            )
            .unwrap(),
        )]);
        let keysets = HashMap::from([(info.id, bcr_wallet_core::util::to_keyset(&keyset, None))]);
        let mut mockclient = MockClowderMintConnector::new();
        let mut mockdb = MockPocketRepository::new();
        setup_commitment_mocks(&mut mockclient, &mut mockdb);
        mockclient
            .expect_post_swap_committed()
            .times(1)
            .returning(move |_, outp, _| {
                let amounts = outp.iter().map(|b| b.amount).collect::<Vec<_>>();
                let signatures = core_tests::generate_ecash_signatures(&keyset, &amounts);
                Ok(signatures)
            });
        mockdb.expect_store_new().times(2).returning(|p| {
            let y = p.y().expect("Hash to curve should not fail");
            Ok(y)
        });

        let arc_client: Arc<dyn ClowderMintConnector> = Arc::new(mockclient);
        let beta = test_beta_provider();
        let amount = super::swap(
            unit,
            inputs,
            premints,
            keysets,
            arc_client,
            &mockdb,
            test_swap_config(),
            &beta,
        )
        .await
        .unwrap();
        assert_eq!(amount, Amount::from(24));
    }

    #[tokio::test]
    async fn send_proofs_ready() {
        let (_, keyset) = core_tests::generate_random_ecash_keyset();
        let amounts = [Amount::from(8), Amount::from(16)];
        let proofs = core_tests::generate_random_ecash_proofs(&keyset, &amounts);
        let ys = proofs.iter().map(|p| p.y().unwrap()).collect::<Vec<_>>();

        let proof_by_y = proofs
            .iter()
            .cloned()
            .map(|p| (p.y().unwrap(), p))
            .collect::<HashMap<_, _>>();

        let mut mockdb = MockPocketRepository::new();
        mockdb
            .expect_mark_as_pendingspent()
            .times(2)
            .returning(move |y| Ok(proof_by_y.get(&y).unwrap().clone()));

        let mockclient = MockClowderMintConnector::new();
        let arc_client: Arc<dyn ClowderMintConnector> = Arc::new(mockclient);
        let beta = test_beta_provider();

        let sent = super::send_proofs(
            SendPlan::Ready { proofs: ys },
            &HashMap::new(),
            Amount::from(24),
            &zero_seed(),
            &mockdb,
            &arc_client,
            test_swap_config(),
            &beta,
        )
        .await
        .unwrap();

        assert_eq!(sent.len(), 2);
        assert_eq!(
            sent.values()
                .cloned()
                .collect::<Vec<_>>()
                .total_amount()
                .unwrap(),
            Amount::from(24)
        );
    }

    #[tokio::test]
    async fn send_proofs_need_split_then_ready() {
        let (info, keyset) = core_tests::generate_random_ecash_keyset();
        let k_infos = test_kinfos(info.clone());

        let swap_proof =
            core_tests::generate_random_ecash_proofs(&keyset, &[Amount::from(16)])[0].clone();
        let swap_y = swap_proof.y().unwrap();
        let load_proofs = HashMap::from([(swap_proof.y().unwrap(), swap_proof.clone())]);

        let mut mockdb = MockPocketRepository::new();
        let mut mockclient = MockClowderMintConnector::new();

        mockdb.expect_counter().times(1).returning(|_| Ok(0));
        mockdb
            .expect_increment_counter()
            .times(1)
            .returning(|_, _, _| Ok(()));

        setup_commitment_mocks(&mut mockclient, &mut mockdb);

        let cloned_keyset_for_get = keyset.clone();
        mockclient
            .expect_get_mint_keyset()
            .times(1)
            .with(eq(info.id))
            .returning(move |_| {
                Ok(bcr_wallet_core::util::to_keyset(
                    &cloned_keyset_for_get,
                    None,
                ))
            });

        let cloned_keyset_for_sign = keyset.clone();
        mockclient
            .expect_post_swap_committed()
            .times(1)
            .returning(move |_, outp, _| {
                let amounts = outp.iter().map(|b| b.amount).collect::<Vec<_>>();
                let signatures =
                    core_tests::generate_ecash_signatures(&cloned_keyset_for_sign, &amounts);
                Ok(signatures)
            });

        mockdb.expect_store_new().returning(|p| Ok(p.y().unwrap()));

        mockdb
            .expect_load_proofs()
            .times(1)
            .returning(move |_| Ok(load_proofs.clone()));

        mockdb
            .expect_mark_as_pendingspent()
            .returning(move |_| Ok(swap_proof.clone()));

        let arc_client: Arc<dyn ClowderMintConnector> = Arc::new(mockclient);

        let beta = test_beta_provider();
        let sent = super::send_proofs(
            SendPlan::NeedSwap {
                inputs: vec![swap_y],
                target: Amount::from(13),
                estimated_fee: Amount::from(0),
            },
            &k_infos,
            Amount::from(13),
            &zero_seed(),
            &mockdb,
            &arc_client,
            test_swap_config(),
            &beta,
        )
        .await
        .unwrap();

        assert_eq!(sent.len(), 3);
        assert_eq!(
            sent.values()
                .cloned()
                .collect::<Vec<_>>()
                .total_amount()
                .unwrap(),
            Amount::from(13)
        );
    }

    #[tokio::test]
    async fn send_proofs_need_split_then_ready_big_amounts() {
        let (info, keyset) = core_tests::generate_random_ecash_keyset();
        let k_infos = test_kinfos(info.clone());

        let amounts = [
            Amount::from(512),
            Amount::from(128),
            Amount::from(64),
            Amount::from(32),
            Amount::from(16),
            Amount::from(8),
            Amount::from(4),
            Amount::from(4),
            Amount::from(4),
            Amount::from(4),
            Amount::from(4),
            Amount::from(4),
            Amount::from(4),
            Amount::from(4), // extra over 788
        ];
        let swap_proofs = core_tests::generate_random_ecash_proofs(&keyset, &amounts).clone();
        let swap_ys = swap_proofs
            .iter()
            .map(|p| p.y().unwrap())
            .collect::<Vec<_>>();

        let proof_by_y = swap_proofs
            .iter()
            .cloned()
            .map(|p| (p.y().unwrap(), p))
            .collect::<HashMap<_, _>>();

        let mut mockdb = MockPocketRepository::new();
        let mut mockclient = MockClowderMintConnector::new();

        mockdb.expect_counter().times(1).returning(|_| Ok(0));
        mockdb
            .expect_increment_counter()
            .times(1)
            .returning(|_, _, _| Ok(()));

        setup_commitment_mocks(&mut mockclient, &mut mockdb);

        let cloned_keyset_for_get = keyset.clone();
        mockclient
            .expect_get_mint_keyset()
            .times(1)
            .with(eq(info.id))
            .returning(move |_| {
                Ok(bcr_wallet_core::util::to_keyset(
                    &cloned_keyset_for_get,
                    None,
                ))
            });

        let cloned_keyset_for_sign = keyset.clone();
        mockclient
            .expect_post_swap_committed()
            .times(1)
            .returning(move |_, outp, _| {
                let amounts = outp.iter().map(|b| b.amount).collect::<Vec<_>>();
                let signatures =
                    core_tests::generate_ecash_signatures(&cloned_keyset_for_sign, &amounts);
                Ok(signatures)
            });

        mockdb.expect_store_new().returning(|p| Ok(p.y().unwrap()));

        let proofs_clone = proof_by_y.clone();
        mockdb
            .expect_load_proofs()
            .times(1)
            .returning(move |_| Ok(proofs_clone.clone()));

        mockdb
            .expect_mark_as_pendingspent()
            .returning(move |_| Ok(swap_proofs[0].clone()));

        let arc_client: Arc<dyn ClowderMintConnector> = Arc::new(mockclient);

        let beta = test_beta_provider();
        let sent = super::send_proofs(
            SendPlan::NeedSwap {
                inputs: swap_ys,
                target: Amount::from(788),
                estimated_fee: Amount::from(0),
            },
            &k_infos,
            Amount::from(788),
            &zero_seed(),
            &mockdb,
            &arc_client,
            test_swap_config(),
            &beta,
        )
        .await
        .unwrap();

        assert_eq!(
            sent.values()
                .cloned()
                .collect::<Vec<_>>()
                .total_amount()
                .unwrap(),
            Amount::from(788)
        );
    }

    #[tokio::test]
    async fn send_proofs_need_split_then_ready_fees() {
        let (info, keyset) = core_tests::generate_random_ecash_keyset();
        let k_infos = test_kinfos(info.clone());

        let swap_proof =
            core_tests::generate_random_ecash_proofs(&keyset, &[Amount::from(16)])[0].clone();
        let swap_y = swap_proof.y().unwrap();
        let load_proofs = HashMap::from([(swap_proof.y().unwrap(), swap_proof.clone())]);

        let mut mockdb = MockPocketRepository::new();
        let mut mockclient = MockClowderMintConnector::new();

        mockdb.expect_counter().times(1).returning(|_| Ok(0));
        mockdb
            .expect_increment_counter()
            .times(1)
            .returning(|_, _, _| Ok(()));

        setup_commitment_mocks(&mut mockclient, &mut mockdb);

        let cloned_keyset_for_get = keyset.clone();
        mockclient
            .expect_get_mint_keyset()
            .times(1)
            .with(eq(info.id))
            .returning(move |_| {
                Ok(bcr_wallet_core::util::to_keyset(
                    &cloned_keyset_for_get,
                    None,
                ))
            });

        let cloned_keyset_for_sign = keyset.clone();
        mockclient
            .expect_post_swap_committed()
            .times(1)
            .returning(move |_, outp, _| {
                let amounts = outp.iter().map(|b| b.amount).collect::<Vec<_>>();
                let signatures =
                    core_tests::generate_ecash_signatures(&cloned_keyset_for_sign, &amounts);
                Ok(signatures)
            });

        mockdb.expect_store_new().returning(|p| Ok(p.y().unwrap()));

        mockdb
            .expect_load_proofs()
            .times(1)
            .returning(move |_| Ok(load_proofs.clone()));

        mockdb
            .expect_mark_as_pendingspent()
            .returning(move |_| Ok(swap_proof.clone()));

        let arc_client: Arc<dyn ClowderMintConnector> = Arc::new(mockclient);
        let beta = test_beta_provider();

        let sent = super::send_proofs(
            SendPlan::NeedSwap {
                inputs: vec![swap_y],
                target: Amount::from(13),
                estimated_fee: Amount::from(1),
            },
            &k_infos,
            Amount::from(13),
            &zero_seed(),
            &mockdb,
            &arc_client,
            test_swap_config(),
            &beta,
        )
        .await
        .unwrap();

        assert_eq!(sent.len(), 3);
        assert_eq!(
            sent.values()
                .cloned()
                .collect::<Vec<_>>()
                .total_amount()
                .unwrap(),
            Amount::from(13)
        );
    }

    #[tokio::test]
    async fn send_proofs_need_split_multi_keys() {
        let (info, keyset) = core_tests::generate_random_ecash_keyset();
        let (info_2, keyset_2) = core_tests::generate_random_ecash_keyset();
        let mut k_infos = test_kinfos(info.clone());
        k_infos.insert(info_2.id, KeySetInfo::from(info_2.clone()));

        let swap_proof =
            core_tests::generate_random_ecash_proofs(&keyset, &[Amount::from(16)])[0].clone();
        let swap_proof_ks_2 =
            core_tests::generate_random_ecash_proofs(&keyset_2, &[Amount::from(16)])[0].clone();
        let swap_y = swap_proof.y().unwrap();
        let swap_y_ks_2 = swap_proof_ks_2.y().unwrap();
        let load_proofs = HashMap::from([
            (swap_proof.y().unwrap(), swap_proof.clone()),
            (swap_proof_ks_2.y().unwrap(), swap_proof_ks_2.clone()),
        ]);

        let mut mockdb = MockPocketRepository::new();
        let mut mockclient = MockClowderMintConnector::new();

        mockdb.expect_counter().times(2).returning(|_| Ok(0));
        mockdb
            .expect_increment_counter()
            .times(2)
            .returning(|_, _, _| Ok(()));

        setup_commitment_mocks(&mut mockclient, &mut mockdb);

        let cloned_keyset_for_get = keyset.clone();
        mockclient
            .expect_get_mint_keyset()
            .times(1)
            .with(eq(info.id))
            .returning(move |_| {
                Ok(bcr_wallet_core::util::to_keyset(
                    &cloned_keyset_for_get,
                    None,
                ))
            });
        let cloned_keyset_for_get_2 = keyset_2.clone();
        mockclient
            .expect_get_mint_keyset()
            .times(1)
            .with(eq(info_2.id))
            .returning(move |_| {
                Ok(bcr_wallet_core::util::to_keyset(
                    &cloned_keyset_for_get_2,
                    None,
                ))
            });

        let ks_clone = keyset.clone();
        let ks_2_clone = keyset_2.clone();
        mockclient
            .expect_post_swap_committed()
            .times(1)
            .returning(move |_, outp, _| {
                let mut signatures = vec![];
                for o in outp {
                    let ks = if o.keyset_id == keyset.id {
                        ks_clone.clone()
                    } else {
                        ks_2_clone.clone()
                    };
                    signatures.extend_from_slice(&core_tests::generate_ecash_signatures(
                        &ks,
                        &[o.amount],
                    ));
                }
                Ok(signatures)
            });

        mockdb.expect_store_new().returning(|p| Ok(p.y().unwrap()));

        mockdb
            .expect_load_proofs()
            .times(1)
            .returning(move |_| Ok(load_proofs.clone()));

        mockdb
            .expect_mark_as_pendingspent()
            .returning(move |_| Ok(swap_proof.clone()));

        let arc_client: Arc<dyn ClowderMintConnector> = Arc::new(mockclient);

        let beta = test_beta_provider();
        let sent = super::send_proofs(
            SendPlan::NeedSwap {
                inputs: vec![swap_y, swap_y_ks_2],
                target: Amount::from(23),
                estimated_fee: Amount::from(1),
            },
            &k_infos,
            Amount::from(23),
            &zero_seed(),
            &mockdb,
            &arc_client,
            test_swap_config(),
            &beta,
        )
        .await
        .unwrap();

        assert_eq!(
            sent.values()
                .cloned()
                .collect::<Vec<_>>()
                .total_amount()
                .unwrap(),
            Amount::from(23)
        );
    }
}
