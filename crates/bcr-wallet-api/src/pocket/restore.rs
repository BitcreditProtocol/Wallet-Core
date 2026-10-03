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
    let mut cursor = 0; // always start at 0 for restore
    while zero_response_counter < EMPTY_RESPONSES_BEFORE_ABORT {
        let restored_proofs = restore_batch(seed, kid, client, db, cursor, BATCH_SIZE).await?;
        cursor += BATCH_SIZE;
        if restored_proofs == 0 {
            zero_response_counter += 1;
        } else {
            zero_response_counter = 0;
            db.advance_counter_to(kid, cursor).await?;
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
    if states.len() != proofs_len {
        return Err(Error::RestoreUnexpectedCheckState);
    }
    let restored = states
        .into_iter()
        .map(|state| {
            proofs
                .remove(&state.y)
                .map(|proof| (state.state, proof))
                .ok_or(Error::RestoreUnexpectedCheckState)
        })
        .collect::<Result<Vec<_>>>()?;
    for (state, proof) in restored {
        match state {
            cdk07::State::Unspent => {
                db.store_new(proof).await?;
            }
            cdk07::State::Pending | cdk07::State::PendingSpent => {
                db.store_pendingspent(proof).await?;
            }
            _ => {}
        }
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

    #[tokio::test]
    async fn restore_keysetid_1stbatch() {
        let seed = zero_seed();
        let (_, mintkeyset) = core_tests::generate_random_ecash_keyset();
        let keyset = bcr_wallet_core::util::to_keyset(&mintkeyset, None);
        let mut client = MockClowderMintConnector::new();
        client
            .expect_get_mint_keyset()
            .times(1)
            .returning(move |_| Ok(keyset.clone()));
        let mut db = MockPocketRepository::new();
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
        db.expect_advance_counter_to()
            .times(1)
            .with(eq(mintkeyset.id), eq(BATCH_SIZE))
            .returning(|_, _| Ok(()));
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
        db.expect_advance_counter_to()
            .times(1)
            .with(eq(mintkeyset.id), eq(2 * BATCH_SIZE))
            .returning(|_, _| Ok(()));
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
        db.expect_advance_counter_to()
            .times(1)
            .with(eq(mintkeyset.id), eq(2 * BATCH_SIZE))
            .returning(|_, _| Ok(()));
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

    type Restored = Vec<(cdk00::BlindedMessage, cashu::BlindSignature)>;

    fn mock_restore(
        respond: impl Fn(&ecash::MintKeySet, Vec<cdk00::BlindedMessage>) -> Restored + Send + 'static,
    ) -> (ecash::Id, MockClowderMintConnector) {
        let (_, mintkeyset) = core_tests::generate_random_ecash_keyset();
        let kid = mintkeyset.id;
        let keyset = bcr_wallet_core::util::to_keyset(&mintkeyset, None);
        let mut client = MockClowderMintConnector::new();
        client
            .expect_post_restore()
            .times(1)
            .returning(move |request| Ok(respond(&mintkeyset, request.outputs)));
        client
            .expect_get_mint_keyset()
            .times(1)
            .returning(move |_| Ok(keyset.clone()));
        (kid, client)
    }

    fn sign_all(
        keyset: &ecash::MintKeySet,
        outputs: Vec<cdk00::BlindedMessage>,
        tamper: impl Fn(&mut cashu::BlindSignature),
    ) -> Restored {
        outputs
            .into_iter()
            .map(|blind| {
                let mut signature = sign_restored_output(keyset, &blind);
                tamper(&mut signature);
                (blind, signature)
            })
            .collect()
    }

    fn mock_check_state(
        client: &mut MockClowderMintConnector,
        respond: impl Fn(Vec<cdk01::PublicKey>) -> Vec<cdk07::ProofState> + Send + 'static,
    ) {
        client
            .expect_post_check_state()
            .times(1)
            .returning(move |request| Ok(respond(request.ys)));
    }

    fn proof_state(y: cdk01::PublicKey, state: cdk07::State) -> cdk07::ProofState {
        cdk07::ProofState {
            y,
            state,
            witness: None,
        }
    }

    async fn run_restore_batch(
        kid: ecash::Id,
        client: MockClowderMintConnector,
        db: &MockPocketRepository,
    ) -> Result<usize> {
        let client: Arc<dyn ClowderMintConnector> = Arc::new(client);
        super::restore_batch(&zero_seed(), kid, &client, db, 0, BATCH_SIZE).await
    }

    async fn restore_batch_error(
        respond: impl Fn(&ecash::MintKeySet, Vec<cdk00::BlindedMessage>) -> Restored + Send + 'static,
    ) -> Error {
        let (kid, client) = mock_restore(respond);
        run_restore_batch(kid, client, &MockPocketRepository::new())
            .await
            .unwrap_err()
    }

    #[tokio::test]
    async fn restore_batch_unrequested_output() {
        let err = restore_batch_error(|keyset, _| {
            let other =
                cdk00::PreMintSecrets::restore_batch(keyset.id.into(), &zero_seed(), 1000, 1001)
                    .expect("premints should be generated");
            sign_all(keyset, other.blinded_messages(), |_| {})
        })
        .await;
        assert!(matches!(err, Error::RestoreUnexpectedOutput));
    }

    #[tokio::test]
    async fn restore_batch_out_of_order_output() {
        let err = restore_batch_error(|keyset, mut outputs| {
            outputs.swap(0, 1);
            sign_all(keyset, outputs, |_| {})
        })
        .await;
        assert!(matches!(err, Error::RestoreUnexpectedOutput));
    }

    #[tokio::test]
    async fn restore_batch_duplicate_output() {
        let err = restore_batch_error(|keyset, outputs| {
            sign_all(keyset, vec![outputs[0].clone(), outputs[0].clone()], |_| {})
        })
        .await;
        assert!(matches!(err, Error::RestoreUnexpectedOutput));
    }

    #[tokio::test]
    async fn restore_batch_missing_dleq() {
        let err = restore_batch_error(|keyset, outputs| {
            sign_all(keyset, outputs, |signature| signature.dleq = None)
        })
        .await;
        assert!(matches!(err, Error::MissingDleq));
    }

    #[tokio::test]
    async fn restore_batch_invalid_dleq() {
        let err = restore_batch_error(|keyset, outputs| {
            sign_all(keyset, outputs, |signature| {
                let dleq = signature.dleq.as_mut().expect("dleq should be present");
                std::mem::swap(&mut dleq.e, &mut dleq.s);
            })
        })
        .await;
        assert!(matches!(err, Error::Cdk12(_)));
    }

    #[tokio::test]
    async fn restore_batch_unknown_keyset_amount() {
        let err = restore_batch_error(|keyset, outputs| {
            sign_all(keyset, outputs, |signature| {
                signature.amount = Amount::from(3u64)
            })
        })
        .await;
        assert!(matches!(err, Error::RestoreUnknownKeysetAmount(_, _)));
    }

    #[tokio::test]
    async fn restore_batch_wrong_keyset_id() {
        let (_, other_keyset) = core_tests::generate_random_ecash_keyset();
        let other_id: cashu::nut02::Id = other_keyset.id.into();
        let (kid, mut client) = mock_restore(move |keyset, outputs| {
            sign_all(keyset, outputs, |signature| signature.keyset_id = other_id)
        });
        mock_check_state(&mut client, |ys| {
            ys.into_iter()
                .map(|y| proof_state(y, cdk07::State::Unspent))
                .collect()
        });
        let expected_kid: cashu::nut02::Id = kid.into();
        let mut db = MockPocketRepository::new();
        db.expect_store_new()
            .times(BATCH_SIZE as usize)
            .returning(move |p| {
                assert_eq!(p.keyset_id, expected_kid);
                Ok(p.y().expect("proof should have y"))
            });

        let restored_proofs = run_restore_batch(kid, client, &db).await.unwrap();
        assert_eq!(restored_proofs, BATCH_SIZE as usize);
    }

    async fn restore_batch_check_state_error(
        respond: impl Fn(Vec<cdk01::PublicKey>) -> Vec<cdk07::ProofState> + Send + 'static,
    ) -> Error {
        let (kid, mut client) = mock_restore(|keyset, outputs| sign_all(keyset, outputs, |_| {}));
        mock_check_state(&mut client, respond);
        run_restore_batch(kid, client, &MockPocketRepository::new())
            .await
            .unwrap_err()
    }

    #[tokio::test]
    async fn restore_batch_duplicate_check_state_y_stores_nothing() {
        let err = restore_batch_check_state_error(|mut ys| {
            ys[1] = ys[0];
            let mut states: Vec<_> = ys
                .into_iter()
                .map(|y| proof_state(y, cdk07::State::Unspent))
                .collect();
            states[0].state = cdk07::State::Spent;
            states
        })
        .await;
        assert!(matches!(err, Error::RestoreUnexpectedCheckState));
    }

    #[tokio::test]
    async fn restore_batch_missing_check_state_y_stores_nothing() {
        let err = restore_batch_check_state_error(|mut ys| {
            ys.pop();
            ys.into_iter()
                .map(|y| proof_state(y, cdk07::State::Unspent))
                .collect()
        })
        .await;
        assert!(matches!(err, Error::RestoreUnexpectedCheckState));
    }
}
