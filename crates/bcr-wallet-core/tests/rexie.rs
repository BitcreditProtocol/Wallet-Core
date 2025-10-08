#![cfg(target_arch = "wasm32")]
// ----- standard library imports
use std::{collections::HashMap, matches, rc::Rc, str::FromStr};
// ----- extra library imports
use bcr_common::core_tests;
use cashu::{Amount, CurrencyUnit, MintUrl, nut07 as cdk07};
use cdk::wallet::types::{Transaction, TransactionDirection};
use rexie::Rexie;
use wasm_bindgen_test::{wasm_bindgen_test, wasm_bindgen_test_configure};
// ----- local imports
use bcr_wallet_core::{
    config::Settings,
    error::Error,
    persistence::rexie::{MintMeltDB, PocketDB, SettingsDB, TransactionDB},
    pocket::{PocketRepository, debit::MintMeltRepository},
    wallet::TransactionRepository,
};

// ----- end imports

wasm_bindgen_test_configure!(run_in_browser);

pub const RANDOMS: [&str; 6] = [
    "0244e4420934530b2bdf5161f4c88b3c4f923db158741da51f3bb22b579495862e",
    "03244bce3f2ea7b12acd2004a6c629acf9d01e7eceadfd7f4ce6f7a09134a84474",
    "0212612cddd9e1aa368c500654538c71ebdf70d5bc4a1b642f9c963269505514cc",
    "0292abc8e9eb2935f0ae6fcf7c491ea124a5860ed954e339a0b7f549cd8c190500",
    "02cc8e0448596f0aaec2c62ef02e5a36f53a4e8b7d5a9e906d2c1f8d5cd738ccae",
    "027a238c992c4a5ea59502b2d6b52e6466bf2a775191cbfaf29b9311e8352d99dc",
];

pub fn publics() -> Vec<cashu::PublicKey> {
    RANDOMS
        .iter()
        .map(|key| cashu::PublicKey::from_hex(key).unwrap())
        .collect()
}

async fn create_pocket_db(test_name: &str) -> PocketDB {
    let unit = CurrencyUnit::Custom(String::from("test"));
    let obj_stores = PocketDB::object_stores(&unit);
    let mut builder = Rexie::builder(test_name).version(1);
    for store in obj_stores {
        builder = builder.add_object_store(store);
    }
    let rexie = Rc::new(builder.build().await.unwrap());
    let db = PocketDB::new(rexie, &unit).unwrap();
    db
}

#[wasm_bindgen_test]
async fn pocket_store_new() {
    let proofdb = create_pocket_db("pocket_store_new").await;

    let (_, keyset) = core_tests::generate_random_ecash_keyset();
    let proof = core_tests::generate_random_ecash_proofs(&keyset, &[Amount::from(8u64)])[0].clone();
    proofdb.store_new(proof).await.unwrap();
}

#[wasm_bindgen_test]
async fn pocket_load_proof() {
    let proofdb = create_pocket_db("pocket_load_proof").await;

    let (_, keyset) = core_tests::generate_random_ecash_keyset();
    let proof = core_tests::generate_random_ecash_proofs(&keyset, &[Amount::from(8u64)])[0].clone();
    let dbkey = proofdb.store_new(proof.clone()).await.unwrap();

    let (loaded_proof, state) = proofdb.load_proof(dbkey).await.unwrap();
    assert_eq!(loaded_proof.c, proof.c);
    assert_eq!(loaded_proof.secret, proof.secret);
    assert!(matches!(state, cdk07::State::Unspent));
}

#[wasm_bindgen_test]
async fn pocket_list_unspent() {
    let proofdb = create_pocket_db("pocket_list_unspent").await;

    let (_, keyset) = core_tests::generate_random_ecash_keyset();
    let new = core_tests::generate_random_ecash_proofs(&keyset, &[Amount::from(8u64)])[0].clone();
    proofdb.store_new(new.clone()).await.unwrap();

    let pending =
        core_tests::generate_random_ecash_proofs(&keyset, &[Amount::from(16u64)])[0].clone();
    proofdb.store_pendingspent(pending).await.unwrap();

    let proofs_map = proofdb.list_unspent().await.unwrap();
    assert_eq!(proofs_map.len(), 1);
    let test_proof = proofs_map.values().collect::<Vec<_>>()[0];
    assert_eq!(new.c, test_proof.c);
}

