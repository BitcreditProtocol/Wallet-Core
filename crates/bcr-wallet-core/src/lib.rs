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
