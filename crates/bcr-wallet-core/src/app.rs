// ----- standard library imports
use std::{cell::RefCell, collections::HashSet, str::FromStr, sync::Arc};
// ----- extra library imports
use anyhow::Error as AnyError;
use bitcoin::{
    bip32 as btc32,
    hashes::{Hash, HashEngine, sha256},
    hex::DisplayHex,
};
use cashu::{CurrencyUnit, KeySetInfo, MintInfo, MintUrl, nut18 as cdk18};
use cdk::wallet::{
    MintConnector,
    types::{Transaction, TransactionId},
};
use uuid::Uuid;
// ----- local imports
use crate::{
    config::{Config, Settings},
    error::{Error, Result},
    purse::Wallet,
    types::{PaymentSummary, RedemptionSummary},
    wallet::{CreditPocket, WalletBalance},
};

// ----- end imports

#[cfg(target_arch = "wasm32")]
mod prod {
    pub type ProductionPocketRepository = crate::persistence::rexie::PocketDB;
    pub type ProductionMintMeltRepository = crate::persistence::rexie::MintMeltDB;
    pub type ProductionPurseRepository = crate::persistence::rexie::PurseDB;
    pub type ProductionTransactionRepository = crate::persistence::rexie::TransactionDB;
    pub type ProductionSettingsRepository = crate::persistence::rexie::SettingsDB;
}
#[cfg(not(target_arch = "wasm32"))]
mod prod {
    pub type ProductionPocketRepository = crate::persistence::inmemory::InMemoryPocketRepository;
    pub type ProductionMintMeltRepository =
        crate::persistence::inmemory::InMemoryMintMeltRepository;
    pub type ProductionPurseRepository = crate::persistence::inmemory::InMemoryPurseRepository;
    pub type ProductionTransactionRepository =
        crate::persistence::inmemory::InMemoryTransactionRepository;
    pub type ProductionSettingsRepository =
        crate::persistence::inmemory::InMemorySettingsRepository;
}

type ProductionConnector = crate::mint::HttpClientExt;
type ProductionDebitPocket = crate::pocket::debit::Pocket;
type ProductionCreditPocket = crate::pocket::credit::Pocket;
type ProductionWallet =
    crate::wallet::Wallet<prod::ProductionTransactionRepository, ProductionDebitPocket>;
type ProductionPurse = crate::purse::Purse<prod::ProductionPurseRepository, ProductionWallet>;

pub struct AppState {
    purse: Option<Arc<ProductionPurse>>,
    settings: Option<Arc<prod::ProductionSettingsRepository>>,
}
impl AppState {
    pub const DB_VERSION: u32 = 1;

    pub fn new(
        purse: Option<Arc<ProductionPurse>>,
        settings: Option<Arc<prod::ProductionSettingsRepository>>,
    ) -> Self {
        tracing::debug!("Creating new AppState");
        Self { purse, settings }
    }

    pub async fn load_settings(&self) -> Result<Settings> {
        match &self.settings {
            Some(db) => db.load().await,
            None => {
                tracing::warn!("Settings DB not initialized, returning default settings");
                Ok(Settings::default())
            }
        }
    }

    pub async fn load_wallets(&mut self) -> Result<()> {
        tracing::debug!("AppState::load_wallets()");

        let settings = self.load_settings().await?;
        let Some(purse) = &self.purse else {
            return Err(Error::Initialization);
        };
        let w_ids = purse.list_wallets().await?;
        for wid in w_ids {
            tracing::debug!("Loading wallet with id: {wid}");
            let w_cfg = purse.load_wallet_config(&wid).await?;
            if w_cfg.network != settings.network {
                tracing::info!(
                    "Skipping wallet {wid} with network {:?}, expected {:?}",
                    w_cfg.network,
                    settings.network,
                );
                continue;
            }
            let wallet = build_wallet(
                w_cfg.name,
                w_cfg.network,
                w_cfg.mint,
                w_cfg.mnemonic,
                LocalDB::Keep,
                Self::DB_VERSION,
            )
            .await?;
            purse.add_wallet(wallet).await?;
        }
        Ok(())
    }
}
impl Default for AppState {
    fn default() -> Self {
        Self::new(None, None)
    }
}

thread_local! {
static APP_STATE: RefCell<AppState> = RefCell::new(AppState::default());
}

