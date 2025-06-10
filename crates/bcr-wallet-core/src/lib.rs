#![feature(type_changing_struct_update)]
// TODO use async trait
#![allow(async_fn_in_trait)]

mod app;
pub mod db;
pub mod mint;
pub mod wallet;

// ----- standard library imports
// ----- extra library imports
use tracing::info;
use wasm_bindgen::prelude::*;
// ----- local modules
// ----- end imports

// Experimental, Many things will change here, a wasm export of using the app, mostly for testing

#[wasm_bindgen]
pub async fn initialize_api() {
    tracing_wasm::set_as_global_default();
    info!("Tracing setup");

    app::initialize().await;
}

#[wasm_bindgen]
pub async fn import_token(token: String, idx: usize) {
    app::import_token_v3(token, idx).await;
}

#[wasm_bindgen]
pub async fn get_wallet_url(idx: usize) -> String {
    app::get_mint_url(idx).await
}

#[wasm_bindgen]
pub async fn print_proofs(idx: usize) -> String {
    let proofs = app::get_proofs(idx).await;
    let mut result = String::new();
    for proof in &proofs {
        let proof_str = format!(
            "amount={} C={} kid={}",
            proof.amount, proof.c, proof.keyset_id
        );
        result.push_str(&proof_str);
        result.push('\n');
    }
    result
}

#[wasm_bindgen]
pub async fn get_balance(idx: usize) -> u64 {
    app::get_balance(idx).await
}

#[wasm_bindgen]
pub async fn send(amount: u64, idx: usize) -> String {
    app::send_proofs_for(amount, idx).await
}

#[wasm_bindgen]
pub fn get_wallet_name() -> String {
    app::get_wallet_info().name
}

#[wasm_bindgen]
pub async fn recover(idx: usize) {
    app::recover(idx).await
}

#[wasm_bindgen]
pub async fn recheck(idx: usize) {
    app::recheck(idx).await
}

#[wasm_bindgen]
pub async fn get_wallets_names() -> Vec<String> {
    app::get_wallets().await.1
}

#[wasm_bindgen]
pub async fn get_wallets_ids() -> Vec<u64> {
    app::get_wallets()
        .await
        .0
        .iter()
        .map(|x| *x as u64)
        .collect()
}

#[wasm_bindgen]
pub async fn add_wallet(
    name: String,
    mint_url: String,
    mnemonic: String,
    unit: String,
    credit: bool,
) {
    app::add_wallet(name, mint_url, mnemonic, unit, credit)
        .await
        .unwrap();
}

#[wasm_bindgen]
pub async fn list_keysets(idx: usize) -> String {
    let keysets = app::list_keysets(idx).await;
    let mut ret = String::new();
    for keyset in &keysets {
        let keyset_str = format!(
            "kid={} unit={} active={}",
            keyset.id, keyset.unit, keyset.active
        );
        ret.push_str(&keyset_str);
        ret.push('\n');
    }
    ret
}

#[wasm_bindgen]
pub async fn get_unit(idx: usize) -> String {
    app::get_unit(idx).await.to_string()
}

#[wasm_bindgen]
pub async fn redeem_inactive(idx: usize) -> String {
    app::redeem_inactive(idx).await
}
