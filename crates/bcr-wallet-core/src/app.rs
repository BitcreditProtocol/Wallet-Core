// ----- standard library imports
use std::cell::RefCell;
// ----- extra library imports
use anyhow::Result;
use cashu::MintUrl;
// ----- local modules
use crate::db::{self, Metadata, WalletDatabase, WalletMetadata, rexie::RexieWalletDatabase};
use crate::mint::{Connector, MintConnector};
use crate::wallet::{self, SwapProofs, Wallet, WalletType, new_credit, new_debit};
// ----- end imports

// Experimental, Many things will change here, mostly for testing

pub enum RexieWallet {
    Credit(Wallet<wallet::CreditWallet, RexieWalletDatabase>),
    Debit(Wallet<wallet::DebitWallet, RexieWalletDatabase>),
}

// This trait just exists to make life easy in this file as an access point
// The underscore _ in front is to avoid ambiguity with existing methods
// Excessive boilerplate but keeps the rest of the code clean
trait WalletInterface {
    async fn _import_token_v3(&self, token: String) -> Result<()>;
    async fn _restore(&self) -> Result<()>;
    async fn _get_active_proofs(&self) -> Result<Vec<cashu::Proof>>;
    async fn _get_balance(&self) -> Result<u64>;
    async fn _recheck(&self) -> Result<()>;
    async fn _split(&self, amount: u64) -> Result<()>;
    async fn _send_proofs_for(&self, amount: u64) -> Result<String>;
    async fn _list_keysets(&self) -> Result<Vec<cashu::KeySetInfo>>;
}

impl<T: WalletType> WalletInterface for Wallet<T, RexieWalletDatabase>
where
    Connector<T>: MintConnector,
    Wallet<T, RexieWalletDatabase>: SwapProofs,
{
    async fn _import_token_v3(&self, token: String) -> Result<()> {
        self.import_token_v3(token).await
    }
    async fn _restore(&self) -> Result<()> {
        self.restore().await
    }
    async fn _get_active_proofs(&self) -> Result<Vec<cashu::Proof>> {
        self.db
            .get_active_proofs()
            .await
            .map_err(anyhow::Error::new)
    }
    async fn _get_balance(&self) -> Result<u64> {
        self.get_balance().await
    }
    async fn _recheck(&self) -> Result<()> {
        self.recheck().await
    }
    async fn _split(&self, amount: u64) -> Result<()> {
        self.split(amount).await
    }
    async fn _send_proofs_for(&self, amount: u64) -> Result<String> {
        self.send_proofs_for(amount).await
    }
    async fn _list_keysets(&self) -> Result<Vec<cashu::KeySetInfo>> {
        Ok(self.connector.list_keysets().await?.keysets)
    }
}

impl WalletInterface for RexieWallet {
    async fn _import_token_v3(&self, token: String) -> Result<()> {
        match self {
            RexieWallet::Credit(w) => w._import_token_v3(token).await,
            RexieWallet::Debit(w) => w._import_token_v3(token).await,
        }
    }

    async fn _restore(&self) -> Result<()> {
        match self {
            RexieWallet::Credit(w) => w._restore().await,
            RexieWallet::Debit(w) => w._restore().await,
        }
    }

    async fn _get_active_proofs(&self) -> Result<Vec<cashu::Proof>> {
        match self {
            RexieWallet::Credit(w) => w._get_active_proofs().await,
            RexieWallet::Debit(w) => w._get_active_proofs().await,
        }
    }

    async fn _get_balance(&self) -> Result<u64> {
        match self {
            RexieWallet::Credit(w) => w._get_balance().await,
            RexieWallet::Debit(w) => w._get_balance().await,
        }
    }

    async fn _recheck(&self) -> Result<()> {
        match self {
            RexieWallet::Credit(w) => w._recheck().await,
            RexieWallet::Debit(w) => w._recheck().await,
        }
    }

    async fn _split(&self, amount: u64) -> Result<()> {
        match self {
            RexieWallet::Credit(w) => w._split(amount).await,
            RexieWallet::Debit(w) => w._split(amount).await,
        }
    }

    async fn _send_proofs_for(&self, amount: u64) -> Result<String> {
        match self {
            RexieWallet::Credit(w) => w._send_proofs_for(amount).await,
            RexieWallet::Debit(w) => w._send_proofs_for(amount).await,
        }
    }