pub async fn initialize_api() -> Result<()> {
    tracing::debug!("Initializing API");

    let pursedb = db::build_pursedb(AppState::DB_VERSION).await?;
    let settingsdb = db::build_settingsdb(AppState::DB_VERSION).await?;
    let settings = settingsdb.load().await?;
    let config = Config::new(settings)?;
    let nostr_cl = nostr_sdk::Client::new(config.nostr_signer);
    for relay in &config.relays {
        nostr_cl.add_relay(relay).await?;
    }
    nostr_cl.connect().await;
    let http_cl = reqwest::Client::new();
    let purse = ProductionPurse::new(pursedb, http_cl, nostr_cl, config.nprofile).await?;
    let mut appstate = AppState::new(Some(Arc::new(purse)), Some(Arc::new(settingsdb)));
    appstate.load_wallets().await?;
    APP_STATE.replace(appstate);
    Ok(())
}

fn get_wallet(idx: usize) -> Result<Arc<ProductionWallet>> {
    APP_STATE.with_borrow(|state| {
        let Some(purse) = &state.purse else {
            return Err(Error::Initialization);
        };
        purse.get_wallet(idx).ok_or(Error::WalletNotFound(idx))
    })
}

fn get_purse() -> Result<Arc<ProductionPurse>> {
    APP_STATE.with_borrow(|state| {
        let Some(purse) = &state.purse else {
            return Err(Error::Initialization);
        };
        Ok(Arc::clone(purse))
    })
}

fn get_settingsdb() -> Result<Arc<prod::ProductionSettingsRepository>> {
    APP_STATE.with_borrow(|state| {
        let Some(db) = &state.settings else {
            return Err(Error::Initialization);
        };
        Ok(Arc::clone(db))
    })
}

/// returns the index of the wallet
pub async fn add_wallet(name: String, mint_url: String, mnemonic: String) -> Result<usize> {
    tracing::debug!("Adding a new wallet for mint {name}, {mint_url}, {mnemonic}");

    let settings = get_settingsdb()?.load().await?;
    let mint_url = MintUrl::from_str(&mint_url)?;
    let mnemonic = bip39::Mnemonic::from_str(&mnemonic)?;
    let wallet = build_wallet(
        name,
        settings.network,
        mint_url,
        mnemonic,
        LocalDB::Keep,
        AppState::DB_VERSION,
    )
    .await?;

    let purse = APP_STATE.with_borrow_mut(|state| {
        let Some(purse) = &state.purse else {
            return Err(Error::Initialization);
        };
        Ok(Arc::clone(purse))
    })?;
    let idx = purse.add_wallet(wallet).await?;

    Ok(idx)
}

/// returns the index of the wallet
pub async fn restore_wallet(name: String, mint_url: String, mnemonic: String) -> Result<usize> {
    tracing::debug!("Restoring a new wallet for mint {name}, {mint_url}, {mnemonic}");

    let settings = get_settingsdb()?.load().await?;
    let mint_url = MintUrl::from_str(&mint_url)?;
    let mnemonic = bip39::Mnemonic::from_str(&mnemonic)?;
    let wallet = build_wallet(
        name,
        settings.network,
        mint_url,
        mnemonic,
        LocalDB::Delete,
        AppState::DB_VERSION,
    )
    .await?;
    wallet.restore_local_proofs().await?;

    let purse = APP_STATE.with_borrow_mut(|state| {
        let Some(purse) = &state.purse else {
            return Err(Error::Initialization);
        };
        Ok(Arc::clone(purse))
    })?;
    let idx = purse.add_wallet(wallet).await?;
    Ok(idx)
}

pub fn wallet_name(idx: usize) -> Result<String> {
    tracing::debug!("name for wallet {idx}");

    let wallet = get_wallet(idx)?;
    Ok(wallet.name())
}

pub fn wallet_mint_url(idx: usize) -> Result<String> {
    tracing::debug!("mint_url for wallet {idx}");
    let wallet = get_wallet(idx)?;
    Ok(wallet.mint_url().to_string())
}

pub struct WalletCurrencyUnit {
    pub credit: String,
    pub debit: String,
}

pub fn wallet_currency_unit(idx: usize) -> Result<WalletCurrencyUnit> {
    tracing::debug!("wallet_currency_unit({idx})");
    let wallet = get_wallet(idx)?;
    Ok(WalletCurrencyUnit {
        credit: wallet.credit_unit().to_string(),
        debit: wallet.debit_unit().to_string(),
    })
}

pub async fn wallet_balance(idx: usize) -> Result<WalletBalance> {
    tracing::debug!("wallet_balance({idx})");

    let wallet = get_wallet(idx)?;
    wallet.balance().await
}

