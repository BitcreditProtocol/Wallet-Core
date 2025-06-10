#![allow(dead_code)]

use bcr_wallet_core::db;
use wasm_bindgen_test::*;

wasm_bindgen_test_configure!(run_in_browser);

#[wasm_bindgen_test]
async fn create_db() {
    let manager = db::rexie::Manager::new("test0000191a_").await.unwrap();
    manager.clear().await.unwrap();
}
