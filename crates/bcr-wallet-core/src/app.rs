// ----- standard library imports
use std::{cell::RefCell, collections::HashSet, rc::Rc, str::FromStr};
// ----- extra library imports
use anyhow::Error as AnyError;
use bitcoin::hashes::{Hash, sha1};
use cdk::wallet::MintConnector;
// ----- local imports
use crate::{
    error::{Error, Result},
    persistence::{self, rexie::ProofDB},
    wallet::{CreditPocket, DebitPocket, WalletBalance},
};

// ----- end imports

type ProductionConnector = cdk::wallet::HttpClient;
type ProductionPocketRepository = crate::persistence::rexie::ProofDB;
type ProductionDebitPocket = crate::pocket::DbPocket<ProductionPocketRepository>;
type ProductionCreditPocket = crate::pocket::CrPocket<ProductionPocketRepository>;
type ProductionWallet = crate::wallet::Wallet<ProductionConnector>;

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

pub fn initialize_api(net: bitcoin::NetworkKind) {
    tracing::debug!("Initializing API with network: {:?}", net);
    APP_STATE.replace(AppState::new(net));
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
    let info = client.get_mint_info().await?;

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

    // building a unique identifier of the mint to name the local DB
    let mint_id = if let Some(pubkey) = info.pubkey {
        sha1::Hash::hash(&pubkey.to_bytes())
    } else if let Some(name) = info.name {
        sha1::Hash::hash(name.as_bytes())
    } else {
        sha1::Hash::hash(mint_url.to_string().as_bytes())
    };
    // building database and object_stores
    let mut rexie_builder = rexie::Rexie::builder(&format!("bitcredit_wallet_{mint_id}"));
    let credit_unit = currencies
        .iter()
        .find(|unit| unit.to_string().starts_with("cr"));
    if let Some(unit) = credit_unit {
        let stores = ProofDB::object_stores(unit);
        for store in stores {
            rexie_builder = rexie_builder.add_object_store(store);
        }
    }
    let debit_unit = currencies
        .iter()
        .find(|unit| !unit.to_string().starts_with("cr"));
    if let Some(unit) = debit_unit {
        let stores = ProofDB::object_stores(unit);
        for store in stores {
            rexie_builder = rexie_builder.add_object_store(store);
        }
    }
    let rexie = Rc::new(rexie_builder.build().await?);

    // building the credit pocket
    let credit_pocket: Box<dyn CreditPocket> = if let Some(unit) = credit_unit {
        let db = persistence::rexie::ProofDB::new(rexie.clone(), unit.clone())?;
        let pocket = ProductionCreditPocket {
            unit: unit.clone(),
            db,
            xpriv: master_xpriv,
        };
        Box::new(pocket)
    } else {
        Box::new(crate::pocket::DummyPocket {})
    };
    // building the debit pocket
    let debit_pocket: Box<dyn DebitPocket> = if let Some(unit) = debit_unit {
        let db = persistence::rexie::ProofDB::new(rexie.clone(), unit.clone())?;
        let pocket = ProductionDebitPocket {
            unit: unit.clone(),
            db,
            xpriv: master_xpriv,
        };
        Box::new(pocket)
    } else {
        Box::new(crate::pocket::DummyPocket {})
    };

    let new_wallet: ProductionWallet = ProductionWallet {
        client,
        url: mint_url,
        debit: debit_pocket,
        credit: credit_pocket,
        mnemonic,
        name,
    };
    let index = APP_STATE.with_borrow_mut(|state| {
        state.wallets.push(Rc::new(new_wallet));
        state.wallets.len() - 1
    });
    Ok(index)
}

pub fn wallet_name(idx: usize) -> Result<String> {
    tracing::debug!("name for wallet {idx}");
    let wallet: Rc<ProductionWallet> =
        APP_STATE.with_borrow(|state| -> Result<Rc<ProductionWallet>> {
            let wallet = state.wallets.get(idx).ok_or(Error::WalletNotFound(idx))?;
            Ok(wallet.clone())
        })?;
    Ok(wallet.name.clone())
}

pub fn wallet_mint_url(idx: usize) -> Result<String> {
    tracing::debug!("mint_url for wallet {idx}");
    let wallet: Rc<ProductionWallet> =
        APP_STATE.with_borrow(|state| -> Result<Rc<ProductionWallet>> {
            let wallet = state.wallets.get(idx).ok_or(Error::WalletNotFound(idx))?;
            Ok(wallet.clone())
        })?;
    Ok(wallet.url.to_string())
}

pub async fn wallet_balance(idx: usize) -> Result<WalletBalance> {
    tracing::debug!("balance for wallet {}", idx);
    let wallet: Rc<ProductionWallet> =
        APP_STATE.with_borrow(|state| -> Result<Rc<ProductionWallet>> {
            let wallet = state.wallets.get(idx).ok_or(Error::WalletNotFound(idx))?;
            Ok(wallet.clone())
        })?;
    wallet.balance().await
}

pub async fn wallet_receive(token: String, idx: usize) -> Result<cashu::Amount> {
    let token = bcr_wallet_lib::wallet::Token::from_str(&token)?;
    let wallet: Rc<ProductionWallet> =
        APP_STATE.with_borrow(|state| -> Result<Rc<ProductionWallet>> {
            let wallet = state.wallets.get(idx).ok_or(Error::WalletNotFound(idx))?;
            Ok(wallet.clone())
        })?;
    let cashed_in = wallet.receive(token).await?;
    Ok(cashed_in)
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
    tracing::debug!("get_wallet_ids");
    let names = APP_STATE.with_borrow(|state| {
        state
            .wallets
            .iter()
            .map(|w| w.name.clone())
            .collect::<Vec<_>>()
    });
    Ok(names)
}
