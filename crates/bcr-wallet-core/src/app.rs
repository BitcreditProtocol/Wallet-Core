// ----- standard library imports

use std::cell::RefCell;
use std::rc::Rc;
use std::str::FromStr;
use std::sync::OnceLock;
use std::sync::RwLock;
// ----- extra library imports
use crate::db;
use crate::db::rexie::RexieWalletDatabase;
use async_trait::async_trait;
use cashu::MintUrl;
// ----- local modules
use crate::db::{MemoryDatabase, WalletDatabase};
use crate::wallet::CreditWallet;
use crate::wallet::{Wallet, new_credit};
// ----- end imports

type TestWallet = Wallet<CreditWallet, RexieWalletDatabase>;
pub struct AppState {
    info: WalletInfo,
    wallets: Vec<TestWallet>,
    db_manager: db::Manager,
}
impl AppState {
    pub fn get_wallet(&self) -> TestWallet {
        self.db_manager.get_wallet("wallet_0".into())
    }
}

#[derive(Debug, Clone)]
pub struct WalletInfo {
    pub name: String,
}

thread_local! {
    static APP_STATE: RefCell<Option<&'static AppState>> = const { RefCell::new(None) } ;
}

pub async fn initialize() {
    let manager = db::Manager::new().await.unwrap();

    let mut wallets = Vec::new();
    for i in 0..10 {
        let rexie_wallet = RexieWalletDatabase::new(format!("wallet_{}", i), manager.get_db());
        let mint_url = MintUrl::from_str("http://127.0.0.1:4343").unwrap();
        let wallet = new_credit()
            .set_mint_url(mint_url)
            .set_database(rexie_wallet)
            .set_seed([0; 32])
            .build();
        wallets.push(wallet);
    }

    let state = AppState {
        db_manager: manager,
        wallets: wallets,
        info: WalletInfo {
            name: "BitCredu".into(),
        },
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
    state.wallets[idx].import_token_v3(token).await;
}

pub async fn get_proofs(idx: usize) -> Vec<cashu::Proof> {
    let state = get_state();
    state.wallets[idx]
        .db
        .get_proofs()
        .await
        .unwrap_or(Vec::new())
}

pub async fn get_balance(idx: usize) -> u64 {
    let state = get_state();
    state.wallets[idx].get_balance().await
}

pub fn get_wallet_info() -> WalletInfo {
    let state = get_state();
    state.info.clone()
}

pub async fn send_proofs_for(amount: u64, idx: usize) -> String {
    let state = get_state();
    state.wallets[idx].split(amount).await;
    state.wallets[idx]
        .send_proofs_for(amount)
        .await
        .unwrap_or("".into())
}
