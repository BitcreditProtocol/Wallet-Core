// ----- standard library imports
use std::{cell::RefCell, collections::HashSet, rc::Rc, str::FromStr, sync::Mutex};
// ----- extra library imports
use anyhow::Error as AnyError;
use bcr_wallet_lib::wallet::Token;
use bitcoin::{
    bip32 as btc32,
    hashes::{Hash, HashEngine, sha256},
    hex::DisplayHex,
};
use cashu::CurrencyUnit;
use cdk::wallet::{
    MintConnector,
    types::{Transaction, TransactionId},
};
// ----- local imports
use crate::{
    SendSummary,
    error::{Error, Result},
    types::WalletConfig,
    wallet::{CreditPocket, Pocket, WalletBalance},
};

// ----- end imports

#[cfg(target_arch = "wasm32")]
mod prod {
    pub type ProductionPocketRepository = crate::persistence::rexie::PocketDB;
    pub type ProductionPurseRepository = crate::persistence::rexie::PurseDB;
    pub type ProductionTransactionRepository = crate::persistence::rexie::TransactionDB;
}
#[cfg(not(target_arch = "wasm32"))]
mod prod {
    pub type ProductionPocketRepository = crate::persistence::inmemory::InMemoryPocketRepository;
    pub type ProductionPurseRepository = crate::persistence::inmemory::InMemoryPurseRepository;
    pub type ProductionTransactionRepository =
        crate::persistence::inmemory::InMemoryTransactionRepository;
}

type ProductionConnector = cdk::wallet::HttpClient;
type ProductionDebitPocket = crate::pocket::DbPocket<prod::ProductionPocketRepository>;
type ProductionCreditPocket = crate::pocket::CrPocket<prod::ProductionPocketRepository>;
type ProductionWallet = crate::wallet::Wallet<
    ProductionConnector,
    prod::ProductionTransactionRepository,
    ProductionDebitPocket,
>;

type ProductionPurse = crate::purse::Purse<prod::ProductionPurseRepository>;

pub struct AppState {
    wallets: Vec<Rc<ProductionWallet>>,
    network: bitcoin::Network,
    purse: Option<Rc<ProductionPurse>>,
}
impl AppState {
    pub const DB_VERSION: u32 = 1;

    pub fn new(network: bitcoin::Network, purse: Option<Rc<ProductionPurse>>) -> Self {
        tracing::debug!("Creating new AppState with network: {:?}", network);
        Self {
            wallets: Vec::new(),
            network,
            purse,
        }
    }

    pub async fn load_wallets(&mut self) -> Result<()> {
        tracing::debug!("AppState::load_wallets({})", self.network);
        let Some(purse) = &self.purse else {
            return Err(Error::BadPurseDB);
        };
        let w_ids = purse.list_wallets().await?;
        for wid in w_ids {
            tracing::debug!("Loading wallet with id: {wid}");
            let w_cfg = purse.load_wallet(&wid).await?;
            if w_cfg.network != self.network {
                tracing::info!(
                    "Skipping wallet {wid} with network {:?}, expected {:?}",
                    w_cfg.network,
                    self.network,
                );
                continue;
            }
            let (txdb, pocketdbs) = db::build_wallet_dbs(
                AppState::DB_VERSION,
                &w_cfg.wallet_id,
                &w_cfg.debit,
                w_cfg.credit.as_ref(),
            )
            .await?;
            let client = ProductionConnector::new(w_cfg.mint.clone());
            let wallet = build_wallet(
                w_cfg.name,
                w_cfg.mint,
                w_cfg.master,
                client,
                txdb,
                pocketdbs,
                (w_cfg.debit, w_cfg.credit),
            )?;
            self.wallets.push(Rc::new(wallet));
        }
        Ok(())
    }
}
impl Default for AppState {
    fn default() -> Self {
        Self::new(bitcoin::Network::Bitcoin, None)
    }
}

thread_local! {
static APP_STATE: RefCell<AppState> = RefCell::new(AppState::default());
}

pub async fn initialize_api(network: String) {
    tracing::debug!("Initializing API with network: {network}");
    let net = bitcoin::Network::from_str(&network)
        .unwrap_or_else(|_| panic!("invalid network: {network}"));

    let pursedb = db::build_appstate_db(AppState::DB_VERSION)
        .await
        .expect("Failed to build app state DB");
    let purse = ProductionPurse { wallets: pursedb };

    let mut appstate = AppState::new(net, Some(Rc::new(purse)));
    appstate
        .load_wallets()
        .await
        .expect("Failed to load wallets");
    APP_STATE.replace(appstate);
}

