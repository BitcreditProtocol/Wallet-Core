// ----- standard library imports
// ----- extra library imports
use tracing::info;
use wasm_bindgen::prelude::*;
// ----- local modules
mod app;
pub mod error;
pub mod persistence;
pub mod pocket;
mod types;
mod utils;
mod wallet;

// ----- end imports

// Experimental, Many things will change here, a wasm export of using the app, mostly for testing

#[wasm_bindgen]
pub async fn initialize_api(network: String) {
    tracing_wasm::set_as_global_default();
    info!("Tracing setup");
    app::initialize_api(network);
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
pub fn get_wallet_mint_url(idx: usize) -> String {
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
pub fn get_wallet_credit_unit(idx: usize) -> String {
    let returned = app::wallet_credit_unit(idx);
    match returned {
        Ok(url) => url,
        Err(e) => {
            tracing::error!("wallet_credit_unit({idx}): {e}");
            String::new()
        }
    }
}

#[wasm_bindgen]
pub fn get_wallet_debit_unit(idx: usize) -> String {
    let returned = app::wallet_debit_unit(idx);
    match returned {
        Ok(url) => url,
        Err(e) => {
            tracing::error!("wallet_debit_unit({idx}): {e}");
            String::new()
        }
    }
}

#[wasm_bindgen]
pub async fn get_wallet_credit_balance(idx: usize) -> u64 {
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
pub async fn get_wallet_debit_balance(idx: usize) -> u64 {
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
pub async fn wallet_receive_token(idx: usize, token: String) -> u64 {
    let returned = app::wallet_receive(idx, token).await;
    match returned {
        Ok(amount) => amount.into(),
        Err(e) => {
            tracing::error!("receive({idx}, <token>): {e}");
            0
        }
    }
}

#[wasm_bindgen]
#[derive(Default)]
pub struct SendSummary {
    pub(crate) request_id: String,
    unit: String,
    pub send_fees: u64,
    pub swap_fees: u64,
}

#[wasm_bindgen]
impl SendSummary {
    #[wasm_bindgen(getter)]
    pub fn request_id(&self) -> String {
        self.request_id.clone()
    }
    #[wasm_bindgen(getter)]
    pub fn unit(&self) -> String {
        self.unit.clone()
    }
}

impl std::convert::From<types::SendSummary> for SendSummary {
    fn from(summary: types::SendSummary) -> Self {
        Self {
            request_id: summary.request_id.to_string(),
            unit: summary.unit.to_string(),
            send_fees: summary.send_fees.into(),
            swap_fees: summary.swap_fees.into(),
        }
    }
}

#[wasm_bindgen]
pub async fn wallet_prepare_send(idx: usize, amount: u64, unit: String) -> SendSummary {
    let returned = app::wallet_prepare_send(idx, amount, unit.clone()).await;
    match returned {
        Ok(summary) => summary,
        Err(e) => {
            tracing::error!("prepare_send({idx}, {amount}, {unit}): {e}");
            SendSummary::default()
        }
    }
}

#[wasm_bindgen]
pub async fn wallet_send(idx: usize, request_id: String, memo: Option<String>) -> String {
    let returned = app::wallet_send(idx, request_id.clone(), memo.clone()).await;
    match returned {
        Ok(token) => token.to_string(),
        Err(e) => {
            tracing::error!("send({idx}, {request_id}, {:?}): {e}", memo);
            String::new()
        }
    }
}

#[wasm_bindgen]
pub fn get_wallets_ids() -> Vec<u32> {
    let returned = app::wallets_ids();
    match returned {
        Ok(indexes) => indexes.into_iter().map(|i| i as u32).collect(),
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