pub async fn wallet_receive(idx: usize, token: String, tstamp: u64) -> Result<TransactionId> {
    tracing::debug!("wallet_receive({idx}, {token}, {tstamp})");

    let token = bcr_wallet_lib::wallet::Token::from_str(&token)?;
    let wallet = get_wallet(idx)?;
    let tx_id = wallet.receive_token(token, tstamp).await?;
    Ok(tx_id)
}

pub async fn wallet_redeem_credit(idx: usize) -> Result<cashu::Amount> {
    tracing::debug!("wallet_redeem_credit({idx})");

    let wallet = get_wallet(idx)?;
    let amount_redeemed = wallet.redeem_credit().await?;
    Ok(amount_redeemed)
}

pub async fn wallet_list_redemptions(
    idx: usize,
    payment_window: std::time::Duration,
) -> Result<Vec<RedemptionSummary>> {
    tracing::debug!(
        "wallet_list_redemptions({idx}, {})",
        payment_window.as_secs()
    );

    let wallet = get_wallet(idx)?;
    let redemptions = wallet.list_redemptions(payment_window).await?;
    Ok(redemptions)
}

pub async fn wallet_clean_local_db(idx: usize) -> Result<u32> {
    tracing::debug!("wallet_clean_local_db({idx})");

    let wallet = get_wallet(idx)?;
    let deleted = wallet.clean_local_db().await?;
    Ok(deleted)
}

pub async fn wallet_load_tx(idx: usize, tx_id: &str) -> Result<Transaction> {
    tracing::debug!("wallet_load_tx({idx}, {tx_id})");

    let tx_id = TransactionId::from_str(tx_id)?;
    let wallet = get_wallet(idx)?;
    let tx = wallet.load_tx(tx_id).await?;
    Ok(tx)
}

pub async fn wallet_list_tx_ids(idx: usize) -> Result<Vec<TransactionId>> {
    tracing::debug!("wallet_list_tx_ids({idx})");

    let wallet = get_wallet(idx)?;
    let tx_ids = wallet.list_tx_ids().await?;
    Ok(tx_ids)
}

pub async fn wallet_prepare_payment(idx: usize, input: String, now: u64) -> Result<PaymentSummary> {
    tracing::debug!("wallet_prepare_payment({idx}, {input})");

    let purse = get_purse()?;
    let summary = purse.prepare_pay(idx, input, now).await?;
    Ok(summary)
}

pub async fn wallet_pay(rid: String, tstamp: u64) -> Result<TransactionId> {
    tracing::debug!("wallet_pay({rid}, {tstamp})");

    let purse = get_purse()?;
    let rid = Uuid::from_str(&rid)?;
    let tx_id = purse.pay(rid, tstamp).await?;
    Ok(tx_id)
}

pub async fn wallet_prepare_payment_request(
    idx: usize,
    amount: u64,
    unit: String,
    description: String,
) -> Result<cdk18::PaymentRequest> {
    tracing::debug!("wallet_prepare_pay_request({idx}, {amount}, {unit}, {description})");

    let amount = cashu::Amount::from(amount);
    let unit = if unit.trim().is_empty() {
        None
    } else {
        cashu::CurrencyUnit::from_str(&unit).ok()
    };
    let description = if description.trim().is_empty() {
        None
    } else {
        Some(description.trim().to_string())
    };
    let purse = get_purse()?;
    let request = purse.prepare_payment_request(amount, unit, description)?;
    Ok(request)
}

pub async fn wallet_check_received_payment(
    max_wait_sec: u64,
    p_id: String,
) -> Result<Option<TransactionId>> {
    tracing::debug!("wallet_check_received_payment({p_id})");

    let p_id = Uuid::from_str(&p_id)?;
    let purse = get_purse()?;
    let max_wait = core::time::Duration::from_secs(max_wait_sec);
    let tx_id = purse.check_received_payment(max_wait, p_id).await?;
    Ok(tx_id)
}

pub async fn wallet_check_pending_melts(idx: usize) -> Result<cashu::Amount> {
    tracing::debug!("wallet_check_pending_melts({idx})");

    let wallet = get_wallet(idx)?;
    wallet.check_pending_melts().await
}

pub fn wallets_ids() -> Result<Vec<u32>> {
    tracing::debug!("get_wallet_ids");
    let ids = APP_STATE.with_borrow(|state| {
        let Some(purse) = &state.purse else {
            return Err(Error::Initialization);
        };
        Ok(purse.ids())
    })?;
    Ok(ids)
}