fn get_wallet(idx: usize) -> Result<Rc<ProductionWallet>> {
    APP_STATE.with_borrow(|state| {
        state
            .wallets
            .get(idx)
            .cloned()
            .ok_or(Error::WalletNotFound(idx))
    })
}
fn get_purse() -> Result<Rc<ProductionPurse>> {
    APP_STATE.with_borrow(|state| state.purse.clone().ok_or(Error::BadPurseDB))
}
/// returns the index of the wallet
pub async fn add_wallet(name: String, mint_url: String, mnemonic: String) -> Result<usize> {
    tracing::debug!("Adding a new wallet for mint {name}, {mint_url}, {mnemonic}");

    let mint_url = cashu::MintUrl::from_str(&mint_url)?;
    let client = ProductionConnector::new(mint_url.clone());
    let info = client.get_mint_info().await?;
    let mint_id = if let Some(pk) = info.pubkey {
        pk.to_bytes().to_vec()
    } else if let Some(name) = info.name {
        name.to_string().as_bytes().to_vec()
    } else {
        mint_url.to_string().as_bytes().to_vec()
    };
    let keyset_infos = client.get_mint_keysets().await?.keysets;
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
        let currencies = currencies.into_iter().collect();
        return Err(Error::NoDebitCurrencyInMint(currencies));
    }
    let debit_unit = debit_unit.unwrap();

    let network = APP_STATE.with_borrow(|state| state.network);
    let mnemonic = bip39::Mnemonic::parse_in_normalized(bip39::Language::English, mnemonic.trim())?;
    let master_xpriv = bitcoin::bip32::Xpriv::new_master(network, &mnemonic.to_seed(""))?;
    let wallet_id = build_wallet_id(&mint_id, &master_xpriv);
    let (txdb, pocketdbs) =
        db::build_wallet_dbs(AppState::DB_VERSION, &wallet_id, debit_unit, credit_unit).await?;
    let wallet = build_wallet(
        name.clone(),
        mint_url.clone(),
        master_xpriv,
        client,
        txdb,
        pocketdbs,
        (debit_unit.clone(), credit_unit.cloned()),
    )?;
    let wallet_cfg = WalletConfig {
        wallet_id: wallet_id.clone(),
        name,
        network,
        mint: mint_url.clone(),
        master: master_xpriv,
        debit: debit_unit.clone(),
        credit: credit_unit.cloned(),
    };
    let purse = get_purse()?;
    purse.store_wallet(wallet_cfg).await?;

    let idx = APP_STATE.with_borrow_mut(|state| {
        state.wallets.push(Rc::new(wallet));
        state.wallets.len() - 1
    });
    Ok(idx)
}

pub fn wallet_name(idx: usize) -> Result<String> {
    tracing::debug!("name for wallet {idx}");

    let wallet = get_wallet(idx)?;
    Ok(wallet.name.clone())
}

pub fn wallet_mint_url(idx: usize) -> Result<String> {
    tracing::debug!("mint_url for wallet {idx}");
    let wallet = get_wallet(idx)?;
    Ok(wallet.url.to_string())
}

pub struct WalletCurrencyUnit {
    pub credit: String,
    pub debit: String,
}

