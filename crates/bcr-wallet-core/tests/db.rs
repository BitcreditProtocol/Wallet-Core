#![allow(dead_code)]

use bcr_wallet_core::db::rexie::RexieWalletDatabase;
use bcr_wallet_core::db::{self, KeysetDatabase, Metadata, WalletDatabase};
use bcr_wallet_core::wallet::new_credit;
use cashu::Id;
use std::str::FromStr;
use wasm_bindgen_test::*;

wasm_bindgen_test_configure!(run_in_browser);

const TESTDB: &str = "test0000191a_";

const MNEMONIC1: &str = "laptop dawn tissue elbow prosper silent start spend desert dove great opinion pizza raise replace glove color gallery dad clinic job boil gift suspect";

#[wasm_bindgen_test]
async fn add_proof_to_db() {
    let manager = db::rexie::Manager::new(TESTDB).await.unwrap();
    manager.clear().await.unwrap();

    let mnemonic =
        bip39::Mnemonic::parse_in_normalized(bip39::Language::English, MNEMONIC1).unwrap();

    let db = RexieWalletDatabase::new(format!("wallet_0"), manager.get_db());

    let id: Id = "0069de3e5a7fab98".parse().unwrap();
    let secret = "c370ea33d5467960c0c123d1238de2e984ef12a5066a5fb26cdfd44182da34b1"
        .parse()
        .unwrap();
    let c = "0214de611080c9c213f19734c801eb92475b2928099a17ea40c8024583d7908675"
        .parse()
        .unwrap();
    let proof = cashu::Proof::new(64.into(), id, secret, c);
    db.add_proof(proof).await.unwrap();

    let wallet = new_credit()
        .set_unit(cashu::CurrencyUnit::Sat)
        .set_mint_url(cashu::MintUrl::from_str("https://example.com/mint").unwrap())
        .set_database(db)
        .set_seed(mnemonic.to_seed(""))
        .build();

    let balance = wallet.get_balance().await.unwrap();

    assert_eq!(balance, 64);

    let token = wallet.send_proofs_for(balance).await.unwrap();

    assert_eq!(
        token,
        "bitcrBo2FteBhodHRwczovL2V4YW1wbGUuY29tL21pbnRhdWNzYXRhdIGiYWlIAGnePlp_q5hhcIGkYWEYQGFzeEBjMzcwZWEzM2Q1NDY3OTYwYzBjMTIzZDEyMzhkZTJlOTg0ZWYxMmE1MDY2YTVmYjI2Y2RmZDQ0MTgyZGEzNGIxYWNYIQIU3mEQgMnCE_GXNMgB65JHWykoCZoX6kDIAkWD15CGdWFk9g=="
    );
}

#[wasm_bindgen_test]
async fn wallet_count() {
    let manager = db::rexie::Manager::new(TESTDB).await.unwrap();
    manager.clear().await.unwrap();

    let metadata = db::rexie::RexieMetadata::new(manager.get_db());

    let minturl = cashu::MintUrl::from_str("https://example.com/mint").unwrap();
    metadata
        .add_wallet(
            "wallet0".into(),
            minturl.clone(),
            MNEMONIC1.split(" ").map(|x| x.to_string()).collect(),
            "sat".into(),
            true,
        )
        .await
        .unwrap();
    metadata
        .add_wallet(
            "wallet2".into(),
            minturl,
            MNEMONIC1.split(" ").map(|x| x.to_string()).collect(),
            "sat".into(),
            true,
        )
        .await
        .unwrap();

    assert_eq!(metadata.get_wallets().await.unwrap().len(), 2);
}

#[wasm_bindgen_test]
async fn keyset_count() {
    let manager = db::rexie::Manager::new(TESTDB).await.unwrap();
    manager.clear().await.unwrap();

    let db = RexieWalletDatabase::new(format!("wallet_0"), manager.get_db());

    let id: Id = "0069de3e5a7fab98".parse().unwrap();

    db.increase_count(id, 3).await.unwrap();
    db.increase_count(id, 3).await.unwrap();
    db.increase_count(id, 1).await.unwrap();

    let count = db.get_count(id).await.unwrap();

    assert_eq!(count, 7);
}
