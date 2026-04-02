use crate::{
    ClowderMintConnector,
    error::{Error, Result},
    wallet::types::SwapConfig,
};
use async_trait::async_trait;
use bcr_common::cashu::{
    self, Amount, CurrencyUnit, KeySet, KeySetInfo, ProofsMethods, amount::SplitTarget,
    nut00 as cdk00, nut01 as cdk01, nut07 as cdk07,
};
use bcr_wallet_core::{
    SendSync,
    types::{Seed, SendSummary},
};
use bcr_wallet_persistence::PocketRepository;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use uuid::Uuid;

pub mod credit;
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
    async fn balance(&self) -> Result<Amount>;
    async fn receive_proofs(
        &self,
        client: Arc<dyn ClowderMintConnector>,
        keysets_info: &[KeySetInfo],
        proofs: Vec<cashu::Proof>,
        swap_config: SwapConfig,
    ) -> Result<(Amount, Vec<cashu::PublicKey>)>;
    async fn prepare_send(&self, amount: Amount, infos: &[KeySetInfo]) -> Result<SendSummary>;
    async fn send_proofs(
        &self,
        rid: Uuid,
        keysets_info: &[KeySetInfo],
        client: Arc<dyn ClowderMintConnector>,
        swap_config: SwapConfig,
    ) -> Result<HashMap<cashu::PublicKey, cashu::Proof>>;
    async fn cleanup_local_proofs(
        &self,
        client: Arc<dyn ClowderMintConnector>,
    ) -> Result<Vec<cashu::PublicKey>>;
    async fn restore_local_proofs(
        &self,
        keysets_info: &[KeySetInfo],
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
        keysets_info: &[KeySetInfo],
        client: Arc<dyn ClowderMintConnector>,
        send_amount: Amount,
        swap_config: SwapConfig,
    ) -> Result<Vec<cashu::Proof>>;
}

///////////////////////////////////////////// SendReference
#[derive(Default, Clone)]
struct SendReference {
    rid: Uuid,
    send_proofs: Vec<cdk01::PublicKey>,
    swap_proof: Option<(Amount, cdk01::PublicKey)>,
}

