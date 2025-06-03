// ----- standard library imports

use std::cell::RefCell;
use std::str::FromStr;
// ----- extra library imports
use crate::db;
use crate::db::rexie::RexieWalletDatabase;
use cashu::MintUrl;
// ----- local modules
use crate::db::WalletDatabase;
use crate::wallet::CreditWallet;
use crate::wallet::{Wallet, new_credit};
// ----- end imports

// Experimental, no error handling

type TestWallet = Wallet<CreditWallet, RexieWalletDatabase>;
pub struct AppState {
    info: WalletInfo,
    wallets: Vec<TestWallet>,
    _db_manager: db::rexie::Manager,
}

#[derive(Debug, Clone)]
pub struct WalletInfo {
    pub name: String,
}

thread_local! {
    static APP_STATE: RefCell<Option<&'static AppState>> = const { RefCell::new(None) } ;
}

pub async fn initialize() {
    let manager = db::rexie::Manager::new().await.unwrap();

    let mut wallets = Vec::new();
    for i in 0..10 {
        let rexie_wallet = RexieWalletDatabase::new(format!("wallet_{}", i), manager.get_db());
        let mint_url = MintUrl::from_str("https://wildcat-dev-docker.minibill.tech").unwrap();
        let wallet = new_credit()
            .set_mint_url(mint_url)
            .set_database(rexie_wallet)
            .set_seed([0; 32])
            .build();
        wallets.push(wallet);
    }

    let state = AppState {
        _db_manager: manager,
        info: WalletInfo {
            name: "BitCredit".into(),
        },
        wallets,
    };
    APP_STATE.with(|context| {
        let mut context_ref = context.borrow_mut();
        if context_ref.is_none() {
            let leaked: &'static AppState = Box::leak(Box::new(state)); // leak to get a static ref
            *context_ref = Some(leaked);
        }
    });
}

fn get_state() -> &'static AppState {
    APP_STATE.with(|slot| *slot.borrow()).unwrap()
}

pub async fn import_token_v3(token: String, idx: usize) {
    let state = get_state();
    state.wallets[idx].import_token_v3(token).await.unwrap();
}

pub async fn get_mint_url(idx: usize) -> String {
    let state = get_state();
    state.wallets[idx].mint_url.to_string()
}

pub async fn get_proofs(idx: usize) -> Vec<cashu::Proof> {
    let state = get_state();
    state.wallets[idx]
        .db
        .get_active_proofs()
        .await
        .unwrap_or(Vec::new())
}

pub async fn get_balance(idx: usize) -> u64 {
    let state = get_state();
    state.wallets[idx].get_balance().await.unwrap()
}

pub fn get_wallet_info() -> WalletInfo {
    let state = get_state();
    state.info.clone()
}

pub async fn send_proofs_for(amount: u64, idx: usize) -> String {
    let state = get_state();

    // Ensures we always have the right powers of 2 to send amount
    let _ = state.wallets[idx].split(amount).await;
    state.wallets[idx]
        .send_proofs_for(amount)
        .await
        .unwrap_or("".into())
}
