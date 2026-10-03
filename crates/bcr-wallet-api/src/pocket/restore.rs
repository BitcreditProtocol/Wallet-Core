use crate::{ClowderMintConnector, error::Error, error::Result};
use bcr_common::{
    cashu::{self, nut00 as cdk00, nut01 as cdk01, nut07 as cdk07, nut09 as cdk09},
    ecash,
};
use bcr_wallet_core::types::Seed;
use bcr_wallet_persistence::PocketRepository;
use std::{collections::HashMap, sync::Arc};

// as recommended by NUT13
const EMPTY_RESPONSES_BEFORE_ABORT: usize = 3;
const BATCH_SIZE: u32 = 100;

pub async fn restore_keysetid(
    seed: &Seed,
    kid: ecash::Id,
    client: &Arc<dyn ClowderMintConnector>,
    db: &dyn PocketRepository,
) -> Result<usize> {
    let mut zero_response_counter = 0;
    let mut total_proofs_restored = 0;
    let mut dbcursor = db.counter(kid).await?;
    let mut cursor = 0; // always start at 0 for restore
    while zero_response_counter < EMPTY_RESPONSES_BEFORE_ABORT {
        let restored_proofs = restore_batch(seed, kid, client, db, cursor, BATCH_SIZE).await?;
        cursor += BATCH_SIZE;
        if restored_proofs == 0 {
            zero_response_counter += 1;
        } else {
            zero_response_counter = 0;
            if cursor > dbcursor {
                db.increment_counter(kid, dbcursor, cursor - dbcursor)
                    .await?;
                dbcursor = cursor;
            }
        }
        total_proofs_restored += restored_proofs;
    }
    Ok(total_proofs_restored)
}

