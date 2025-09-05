/// ----- standard library imports
use std::collections::HashMap;
// ----- extra library imports
use bitcoin::bip32 as btc32;
use cashu::{nut00 as cdk00, nut01 as cdk01, nut07 as cdk07, nut09 as cdk09};
// ----- local imports
use crate::{MintConnector, error::Result, pocket::PocketRepository};

// ----- end imports

// as recommended by NUT13
const EMPTY_RESPONSES_BEFORE_ABORT: usize = 3;
const BATCH_SIZE: u32 = 100;

pub async fn restore_keysetid(
    xpriv: btc32::Xpriv,
    kid: cashu::Id,
    client: &dyn MintConnector,
    db: &dyn PocketRepository,
) -> Result<usize> {
    let mut zero_response_counter = 0;
    let mut total_proofs_restored = 0;
    let mut dbcursor = db.counter(kid).await?;
    let mut cursor = dbcursor;
    while zero_response_counter < EMPTY_RESPONSES_BEFORE_ABORT {
        let restored_proofs = restore_batch(xpriv, kid, client, db, cursor, BATCH_SIZE).await?;
        cursor += BATCH_SIZE;
        if restored_proofs == 0 {
            zero_response_counter += 1;
        } else {
            zero_response_counter = 0;
            db.increment_counter(kid, dbcursor, cursor - dbcursor)
                .await?;
            dbcursor = cursor;
        }
        total_proofs_restored += restored_proofs;
    }
    Ok(total_proofs_restored)
}

