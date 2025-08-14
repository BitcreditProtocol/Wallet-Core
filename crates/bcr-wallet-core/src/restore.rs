/// ----- standard library imports
use std::collections::HashMap;
// ----- extra library imports
use bitcoin::bip32 as btc32;
use cashu::{KeySet, nut00 as cdk00, nut01 as cdk01, nut07 as cdk07, nut09 as cdk09};
use cdk::wallet::MintConnector;
// ----- local imports
use crate::{error::Result, pocket::PocketRepository};

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
    let keyset = client.get_mint_keyset(kid).await?;
    let mut zero_response_counter = 0;
    let mut total_proofs_restored = 0;
    while zero_response_counter < EMPTY_RESPONSES_BEFORE_ABORT {
        let restored_proofs = restore_batch(xpriv, &keyset, client, db).await?;
        if restored_proofs == 0 {
            zero_response_counter += 1;
        } else {
            zero_response_counter = 0;
        }
        total_proofs_restored += restored_proofs;
    }
    Ok(total_proofs_restored)
}

async fn restore_batch(
    xpriv: btc32::Xpriv,
    keyset: &KeySet,
    client: &dyn MintConnector,
    db: &dyn PocketRepository,
) -> Result<usize> {
    let counter = db.counter(keyset.id).await?;
    let premints =
        cdk00::PreMintSecrets::restore_batch(keyset.id, xpriv, counter, counter + BATCH_SIZE - 1)?;
    let request = cdk09::RestoreRequest {
        outputs: premints.blinded_messages(),
    };
    let cdk09::RestoreResponse {
        outputs,
        signatures,
        ..
    } = client.post_restore(request).await?;
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
    db.increment_counter(keyset.id, counter, BATCH_SIZE).await?;
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
    use cashu::{Amount, RestoreResponse, nut07 as cdk07};
    use mockall::predicate::eq;
    use rand::Rng;

    #[tokio::test]
    async fn restore_batch_empty_response() {
        let seed = [0u8; 32];
        let xpriv = btc32::Xpriv::new_master(bitcoin::Network::Regtest, &seed).unwrap();
        let (_, keyset) = keys_test::generate_random_keyset();
        let keyset = KeySet::from(keyset);
        let mut client = MockMintConnector::new();
        let mut db = MockPocketRepository::new();
        db.expect_counter()
            .with(eq(keyset.id))
            .times(1)
            .returning(|_| Ok(0));
        client.expect_post_restore().times(1).returning(|_| {
            Ok(RestoreResponse {
                outputs: Default::default(),
                signatures: Default::default(),
                promises: Default::default(),
            })
        });
        db.expect_increment_counter()
            .with(eq(keyset.id), eq(0), eq(BATCH_SIZE))
            .times(1)
            .returning(|_, _, _| Ok(()));

        super::restore_batch(xpriv, &keyset, &client, &db)
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
        let mut db = MockPocketRepository::new();
        db.expect_counter()
            .with(eq(keyset.id))
            .times(1)
            .returning(|_| Ok(0));
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
                        keys_utils::sign_with_keys(&mintkeyset, &bblind)
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
        db.expect_increment_counter()
            .with(eq(keyset.id), eq(0), eq(BATCH_SIZE))
            .times(1)
            .returning(|_, _, _| Ok(()));

        super::restore_batch(xpriv, &keyset, &client, &db)
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
        db.expect_counter()
            .with(eq(keyset.id))
            .times(1)
            .returning(|_| Ok(0));
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
                        keys_utils::sign_with_keys(&mintkeyset, &bblind)
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
        db.expect_increment_counter()
            .with(eq(keyset.id), eq(0), eq(BATCH_SIZE))
            .times(1)
            .returning(|_, _, _| Ok(()));
        db.expect_store_new()
            .times(BATCH_SIZE as usize)
            .returning(|p| Ok(p.y().expect("proof should have y")));

        super::restore_batch(xpriv, &keyset, &client, &db)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn restore_batch_all_pending() {
        let seed = [0u8; 32];
        let xpriv = btc32::Xpriv::new_master(bitcoin::Network::Regtest, &seed).unwrap();
        let (_, mintkeyset) = keys_test::generate_random_keyset();
        let keyset = KeySet::from(mintkeyset.clone());
        let mut client = MockMintConnector::new();
        let mut db = MockPocketRepository::new();
        db.expect_counter()
            .with(eq(keyset.id))
            .times(1)
            .returning(|_| Ok(0));
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
                        keys_utils::sign_with_keys(&mintkeyset, &bblind)
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
        db.expect_increment_counter()
            .with(eq(keyset.id), eq(0), eq(BATCH_SIZE))
            .times(1)
            .returning(|_, _, _| Ok(()));
        db.expect_store_pendingspent()
            .times(BATCH_SIZE as usize)
            .returning(|p| Ok(p.y().expect("proof should have y")));

        super::restore_batch(xpriv, &keyset, &client, &db)
            .await
            .unwrap();
    }
}
