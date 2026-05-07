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
    core::swap::wallet::{PaymentPlan, prepare_payment, prepare_swap},
};
use bcr_wallet_core::{
    SendSync,
    types::{Seed, SendSummary},
};
use bcr_wallet_persistence::PocketRepository;
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
    async fn balance(&self, keysets_info: &[KeySetInfo]) -> Result<PocketBalance>;
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
    async fn dev_mode_detailed_balance(
        &self,
        keysets_info: &[KeySetInfo],
    ) -> Result<HashMap<cashu::Id, (Option<u64>, Amount)>>;
    async fn delete(&self) -> Result<()>;
}

#[derive(Default, Debug, Clone)]
pub struct PocketBalance {
    pub debit: Amount,
    pub credit: Amount,
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
    NeedSplit {
        proof: cdk01::PublicKey,
        split_amount: Amount,
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
) -> Result<Vec<cdk00::BlindSignature>> {
    let commit_result = client
        .post_swap_commitment(
            inputs.clone(),
            outputs.clone(),
            swap_config.expiry,
            swap_config.alpha_pk,
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

    let request = bcr_common::wire::swap::SwapRequest {
        inputs,
        outputs,
        commitment: commitment_sig,
    };
    let response = client.post_swap_committed(request).await?;

    if let Some(db) = db
        && let Err(e) = db.delete_commitment(commitment_sig).await
    {
        tracing::warn!("Failed to delete commitment after swap: {e}");
    }

    Ok(response.signatures)
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

    let signatures = committed_swap(
        client.as_ref(),
        Some(db),
        inputs,
        blinds,
        &swap_config,
        premints.iter().map(|(k, v)| (*k, v.clone())).collect(),
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
            .and_modify(|v| v.push(signature.clone()))
            .or_insert_with(|| vec![signature]);
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

///////////////////////////////////////////// swap_proof_to_target
async fn swap_proof_to_target(
    proof: cdk00::Proof,
    keysets_info: &[KeySetInfo],
    target_keyset: &KeySet,
    target_amount: Amount,
    seed: &Seed,
    db: &dyn PocketRepository,
    client: &Arc<dyn ClowderMintConnector>,
    swap_config: SwapConfig,
) -> Result<HashMap<cdk01::PublicKey, cdk00::Proof>> {
    let kinfos: HashMap<cashu::Id, KeySetInfo> =
        keysets_info.iter().cloned().map(|k| (k.id, k)).collect();

    let swap_plan = prepare_swap(std::slice::from_ref(&proof), &kinfos)?;
    tracing::debug!("Swapping Proof to Target {target_amount}, {swap_plan:?}");
    let Some(swap_amount) = swap_plan.get(&proof.keyset_id) else {
        return Err(Error::Swap(
            "Swap Plan didn't contain proof keyset to swap to".to_string(),
        ));
    };
    let target = SplitTarget::Value(target_amount);
    let counter = db.counter(target_keyset.id).await?;
    let premint =
        cdk00::PreMintSecrets::from_seed(target_keyset.id, counter, seed, *swap_amount, &target)?;
    let blinds = premint.blinded_messages();
    db.increment_counter(target_keyset.id, counter, premint.len() as u32)
        .await?;

    let signatures = committed_swap(
        client.as_ref(),
        Some(db),
        vec![proof],
        blinds,
        &swap_config,
        HashMap::from([(target_keyset.id, premint.clone())]),
    )
    .await?;
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
    keysets_info: &[KeySetInfo],
    target_amount: Amount,
    seed: &Seed,
    db: &dyn PocketRepository,
    client: &Arc<dyn ClowderMintConnector>,
    swap_config: SwapConfig,
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
        SendPlan::NeedSplit {
            proof,
            split_amount,
            estimated_fee,
        } => {
            tracing::debug!(
                "Send Proof for {target_amount} - splitting with split {split_amount} and {estimated_fee} fee"
            );
            let swap_proof = db.mark_as_pendingspent(proof).await?;
            let target_kid = swap_proof.keyset_id;
            let swap_proof_keyset = client.get_mint_keyset(target_kid).await?;

            let _swapped_to_target = swap_proof_to_target(
                swap_proof,
                keysets_info,
                &swap_proof_keyset,
                split_amount,
                seed,
                db,
                client,
                swap_config,
            )
            .await?;

            // after swap, do prepare_payment again, expecting Ready and send proofs
            let unspent_proofs = db.list_unspent().await?;
            let mut proofs: Vec<cashu::Proof> = unspent_proofs.values().cloned().collect();
            // sort by amount as required by `prepare_payment`
            proofs.sort_by_key(|proof| proof.amount);

            let infos = collect_keyset_infos_from_proofs(unspent_proofs.values(), keysets_info)?;
            let kinfos: HashMap<cashu::Id, KeySetInfo> =
                infos.iter().map(|(k, v)| (*k, (*v).clone())).collect();

            let payment_plan = prepare_payment(&proofs, target_amount, &kinfos)?;

            match payment_plan {
                PaymentPlan::Ready { inputs, .. } => {
                    let proofs_to_send = inputs
                        .iter()
                        .map(|proof| proof.y())
                        .collect::<std::result::Result<Vec<cashu::PublicKey>, _>>()?;
                    for y in proofs_to_send {
                        let proof = db.mark_as_pendingspent(y).await?;
                        current_amount += proof.amount;
                        sending_proofs.insert(y, proof);
                    }
                }
                PaymentPlan::NeedSplit { .. } => {
                    return Err(Error::ExcessiveSplitting(target_amount));
                }
            };
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
        SendPlan::NeedSplit {
            proof,
            split_amount,
            ..
        } => {
            // Also add swap proof as-is, without swapping to target
            let swap_proof = db.mark_as_pendingspent(proof).await?;
            sending_proofs.insert(proof, swap_proof);
            send_amount += split_amount;
        }
    };

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

    use crate::pocket::test_utils::tests::{setup_commitment_mocks, test_swap_config};

    #[tokio::test]
    async fn swap_proof_to_target() {
        let (info, keyset) = core_tests::generate_random_ecash_keyset();
        let k_infos = vec![KeySetInfo::from(info)];
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
                let amounts = request.outputs.iter().map(|b| b.amount).collect::<Vec<_>>();
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
            &k_infos,
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
                let amounts = request.outputs.iter().map(|b| b.amount).collect::<Vec<_>>();
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

    #[tokio::test]
    async fn send_proofs_ready() {
        let (_, keyset) = core_tests::generate_random_ecash_keyset();
        let amounts = [Amount::from(8u64), Amount::from(16u64)];
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

        let mockclient = MockMintConnector::new();
        let arc_client: Arc<dyn ClowderMintConnector> = Arc::new(mockclient);

        let sent = super::send_proofs(
            SendPlan::Ready { proofs: ys },
            &[],
            Amount::from(24u64),
            &zero_seed(),
            &mockdb,
            &arc_client,
            test_swap_config(),
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
            Amount::from(24u64)
        );
    }

    #[tokio::test]
    async fn send_proofs_need_split_then_ready() {
        let (info, keyset) = core_tests::generate_random_ecash_keyset();
        let k_infos = vec![KeySetInfo::from(info.clone())];

        let swap_proof =
            core_tests::generate_random_ecash_proofs(&keyset, &[Amount::from(16u64)])[0].clone();
        let swap_y = swap_proof.y().unwrap();

        let ready_proofs = core_tests::generate_random_ecash_proofs(
            &keyset,
            &[Amount::from(8u64), Amount::from(4u64), Amount::from(1u64)],
        );

        let ready_by_y = ready_proofs
            .iter()
            .cloned()
            .map(|p| (p.y().unwrap(), p))
            .collect::<HashMap<_, _>>();

        let unspent = ready_by_y.clone();

        let mut mockdb = MockPocketRepository::new();
        let mut mockclient = MockMintConnector::new();

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
            .returning(move |_| Ok(KeySet::from(cloned_keyset_for_get.clone())));

        let cloned_keyset_for_sign = keyset.clone();
        mockclient
            .expect_post_swap_committed()
            .times(1)
            .returning(move |request| {
                let amounts = request.outputs.iter().map(|b| b.amount).collect::<Vec<_>>();
                let signatures =
                    core_tests::generate_ecash_signatures(&cloned_keyset_for_sign, &amounts);
                Ok(bcr_common::wire::swap::SwapResponse { signatures })
            });

        mockdb.expect_store_new().returning(|p| Ok(p.y().unwrap()));

        mockdb
            .expect_list_unspent()
            .times(1)
            .returning(move || Ok(unspent.clone()));

        mockdb
            .expect_mark_as_pendingspent()
            .times(4)
            .returning(move |y| {
                if y == swap_y {
                    Ok(swap_proof.clone())
                } else {
                    Ok(ready_by_y.get(&y).unwrap().clone())
                }
            });

        let arc_client: Arc<dyn ClowderMintConnector> = Arc::new(mockclient);

        let sent = super::send_proofs(
            SendPlan::NeedSplit {
                proof: swap_y,
                split_amount: Amount::from(13u64),
                estimated_fee: Amount::from(0u64),
            },
            &k_infos,
            Amount::from(13u64),
            &zero_seed(),
            &mockdb,
            &arc_client,
            test_swap_config(),
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
            Amount::from(13u64)
        );
    }

    #[tokio::test]
    async fn send_proofs_need_split_errors_if_second_plan_still_needs_split() {
        let (info, keyset) = core_tests::generate_random_ecash_keyset();
        let k_infos = vec![KeySetInfo::from(info.clone())];

        let swap_proof =
            core_tests::generate_random_ecash_proofs(&keyset, &[Amount::from(16u64)])[0].clone();
        let swap_y = swap_proof.y().unwrap();

        let unsplittable =
            core_tests::generate_random_ecash_proofs(&keyset, &[Amount::from(16u64)])[0].clone();
        let unspent = HashMap::from([(unsplittable.y().unwrap(), unsplittable)]);

        let mut mockdb = MockPocketRepository::new();
        let mut mockclient = MockMintConnector::new();

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
            .returning(move |_| Ok(KeySet::from(cloned_keyset_for_get.clone())));

        let cloned_keyset_for_sign = keyset.clone();
        mockclient
            .expect_post_swap_committed()
            .times(1)
            .returning(move |request| {
                let amounts = request.outputs.iter().map(|b| b.amount).collect::<Vec<_>>();
                let signatures =
                    core_tests::generate_ecash_signatures(&cloned_keyset_for_sign, &amounts);
                Ok(bcr_common::wire::swap::SwapResponse { signatures })
            });

        mockdb.expect_store_new().returning(|p| Ok(p.y().unwrap()));

        mockdb
            .expect_list_unspent()
            .times(1)
            .returning(move || Ok(unspent.clone()));

        mockdb
            .expect_mark_as_pendingspent()
            .times(1)
            .with(eq(swap_y))
            .returning(move |_| Ok(swap_proof.clone()));

        let arc_client: Arc<dyn ClowderMintConnector> = Arc::new(mockclient);

        let err = super::send_proofs(
            SendPlan::NeedSplit {
                proof: swap_y,
                split_amount: Amount::from(13u64),
                estimated_fee: Amount::from(0u64),
            },
            &k_infos,
            Amount::from(13u64),
            &zero_seed(),
            &mockdb,
            &arc_client,
            test_swap_config(),
        )
        .await
        .unwrap_err();

        assert!(matches!(err, Error::ExcessiveSplitting(a) if a == Amount::from(13u64)));
    }
}
