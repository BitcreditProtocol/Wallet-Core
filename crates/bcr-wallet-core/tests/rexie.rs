#![cfg(target_arch = "wasm32")]
// ----- standard library imports
use std::rc::Rc;
// ----- extra library imports
use bcr_wdc_utils::{keys::test_utils as keys_test, signatures::test_utils as signatures_test};
use cashu::{Amount, CurrencyUnit};
use rexie::Rexie;
use wasm_bindgen_test::{wasm_bindgen_test, wasm_bindgen_test_configure};
// ----- local imports
use bcr_wallet_core::{error::Error, persistence::rexie::ProofDB, pocket::PocketRepository};

// ----- end imports

wasm_bindgen_test_configure!(run_in_browser);

async fn create_proof_db(test_name: &str) -> ProofDB {
    let unit = CurrencyUnit::Custom(String::from("test"));
    let obj_stores = ProofDB::object_stores(&unit);
    let mut builder = Rexie::builder(test_name).version(1);
    for store in obj_stores {
        builder = builder.add_object_store(store);
    }
    let rexie = Rc::new(builder.build().await.unwrap());
    let proof = ProofDB::new(rexie, unit).unwrap();
    proof
}

#[wasm_bindgen_test]
async fn proof_store_name() {
    let proofdb = create_proof_db("proof_store_name").await;

    let (_, keyset) = keys_test::generate_keyset();
    let proof = signatures_test::generate_proofs(&keyset, &[Amount::from(8u64)])[0].clone();
    proofdb.store_new(proof).await.unwrap();
}

#[wasm_bindgen_test]
async fn proof_list_unspent() {
    let proofdb = create_proof_db("proof_list_unspent").await;

    let (_, keyset) = keys_test::generate_keyset();
    let new = signatures_test::generate_proofs(&keyset, &[Amount::from(8u64)])[0].clone();
    proofdb.store_new(new.clone()).await.unwrap();

    let pending = signatures_test::generate_proofs(&keyset, &[Amount::from(16u64)])[0].clone();
    proofdb.store_pendingspent(pending).await.unwrap();

    let proofs_map = proofdb.list_unspent().await.unwrap();
    assert_eq!(proofs_map.len(), 1);
    let test_proof = proofs_map.values().collect::<Vec<_>>()[0];
    assert_eq!(new.c, test_proof.c);
}

#[wasm_bindgen_test]
async fn proof_list_all() {
    let proofdb = create_proof_db("proof_list_all").await;

    let (_, keyset) = keys_test::generate_keyset();
    let new = signatures_test::generate_proofs(&keyset, &[Amount::from(8u64)])[0].clone();
    let new_y = proofdb.store_new(new.clone()).await.unwrap();
    let pending = signatures_test::generate_proofs(&keyset, &[Amount::from(16u64)])[0].clone();
    let pending_y = proofdb.store_pendingspent(pending).await.unwrap();

    let ys = proofdb.list_all().await.unwrap();
    assert_eq!(ys.len(), 2);
    assert!(ys.contains(&new_y));
    assert!(ys.contains(&pending_y));
}

#[wasm_bindgen_test]
async fn proof_mark_pending() {
    let proofdb = create_proof_db("proof_mark_pending").await;

    let (_, keyset) = keys_test::generate_keyset();
    let proof = signatures_test::generate_proofs(&keyset, &[Amount::from(8u64)])[0].clone();
    let y =
        cashu::dhke::hash_to_curve(proof.secret.as_bytes()).expect("Hash to curve should not fail");
    proofdb.store_new(proof.clone()).await.unwrap();

    let new_proof = proofdb.mark_as_pendingspent(y).await.unwrap();
    assert_eq!(proof.c, new_proof.c);
}

#[wasm_bindgen_test]
async fn proof_mark_pending_twice_is_error() {
    let proofdb = create_proof_db("proof_mark_pending").await;

    let (_, keyset) = keys_test::generate_keyset();
    let proof = signatures_test::generate_proofs(&keyset, &[Amount::from(8u64)])[0].clone();
    let y =
        cashu::dhke::hash_to_curve(proof.secret.as_bytes()).expect("Hash to curve should not fail");
    proofdb.store_new(proof.clone()).await.unwrap();

    let new_proof = proofdb.mark_as_pendingspent(y).await.unwrap();
    assert_eq!(proof.c, new_proof.c);

    let response = proofdb.mark_as_pendingspent(y).await;
    assert!(matches!(response, Err(Error::InvalidProofState(_))));
}

#[wasm_bindgen_test]
async fn new_counter() {
    let proofdb = create_proof_db("new_counter").await;

    let kid = keys_test::generate_random_keysetid();
    let counter = proofdb.counter(kid).await.unwrap();
    assert_eq!(counter, 0);
}

#[wasm_bindgen_test]
async fn increment_counter() {
    let proofdb = create_proof_db("increment_counter").await;

    let kid = keys_test::generate_random_keysetid();
    let counter = proofdb.counter(kid).await.unwrap();
    assert_eq!(counter, 0);

    proofdb.increment_counter(kid, 0, 10).await.unwrap();
    let counter = proofdb.counter(kid).await.unwrap();
    assert_eq!(counter, 10);
}

#[wasm_bindgen_test]
async fn increment_nonexisting_counter() {
    let proofdb = create_proof_db("increment_nonexisting_counter").await;

    let kid = keys_test::generate_random_keysetid();
    let result = proofdb.increment_counter(kid, 0, 10).await;
    assert!(result.is_err());
}
