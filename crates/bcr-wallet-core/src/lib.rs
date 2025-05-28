#![feature(type_changing_struct_update)]

mod app;
pub mod db;
mod error;
pub mod wallet;

use tracing::info;
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
pub async fn initialize_api() {
    tracing_wasm::set_as_global_default();
    info!("Tracing setup");
}

#[wasm_bindgen]
pub async fn import_token(token: String) {
    app::import_token_v3(token).await;
}

#[wasm_bindgen]
pub async fn print_proofs() -> String {
    let proofs = app::get_proofs().await;
    let mut result = String::new();
    for proof in &proofs {
        let proof_str = format!(
            "amount={} C={} kid={}",
            proof.amount,
            proof.c.to_string(),
            proof.keyset_id.to_string()
        );
        result.push_str(&proof_str);
        result.push('\n');
    }
    result
}

#[wasm_bindgen]
pub async fn get_balance() -> u64 {
    app::get_balance().await
}

#[wasm_bindgen]
pub async fn send(amount: u64) -> String {
    app::send_proofs_for(amount).await
}

#[wasm_bindgen]
pub fn get_wallet_name() -> String {
    app::get_wallet_info().name
}