pub fn wallet_currency_unit(idx: usize) -> Result<WalletCurrencyUnit> {
    tracing::debug!("wallet_currency_unit({idx})");
    let wallet = get_wallet(idx)?;
    Ok(WalletCurrencyUnit {
        credit: wallet.credit.unit().to_string(),
        debit: wallet.debit.unit().to_string(),
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

pub async fn wallet_prepare_send(idx: usize, amount: u64, unit: String) -> Result<SendSummary> {
    tracing::debug!("wallet_prepare_send({idx}, {amount}, {unit})");

    let amount = cashu::Amount::from(amount);
    let unit = if unit.is_empty() {
        None
    } else {
        Some(cashu::CurrencyUnit::from_str(&unit)?)
    };
    let wallet = get_wallet(idx)?;
    let summary = wallet.prepare_send(amount, unit).await?;
    Ok(SendSummary::from(summary))
}

pub async fn wallet_send(
    idx: usize,
    request_id: String,
    memo: Option<String>,
    tstamp: u64,
) -> Result<(Token, TransactionId)> {
    tracing::debug!("wallet_send({idx}, {request_id}, {:?}, {tstamp})", memo);

    let rid = uuid::Uuid::from_str(&request_id)?;
    let wallet = get_wallet(idx)?;
    let (token, tx_id) = wallet.send(rid, memo, tstamp).await?;
    Ok((token, tx_id))
}

pub async fn wallet_reclaim_funds(idx: usize) -> Result<WalletBalance> {
    tracing::debug!("wallet_reclaim_funds({idx})");

    let wallet = get_wallet(idx)?;
    let balance = wallet.reclaim_funds().await?;
    Ok(balance)
}

pub async fn wallet_redeem_credit(idx: usize) -> Result<cashu::Amount> {
    tracing::debug!("wallet_redeem_credit({idx})");

    let wallet = get_wallet(idx)?;
    let amount_redeemed = wallet.redeem_credit().await?;
    Ok(amount_redeemed)
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

pub fn wallets_ids() -> Result<Vec<u64>> {
    tracing::debug!("get_wallet_ids");
    let ids = APP_STATE.with_borrow(|state| {
        state
            .wallets
            .iter()
            .enumerate()
            .map(|(i, _)| i as u64)
            .collect::<Vec<_>>()
    });
    Ok(ids)
}

pub fn wallets_names() -> Result<Vec<String>> {
    tracing::debug!("get_wallet_names");
    let names = APP_STATE.with_borrow(|state| {
        state
            .wallets
            .iter()
            .map(|w| w.name.clone())
            .collect::<Vec<_>>()
    });
    Ok(names)
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

#[cfg(target_arch = "wasm32")]
mod db {
    use super::*;

    pub async fn build_appstate_db(db_version: u32) -> Result<prod::ProductionPurseRepository> {
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
    ) -> Result<(
        prod::ProductionTransactionRepository,
        (
            prod::ProductionPocketRepository,
            Option<prod::ProductionPocketRepository>,
        ),
    )> {
        let rexie_db_name = format!("bitcredit_wallet_{wallet_id}");
        let transaction_stores = prod::ProductionTransactionRepository::object_stores(wallet_id);
        let mut rexie_builder = rexie::Rexie::builder(&rexie_db_name).version(db_version);
        for store in transaction_stores {
            rexie_builder = rexie_builder.add_object_store(store);
        }
        let stores = prod::ProductionPocketRepository::object_stores(debit);
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
        let debitdb = prod::ProductionPocketRepository::new(rexiedb.clone(), debit.clone())?;
        let creditdb = if let Some(unit) = credit {
            Some(prod::ProductionPocketRepository::new(
                rexiedb.clone(),
                unit.clone(),
            )?)
        } else {
            None
        };
        Ok((tx_repo, (debitdb, creditdb)))
    }
}
#[cfg(not(target_arch = "wasm32"))]
mod db {
    use super::*;

    pub async fn build_appstate_db(_db_version: u32) -> Result<prod::ProductionPurseRepository> {
        Ok(prod::ProductionPurseRepository::default())
    }
    pub async fn build_wallet_dbs(
        _db_version: u32,
        _wallet_id: &str,
        _debit: &CurrencyUnit,
        credit: Option<&CurrencyUnit>,
    ) -> Result<(
        prod::ProductionTransactionRepository,
        (
            prod::ProductionPocketRepository,
            Option<prod::ProductionPocketRepository>,
        ),
    )> {
        let txdb = prod::ProductionTransactionRepository::default();
        let debitdb = prod::ProductionPocketRepository::default();
        let creditdb = if credit.is_some() {
            Some(prod::ProductionPocketRepository::default())
        } else {
            None
        };
        Ok((txdb, (debitdb, creditdb)))
    }
}

fn build_wallet(
    name: String,
    mint_url: cashu::MintUrl,
    master: btc32::Xpriv,
    client: ProductionConnector,
    tx_repo: prod::ProductionTransactionRepository,
    (debitdb, creditdb): (
        prod::ProductionPocketRepository,
        Option<prod::ProductionPocketRepository>,
    ),
    (debit, credit): (CurrencyUnit, Option<CurrencyUnit>),
) -> Result<ProductionWallet> {
    // building the debit pocket
    let debit_pocket = ProductionDebitPocket::new(debit.clone(), debitdb, master);
    // building the credit pocket
    let credit_pocket: Box<dyn CreditPocket> = if let Some(unit) = credit {
        let creditdb = creditdb.expect("Credit pocket DB should be present");
        let pocket = ProductionCreditPocket::new(unit.clone(), creditdb, master);
        Box::new(pocket)
    } else {
        tracing::warn!("app::add_wallet: credit_pocket = DummyPocket");
        Box::new(crate::pocket::DummyPocket {})
    };
    let new_wallet: ProductionWallet = ProductionWallet {
        client,
        url: mint_url,
        tx_repo,
        debit: debit_pocket,
        credit: credit_pocket,
        name,
        current_send: Mutex::new(None),
    };
    Ok(new_wallet)
}