async fn restore_batch(
    xpriv: btc32::Xpriv,
    kid: cashu::Id,
    client: &dyn MintConnector,
    db: &dyn PocketRepository,
    counter: u32,
    batch_size: u32,
) -> Result<usize> {
    let premints =
        cdk00::PreMintSecrets::restore_batch(kid, xpriv, counter, counter + batch_size - 1)?;
    let request = cdk09::RestoreRequest {
        outputs: premints.blinded_messages(),
    };
    let cdk09::RestoreResponse {
        outputs,
        signatures,
        ..
    } = client.post_restore(request).await?;
    if signatures.is_empty() {
        return Ok(0);
    }
    let keyset = client.get_mint_keyset(kid).await?;
    let mut proofs: HashMap<cdk01::PublicKey, cdk00::Proof> = HashMap::new();
    let mut premints_cursor = premints.iter();
    for (output, signature) in outputs.into_iter().zip(signatures.into_iter()) {
        let premint = loop {
            let premint = premints_cursor
                .next()
                .expect("premint cursor should have next item");
            if premint.blinded_message == output {
                break premint;
            }
        };
        let Some(key) = keyset.keys.get(&signature.amount) else {
            tracing::error!(
                "No mint key for amount: {} in kid: {}",
                signature.amount,
                keyset.id,
            );
            continue;
        };
        let result = cashu::dhke::unblind_message(&signature.c, &premint.r, key);
        let Ok(c) = result else {
            tracing::error!(
                "unblind_message fail: kid: {}, amount {}",
                signature.amount,
                keyset.id,
            );
            continue;
        };
        let proof = cdk00::Proof::new(
            signature.amount,
            signature.keyset_id,
            premint.secret.clone(),
            c,
        );
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
    let cdk07::CheckStateResponse { states } = client.post_check_state(request).await?;
    for state in states.into_iter() {
        match state.state {
            cdk07::State::Unspent => {
                let proof = proofs
                    .remove(&state.y)
                    .expect("y in response comes from proofs");
                db.store_new(proof).await?;
            }
            cdk07::State::Pending | cdk07::State::PendingSpent => {
                let proof = proofs
                    .remove(&state.y)
                    .expect("y in response comes from proofs");
                db.store_pendingspent(proof).await?;
            }
            _ => {}
        }
    }
    Ok(proofs_len)
}

// tests contain rand related stuff, better skip them on wasm32
#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use crate::{pocket::MockPocketRepository, utils::tests::MockMintConnector};
    use bcr_wdc_utils::{keys as keys_utils, keys::test_utils as keys_test};
    use cashu::{Amount, KeySet, RestoreResponse, nut07 as cdk07};
    use mockall::predicate::eq;
    use rand::Rng;

    #[tokio::test]
    async fn restore_batch_empty_response() {
        let seed = [0u8; 32];
        let xpriv = btc32::Xpriv::new_master(bitcoin::Network::Regtest, &seed).unwrap();
        let (_, keyset) = keys_test::generate_random_keyset();
        let keyset = KeySet::from(keyset);
        let mut client = MockMintConnector::new();
        let db = MockPocketRepository::new();
        client.expect_post_restore().times(1).returning(|_| {
            Ok(RestoreResponse {
                outputs: Default::default(),
                signatures: Default::default(),
                promises: Default::default(),
            })
        });

        super::restore_batch(xpriv, keyset.id, &client, &db, 0, BATCH_SIZE)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn restore_batch_all_spent() {
        let seed = [0u8; 32];
        let xpriv = btc32::Xpriv::new_master(bitcoin::Network::Regtest, &seed).unwrap();
        let (_, mintkeyset) = keys_test::generate_random_keyset();
        let keyset = KeySet::from(mintkeyset.clone());
        let mut client = MockMintConnector::new();
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
                        keys_utils::sign_with_keys(&cloned, &bblind)
                            .expect("signatures should be generated")
                    })
                    .collect();
                Ok(RestoreResponse {
                    outputs: request.outputs,
                    signatures,
                    promises: Default::default(),
                })
            });
        client
            .expect_get_mint_keyset()
            .times(1)
            .returning(move |_| Ok(KeySet::from(mintkeyset.clone())));
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
                let response = cdk07::CheckStateResponse { states };
                Ok(response)
            });

        super::restore_batch(xpriv, keyset.id, &client, &db, 0, BATCH_SIZE)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn restore_batch_all_unspent() {
        let seed = [0u8; 32];
        let xpriv = btc32::Xpriv::new_master(bitcoin::Network::Regtest, &seed).unwrap();
        let (_, mintkeyset) = keys_test::generate_random_keyset();
        let keyset = KeySet::from(mintkeyset.clone());
        let mut client = MockMintConnector::new();
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
                        keys_utils::sign_with_keys(&cloned_mintkeyset, &bblind)
                            .expect("signatures should be generated")
                    })
                    .collect();
                Ok(RestoreResponse {
                    outputs: request.outputs,
                    signatures,
                    promises: Default::default(),
                })
            });
        client
            .expect_get_mint_keyset()
            .times(1)
            .returning(move |_| Ok(KeySet::from(mintkeyset.clone())));
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
                let response = cdk07::CheckStateResponse { states };
                Ok(response)
            });
        db.expect_store_new()
            .times(BATCH_SIZE as usize)
            .returning(|p| Ok(p.y().expect("proof should have y")));

        let restored_proofs = super::restore_batch(xpriv, keyset.id, &client, &db, 0, BATCH_SIZE)
            .await
            .unwrap();
        assert_eq!(restored_proofs, BATCH_SIZE as usize);
    }

    #[tokio::test]
    async fn restore_batch_all_pending() {
        let seed = [0u8; 32];
        let xpriv = btc32::Xpriv::new_master(bitcoin::Network::Regtest, &seed).unwrap();
        let (_, mintkeyset) = keys_test::generate_random_keyset();
        let keyset = KeySet::from(mintkeyset.clone());
        let mut client = MockMintConnector::new();
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
                        keys_utils::sign_with_keys(&cloned_mintkeyset, &bblind)
                            .expect("signatures should be generated")
                    })
                    .collect();
                Ok(RestoreResponse {
                    outputs: request.outputs,
                    signatures,
                    promises: Default::default(),
                })
            });
        client
            .expect_get_mint_keyset()
            .times(1)
            .returning(move |_| Ok(KeySet::from(mintkeyset.clone())));
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
                let response = cdk07::CheckStateResponse { states };
                Ok(response)
            });
        db.expect_store_pendingspent()
            .times(BATCH_SIZE as usize)
            .returning(|p| Ok(p.y().expect("proof should have y")));

        let restored_proofs = super::restore_batch(xpriv, keyset.id, &client, &db, 0, BATCH_SIZE)
            .await
            .unwrap();
        assert_eq!(restored_proofs, BATCH_SIZE as usize);
    }

    #[tokio::test]
    async fn restore_keysetid_1stbatch() {
        let seed = [0u8; 32];
        let xpriv = btc32::Xpriv::new_master(bitcoin::Network::Regtest, &seed).unwrap();
        let (_, mintkeyset) = keys_test::generate_random_keyset();
        let keyset = KeySet::from(mintkeyset.clone());
        let mut client = MockMintConnector::new();
        client
            .expect_get_mint_keyset()
            .times(1)
            .returning(move |_| Ok(keyset.clone()));
        let mut db = MockPocketRepository::new();
        db.expect_counter()
            .times(1)
            .with(eq(mintkeyset.id))
            .returning(move |_| Ok(0));
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
                        keys_utils::sign_with_keys(&cloned_mintkeyset, &bblind)
                            .expect("signatures should be generated")
                    })
                    .collect();
                let response = cdk09::RestoreResponse {
                    outputs,
                    signatures,
                    promises: Option::default(),
                };
                Ok(response)
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
                let response = cdk07::CheckStateResponse { states };
                Ok(response)
            });
        db.expect_store_new()
            .times(BATCH_SIZE as usize)
            .returning(|p| Ok(p.y().unwrap()));
        db.expect_increment_counter()
            .times(1)
            .with(eq(mintkeyset.id), eq(0), eq(BATCH_SIZE))
            .returning(|_, _, _| Ok(()));
        client
            .expect_post_restore()
            .times(EMPTY_RESPONSES_BEFORE_ABORT)
            .returning(move |_| {
                let response = cdk09::RestoreResponse {
                    outputs: Vec::default(),
                    signatures: Vec::default(),
                    promises: Option::default(),
                };
                Ok(response)
            });
        let total_restored = restore_keysetid(xpriv, mintkeyset.id, &client, &db)
            .await
            .unwrap();
        assert_eq!(total_restored, BATCH_SIZE as usize);
    }

    #[tokio::test]
    async fn restore_keysetid_2ndbatch() {
        let seed = [0u8; 32];
        let xpriv = btc32::Xpriv::new_master(bitcoin::Network::Regtest, &seed).unwrap();
        let (_, mintkeyset) = keys_test::generate_random_keyset();
        let keyset = KeySet::from(mintkeyset.clone());
        let mut client = MockMintConnector::new();
        client
            .expect_get_mint_keyset()
            .times(1)
            .returning(move |_| Ok(keyset.clone()));
        let mut db = MockPocketRepository::new();
        db.expect_counter()
            .times(1)
            .with(eq(mintkeyset.id))
            .returning(move |_| Ok(0));
        client.expect_post_restore().times(1).returning(move |_| {
            let response = cdk09::RestoreResponse {
                outputs: Vec::default(),
                signatures: Vec::default(),
                promises: Option::default(),
            };
            Ok(response)
        });
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
                        keys_utils::sign_with_keys(&cloned_mintkeyset, &bblind)
                            .expect("signatures should be generated")
                    })
                    .collect();
                let response = cdk09::RestoreResponse {
                    outputs,
                    signatures,
                    promises: Option::default(),
                };
                Ok(response)
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
                let response = cdk07::CheckStateResponse { states };
                Ok(response)
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
            .returning(move |_| {
                let response = cdk09::RestoreResponse {
                    outputs: Vec::default(),
                    signatures: Vec::default(),
                    promises: Option::default(),
                };
                Ok(response)
            });
        let total_restored = restore_keysetid(xpriv, mintkeyset.id, &client, &db)
            .await
            .unwrap();
        assert_eq!(total_restored, BATCH_SIZE as usize);
    }

    #[tokio::test]
    async fn restore_keysetid_2ndpartial() {
        let seed = [0u8; 32];
        let xpriv = btc32::Xpriv::new_master(bitcoin::Network::Regtest, &seed).unwrap();
        let (_, mintkeyset) = keys_test::generate_random_keyset();
        let keyset = KeySet::from(mintkeyset.clone());
        let mut client = MockMintConnector::new();
        client
            .expect_get_mint_keyset()
            .times(1)
            .returning(move |_| Ok(keyset.clone()));
        let mut db = MockPocketRepository::new();
        db.expect_counter()
            .times(1)
            .with(eq(mintkeyset.id))
            .returning(move |_| Ok(0));
        client.expect_post_restore().times(1).returning(move |_| {
            let response = cdk09::RestoreResponse {
                outputs: Vec::default(),
                signatures: Vec::default(),
                promises: Option::default(),
            };
            Ok(response)
        });
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
                        keys_utils::sign_with_keys(&cloned_mintkeyset, &bblind)
                            .expect("signatures should be generated")
                    })
                    .collect();
                let response = cdk09::RestoreResponse {
                    outputs,
                    signatures,
                    promises: Option::default(),
                };
                Ok(response)
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
                let response = cdk07::CheckStateResponse { states };
                Ok(response)
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
            .returning(move |_| {
                let response = cdk09::RestoreResponse {
                    outputs: Vec::default(),
                    signatures: Vec::default(),
                    promises: Option::default(),
                };
                Ok(response)
            });
        //
        let total_restored = restore_keysetid(xpriv, mintkeyset.id, &client, &db)
            .await
            .unwrap();
        assert_eq!(total_restored, (BATCH_SIZE / 3) as usize);
    }
}