async fn restore_batch(
    seed: &Seed,
    kid: ecash::Id,
    client: &Arc<dyn ClowderMintConnector>,
    db: &dyn PocketRepository,
    counter: u32,
    batch_size: u32,
) -> Result<usize> {
    let premints =
        cdk00::PreMintSecrets::restore_batch(kid.into(), seed, counter, counter + batch_size)?;
    let request = cdk09::RestoreRequest {
        outputs: premints.blinded_messages(),
    };
    let resp = client.post_restore(request).await?;
    if resp.is_empty() {
        return Ok(0);
    }
    let keyset = client.get_mint_keyset(kid).await?;
    let mut proofs: HashMap<cdk01::PublicKey, cdk00::Proof> = HashMap::new();
    let mut premints_cursor = premints.iter();
    for (output, signature) in resp.into_iter() {
        let premint = loop {
            let Some(premint) = premints_cursor.next() else {
                return Err(Error::RestoreUnexpectedOutput);
            };
            if premint.blinded_message == output {
                break premint;
            }
        };
        let Some(dleq) = &signature.dleq else {
            return Err(Error::MissingDleq);
        };
        let Some(key) = keyset.keys.get(&signature.amount) else {
            return Err(Error::RestoreUnknownKeysetAmount(signature.amount, kid));
        };
        signature.verify_dleq(*key, output.blinded_secret)?;
        let c = cashu::dhke::unblind_message(&signature.c, &premint.r, key)?;
        let mut proof = cdk00::Proof::new(signature.amount, kid.into(), premint.secret.clone(), c);
        proof.dleq = Some(cashu::ProofDleq::new(
            dleq.e.clone(),
            dleq.s.clone(),
            premint.r.clone(),
        ));
        let y = proof.y()?;
        proofs.insert(y, proof);
    }
    if proofs.is_empty() {
        return Ok(0);
    }
    let proofs_len = proofs.len();
    let request = cdk07::CheckStateRequest {
        ys: proofs.keys().cloned().collect(),
    };
    let states = client.post_check_state(request).await?;
    let mut new_proofs = Vec::new();
    let mut pendingspent_proofs = Vec::new();
    for state in states.into_iter() {
        let proof = proofs
            .remove(&state.y)
            .ok_or(Error::RestoreUnexpectedCheckState)?;
        match state.state {
            cdk07::State::Unspent => new_proofs.push(proof),
            cdk07::State::Pending | cdk07::State::PendingSpent => pendingspent_proofs.push(proof),
            _ => {}
        }
    }
    for proof in new_proofs {
        db.store_new(proof).await?;
    }
    for proof in pendingspent_proofs {
        db.store_pendingspent(proof).await?;
    }
    Ok(proofs_len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external::mint::MockClowderMintConnector;
    use bcr_common::{core::signature, core_tests};
    use bcr_wallet_persistence::{MockPocketRepository, test_utils::tests::zero_seed};
    use cashu::{Amount, nut07 as cdk07};
    use mockall::predicate::eq;
    use rand::RngExt;

    /// Signs a restored output at a real keyset amount, as fixtures carry no zero-amount key.
    fn sign_restored_output(
        keyset: &ecash::MintKeySet,
        blind: &cdk00::BlindedMessage,
    ) -> cashu::BlindSignature {
        let signable = cdk00::BlindedMessage {
            amount: Amount::from(1),
            ..blind.clone()
        };
        signature::sign_ecash(keyset, &signable).expect("signature should be generated")
    }

    #[tokio::test]
    async fn restore_batch_empty_response() {
        let seed = zero_seed();
        let (_, keyset) = core_tests::generate_random_ecash_keyset();
        let keyset = bcr_wallet_core::util::to_keyset(&keyset, None);
        let mut client = MockClowderMintConnector::new();
        let db = MockPocketRepository::new();
        client
            .expect_post_restore()
            .times(1)
            .returning(|_| Ok(vec![]));

        let arc_client: Arc<dyn ClowderMintConnector> = Arc::new(client);
        super::restore_batch(&seed, keyset.id, &arc_client, &db, 0, BATCH_SIZE)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn restore_batch_all_spent() {
        let seed = zero_seed();
        let (_, mintkeyset) = core_tests::generate_random_ecash_keyset();
        let keyset = bcr_wallet_core::util::to_keyset(&mintkeyset, None);
        let mut client = MockClowderMintConnector::new();
        let db = MockPocketRepository::new();
        let cloned = mintkeyset.clone();
        client
            .expect_post_restore()
            .times(1)
            .returning(move |request| {
                let mut rng = rand::rng();
                let signatures = request
                    .outputs
                    .iter()
                    .map(|blind| {
                        let mut bblind = blind.clone();
                        let num = rng.random_range(..10);
                        bblind.amount = Amount::from(2u64.pow(num));
                        signature::sign_ecash(&cloned, &bblind)
                            .expect("signatures should be generated")
                    })
                    .collect::<Vec<_>>();
                Ok(request
                    .outputs
                    .into_iter()
                    .zip(signatures)
                    .collect::<Vec<_>>())
            });
        client
            .expect_get_mint_keyset()
            .times(1)
            .returning(move |_| Ok(bcr_wallet_core::util::to_keyset(&mintkeyset, None)));
        client
            .expect_post_check_state()
            .times(1)
            .returning(move |request| {
                let states = request
                    .ys
                    .iter()
                    .map(|y| cdk07::ProofState {
                        y: *y,
                        state: cdk07::State::Spent,
                        witness: None,
                    })
                    .collect();
                Ok(states)
            });

        let arc_client: Arc<dyn ClowderMintConnector> = Arc::new(client);
        super::restore_batch(&seed, keyset.id, &arc_client, &db, 0, BATCH_SIZE)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn restore_batch_all_unspent() {
        let seed = zero_seed();
        let (_, mintkeyset) = core_tests::generate_random_ecash_keyset();
        let keyset = bcr_wallet_core::util::to_keyset(&mintkeyset, None);
        let mut client = MockClowderMintConnector::new();
        let mut db = MockPocketRepository::new();
        let cloned_mintkeyset = mintkeyset.clone();
        client
            .expect_post_restore()
            .times(1)
            .returning(move |request| {
                let mut rng = rand::rng();
                let signatures = request
                    .outputs
                    .iter()
                    .map(|blind| {
                        let mut bblind = blind.clone();
                        let num = rng.random_range(..10);
                        bblind.amount = Amount::from(2u64.pow(num));
                        signature::sign_ecash(&cloned_mintkeyset, &bblind)
                            .expect("signatures should be generated")
                    })
                    .collect::<Vec<_>>();
                Ok(request
                    .outputs
                    .into_iter()
                    .zip(signatures)
                    .collect::<Vec<_>>())
            });
        client
            .expect_get_mint_keyset()
            .times(1)
            .returning(move |_| Ok(bcr_wallet_core::util::to_keyset(&mintkeyset, None)));
        client
            .expect_post_check_state()
            .times(1)
            .returning(move |request| {
                let states = request
                    .ys
                    .iter()
                    .map(|y| cdk07::ProofState {
                        y: *y,
                        state: cdk07::State::Unspent,
                        witness: None,
                    })
                    .collect();
                Ok(states)
            });
        db.expect_store_new()
            .times(BATCH_SIZE as usize)
            .returning(|p| Ok(p.y().expect("proof should have y")));

        let arc_client: Arc<dyn ClowderMintConnector> = Arc::new(client);
        let restored_proofs =
            super::restore_batch(&seed, keyset.id, &arc_client, &db, 0, BATCH_SIZE)
                .await
                .unwrap();
        assert_eq!(restored_proofs, BATCH_SIZE as usize);
    }

    #[tokio::test]
    async fn restore_batch_all_pending() {
        let seed = zero_seed();
        let (_, mintkeyset) = core_tests::generate_random_ecash_keyset();
        let keyset = bcr_wallet_core::util::to_keyset(&mintkeyset, None);
        let mut client = MockClowderMintConnector::new();
        let mut db = MockPocketRepository::new();
        let cloned_mintkeyset = mintkeyset.clone();
        client
            .expect_post_restore()
            .times(1)
            .returning(move |request| {
                let mut rng = rand::rng();
                let signatures = request
                    .outputs
                    .iter()
                    .map(|blind| {
                        let mut bblind = blind.clone();
                        let num = rng.random_range(..10);
                        bblind.amount = Amount::from(2u64.pow(num));
                        signature::sign_ecash(&cloned_mintkeyset, &bblind)
                            .expect("signatures should be generated")
                    })
                    .collect::<Vec<_>>();
                Ok(request
                    .outputs
                    .into_iter()
                    .zip(signatures)
                    .collect::<Vec<_>>())
            });
        client
            .expect_get_mint_keyset()
            .times(1)
            .returning(move |_| Ok(bcr_wallet_core::util::to_keyset(&mintkeyset, None)));
        client
            .expect_post_check_state()
            .times(1)
            .returning(move |request| {
                let states = request
                    .ys
                    .iter()
                    .map(|y| cdk07::ProofState {
                        y: *y,
                        state: cdk07::State::PendingSpent,
                        witness: None,
                    })
                    .collect();
                Ok(states)
            });
        db.expect_store_pendingspent()
            .times(BATCH_SIZE as usize)
            .returning(|p| Ok(p.y().expect("proof should have y")));

        let arc_client: Arc<dyn ClowderMintConnector> = Arc::new(client);
        let restored_proofs =
            super::restore_batch(&seed, keyset.id, &arc_client, &db, 0, BATCH_SIZE)
                .await
                .unwrap();
        assert_eq!(restored_proofs, BATCH_SIZE as usize);
    }

    async fn restore_keysetid_1stbatch_with_counter(stored: u32, increments: usize) {
        let seed = zero_seed();
        let (_, mintkeyset) = core_tests::generate_random_ecash_keyset();
        let keyset = bcr_wallet_core::util::to_keyset(&mintkeyset, None);
        let mut client = MockClowderMintConnector::new();
        client
            .expect_get_mint_keyset()
            .times(1)
            .returning(move |_| Ok(keyset.clone()));
        let mut db = MockPocketRepository::new();
        db.expect_counter()
            .times(1)
            .with(eq(mintkeyset.id))
            .returning(move |_| Ok(stored));
        let cloned_mintkeyset = mintkeyset.clone();
        client
            .expect_post_restore()
            .times(1)
            .returning(move |request| {
                let cdk09::RestoreRequest { outputs } = request;
                let signatures = outputs
                    .iter()
                    .map(|blind| {
                        let mut bblind = blind.clone();
                        bblind.amount = Amount::from(1u64);
                        signature::sign_ecash(&cloned_mintkeyset, &bblind)
                            .expect("signatures should be generated")
                    })
                    .collect::<Vec<_>>();
                Ok(outputs.into_iter().zip(signatures).collect::<Vec<_>>())
            });
        client
            .expect_post_check_state()
            .times(1)
            .returning(move |request| {
                let states: Vec<cdk07::ProofState> = request
                    .ys
                    .iter()
                    .map(|y| cdk07::ProofState {
                        state: cdk07::State::Unspent,
                        y: *y,
                        witness: None,
                    })
                    .collect();
                Ok(states)
            });
        db.expect_store_new()
            .times(BATCH_SIZE as usize)
            .returning(|p| Ok(p.y().unwrap()));
        db.expect_increment_counter()
            .times(increments)
            .with(eq(mintkeyset.id), eq(0), eq(BATCH_SIZE))
            .returning(|_, _, _| Ok(()));
        client
            .expect_post_restore()
            .times(EMPTY_RESPONSES_BEFORE_ABORT)
            .returning(move |_| Ok(vec![]));
        let arc_client: Arc<dyn ClowderMintConnector> = Arc::new(client);
        let total_restored = restore_keysetid(&seed, mintkeyset.id, &arc_client, &db)
            .await
            .unwrap();
        assert_eq!(total_restored, BATCH_SIZE as usize);
    }

    #[tokio::test]
    async fn restore_keysetid_1stbatch() {
        restore_keysetid_1stbatch_with_counter(0, 1).await;
    }

    #[tokio::test]
    async fn restore_keysetid_never_lowers_counter() {
        restore_keysetid_1stbatch_with_counter(3 * BATCH_SIZE, 0).await;
    }

    #[tokio::test]
    async fn restore_keysetid_2ndbatch() {
        let seed = zero_seed();
        let (_, mintkeyset) = core_tests::generate_random_ecash_keyset();
        let keyset = bcr_wallet_core::util::to_keyset(&mintkeyset, None);
        let mut client = MockClowderMintConnector::new();
        client
            .expect_get_mint_keyset()
            .times(1)
            .returning(move |_| Ok(keyset.clone()));
        let mut db = MockPocketRepository::new();
        db.expect_counter()
            .times(1)
            .with(eq(mintkeyset.id))
            .returning(move |_| Ok(0));
        client
            .expect_post_restore()
            .times(1)
            .returning(move |_| Ok(vec![]));
        let cloned_mintkeyset = mintkeyset.clone();
        client
            .expect_post_restore()
            .times(1)
            .returning(move |request| {
                let cdk09::RestoreRequest { outputs } = request;
                let signatures = outputs
                    .iter()
                    .map(|blind| {
                        let mut bblind = blind.clone();
                        bblind.amount = Amount::from(1u64);
                        signature::sign_ecash(&cloned_mintkeyset, &bblind)
                            .expect("signatures should be generated")
                    })
                    .collect::<Vec<_>>();
                Ok(outputs.into_iter().zip(signatures).collect::<Vec<_>>())
            });
        client
            .expect_post_check_state()
            .times(1)
            .returning(move |request| {
                let states: Vec<cdk07::ProofState> = request
                    .ys
                    .iter()
                    .map(|y| cdk07::ProofState {
                        state: cdk07::State::Unspent,
                        y: *y,
                        witness: None,
                    })
                    .collect();
                Ok(states)
            });
        db.expect_store_new()
            .times(BATCH_SIZE as usize)
            .returning(|p| Ok(p.y().unwrap()));
        db.expect_increment_counter()
            .times(1)
            .with(eq(mintkeyset.id), eq(0), eq(2 * BATCH_SIZE))
            .returning(|_, _, _| Ok(()));
        client
            .expect_post_restore()
            .times(EMPTY_RESPONSES_BEFORE_ABORT)
            .returning(move |_| Ok(vec![]));
        let arc_client: Arc<dyn ClowderMintConnector> = Arc::new(client);
        let total_restored = restore_keysetid(&seed, mintkeyset.id, &arc_client, &db)
            .await
            .unwrap();
        assert_eq!(total_restored, BATCH_SIZE as usize);
    }

    #[tokio::test]
    async fn restore_keysetid_2ndpartial() {
        let seed = zero_seed();
        let (_, mintkeyset) = core_tests::generate_random_ecash_keyset();
        let keyset = bcr_wallet_core::util::to_keyset(&mintkeyset, None);
        let mut client = MockClowderMintConnector::new();
        client
            .expect_get_mint_keyset()
            .times(1)
            .returning(move |_| Ok(keyset.clone()));
        let mut db = MockPocketRepository::new();
        db.expect_counter()
            .times(1)
            .with(eq(mintkeyset.id))
            .returning(move |_| Ok(0));
        client
            .expect_post_restore()
            .times(1)
            .returning(move |_| Ok(vec![]));
        let cloned_mintkeyset = mintkeyset.clone();
        client
            .expect_post_restore()
            .times(1)
            .returning(move |request| {
                let cdk09::RestoreRequest { mut outputs } = request;
                outputs.truncate(outputs.len() / 3);
                let signatures = outputs
                    .iter()
                    .map(|blind| {
                        let mut bblind = blind.clone();
                        bblind.amount = Amount::from(1u64);
                        signature::sign_ecash(&cloned_mintkeyset, &bblind)
                            .expect("signatures should be generated")
                    })
                    .collect::<Vec<_>>();
                Ok(outputs.into_iter().zip(signatures).collect::<Vec<_>>())
            });
        client
            .expect_post_check_state()
            .times(1)
            .returning(move |request| {
                let states: Vec<cdk07::ProofState> = request
                    .ys
                    .iter()
                    .map(|y| cdk07::ProofState {
                        state: cdk07::State::Unspent,
                        y: *y,
                        witness: None,
                    })
                    .collect();
                Ok(states)
            });
        db.expect_store_new()
            .times((BATCH_SIZE / 3) as usize)
            .returning(|p| Ok(p.y().unwrap()));
        db.expect_increment_counter()
            .times(1)
            .with(eq(mintkeyset.id), eq(0), eq(2 * BATCH_SIZE))
            .returning(|_, _, _| Ok(()));
        client
            .expect_post_restore()
            .times(EMPTY_RESPONSES_BEFORE_ABORT)
            .returning(move |_| Ok(vec![]));
        //
        let arc_client: Arc<dyn ClowderMintConnector> = Arc::new(client);
        let total_restored = restore_keysetid(&seed, mintkeyset.id, &arc_client, &db)
            .await
            .unwrap();
        assert_eq!(total_restored, (BATCH_SIZE / 3) as usize);
    }

    #[tokio::test]
    async fn restore_batch_unrequested_output() {
        let seed = zero_seed();
        let (_, mintkeyset) = core_tests::generate_random_ecash_keyset();
        let keyset = bcr_wallet_core::util::to_keyset(&mintkeyset, None);
        let mut client = MockClowderMintConnector::new();
        let db = MockPocketRepository::new();
        let cloned = mintkeyset.clone();
        client.expect_post_restore().times(1).returning(move |_| {
            let other_premints =
                cdk00::PreMintSecrets::restore_batch(cloned.id.into(), &zero_seed(), 1000, 1001)
                    .expect("premints should be generated");
            let other_blind = other_premints.blinded_messages().remove(0);
            let signature = sign_restored_output(&cloned, &other_blind);
            Ok(vec![(other_blind, signature)])
        });
        client
            .expect_get_mint_keyset()
            .times(1)
            .returning(move |_| Ok(bcr_wallet_core::util::to_keyset(&mintkeyset, None)));

        let arc_client: Arc<dyn ClowderMintConnector> = Arc::new(client);
        let result = super::restore_batch(&seed, keyset.id, &arc_client, &db, 0, BATCH_SIZE).await;
        assert!(matches!(
            result,
            Err(crate::error::Error::RestoreUnexpectedOutput)
        ));
    }

    #[tokio::test]
    async fn restore_batch_out_of_order_output() {
        let seed = zero_seed();
        let (_, mintkeyset) = core_tests::generate_random_ecash_keyset();
        let keyset = bcr_wallet_core::util::to_keyset(&mintkeyset, None);
        let mut client = MockClowderMintConnector::new();
        let db = MockPocketRepository::new();
        let cloned = mintkeyset.clone();
        client
            .expect_post_restore()
            .times(1)
            .returning(move |request| {
                let outputs = request.outputs;
                let sign = |b: &cdk00::BlindedMessage| sign_restored_output(&cloned, b);
                let s0 = sign(&outputs[1]);
                let s1 = sign(&outputs[0]);
                Ok(vec![(outputs[1].clone(), s0), (outputs[0].clone(), s1)])
            });
        client
            .expect_get_mint_keyset()
            .times(1)
            .returning(move |_| Ok(bcr_wallet_core::util::to_keyset(&mintkeyset, None)));

        let arc_client: Arc<dyn ClowderMintConnector> = Arc::new(client);
        let result = super::restore_batch(&seed, keyset.id, &arc_client, &db, 0, 2).await;
        assert!(matches!(
            result,
            Err(crate::error::Error::RestoreUnexpectedOutput)
        ));
    }

    #[tokio::test]
    async fn restore_batch_wrong_keyset_id() {
        let seed = zero_seed();
        let (_, mintkeyset) = core_tests::generate_random_ecash_keyset();
        let (_, other_keyset) = core_tests::generate_random_ecash_keyset();
        let keyset = bcr_wallet_core::util::to_keyset(&mintkeyset, None);
        let mut client = MockClowderMintConnector::new();
        let mut db = MockPocketRepository::new();
        let cloned = mintkeyset.clone();
        let other_id: cashu::nut02::Id = other_keyset.id.into();
        client
            .expect_post_restore()
            .times(1)
            .returning(move |request| {
                let outputs = request.outputs;
                let signatures = outputs
                    .iter()
                    .map(|blind| {
                        let mut signature = sign_restored_output(&cloned, blind);
                        signature.keyset_id = other_id;
                        signature
                    })
                    .collect::<Vec<_>>();
                Ok(outputs.into_iter().zip(signatures).collect::<Vec<_>>())
            });
        client
            .expect_get_mint_keyset()
            .times(1)
            .returning(move |_| Ok(bcr_wallet_core::util::to_keyset(&mintkeyset, None)));
        client
            .expect_post_check_state()
            .times(1)
            .returning(move |request| {
                let states = request
                    .ys
                    .iter()
                    .map(|y| cdk07::ProofState {
                        y: *y,
                        state: cdk07::State::Unspent,
                        witness: None,
                    })
                    .collect();
                Ok(states)
            });
        let expected_kid: cashu::nut02::Id = keyset.id.into();
        db.expect_store_new()
            .times(BATCH_SIZE as usize)
            .returning(move |p| {
                assert_eq!(p.keyset_id, expected_kid);
                Ok(p.y().expect("proof should have y"))
            });

        let arc_client: Arc<dyn ClowderMintConnector> = Arc::new(client);
        let restored_proofs =
            super::restore_batch(&seed, keyset.id, &arc_client, &db, 0, BATCH_SIZE)
                .await
                .unwrap();
        assert_eq!(restored_proofs, BATCH_SIZE as usize);
    }

    #[tokio::test]
    async fn restore_batch_missing_dleq() {
        let seed = zero_seed();
        let (_, mintkeyset) = core_tests::generate_random_ecash_keyset();
        let keyset = bcr_wallet_core::util::to_keyset(&mintkeyset, None);
        let mut client = MockClowderMintConnector::new();
        let db = MockPocketRepository::new();
        let cloned = mintkeyset.clone();
        client
            .expect_post_restore()
            .times(1)
            .returning(move |request| {
                let outputs = request.outputs;
                let signatures = outputs
                    .iter()
                    .map(|blind| {
                        let mut signature = sign_restored_output(&cloned, blind);
                        signature.dleq = None;
                        signature
                    })
                    .collect::<Vec<_>>();
                Ok(outputs.into_iter().zip(signatures).collect::<Vec<_>>())
            });
        client
            .expect_get_mint_keyset()
            .times(1)
            .returning(move |_| Ok(bcr_wallet_core::util::to_keyset(&mintkeyset, None)));

        let arc_client: Arc<dyn ClowderMintConnector> = Arc::new(client);
        let result = super::restore_batch(&seed, keyset.id, &arc_client, &db, 0, BATCH_SIZE).await;
        assert!(matches!(result, Err(crate::error::Error::MissingDleq)));
    }

    #[tokio::test]
    async fn restore_batch_invalid_dleq() {
        let seed = zero_seed();
        let (_, mintkeyset) = core_tests::generate_random_ecash_keyset();
        let keyset = bcr_wallet_core::util::to_keyset(&mintkeyset, None);
        let mut client = MockClowderMintConnector::new();
        let db = MockPocketRepository::new();
        let cloned = mintkeyset.clone();
        client
            .expect_post_restore()
            .times(1)
            .returning(move |request| {
                let outputs = request.outputs;
                let signatures = outputs
                    .iter()
                    .map(|blind| {
                        let mut signature = sign_restored_output(&cloned, blind);
                        let dleq = signature.dleq.as_mut().expect("dleq should be present");
                        std::mem::swap(&mut dleq.e, &mut dleq.s);
                        signature
                    })
                    .collect::<Vec<_>>();
                Ok(outputs.into_iter().zip(signatures).collect::<Vec<_>>())
            });
        client
            .expect_get_mint_keyset()
            .times(1)
            .returning(move |_| Ok(bcr_wallet_core::util::to_keyset(&mintkeyset, None)));

        let arc_client: Arc<dyn ClowderMintConnector> = Arc::new(client);
        let result = super::restore_batch(&seed, keyset.id, &arc_client, &db, 0, BATCH_SIZE).await;
        assert!(matches!(result, Err(crate::error::Error::Cdk12(_))));
    }

    #[tokio::test]
    async fn restore_batch_duplicate_output() {
        let seed = zero_seed();
        let (_, mintkeyset) = core_tests::generate_random_ecash_keyset();
        let keyset = bcr_wallet_core::util::to_keyset(&mintkeyset, None);
        let mut client = MockClowderMintConnector::new();
        let db = MockPocketRepository::new();
        let cloned = mintkeyset.clone();
        client
            .expect_post_restore()
            .times(1)
            .returning(move |request| {
                let outputs = request.outputs;
                let signature = sign_restored_output(&cloned, &outputs[0]);
                Ok(vec![
                    (outputs[0].clone(), signature.clone()),
                    (outputs[0].clone(), signature),
                ])
            });
        client
            .expect_get_mint_keyset()
            .times(1)
            .returning(move |_| Ok(bcr_wallet_core::util::to_keyset(&mintkeyset, None)));

        let arc_client: Arc<dyn ClowderMintConnector> = Arc::new(client);
        let result = super::restore_batch(&seed, keyset.id, &arc_client, &db, 0, BATCH_SIZE).await;
        assert!(matches!(
            result,
            Err(crate::error::Error::RestoreUnexpectedOutput)
        ));
    }

    #[tokio::test]
    async fn restore_batch_unknown_keyset_amount() {
        let seed = zero_seed();
        let (_, mintkeyset) = core_tests::generate_random_ecash_keyset();
        let keyset = bcr_wallet_core::util::to_keyset(&mintkeyset, None);
        let mut client = MockClowderMintConnector::new();
        let db = MockPocketRepository::new();
        let cloned = mintkeyset.clone();
        client
            .expect_post_restore()
            .times(1)
            .returning(move |request| {
                let outputs = request.outputs;
                let signatures = outputs
                    .iter()
                    .map(|blind| {
                        let mut signature = sign_restored_output(&cloned, blind);
                        signature.amount = Amount::from(3u64);
                        signature
                    })
                    .collect::<Vec<_>>();
                Ok(outputs.into_iter().zip(signatures).collect::<Vec<_>>())
            });
        client
            .expect_get_mint_keyset()
            .times(1)
            .returning(move |_| Ok(bcr_wallet_core::util::to_keyset(&mintkeyset, None)));

        let arc_client: Arc<dyn ClowderMintConnector> = Arc::new(client);
        let result = super::restore_batch(&seed, keyset.id, &arc_client, &db, 0, BATCH_SIZE).await;
        assert!(matches!(
            result,
            Err(crate::error::Error::RestoreUnknownKeysetAmount(_, _))
        ));
    }

    #[tokio::test]
    async fn restore_batch_duplicate_check_state_y() {
        let seed = zero_seed();
        let (_, mintkeyset) = core_tests::generate_random_ecash_keyset();
        let keyset = bcr_wallet_core::util::to_keyset(&mintkeyset, None);
        let mut client = MockClowderMintConnector::new();
        let db = MockPocketRepository::new();
        let cloned = mintkeyset.clone();
        client
            .expect_post_restore()
            .times(1)
            .returning(move |request| {
                let outputs = request.outputs;
                let sign = |b: &cdk00::BlindedMessage| sign_restored_output(&cloned, b);
                let signatures = outputs.iter().map(sign).collect::<Vec<_>>();
                Ok(outputs.into_iter().zip(signatures).collect::<Vec<_>>())
            });
        client
            .expect_get_mint_keyset()
            .times(1)
            .returning(move |_| Ok(bcr_wallet_core::util::to_keyset(&mintkeyset, None)));
        client
            .expect_post_check_state()
            .times(1)
            .returning(move |request| {
                let y = request.ys[0];
                Ok(vec![
                    cdk07::ProofState {
                        y,
                        state: cdk07::State::Unspent,
                        witness: None,
                    },
                    cdk07::ProofState {
                        y,
                        state: cdk07::State::Unspent,
                        witness: None,
                    },
                ])
            });

        let arc_client: Arc<dyn ClowderMintConnector> = Arc::new(client);
        let result = super::restore_batch(&seed, keyset.id, &arc_client, &db, 0, BATCH_SIZE).await;
        assert!(matches!(
            result,
            Err(crate::error::Error::RestoreUnexpectedCheckState)
        ));
    }

    #[tokio::test]
    async fn restore_batch_spent_then_unspent_for_same_y_is_rejected() {
        let seed = zero_seed();
        let (_, mintkeyset) = core_tests::generate_random_ecash_keyset();
        let keyset = bcr_wallet_core::util::to_keyset(&mintkeyset, None);
        let mut client = MockClowderMintConnector::new();
        let db = MockPocketRepository::new();
        let cloned = mintkeyset.clone();
        client
            .expect_post_restore()
            .times(1)
            .returning(move |request| {
                let outputs = request.outputs;
                let sign = |b: &cdk00::BlindedMessage| sign_restored_output(&cloned, b);
                let signatures = outputs.iter().map(sign).collect::<Vec<_>>();
                Ok(outputs.into_iter().zip(signatures).collect::<Vec<_>>())
            });
        client
            .expect_get_mint_keyset()
            .times(1)
            .returning(move |_| Ok(bcr_wallet_core::util::to_keyset(&mintkeyset, None)));
        client
            .expect_post_check_state()
            .times(1)
            .returning(move |request| {
                let y = request.ys[0];
                Ok(vec![
                    cdk07::ProofState {
                        y,
                        state: cdk07::State::Spent,
                        witness: None,
                    },
                    cdk07::ProofState {
                        y,
                        state: cdk07::State::Unspent,
                        witness: None,
                    },
                ])
            });

        let arc_client: Arc<dyn ClowderMintConnector> = Arc::new(client);
        let result = super::restore_batch(&seed, keyset.id, &arc_client, &db, 0, BATCH_SIZE).await;
        assert!(matches!(
            result,
            Err(crate::error::Error::RestoreUnexpectedCheckState)
        ));
    }
}
