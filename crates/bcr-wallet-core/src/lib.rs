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
mod restore;
mod types;
mod utils;
pub mod wallet;

// ----- end imports

const TEASER_SIZE: usize = 25;

#[cfg(target_arch = "wasm32")]
mod sync {
    pub trait SendSync {}
    impl<T> SendSync for T where T: ?Sized {}
}
#[cfg(not(target_arch = "wasm32"))]
mod sync {
    pub trait SendSync: Send + Sync {}
    impl<T> SendSync for T where T: Send + Sync {}
}

pub trait MintConnector: cdk::wallet::MintConnector + sync::SendSync {}
impl<T> MintConnector for T where T: cdk::wallet::MintConnector + sync::SendSync {}

// --------------------------------------------------------------- initialize_api
#[wasm_bindgen]
pub async fn initialize_api(network: String) {
    tracing_wasm::set_as_global_default();
    info!("Tracing setup");
    app::initialize_api(network).await;
}

// --------------------------------------------------------------- generate_random_seed
#[wasm_bindgen]
pub async fn generate_random_mnemonic(mnemonic_len: u32) -> String {
    let mnemonic_len = if mnemonic_len == 0 { 12 } else { mnemonic_len };
    info!("Generate random {}-word mnemonic", mnemonic_len);

    const VALID_MNEMONIC_LENGTHS: [u32; 5] = [12, 15, 18, 21, 24];
    assert!(
        VALID_MNEMONIC_LENGTHS.contains(&mnemonic_len),
        "word count must be one of: {VALID_MNEMONIC_LENGTHS:?}"
    );
    let returned = bip39::Mnemonic::generate_in(bip39::Language::English, mnemonic_len as usize);
    match returned {
        Ok(mnemonic) => mnemonic.to_string(),
        Err(e) => {
            tracing::error!("generate_random_mnemonic({mnemonic_len}): {e}");
            String::default()
        }
    }
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

// --------------------------------------------------------------- restore_wallet
#[wasm_bindgen]
pub async fn restore_wallet(mint_url: String, mnemonic: String, name: String) -> u32 {
    let returned = app::restore_wallet(mint_url.clone(), mnemonic.clone(), name.clone()).await;
    match returned {
        Ok(idx) => idx as u32,
        Err(e) => {
            tracing::error!("restore_wallet({mint_url}, {mnemonic}, {name}): {e}");
            0
        }
    }
}

// --------------------------------------------------------------- get_wallet_name
#[wasm_bindgen]
pub fn get_wallet_name(idx: u32) -> String {
    let returned = app::wallet_name(idx as usize);
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
pub fn get_wallet_mint_url(idx: u32) -> String {
    let returned = app::wallet_mint_url(idx as usize);
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
pub fn get_wallet_currency_unit(idx: u32) -> WalletCurrencyUnit {
    let returned = app::wallet_currency_unit(idx as usize);
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
pub async fn get_wallet_balance(idx: u32) -> WalletBalance {
    let returned = app::wallet_balance(idx as usize).await;
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
pub async fn wallet_receive_token(idx: u32, token: String) -> String {
    let teaser = token.chars().take(TEASER_SIZE).collect::<String>();
    let tstamp = chrono::Utc::now().timestamp() as u64;
    let returned = app::wallet_receive(idx as usize, token, tstamp).await;
    match returned {
        Ok(tx_id) => tx_id.to_string(),
        Err(e) => {
            tracing::error!("wallet_receive_token({idx}, {teaser}...): {e}");
            String::default()
        }
    }
}

// --------------------------------------------------------------- wallet_reclaim_funds
#[wasm_bindgen]
pub async fn wallet_reclaim_funds(idx: u32) -> WalletBalance {
    let returned = app::wallet_reclaim_funds(idx as usize).await;
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
pub async fn wallet_prepare_send(idx: u32, amount: u32, unit: String) -> SendSummary {
    let returned = app::wallet_prepare_send(idx as usize, amount as u64, unit.clone()).await;
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
pub async fn wallet_send(idx: u32, request_id: String, memo: Option<String>) -> TokenTxId {
    let tstamp = chrono::Utc::now().timestamp() as u64;
    let returned = app::wallet_send(idx as usize, request_id.clone(), memo.clone(), tstamp).await;
    match returned {
        Ok((token, tx_id)) => TokenTxId {
            token: token.to_string(),
            tx_id: tx_id.to_string(),
        },
        Err(e) => {
            tracing::error!(
                "wallet_send({idx}, {request_id}, {:?}): {e}",
                memo
            );
            TokenTxId::default()
        }
    }
}

// --------------------------------------------------------------- wallet_redeem
#[wasm_bindgen]
pub async fn wallet_redeem_credit(idx: u32) -> u64 {
    let returned = app::wallet_redeem_credit(idx as usize).await;
    match returned {
        Ok(amount_redeemed) => u64::from(amount_redeemed),
        Err(e) => {
            tracing::error!("wallet_redeem({idx}): {e}");
            0
        }
    }
}

// --------------------------------------------------------------- wallet_list_redemptions
#[wasm_bindgen]
pub struct RedemptionSummary {
    #[wasm_bindgen(readonly)]
    pub tstamp: u32,
    #[wasm_bindgen(readonly)]
    pub amount: u64,
}
impl std::convert::From<types::RedemptionSummary> for RedemptionSummary {
    fn from(summary: types::RedemptionSummary) -> Self {
        Self {
            tstamp: summary.tstamp as u32,
            amount: u64::from(summary.amount),
        }
    }
}
#[wasm_bindgen]
pub async fn wallet_list_redemptions(idx: u32, payment_window: u32) -> Vec<RedemptionSummary> {
    let window = std::time::Duration::from_secs(payment_window as u64);
    let returned = app::wallet_list_redemptions(idx as usize, window).await;
    match returned {
        Ok(redemptions) => redemptions
            .into_iter()
            .map(RedemptionSummary::from)
            .collect(),
        Err(e) => {
            tracing::error!("wallet_list_redemptions({idx}, {payment_window}): {e}");
            Vec::default()
        }
    }
}

// --------------------------------------------------------------- wallet_clean_local_db
#[wasm_bindgen]
pub async fn wallet_clean_local_db(idx: u32) -> u32 {
    let returned = app::wallet_clean_local_db(idx as usize).await;
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
pub async fn wallet_load_tx(idx: u32, tx_id: String) -> Transaction {
    let returned = app::wallet_load_tx(idx as usize, &tx_id).await;
    match returned {
        Ok(tx) => Transaction::from(tx),
        Err(e) => {
            tracing::error!("wallet_load_tx({idx}, {tx_id}): {e}");
            Transaction::default()
        }
    }
}
// --------------------------------------------------------------- wallet_prepare_payment
#[wasm_bindgen]
#[derive(Default)]
pub struct PaymentSummary {
    #[wasm_bindgen(getter_with_clone)]
    pub request_id: String,
    #[wasm_bindgen(getter_with_clone)]
    pub unit: String,
    #[wasm_bindgen(readonly)]
    pub fees: u64,
    #[wasm_bindgen(readonly)]
    pub reserved_fees: u64,
    #[wasm_bindgen(readonly)]
    pub expiry: u64,
}
impl std::convert::From<types::PaymentSummary> for PaymentSummary {
    fn from(summary: types::PaymentSummary) -> Self {
        Self {
            request_id: summary.request_id.to_string(),
            unit: summary.unit.to_string(),
            fees: summary.fees.into(),
            reserved_fees: summary.reserved_fees.into(),
            expiry: summary.expiry,
        }
    }
}
#[wasm_bindgen]
pub async fn wallet_prepare_payment(idx: u32, input: String) -> PaymentSummary {
    let teaser = input.chars().take(TEASER_SIZE).collect::<String>();
    let returned = app::wallet_prepare_payment(idx as usize, input).await;
    match returned {
        Ok(summary) => PaymentSummary::from(summary),
        Err(e) => {
            tracing::error!("wallet_prepare_payment({idx}, {teaser}): {e}");
            PaymentSummary::default()
        }
    }
}

// --------------------------------------------------------------- wallet_pay
#[wasm_bindgen]
pub async fn wallet_pay(idx: u32, request_id: String) -> String {
    let tstamp = chrono::Utc::now().timestamp() as u64;
    let returned = app::wallet_pay(idx as usize, request_id.clone(), tstamp).await;
    match returned {
        Ok(tx_id) => tx_id.to_string(),
        Err(e) => {
            tracing::error!("wallet_pay({idx}, {request_id}, {tstamp}): {e}",);
            String::default()
        }
    }
}

// --------------------------------------------------------------- wallet_check_pending_melts
#[wasm_bindgen]
pub async fn wallet_check_pending_melts(idx: u32) -> u64 {
    let returned = app::wallet_check_pending_melts(idx as usize).await;
    match returned {
        Ok(amount) => u64::from(amount),
        Err(e) => {
            tracing::error!("wallet_check_pending_melts({idx}): {e}",);
            0
        }
    }
}

// --------------------------------------------------------------- wallet_list_tx_ids
#[wasm_bindgen]
pub async fn wallet_list_tx_ids(idx: u32) -> Vec<String> {
    let returned = app::wallet_list_tx_ids(idx as usize).await;
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