#[wasm_bindgen_test]
async fn pocket_list_all() {
    let proofdb = create_pocket_db("pocket_list_all").await;

    let (_, keyset) = core_tests::generate_random_ecash_keyset();
    let new = core_tests::generate_random_ecash_proofs(&keyset, &[Amount::from(8u64)])[0].clone();
    let new_y = proofdb.store_new(new.clone()).await.unwrap();
    let pending =
        core_tests::generate_random_ecash_proofs(&keyset, &[Amount::from(16u64)])[0].clone();
    let pending_y = proofdb.store_pendingspent(pending).await.unwrap();

    let ys = proofdb.list_all().await.unwrap();
    assert_eq!(ys.len(), 2);
    assert!(ys.contains(&new_y));
    assert!(ys.contains(&pending_y));
}

#[wasm_bindgen_test]
async fn pocket_mark_pending() {
    let proofdb = create_pocket_db("pocket_mark_pending").await;

    let (_, keyset) = core_tests::generate_random_ecash_keyset();
    let proof = core_tests::generate_random_ecash_proofs(&keyset, &[Amount::from(8u64)])[0].clone();
    let y =
        cashu::dhke::hash_to_curve(proof.secret.as_bytes()).expect("Hash to curve should not fail");
    proofdb.store_new(proof.clone()).await.unwrap();

    let new_proof = proofdb.mark_as_pendingspent(y).await.unwrap();
    assert_eq!(proof.c, new_proof.c);
}

#[wasm_bindgen_test]
async fn pocket_mark_pending_twice_is_error() {
    let proofdb = create_pocket_db("pocket_mark_pending").await;

    let (_, keyset) = core_tests::generate_random_ecash_keyset();
    let proof = core_tests::generate_random_ecash_proofs(&keyset, &[Amount::from(8u64)])[0].clone();
    let y =
        cashu::dhke::hash_to_curve(proof.secret.as_bytes()).expect("Hash to curve should not fail");
    proofdb.store_new(proof.clone()).await.unwrap();

    let new_proof = proofdb.mark_as_pendingspent(y).await.unwrap();
    assert_eq!(proof.c, new_proof.c);

    let response = proofdb.mark_as_pendingspent(y).await;
    assert!(matches!(response, Err(Error::InvalidProofState(_))));
}

#[wasm_bindgen_test]
async fn pocket_new_counter() {
    let proofdb = create_pocket_db("pocket_new_counter").await;

    let kid = core_tests::generate_random_ecash_keyset().0.id;
    let counter = proofdb.counter(kid).await.unwrap();
    assert_eq!(counter, 0);
}

#[wasm_bindgen_test]
async fn pocket_increment_counter() {
    let proofdb = create_pocket_db("pocket_increment_counter").await;

    let kid = core_tests::generate_random_ecash_keyset().0.id;
    let counter = proofdb.counter(kid).await.unwrap();
    assert_eq!(counter, 0);

    proofdb.increment_counter(kid, 0, 10).await.unwrap();
    let counter = proofdb.counter(kid).await.unwrap();
    assert_eq!(counter, 10);
}

#[wasm_bindgen_test]
async fn pocket_increment_nonexisting_counter() {
    let proofdb = create_pocket_db("pocket_increment_nonexisting_counter").await;

    let kid = core_tests::generate_random_ecash_keyset().0.id;
    let result = proofdb.increment_counter(kid, 0, 10).await;
    assert!(result.is_err());
}

async fn create_transaction_db(test_name: &str) -> TransactionDB {
    let id = "test";
    let obj_stores = TransactionDB::object_stores(id);
    let mut builder = Rexie::builder(test_name).version(1);
    for store in obj_stores {
        builder = builder.add_object_store(store);
    }
    let rexie = Rc::new(builder.build().await.unwrap());
    let db = TransactionDB::new(rexie, id).unwrap();
    db
}

#[wasm_bindgen_test]
async fn transaction_store_tx() {
    let transactiondb = create_transaction_db("transaction_store_tx").await;

    let ys = publics()[0..3].to_vec();
    let tx = Transaction {
        mint_url: MintUrl::from_str("https://test.com/mint").expect("Valid mint URL"),
        direction: TransactionDirection::Incoming,
        amount: Amount::from(100u64),
        fee: Amount::from(1u64),
        unit: CurrencyUnit::Custom(String::from("test")),
        ys,
        timestamp: 42,
        memo: None,
        metadata: HashMap::new(),
        quote_id: None,
    };
    transactiondb.store_tx(tx).await.unwrap();
}

