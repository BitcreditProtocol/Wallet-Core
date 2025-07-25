// ----- standard library imports
use std::{cell::RefCell, collections::HashSet, rc::Rc, str::FromStr, sync::Mutex};
// ----- extra library imports
use anyhow::Error as AnyError;
use bcr_wallet_lib::wallet::Token;
use bitcoin::{
    hashes::{Hash, HashEngine, sha256},
    hex::DisplayHex,
};
use cdk::wallet::{
    MintConnector,
    types::{Transaction, TransactionId},
};
// ----- local imports
use crate::{
    SendSummary,
    error::{Error, Result},
    persistence::{self, rexie::PocketDB},
    wallet::{CreditPocket, DebitPocket, WalletBalance},
};

// ----- end imports

type ProductionConnector = cdk::wallet::HttpClient;
type ProductionTransactionRepository = crate::persistence::rexie::TransactionDB;
type ProductionPocketRepository = crate::persistence::rexie::PocketDB;
type ProductionDebitPocket = crate::pocket::DbPocket<ProductionPocketRepository>;
type ProductionCreditPocket = crate::pocket::CrPocket<ProductionPocketRepository>;
type ProductionWallet = crate::wallet::Wallet<ProductionConnector, ProductionTransactionRepository>;

pub struct AppState {
    wallets: Vec<Rc<ProductionWallet>>,
    network: bitcoin::NetworkKind,
}
impl AppState {
    pub fn new(network: bitcoin::NetworkKind) -> Self {
        Self {
            wallets: Vec::new(),
            network,
        }
    }
}
impl Default for AppState {
    fn default() -> Self {
        Self::new(bitcoin::NetworkKind::Main)
    }
}

thread_local! {
static APP_STATE: RefCell<AppState> = RefCell::new(AppState::default());
}

pub fn initialize_api(network: String) {
    tracing::debug!("Initializing API with network: {:?}", network);
    let net = match network.as_str() {
        "main" => bitcoin::NetworkKind::Main,
        "test" => bitcoin::NetworkKind::Test,
        _ => panic!("Unknown network: {network}"),
    };
    APP_STATE.replace(AppState::new(net));
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

/// returns the index of the wallet
pub async fn add_wallet(name: String, mint_url: String, mnemonic: String) -> Result<usize> {
    tracing::debug!("Adding a new wallet for mint {name}, {mint_url}, {mnemonic}");
    let network = APP_STATE.with_borrow(|state| state.network);
    // Validation
    let mnemonic = bip39::Mnemonic::parse_in_normalized(bip39::Language::English, mnemonic.trim())?;
    let master_xpriv = bitcoin::bip32::Xpriv::new_master(network, &mnemonic.to_seed(""))?;

    let mint_url = cashu::MintUrl::from_str(&mint_url)?;
    let client = ProductionConnector::new(mint_url.clone());

    // building a unique identifier for the local DB
    let mut hasher = sha256::HashEngine::default();
    hasher.input(mnemonic.to_entropy().as_slice());

    let info = client.get_mint_info().await?;
    if let Some(pubkey) = info.pubkey {
        hasher.input(&pubkey.to_bytes());
    } else if let Some(name) = info.name {
        hasher.input(name.as_bytes());
    } else {
        hasher.input(mint_url.to_string().as_bytes());
    }

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

    // building database and object_stores
    let db_id = sha256::Hash::from_engine(hasher)
        .as_byte_array()
        .as_hex()
        .to_string();
    let rexie_db_name = format!("bitcredit_wallet_{db_id}");
    let transaction_stores = ProductionTransactionRepository::object_stores(&db_id);
    let mut rexie_builder = rexie::Rexie::builder(&rexie_db_name);
    for store in transaction_stores {
        rexie_builder = rexie_builder.add_object_store(store);
    }
    let credit_unit = currencies
        .iter()
        .find(|unit| unit.to_string().starts_with("cr"));
    if let Some(unit) = credit_unit {
        let stores = PocketDB::object_stores(unit);
        for store in stores {
            rexie_builder = rexie_builder.add_object_store(store);
        }
    }
    let debit_unit = currencies
        .iter()
        .find(|unit| !unit.to_string().starts_with("cr"));
    if let Some(unit) = debit_unit {
        let stores = PocketDB::object_stores(unit);
        for store in stores {
            rexie_builder = rexie_builder.add_object_store(store);
        }
    }
    let rexie = Rc::new(rexie_builder.build().await?);
    tracing::debug!("Rexie DB created: {}", rexie.name());

    // building the credit pocket
    let credit_pocket: Box<dyn CreditPocket> = if let Some(unit) = credit_unit {
        let db = persistence::rexie::PocketDB::new(rexie.clone(), unit.clone())?;
        let pocket = ProductionCreditPocket::new(unit.clone(), db, master_xpriv);
        Box::new(pocket)
    } else {
        tracing::warn!("app::add_wallet: credit_pocket = DummyPocket");
        Box::new(crate::pocket::DummyPocket {})
    };
    // building the debit pocket
    let debit_pocket: Box<dyn DebitPocket> = if let Some(unit) = debit_unit {
        let db = persistence::rexie::PocketDB::new(rexie.clone(), unit.clone())?;
        let pocket = ProductionDebitPocket::new(unit.clone(), db, master_xpriv);
        Box::new(pocket)
    } else {
        tracing::warn!("app::add_wallet: debit_pocket = DummyPocket");
        Box::new(crate::pocket::DummyPocket {})
    };

    let tx_repo = ProductionTransactionRepository::new(rexie.clone(), &db_id)?;

    let new_wallet: ProductionWallet = ProductionWallet {
        client,
        url: mint_url,
        tx_repo,
        debit: debit_pocket,
        credit: credit_pocket,
        mnemonic,
        name,
        current_send: Mutex::new(None),
    };
    let index = APP_STATE.with_borrow_mut(|state| {
        state.wallets.push(Rc::new(new_wallet));
        state.wallets.len() - 1
    });
    Ok(index)
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