pub fn wallets_names() -> Result<Vec<String>> {
    tracing::debug!("get_wallet_names");
    let names = APP_STATE.with_borrow(|state| {
        let Some(purse) = &state.purse else {
            return Err(Error::Initialization);
        };
        Ok(purse.names())
    })?;
    Ok(names)
}

pub enum LocalDB {
    Delete,
    Keep,
}

#[cfg(target_arch = "wasm32")]
mod db {
    use super::*;
    use std::rc::Rc;

    pub async fn build_pursedb(db_version: u32) -> Result<prod::ProductionPurseRepository> {
        let rexie_db_name = "bitcredit_wallet";
        let mut rexie_builder = rexie::Rexie::builder(rexie_db_name).version(db_version);
        let purse_stores = prod::ProductionPurseRepository::object_stores();
        for store in purse_stores {
            rexie_builder = rexie_builder.add_object_store(store);
        }
        let rexie = Rc::new(rexie_builder.build().await?);
        let pursedb = prod::ProductionPurseRepository::new(rexie)?;
        Ok(pursedb)
    }

    pub async fn build_wallet_dbs(
        db_version: u32,
        wallet_id: &str,
        debit: &CurrencyUnit,
        credit: Option<&CurrencyUnit>,
        local: LocalDB,
    ) -> Result<(
        prod::ProductionTransactionRepository,
        (
            (
                prod::ProductionPocketRepository,
                prod::ProductionMintMeltRepository,
            ),
            Option<prod::ProductionPocketRepository>,
        ),
    )> {
        let rexie_db_name = format!("bitcredit_wallet_{wallet_id}");
        let transaction_stores = prod::ProductionTransactionRepository::object_stores(wallet_id);
        let mut rexie_builder = rexie::Rexie::builder(&rexie_db_name).version(db_version);
        if matches!(local, db::LocalDB::Delete) {
            rexie_builder.delete().await.unwrap_or_else(|e| {
                tracing::warn!("Failed to delete existing DB: {e}");
            });
            rexie_builder = rexie::Rexie::builder(&rexie_db_name).version(db_version);
        }
        for store in transaction_stores {
            rexie_builder = rexie_builder.add_object_store(store);
        }
        let stores = prod::ProductionPocketRepository::object_stores(debit);
        for store in stores {
            rexie_builder = rexie_builder.add_object_store(store);
        }
        let stores = prod::ProductionMintMeltRepository::object_stores(debit);
        for store in stores {
            rexie_builder = rexie_builder.add_object_store(store);
        }
        if let Some(unit) = credit {
            let stores = prod::ProductionPocketRepository::object_stores(unit);
            for store in stores {
                rexie_builder = rexie_builder.add_object_store(store);
            }
        }
        let rexiedb = Rc::new(rexie_builder.build().await?);
        let tx_repo = prod::ProductionTransactionRepository::new(rexiedb.clone(), wallet_id)?;
        let debitdb = prod::ProductionPocketRepository::new(rexiedb.clone(), &debit)?;
        let mintmeltdb = prod::ProductionMintMeltRepository::new(rexiedb.clone(), &debit)?;
        let creditdb = if let Some(unit) = credit {
            Some(prod::ProductionPocketRepository::new(
                rexiedb.clone(),
                unit,
            )?)
        } else {
            None
        };
        Ok((tx_repo, ((debitdb, mintmeltdb), creditdb)))
    }

    pub async fn build_settingsdb(db_version: u32) -> Result<prod::ProductionSettingsRepository> {
        let rexie_db_name = "bitcredit_settings";
        let mut rexie_builder = rexie::Rexie::builder(rexie_db_name).version(db_version);
        let settings_stores = prod::ProductionSettingsRepository::object_stores();
        for store in settings_stores {
            rexie_builder = rexie_builder.add_object_store(store);
        }
        let rexie = Rc::new(rexie_builder.build().await?);
        let settingsdb = prod::ProductionSettingsRepository::new(rexie)?;
        Ok(settingsdb)
    }
}
#[cfg(not(target_arch = "wasm32"))]
mod db {
    use super::*;

    pub async fn build_pursedb(_db_version: u32) -> Result<prod::ProductionPurseRepository> {
        Ok(prod::ProductionPurseRepository::default())
    }

