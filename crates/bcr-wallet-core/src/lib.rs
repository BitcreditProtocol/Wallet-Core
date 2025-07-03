// ----- standard library imports
// ----- extra library imports
use tracing::info;
use wasm_bindgen::prelude::*;
// ----- local modules
mod app;
mod error;
pub mod persistence;
pub mod pocket;
mod wallet;

// ----- end imports

// Experimental, Many things will change here, a wasm export of using the app, mostly for testing

#[wasm_bindgen]
pub async fn initialize_api(network: String) {
    tracing_wasm::set_as_global_default();
    info!("Tracing setup");
    let net = match network.as_str() {
        "main" => bitcoin::NetworkKind::Main,
        "test" => bitcoin::NetworkKind::Test,
        _ => panic!("Unknown network: {network}"),
    };
    app::initialize_api(net);
}

#[wasm_bindgen]
pub async fn add_wallet(mint_url: String, mnemonic: String, name: String) {
    let returned = app::add_wallet(mint_url, mnemonic, name).await;
    if let Err(e) = returned {
        tracing::error!("Failed to add wallet: {e}");
    }
}

#[wasm_bindgen]
pub fn get_wallet_name(idx: usize) -> String {
    let returned = app::wallet_name(idx);
    match returned {
        Ok(name) => name,
        Err(e) => {
            tracing::error!("mint_url({idx}): {e}");
            String::new()
        }
    }
}

#[wasm_bindgen]
pub fn get_mint_url(idx: usize) -> String {
    let returned = app::wallet_mint_url(idx);
    match returned {
        Ok(url) => url,
        Err(e) => {
            tracing::error!("mint_url({idx}): {e}");
            String::new()
        }
    }
}

#[wasm_bindgen]
pub async fn credit_balance(idx: usize) -> u64 {
    let returned = app::wallet_balance(idx).await;
    match returned {
        Ok(balance) => balance.credit.into(),
        Err(e) => {
            tracing::error!("credit_balance({idx}): {e}");
            0
        }
    }
}

#[wasm_bindgen]
pub async fn debit_balance(idx: usize) -> u64 {
    let returned = app::wallet_balance(idx).await;
    match returned {
        Ok(balance) => balance.debit.into(),
        Err(e) => {
            tracing::error!("debit_balance({idx}): {e}");
            0
        }
    }
}

#[wasm_bindgen]
pub async fn receive(token: String, idx: usize) -> u64 {
    let returned = app::wallet_receive(token, idx).await;
    match returned {
        Ok(amount) => amount.into(),
        Err(e) => {
            tracing::error!("receive(<token>, {idx}): {e}");
            0
        }
    }
}

#[wasm_bindgen]
pub fn get_wallets_ids() -> Vec<u64> {
    let returned = app::wallets_ids();
    match returned {
        Ok(indexes) => indexes,
        Err(e) => {
            tracing::error!("get_wallets_ids: {e}");
            Vec::new()
        }
    }
}

#[wasm_bindgen]
pub fn get_wallets_names() -> Vec<String> {
    let returned = app::wallets_names();
    match returned {
        Ok(names) => names,
        Err(e) => {
            tracing::error!("get_wallets_names: {e}");
            Vec::new()
        }
    }
}