#[wasm_bindgen_test]
async fn transaction_load_tx() {
    let transactiondb = create_transaction_db("transaction_load_tx").await;

    let ys = publics()[0..3].to_vec();
    let tx = Transaction {
        mint_url: MintUrl::from_str("https://test.com/mint").expect("Valid mint URL"),
        direction: TransactionDirection::Incoming,
        amount: Amount::from(100u64),
        fee: Amount::from(1u64),
        // keep an eye on https://github.com/cashubtc/cdk/issues/908
        unit: CurrencyUnit::Sat,
        ys,
        timestamp: 42,
        memo: None,
        metadata: HashMap::new(),
        quote_id: None,
    };
    let txid = transactiondb.store_tx(tx.clone()).await.unwrap();

    let loaded_tx = transactiondb.load_tx(txid).await.unwrap();
    assert_eq!(
        loaded_tx.mint_url,
        MintUrl::from_str("https://test.com/mint").unwrap()
    );
    assert_eq!(loaded_tx.direction, TransactionDirection::Incoming);
    assert_eq!(loaded_tx.amount, tx.amount);
    assert_eq!(loaded_tx.fee, tx.fee);
    assert_eq!(loaded_tx.unit, tx.unit);
    assert_eq!(loaded_tx.ys, tx.ys);
}

#[wasm_bindgen_test]
async fn transaction_load_tx_nonexisting() {
    let transactiondb = create_transaction_db("transaction_load_tx_nonexisting").await;

    let ys = publics()[0..3].to_vec();
    let tx = Transaction {
        mint_url: MintUrl::from_str("https://test.com/mint").expect("Valid mint URL"),
        direction: TransactionDirection::Incoming,
        amount: Amount::from(100u64),
        fee: Amount::from(1u64),
        // keep an eye on https://github.com/cashubtc/cdk/issues/908
        unit: CurrencyUnit::Sat,
        ys,
        timestamp: 42,
        memo: None,
        metadata: HashMap::new(),
        quote_id: None,
    };
    let txid = tx.id();

    let loaded_tx = transactiondb.load_tx(txid).await;
    assert!(matches!(loaded_tx, Err(Error::TransactionNotFound(..))));
}

#[wasm_bindgen_test]
async fn transaction_delete_tx() {
    let transactiondb = create_transaction_db("transaction_load_tx").await;

    let ys = publics()[0..3].to_vec();
    let tx = Transaction {
        mint_url: MintUrl::from_str("https://test.com/mint").expect("Valid mint URL"),
        direction: TransactionDirection::Incoming,
        amount: Amount::from(100u64),
        fee: Amount::from(1u64),
        // keep an eye on https://github.com/cashubtc/cdk/issues/908
        unit: CurrencyUnit::Sat,
        ys,
        timestamp: 42,
        memo: None,
        metadata: HashMap::new(),
        quote_id: None,
    };
    let txid = transactiondb.store_tx(tx.clone()).await.unwrap();
    transactiondb.delete_tx(txid).await.unwrap();
    let res = transactiondb.load_tx(txid).await;
    assert!(matches!(res, Err(Error::TransactionNotFound(..))));
}

#[wasm_bindgen_test]
async fn transaction_list_tx_idxs() {
    let transactiondb = create_transaction_db("transaction_list_tx_idxs").await;
    let ys = publics()[0..2].to_vec();
    let tx_new = Transaction {
        mint_url: MintUrl::from_str("https://test.com/mint").expect("Valid mint URL"),
        direction: TransactionDirection::Incoming,
        amount: Amount::from(100u64),
        fee: Amount::from(1u64),
        // keep an eye on https://github.com/cashubtc/cdk/issues/908
        unit: CurrencyUnit::Sat,
        ys,
        timestamp: 84,
        memo: None,
        metadata: HashMap::new(),
        quote_id: None,
    };
    let txid_new = transactiondb.store_tx(tx_new).await.unwrap();
    let ys = publics()[1..3].to_vec();
    let tx_old = Transaction {
        mint_url: MintUrl::from_str("https://test.com/mint").expect("Valid mint URL"),
        direction: TransactionDirection::Incoming,
        amount: Amount::from(100u64),
        fee: Amount::from(1u64),
        // keep an eye on https://github.com/cashubtc/cdk/issues/908
        unit: CurrencyUnit::Sat,
        ys,
        timestamp: 42,
        memo: None,
        metadata: HashMap::new(),
        quote_id: None,
    };
    let txid_old = transactiondb.store_tx(tx_old).await.unwrap();
    assert!(txid_new < txid_old); // otherwise test does not make sense
    let txs = transactiondb.list_tx_ids().await.unwrap();
    assert_eq!(txs.len(), 2);
    assert_eq!(txs[0], txid_old);
    assert_eq!(txs[1], txid_new);
}

