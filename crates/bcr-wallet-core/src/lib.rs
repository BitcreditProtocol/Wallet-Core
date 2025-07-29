// ----- standard library imports
// ----- extra library imports
use tracing::info;
use wasm_bindgen::prelude::*;
// ----- local modules
mod app;
pub mod error;
pub mod persistence;
pub mod pocket;
mod purse;
mod types;
mod utils;
pub mod wallet;

// ----- end imports

// --------------------------------------------------------------- initialize_api
#[wasm_bindgen]
pub async fn initialize_api(network: String) {
    tracing_wasm::set_as_global_default();
    info!("Tracing setup");
    app::initialize_api(network);
}

// --------------------------------------------------------------- add_wallet
#[wasm_bindgen]
pub async fn add_wallet(mint_url: String, mnemonic: String, name: String) -> u32 {
    let returned = app::add_wallet(mint_url.clone(), mnemonic.clone(), name.clone()).await;
    match returned {
        Ok(idx) => idx as u32,
        Err(e) => {
            tracing::error!("add_wallet({mint_url}, {mnemonic}, {name}): {e}");
            0
        }
    }
}

// --------------------------------------------------------------- get_wallet_name
#[wasm_bindgen]
pub fn get_wallet_name(idx: usize) -> String {
    let returned = app::wallet_name(idx);
    match returned {
        Ok(name) => name,
        Err(e) => {
            tracing::error!("mint_url({idx}): {e}");
            String::default()
        }
    }
}

// --------------------------------------------------------------- get_wallet_mint_url
#[wasm_bindgen]
pub fn get_wallet_mint_url(idx: usize) -> String {
    let returned = app::wallet_mint_url(idx);
    match returned {
        Ok(url) => url,
        Err(e) => {
            tracing::error!("mint_url({idx}): {e}");
            String::default()
        }
    }
}

// --------------------------------------------------------------- get_wallet_units
#[wasm_bindgen]
#[derive(Default)]
pub struct WalletCurrencyUnit {
    #[wasm_bindgen(getter_with_clone)]
    pub credit: String,
    #[wasm_bindgen(getter_with_clone)]
    pub debit: String,
}
impl std::convert::From<app::WalletCurrencyUnit> for WalletCurrencyUnit {
    fn from(unit: app::WalletCurrencyUnit) -> Self {
        Self {
            credit: unit.credit.to_string(),
            debit: unit.debit.to_string(),
        }
    }
}

#[wasm_bindgen]
pub fn get_wallet_currency_unit(idx: usize) -> WalletCurrencyUnit {
    let returned = app::wallet_currency_unit(idx);
    match returned {
        Ok(units) => WalletCurrencyUnit::from(units),
        Err(e) => {
            tracing::error!("wallet_currency_units({idx}): {e}");
            WalletCurrencyUnit::default()
        }
    }
}

// --------------------------------------------------------------- get_wallet_balance
#[wasm_bindgen]
#[derive(Default)]
pub struct WalletBalance {
    #[wasm_bindgen(readonly)]
    pub credit: u64,
    #[wasm_bindgen(readonly)]
    pub debit: u64,
}
impl std::convert::From<wallet::WalletBalance> for WalletBalance {
    fn from(balance: wallet::WalletBalance) -> Self {
        Self {
            credit: u64::from(balance.credit),
            debit: u64::from(balance.debit),
        }
    }
}

#[wasm_bindgen]
pub async fn get_wallet_balance(idx: usize) -> WalletBalance {
    let returned = app::wallet_balance(idx).await;
    match returned {
        Ok(balance) => WalletBalance::from(balance),
        Err(e) => {
            tracing::error!("get_wallet_balance({idx}): {e}");
            WalletBalance::default()
        }
    }
}

// --------------------------------------------------------------- wallet_receive_token
#[wasm_bindgen]
pub async fn wallet_receive_token(idx: usize, token: String, tstamp: u32) -> String {
    let preview_token = token[0..10].to_string();
    let returned = app::wallet_receive(idx, token, tstamp as u64).await;
    match returned {
        Ok(tx_id) => tx_id.to_string(),
        Err(e) => {
            tracing::error!("wallet_receive_token({idx}, {preview_token}...): {e}");
            String::default()
        }
    }
}

// --------------------------------------------------------------- wallet_reclaim_funds
#[wasm_bindgen]
pub async fn wallet_reclaim_funds(idx: usize) -> WalletBalance {
    let returned = app::wallet_reclaim_funds(idx).await;
    match returned {
        Ok(balance) => WalletBalance::from(balance),
        Err(e) => {
            tracing::error!("wallet_reclaim_funds({idx}): {e}");
            WalletBalance::default()
        }
    }
}