    pub async fn build_wallet_dbs(
        _db_version: u32,
        _wallet_id: &str,
        _debit: &CurrencyUnit,
        credit: Option<&CurrencyUnit>,
        _local: LocalDB,
    ) -> Result<(
        prod::ProductionTransactionRepository,
        (
            (
                prod::ProductionPocketRepository,
                prod::ProductionMintMeltRepository,
            ),
            Option<prod::ProductionPocketRepository>,
        ),
    )> {
        let txdb = prod::ProductionTransactionRepository::default();
        let debitdb = prod::ProductionPocketRepository::default();
        let mintmeltdb = prod::ProductionMintMeltRepository::default();
        let creditdb = if credit.is_some() {
            Some(prod::ProductionPocketRepository::default())
        } else {
            None
        };
        Ok((txdb, ((debitdb, mintmeltdb), creditdb)))
    }

    pub async fn build_settingsdb(_db_version: u32) -> Result<prod::ProductionSettingsRepository> {
        Ok(prod::ProductionSettingsRepository::default())
    }
}

fn build_mint_id(url: &MintUrl, info: &MintInfo) -> Vec<u8> {
    if let Some(pk) = info.pubkey {
        pk.to_bytes().to_vec()
    } else if let Some(name) = &info.name {
        name.to_string().as_bytes().to_vec()
    } else {
        url.to_string().as_bytes().to_vec()
    }
}

fn find_currency_units(
    keyset_infos: &[KeySetInfo],
) -> Result<(CurrencyUnit, Option<CurrencyUnit>)> {
    let currencies = keyset_infos
        .iter()
        .map(|k| k.unit.clone())
        .collect::<HashSet<_>>();
    if currencies.len() > 2 {
        return Err(Error::Any(AnyError::msg(
            "Mint supports more than 2 currencies, not supported yet",
        )));
    }
    let credit_unit = currencies
        .iter()
        .find(|unit| unit.to_string().starts_with("cr"));
    let debit_unit = currencies
        .iter()
        .find(|unit| !unit.to_string().starts_with("cr"));
    if debit_unit.is_none() {
        let currencies = currencies.iter().cloned().collect();
        return Err(Error::NoDebitCurrencyInMint(currencies));
    }
    let debit_unit = debit_unit.unwrap();
    Ok((debit_unit.clone(), credit_unit.cloned()))
}

fn build_wallet_id(mint_id: &[u8], master: &btc32::Xpriv) -> String {
    let secp = secp256k1::Secp256k1::signing_only();
    let xpub = btc32::Xpub::from_priv(&secp, master);
    let mut hasher = sha256::HashEngine::default();
    hasher.input(mint_id);
    hasher.input(xpub.fingerprint().to_bytes().as_slice());
    sha256::Hash::from_engine(hasher)
        .as_byte_array()
        .as_hex()
        .to_string()
}
async fn build_wallet(
    name: String,
    network: bitcoin::Network,
    mint_url: cashu::MintUrl,
    mnemonic: bip39::Mnemonic,
    local: LocalDB,
    db_version: u32,
) -> Result<ProductionWallet> {
    let master = bitcoin::bip32::Xpriv::new_master(network, &mnemonic.to_seed(""))?;
    // retrieving mint details
    let client = Box::new(ProductionConnector::new(mint_url.clone()));
    let info = client.get_mint_info().await?;
    let mint_id = build_mint_id(&mint_url, &info);
    let keyset_infos = client.get_mint_keysets().await?.keysets;
    let (debit_unit, credit_unit) = find_currency_units(&keyset_infos)?;
    // building wallet dbs
    let wallet_id = build_wallet_id(&mint_id, &master);
    let (tx_repo, ((debitdb, mintmeltdb), creditdb)) = db::build_wallet_dbs(
        db_version,
        &wallet_id,
        &debit_unit,
        credit_unit.as_ref(),
        local,
    )
    .await?;
    // building the debit pocket
    let debit_pocket = ProductionDebitPocket::new(
        debit_unit.clone(),
        Arc::new(debitdb),
        Arc::new(mintmeltdb),
        master,
    );
    // building the credit pocket
    let credit_pocket: Box<dyn CreditPocket> = if let Some(unit) = &credit_unit {
        let creditdb = creditdb.expect("Credit pocket DB should be present");
        let pocket = ProductionCreditPocket::new(unit.clone(), Arc::new(creditdb), master);
        Box::new(pocket)
    } else {
        tracing::warn!("app::add_wallet: credit_pocket = DummyPocket");
        Box::new(crate::pocket::credit::DummyPocket {})
    };
    let new_wallet: ProductionWallet = ProductionWallet::new(
        network,
        client,
        tx_repo,
        debit_pocket,
        credit_pocket,
        name,
        wallet_id,
        mnemonic,
    )
    .await?;
    Ok(new_wallet)
}