#[wasm_bindgen_test]
async fn transaction_update_metadata() {
    let transactiondb = create_transaction_db("transaction_update_metadata").await;
    let ys = publics()[0..2].to_vec();
    let tx = Transaction {
        mint_url: MintUrl::from_str("https://test.com/mint").expect("Valid mint URL"),
        direction: TransactionDirection::Incoming,
        amount: Amount::from(100u64),
        fee: Amount::from(1u64),
        unit: CurrencyUnit::Sat,
        ys,
        timestamp: 84,
        memo: None,
        metadata: HashMap::new(),
        quote_id: None,
    };
    let txid = transactiondb.store_tx(tx).await.unwrap();
    let oldv = transactiondb
        .update_metadata(txid, String::from("key"), String::from("value1"))
        .await
        .unwrap();
    assert!(oldv.is_none());
    let oldv = transactiondb
        .update_metadata(txid, String::from("key"), String::from("value2"))
        .await
        .unwrap();
    assert_eq!(oldv, Some(String::from("value1")));
}

async fn create_mintmelt_db(test_name: &str) -> MintMeltDB {
    let unit = CurrencyUnit::Custom(String::from("test"));
    let obj_stores = MintMeltDB::object_stores(&unit);
    let mut builder = Rexie::builder(test_name).version(1);
    for store in obj_stores {
        builder = builder.add_object_store(store);
    }
    let rexie = Rc::new(builder.build().await.unwrap());
    let db = MintMeltDB::new(rexie, &unit).unwrap();
    db
}

#[wasm_bindgen_test]
async fn mintmelt_store_melt() {
    let qid = String::from("quoteID");
    let kid = core_tests::generate_random_ecash_keyset().0.id;
    let premints =
        cashu::PreMintSecrets::random(kid, Amount::from(16u64), &cashu::amount::SplitTarget::None)
            .unwrap();
    let mintmeltdb = create_mintmelt_db("mintmelt_store_melt").await;
    let id = mintmeltdb
        .store_melt(qid.clone(), Some(premints.clone()))
        .await
        .unwrap();
    assert_eq!(id, qid);
    let expected = mintmeltdb.load_melt(qid).await.unwrap();
    assert_eq!(premints, expected);
}

#[wasm_bindgen_test]
async fn mintmelt_store_melt_no_premint() {
    let qid = String::from("quoteID");
    let mintmeltdb = create_mintmelt_db("mintmelt_store_melt_no_premint").await;
    let id = mintmeltdb.store_melt(qid.clone(), None).await.unwrap();
    assert_eq!(id, qid);
    let result = mintmeltdb.load_melt(qid).await;
    assert!(result.is_err());
}

#[wasm_bindgen_test]
async fn mintmelt_list_ids() {
    let mintmeltdb = create_mintmelt_db("mintmelt_list_ids").await;
    mintmeltdb
        .store_melt(String::from("id1"), None)
        .await
        .unwrap();
    let kid = core_tests::generate_random_ecash_keyset().0.id;
    let premints =
        cashu::PreMintSecrets::random(kid, Amount::from(16u64), &cashu::amount::SplitTarget::None)
            .unwrap();
    mintmeltdb
        .store_melt(String::from("id2"), Some(premints))
        .await
        .unwrap();

    let ids = mintmeltdb.list_melts().await.unwrap();
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&String::from("id1")));
    assert!(ids.contains(&String::from("id2")));
}

async fn create_settings_db(test_name: &str) -> SettingsDB {
    let obj_stores = SettingsDB::object_stores();
    let mut builder = Rexie::builder(test_name).version(1);
    for store in obj_stores {
        builder = builder.add_object_store(store);
    }
    let rexie = Rc::new(builder.build().await.unwrap());
    let db = SettingsDB::new(rexie).unwrap();
    db
}

#[wasm_bindgen_test]
async fn settings_load_default() {
    let settingsdb = create_settings_db("settings_load_default").await;
    settingsdb.load().await.unwrap();
}

#[wasm_bindgen_test]
async fn settings_store() {
    let settingsdb = create_settings_db("settings_store").await;
    let settings = Settings {
        network: bitcoin::Network::Signet,
        ..Default::default()
    };
    settingsdb.store(settings).await.unwrap();
    let cfg = settingsdb.load().await.unwrap();
    assert_eq!(cfg.network, bitcoin::Network::Signet);
}