// --------------------------------------------------------------- wallet_prepare_send
#[wasm_bindgen]
#[derive(Default)]
pub struct SendSummary {
    #[wasm_bindgen(getter_with_clone)]
    pub request_id: String,
    #[wasm_bindgen(getter_with_clone)]
    pub unit: String,
    #[wasm_bindgen(readonly)]
    pub send_fees: u64,
    #[wasm_bindgen(readonly)]
    pub swap_fees: u64,
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
            tracing::error!("wallet_prepare_send({idx}, {amount}, {unit}): {e}");
            SendSummary::default()
        }
    }
}

// --------------------------------------------------------------- wallet_send
#[wasm_bindgen]
#[derive(Default)]
pub struct TokenTxId {
    #[wasm_bindgen(getter_with_clone)]
    pub token: String,
    #[wasm_bindgen(getter_with_clone)]
    pub tx_id: String,
}
#[wasm_bindgen]
pub async fn wallet_send(
    idx: usize,
    request_id: String,
    tstamp: u32,
    memo: Option<String>,
) -> TokenTxId {
    let returned = app::wallet_send(idx, request_id.clone(), memo.clone(), tstamp as u64).await;
    match returned {
        Ok((token, tx_id)) => TokenTxId {
            token: token.to_string(),
            tx_id: tx_id.to_string(),
        },
        Err(e) => {
            tracing::error!(
                "wallet_send({idx}, {request_id}, {tstamp}, {:?}): {e}",
                memo
            );
            TokenTxId::default()
        }
    }
}

// --------------------------------------------------------------- wallet_redeem
#[wasm_bindgen]
pub async fn wallet_redeem_credit(idx: usize) -> u64 {
    let returned = app::wallet_redeem_credit(idx).await;
    match returned {
        Ok(amount_redeemed) => u64::from(amount_redeemed),
        Err(e) => {
            tracing::error!("wallet_redeem({idx}): {e}");
            0
        }
    }
}

// --------------------------------------------------------------- wallet_clean_local_db
#[wasm_bindgen]
pub async fn wallet_clean_local_db(idx: usize) -> u32 {
    let returned = app::wallet_clean_local_db(idx).await;
    match returned {
        Ok(proofs_removed) => proofs_removed,
        Err(e) => {
            tracing::error!("wallet_clean_local_db({idx}): {e}");
            0
        }
    }
}

// --------------------------------------------------------------- wallet_load_tx
#[wasm_bindgen]
#[derive(Default)]
pub struct Transaction {
    #[wasm_bindgen(readonly)]
    pub amount: u64,
    #[wasm_bindgen(readonly)]
    pub fees: u64,
    #[wasm_bindgen(getter_with_clone)]
    pub unit: String,
    #[wasm_bindgen(readonly)]
    pub tstamp: u64,
    #[wasm_bindgen(getter_with_clone)]
    pub direction: String,
    #[wasm_bindgen(getter_with_clone)]
    pub memo: String,
}
impl std::convert::From<cdk::wallet::types::Transaction> for Transaction {
    fn from(tx: cdk::wallet::types::Transaction) -> Self {
        Self {
            amount: u64::from(tx.amount),
            fees: u64::from(tx.fee),
            unit: tx.unit.to_string(),
            direction: tx.direction.to_string(),
            tstamp: tx.timestamp,
            memo: tx.memo.unwrap_or_default(),
        }
    }
}

#[wasm_bindgen]
pub async fn wallet_load_tx(idx: usize, tx_id: String) -> Transaction {
    let returned = app::wallet_load_tx(idx, &tx_id).await;
    match returned {
        Ok(tx) => Transaction::from(tx),
        Err(e) => {
            tracing::error!("wallet_load_tx({idx}, {tx_id}): {e}");
            Transaction::default()
        }
    }
}

// --------------------------------------------------------------- wallet_list_tx_ids
#[wasm_bindgen]
pub async fn wallet_list_tx_ids(idx: usize) -> Vec<String> {
    let returned = app::wallet_list_tx_ids(idx).await;
    match returned {
        Ok(tx_ids) => tx_ids.into_iter().map(|id| id.to_string()).collect(),
        Err(e) => {
            tracing::error!("wallet_list_tx_ids({idx}): {e}");
            Vec::default()
        }
    }
}

// --------------------------------------------------------------- get_wallets_ids
#[wasm_bindgen]
pub fn get_wallets_ids() -> Vec<u32> {
    let returned = app::wallets_ids();
    match returned {
        Ok(indexes) => indexes.into_iter().map(|i| i as u32).collect(),
        Err(e) => {
            tracing::error!("get_wallets_ids: {e}");
            Vec::default()
        }
    }
}

// --------------------------------------------------------------- get_wallets_names
#[wasm_bindgen]
pub fn get_wallets_names() -> Vec<String> {
    let returned = app::wallets_names();
    match returned {
        Ok(names) => names,
        Err(e) => {
            tracing::error!("get_wallets_names: {e}");
            Vec::default()
        }
    }
}