///////////////////////////////////////////// cleanup_local_proofs
// Removes Spent proofs from local DB
async fn cleanup_local_proofs(
    db: &dyn PocketRepository,
    client: Arc<dyn ClowderMintConnector>,
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

///////////////////////////////////////////// swap
async fn swap(
    output_unit: CurrencyUnit,
    inputs: Vec<cdk00::Proof>,
    mut premints: HashMap<cashu::Id, cdk00::PreMintSecrets>,
    keysets: HashMap<cashu::Id, KeySet>,
    client: Arc<dyn ClowderMintConnector>,
    db: &dyn PocketRepository,
    swap_config: SwapConfig,
) -> Result<Amount> {
    let total_input = inputs.total_amount()?;
    let input_len = inputs.len();
    let blinds: Vec<cdk00::BlindedMessage> = premints
        .values()
        .flat_map(|premint| premint.blinded_messages())
        .collect();

    // Commitment phase
    let commit_result = client
        .post_swap_commitment(
            inputs.clone(),
            blinds.clone(),
            swap_config.expiry,
            swap_config.alpha_pk,
        )
        .await?;
    let commitment_sig = commit_result.commitment;
    db.store_commitment(bcr_wallet_persistence::SwapCommitmentRecord {
        inputs: commit_result.inputs_ys,
        outputs: commit_result.outputs,
        expiry_height: commit_result.expiry_height,
        commitment: commitment_sig,
        ephemeral_secret: commit_result.ephemeral_secret,
        body_content: commit_result.body_content,
        wallet_key: commit_result.wallet_key,
    })
    .await?;

    // Swap phase
    let request = bcr_common::wire::swap::SwapRequest {
        inputs,
        outputs: blinds,
        commitment: commitment_sig,
    };
    let response = client.post_swap_committed(request).await?;

    // Clean up commitment after successful swap
    if let Err(e) = db.delete_commitment(commitment_sig).await {
        tracing::warn!("Failed to delete commitment after swap: {e}");
    }
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
    for (kid, signatures) in signatures.into_iter() {
        let premint = premints.remove(&kid).expect("premint should be here");
        let keyset = keysets.get(&kid).expect("keyset should be here");
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
    seed: &Seed,
    db: &dyn PocketRepository,
    client: &Arc<dyn ClowderMintConnector>,
    swap_config: SwapConfig,
) -> Result<HashMap<cdk01::PublicKey, cdk00::Proof>> {
    let target = SplitTarget::Value(target_amount);
    let counter = db.counter(target_keyset.id).await?;
    let premint =
        cdk00::PreMintSecrets::from_seed(target_keyset.id, counter, seed, proof.amount, &target)?;
    let blinds = premint.blinded_messages();
    // Commitment phase
    let commit_result = client
        .post_swap_commitment(
            vec![proof.clone()],
            blinds.clone(),
            swap_config.expiry,
            swap_config.alpha_pk,
        )
        .await?;
    let commitment_sig = commit_result.commitment;
    db.store_commitment(bcr_wallet_persistence::SwapCommitmentRecord {
        inputs: commit_result.inputs_ys,
        outputs: commit_result.outputs,
        expiry_height: commit_result.expiry_height,
        commitment: commitment_sig,
        ephemeral_secret: commit_result.ephemeral_secret,
        body_content: commit_result.body_content,
        wallet_key: commit_result.wallet_key,
    })
    .await?;

    // Swap phase
    let request = bcr_common::wire::swap::SwapRequest {
        inputs: vec![proof],
        outputs: blinds,
        commitment: commitment_sig,
    };
    db.increment_counter(target_keyset.id, counter, premint.len() as u32)
        .await?;
    let signatures = client.post_swap_committed(request).await?.signatures;

    // Clean up commitment after successful swap
    if let Err(e) = db.delete_commitment(commitment_sig).await {
        tracing::warn!("Failed to delete commitment after swap: {e}");
    }
    let mut on_target: HashMap<cdk01::PublicKey, cdk00::Proof> = HashMap::new();
    let mut proofs = unblind_proofs(target_keyset, signatures, premint);
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
    seed: &Seed,
    db: &dyn PocketRepository,
    client: &Arc<dyn ClowderMintConnector>,
    target_swap_keysetid: Option<cashu::Id>,
    swap_config: SwapConfig,
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
            seed,
            db,
            client,
            swap_config,
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

///////////////////////////////////////////// return proofs to send for offline payment
// WARN: This does not swap to target and is suited only for the current temporary offline pay by token flow
// This just sets the proofs to pending-spent and returns them
async fn return_proofs_to_send_for_offline_payment(
    send_proofs: Vec<cdk01::PublicKey>,
    swap_proof: Option<(Amount, cdk01::PublicKey)>,
    db: &dyn PocketRepository,
) -> Result<(Amount, HashMap<cdk01::PublicKey, cdk00::Proof>)> {
    let mut send_amount = Amount::ZERO;
    let mut sending_proofs: HashMap<cdk01::PublicKey, cdk00::Proof> = HashMap::new();
    for y in send_proofs {
        let proof = db.mark_as_pendingspent(y).await?;
        send_amount += proof.amount;
        sending_proofs.insert(y, proof);
    }

    // Also add swap proof as-is, without swapping to target
    if let Some((swap_amount, swap_y)) = swap_proof {
        let swap_proof = db.mark_as_pendingspent(swap_y).await?;
        sending_proofs.insert(swap_y, swap_proof);
        send_amount += swap_amount;
    }

    Ok((send_amount, sending_proofs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external::test_utils::tests::MockMintConnector;
    use bcr_common::{cashu::Proof, core::signature, core_tests};
    use bcr_wallet_persistence::{MockPocketRepository, test_utils::tests::zero_seed};
    use cashu::nut02 as cdk02;
    use mockall::predicate::*;

    #[test]
    fn unblind_proofs() {
        let amounts = [Amount::from(8u64)];
        let (_, mintkeyset) = core_tests::generate_random_ecash_keyset();
        let keyset = cdk02::KeySet::from(mintkeyset.clone());
        let premint =
            cdk00::PreMintSecrets::random(keyset.id, amounts[0], &SplitTarget::None).unwrap();
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
        let keyset = cdk02::KeySet::from(mintkeyset.clone());
        let premint =
            cdk00::PreMintSecrets::random(keyset.id, Amount::from(8u64), &SplitTarget::None)
                .unwrap();
        assert_eq!(premint.blinded_messages().len(), 1);
        let signatures = core_tests::generate_ecash_signatures(
            &mintkeyset,
            &[Amount::from(8u64), Amount::from(32u64)],
        );
        let proofs = super::unblind_proofs(&keyset, signatures, premint);
        assert_eq!(proofs.len(), 1);
    }

    #[test]
    fn unblind_proofs_amount_mismatch() {
        let (_, mintkeyset) = core_tests::generate_random_ecash_keyset();
        let keyset = cdk02::KeySet::from(mintkeyset.clone());
        let premint =
            cdk00::PreMintSecrets::random(keyset.id, Amount::from(40u64), &SplitTarget::None)
                .unwrap();
        assert_eq!(premint.blinded_messages().len(), 2);
        let signatures = core_tests::generate_ecash_signatures(
            &mintkeyset,
            &[Amount::from(16u64), Amount::from(4u64)],
        );
        let proofs = super::unblind_proofs(&keyset, signatures, premint);
        assert_eq!(proofs.len(), 0);
    }

    #[test]
    fn unblind_proofs_kid_mismatch() {
        let (_, mintkeyset) = core_tests::generate_random_ecash_keyset();
        let keyset = cdk02::KeySet::from(mintkeyset.clone());
        let kid2 = core_tests::generate_random_ecash_keyset().0.id;
        let premint =
            cdk00::PreMintSecrets::random(kid2, Amount::from(16u64), &SplitTarget::None).unwrap();
        assert_eq!(premint.blinded_messages().len(), 1);
        let signatures = core_tests::generate_ecash_signatures(&mintkeyset, &[Amount::from(16u64)]);
        let proofs = super::unblind_proofs(&keyset, signatures, premint);
        assert_eq!(proofs.len(), 0);
    }

    use crate::pocket::test_utils::tests::{
        setup_commitment_mocks, test_swap_config,
    };

    #[tokio::test]
    async fn swap_proof_to_target() {
        let (_, keyset) = core_tests::generate_random_ecash_keyset();
        let amount = Amount::from(16u64);
        let target = Amount::from(13u64);
        let proof = core_tests::generate_random_ecash_proofs(&keyset, &[amount])[0].clone();
        let seed = zero_seed();
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
        setup_commitment_mocks(&mut mockclient, &mut mockdb);
        mockclient
            .expect_post_swap_committed()
            .times(1)
            .returning(move |request| {
                let amounts = request
                    .outputs
                    .iter()
                    .map(|b| b.amount)
                    .collect::<Vec<_>>();
                let mock_signatures =
                    core_tests::generate_ecash_signatures(&cloned_keyset, &amounts);
                Ok(bcr_common::wire::swap::SwapResponse {
                    signatures: mock_signatures,
                })
            });
        mockdb.expect_store_new().times(5).returning(|p| {
            let y = p.y().expect("Hash to curve should not fail");
            Ok(y)
        });

        let arc_client: Arc<dyn ClowderMintConnector> = Arc::new(mockclient);
        let proofs = super::swap_proof_to_target(
            proof,
            &KeySet::from(keyset),
            target,
            &seed,
            &mockdb,
            &arc_client,
            test_swap_config(),
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
        let amounts = [Amount::from(8u64), Amount::from(16u64)];
        let unit = CurrencyUnit::Sat;
        let inputs = core_tests::generate_random_ecash_proofs(&keyset, &amounts);
        let premints = HashMap::from_iter([(
            info.id,
            cdk00::PreMintSecrets::random(info.id, Amount::from(24u64), &SplitTarget::None)
                .unwrap(),
        )]);
        let keysets = HashMap::from([(info.id, KeySet::from(keyset.clone()))]);
        let mut mockclient = MockMintConnector::new();
        let mut mockdb = MockPocketRepository::new();
        setup_commitment_mocks(&mut mockclient, &mut mockdb);
        mockclient
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
        mockdb.expect_store_new().times(2).returning(|p| {
            let y = p.y().expect("Hash to curve should not fail");
            Ok(y)
        });

        let arc_client: Arc<dyn ClowderMintConnector> = Arc::new(mockclient);
        let amount = super::swap(
            unit,
            inputs,
            premints,
            keysets,
            arc_client,
            &mockdb,
            test_swap_config(),
        )
        .await
        .unwrap();
        assert_eq!(amount, Amount::from(24u64));
    }
}
