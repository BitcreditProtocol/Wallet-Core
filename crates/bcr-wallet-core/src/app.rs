use anyhow::Result;
use cashu::MintUrl;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::OnceLock;
use std::sync::RwLock;
use tracing::{info, warn};

use crate::db::{MemoryDatabase, WalletDatabase};
use crate::wallet::CreditWallet;
use crate::wallet::{Wallet, new_credit};

type TEST_WALLET = Wallet<CreditWallet, MemoryDatabase>;

// trait WalletTrait {
//     fn get_mint_url(&self) -> &MintUrl;
//     fn get_unit(&self) -> &CurrencyUnit;
// }
// impl<T: WalletType, DB: WalletDatabase> WalletTrait for Wallet<T, DB> {
//     fn get_mint_url(&self) -> &MintUrl {
//         &self.mint_url
//     }
//     fn get_unit(&self) -> &CurrencyUnit {
//         &self.unit
//     }
// }
// let wallets: Vec<Box<dyn WalletTrait>> = Vec::new();
// let wallet_refs: Vec<&dyn WalletTrait> = Vec::new();

pub struct AppState {
    info: WalletInfo,
    wallet: TEST_WALLET,
}

#[derive(Debug, Clone)]
pub struct WalletInfo {
    pub name: String,
}

impl Default for AppState {
    fn default() -> AppState {
        let db = MemoryDatabase::default();
        let mint_url = MintUrl::from_str("http://127.0.0.1:4343".into()).unwrap();
        let wallet = new_credit()
            .set_mint_url(mint_url)
            .set_database(db)
            .set_seed([0; 32])
            .build();
        AppState {
            wallet: wallet,
            info: WalletInfo {
                name: "BitCredu".into(),
            },
        }
    }
}

static APP_STATE: OnceLock<RwLock<AppState>> = OnceLock::new();

fn get_state() -> &'static RwLock<AppState> {
    APP_STATE.get_or_init(|| RwLock::new(AppState::default()))
}

// APP STATE

pub async fn import_token_v3(token: String) {
    let mut state = get_state().write().unwrap();
    state.wallet.import_token_v3(token).await;
}

pub async fn get_proofs() -> Vec<cashu::Proof> {
    let state = get_state().read().unwrap();
    state.wallet.db.get_proofs().await
}

pub async fn get_balance() -> u64 {
    let state = get_state().read().unwrap();
    state.wallet.get_balance().await
}

pub fn get_wallet_info() -> WalletInfo {
    let state = get_state().read().unwrap();
    state.info.clone()
}

pub async fn send_proofs_for(amount: u64) -> String {
    let mut state = get_state().write().unwrap();
    state.wallet.split(amount).await;
    state.wallet.send_proofs_for(amount).await
}
