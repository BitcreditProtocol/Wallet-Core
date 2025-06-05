// ----- standard library imports

use std::cell::RefCell;
use std::str::FromStr;
// ----- extra library imports
use crate::db::rexie::RexieWalletDatabase;
use crate::db::{self, Metadata, WalletMetadata};
use anyhow::Result;
use cashu::MintUrl;
// ----- local modules
use crate::db::WalletDatabase;
use crate::wallet;
use crate::wallet::{Wallet, new_credit};
// ----- end imports

// Experimental, no error handling

pub enum RexieWallet {
    Credit(Wallet<wallet::CreditWallet, RexieWalletDatabase>),
    Debit(Wallet<wallet::DebitWallet, RexieWalletDatabase>),
}

trait WalletInterface {
    async fn import_token_v3(&self, token: String) -> Result<()>;
    async fn restore(&self) -> Result<()>;
    async fn get_active_proofs(&self) -> Result<Vec<cashu::Proof>>;
    async fn get_balance(&self) -> Result<u64>;
    async fn recheck(&self) -> Result<()>;
    async fn split(&self, amount: u64) -> Result<()>;
    async fn send_proofs_for(&self, amount: u64) -> Result<String>;
}

impl WalletInterface for RexieWallet {
    async fn import_token_v3(&self, token: String) -> Result<()> {
        match self {
            RexieWallet::Credit(wallet) => wallet.import_token_v3(token).await,
            RexieWallet::Debit(_) => todo!("DebitWALLET"),
        }
    }
    async fn restore(&self) -> Result<()> {
        match self {
            RexieWallet::Credit(wallet) => wallet.restore().await,
            RexieWallet::Debit(_) => todo!("DebitWALLET"),
        }
    }
    async fn get_active_proofs(&self) -> Result<Vec<cashu::Proof>> {
        match self {
            RexieWallet::Credit(wallet) => wallet
                .db
                .get_active_proofs()
                .await
                .map_err(|e| anyhow::Error::new(e)),
            RexieWallet::Debit(_) => todo!("DebitWALLET"),
        }
    }
    async fn get_balance(&self) -> Result<u64> {
        match self {
            RexieWallet::Credit(wallet) => wallet.get_balance().await,
            RexieWallet::Debit(_) => todo!("DebitWALLET"),
        }
    }
    async fn recheck(&self) -> Result<()> {
        match self {
            RexieWallet::Credit(wallet) => wallet.recheck().await,
            RexieWallet::Debit(_) => todo!("DebitWALLET"),
        }
    }
    async fn split(&self, amount: u64) -> Result<()> {
        match self {
            RexieWallet::Credit(wallet) => wallet.split(amount).await,
            RexieWallet::Debit(_) => todo!("DebitWALLET"),
        }
    }
    async fn send_proofs_for(&self, amount: u64) -> Result<String> {
        match self {
            RexieWallet::Credit(wallet) => wallet.send_proofs_for(amount).await,
            RexieWallet::Debit(_) => todo!("DebitWALLET"),
        }
    }
}

pub struct AppState {
    info: WalletInfo,
    db_manager: db::rexie::Manager,
    metadata: db::rexie::RexieMetadata,
}

#[derive(Debug, Clone)]
pub struct WalletInfo {
    pub name: String,
}

thread_local! {
    static APP_STATE: RefCell<Option<&'static AppState>> = const { RefCell::new(None) } ;
}

pub async fn add_wallet(
    name: String,
    mint_url: String,
    mnemonic: String,
    unit: String,
    credit: bool,
) -> Result<()> {
    let mint_url: MintUrl = mint_url.parse().map_err(anyhow::Error::msg)?;

    // TODO verify mnemonic
    let mnemonic: Vec<String> = mnemonic.split_whitespace().map(String::from).collect();

    let state = get_state();
    state
        .metadata
        .add_wallet(name, mint_url, mnemonic, unit, credit)
        .await?;

    Ok(())
}

pub async fn get_wallet(id: usize) -> Option<RexieWallet> {
    let state = get_state();
    match state.metadata.get_wallet(id).await {
        Ok(
            metadata @ WalletMetadata {
                is_credit: true, ..
            },
        ) => {
            let db = RexieWalletDatabase::new(format!("wallet_{}", id), state.db_manager.get_db());
            let mnemonic = metadata.mnemonic.join(" ");
            let mnemonic =
                bip39::Mnemonic::parse_in_normalized(bip39::Language::English, &mnemonic).unwrap();

            let seed = mnemonic.to_seed("");
            let wallet = new_credit()
                .set_mint_url(metadata.mint_url)
                .set_database(db)
                .set_seed(seed)
                .build();

            return Some(RexieWallet::Credit(wallet));
        }
        _ => {}
    }

    None
}

pub async fn initialize() {
    let manager = db::rexie::Manager::new().await.unwrap();

    let metadata = db::rexie::RexieMetadata::new(manager.get_db());

    let state = AppState {
        db_manager: manager,
        metadata,
        info: WalletInfo {
            name: "BitCredit".into(),
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

pub async fn get_wallets() -> (Vec<usize>, Vec<String>) {
    let state = get_state();
    if let Ok(wallets) = state.metadata.get_wallets().await {
        let ids = wallets.iter().map(|w| w.id).collect::<Vec<_>>();
        let names = wallets.iter().map(|w| w.name.clone()).collect();
        (ids, names)
    } else {
        Default::default()
    }
}

pub async fn import_token_v3(token: String, idx: usize) {
    let wallet = get_wallet(idx).await.unwrap();
    wallet.import_token_v3(token).await.unwrap();
}

pub async fn recover(idx: usize) {
    let wallet = get_wallet(idx).await.unwrap();
    wallet.restore().await.unwrap();
}

pub async fn get_mint_url(idx: usize) -> String {
    let state = get_state();
    let md = state.metadata.get_wallet(idx).await.unwrap();

    md.mint_url.to_string()
}

pub async fn get_proofs(idx: usize) -> Vec<cashu::Proof> {
    let wallet = get_wallet(idx).await.unwrap();
    wallet.get_active_proofs().await.unwrap_or(Vec::new())
}

pub async fn get_balance(idx: usize) -> u64 {
    let wallet = get_wallet(idx).await.unwrap();
    wallet.get_balance().await.unwrap()
}

pub async fn recheck(idx: usize) {
    let wallet = get_wallet(idx).await.unwrap();
    wallet.recheck().await.unwrap()
}

pub fn get_wallet_info() -> WalletInfo {
    let state = get_state();
    state.info.clone()
}

pub async fn send_proofs_for(amount: u64, idx: usize) -> String {
    let wallet = get_wallet(idx).await.unwrap();

    // Ensures we always have the right powers of 2 to send amount
    let _ = wallet.split(amount).await;
    wallet.send_proofs_for(amount).await.unwrap_or("".into())
}