    async fn _list_keysets(&self) -> Result<Vec<cashu::KeySetInfo>> {
        match self {
            RexieWallet::Credit(w) => w._list_keysets().await,
            RexieWallet::Debit(w) => w._list_keysets().await,
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

    // Validation
    let mnemonic = bip39::Mnemonic::parse_in_normalized(bip39::Language::English, mnemonic.trim())?;
    let mnemonic: Vec<String> = mnemonic.words().map(String::from).collect();

    let state = get_state();
    state
        .metadata
        .add_wallet(name, mint_url, mnemonic, unit, credit)
        .await?;

    Ok(())
}

pub async fn get_wallet(id: usize) -> anyhow::Result<RexieWallet> {
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
                bip39::Mnemonic::parse_in_normalized(bip39::Language::English, &mnemonic)?;

            let unit = metadata.unit.parse()?;

            let seed = mnemonic.to_seed("");
            let wallet = new_credit()
                .set_unit(unit)
                .set_mint_url(metadata.mint_url)
                .set_database(db)
                .set_seed(seed)
                .build();

            tracing::info!(mint_url=?wallet.mint_url, "Wallet loaded successfully");

            return Ok(RexieWallet::Credit(wallet));
        }
        Ok(
            metadata @ WalletMetadata {
                is_credit: false, ..
            },
        ) => {
            let db = RexieWalletDatabase::new(format!("wallet_{}", id), state.db_manager.get_db());
            let mnemonic = metadata.mnemonic.join(" ");
            let mnemonic =
                bip39::Mnemonic::parse_in_normalized(bip39::Language::English, &mnemonic)?;

            let unit = metadata.unit.parse()?;

            let seed = mnemonic.to_seed("");
            let wallet = new_debit()
                .set_unit(unit)
                .set_mint_url(metadata.mint_url)
                .set_database(db)
                .set_seed(seed)
                .build();

            tracing::info!(mint_url=?wallet.mint_url, "Wallet loaded successfully");

            return Ok(RexieWallet::Debit(wallet));
        }
        _ => {}
    }

    Err(anyhow::Error::msg("Wallet not found"))
}

pub async fn initialize() {
    let manager = db::rexie::Manager::new("wallets_db_7").await.unwrap();

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
    tracing::debug!("Listing wallets");
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
    wallet._import_token_v3(token).await.unwrap();
}

pub async fn recover(idx: usize) {
    tracing::debug!("Recovering wallet {}", idx);
    let wallet = get_wallet(idx).await.unwrap();
    wallet._restore().await.unwrap();
}

pub async fn get_mint_url(idx: usize) -> String {
    tracing::debug!("Getting mint URL for wallet {}", idx);
    let state = get_state();
    let md = state.metadata.get_wallet(idx).await.unwrap();

    md.mint_url.to_string()
}

pub async fn get_proofs(idx: usize) -> Vec<cashu::Proof> {
    tracing::debug!("Listing proofs for wallet {}", idx);
    let wallet = get_wallet(idx).await.unwrap();
    wallet._get_active_proofs().await.unwrap_or(Vec::new())
}

pub async fn list_keysets(idx: usize) -> Vec<cashu::KeySetInfo> {
    tracing::debug!("Listing keysets for wallet {}", idx);
    let wallet = get_wallet(idx).await.unwrap();

    let unit = match &wallet {
        RexieWallet::Debit(debit) => debit.unit.clone(),
        RexieWallet::Credit(credit) => credit.unit.clone(),
    };

    let keysets = wallet._list_keysets().await.unwrap();

    let keysets = keysets.into_iter().filter(|k| k.unit == unit).collect();
    tracing::debug!("Keysets: {:?}", keysets);
    keysets
}

pub async fn get_unit(idx: usize) -> cashu::CurrencyUnit {
    tracing::debug!("Listing keysets for wallet {}", idx);
    let wallet = get_wallet(idx).await.unwrap();

    match &wallet {
        RexieWallet::Debit(debit) => debit.unit.clone(),
        RexieWallet::Credit(credit) => credit.unit.clone(),
    }
}

pub async fn get_balance(idx: usize) -> u64 {
    tracing::debug!("Getting balance for wallet {}", idx);
    let wallet = get_wallet(idx).await.unwrap();
    wallet._get_balance().await.unwrap()
}

pub async fn recheck(idx: usize) {
    tracing::debug!("Rechecking wallet {}", idx);
    let wallet = get_wallet(idx).await.unwrap();
    wallet._recheck().await.unwrap()
}

pub fn get_wallet_info() -> WalletInfo {
    let state = get_state();
    state.info.clone()
}

pub async fn send_proofs_for(amount: u64, idx: usize) -> String {
    let wallet = get_wallet(idx).await.unwrap();

    // Ensures we always have the right powers of 2 to send amount
    let _ = wallet._split(amount).await;
    wallet._send_proofs_for(amount).await.unwrap_or("".into())
}

pub async fn redeem_inactive(idx: usize) -> String {
    let wallet = get_wallet(idx).await.unwrap();

    match wallet {
        RexieWallet::Debit(_) => "".into(),
        RexieWallet::Credit(credit) => credit.redeem_inactive().await.unwrap_or("".into()),
    }
}
