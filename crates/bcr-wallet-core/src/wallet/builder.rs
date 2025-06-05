// ----- standard library imports
use bitcoin::bip32::Xpriv;
use std::marker::PhantomData;

// ----- extra library imports
// ----- local modules
use crate::db::WalletDatabase;
use crate::mint::{Connector, MintConnector};
use crate::wallet::{CreditWallet, DebitWallet, Wallet, WalletType};
// ----- end imports

pub struct Unconfigured;
pub struct MintSet;
pub struct UnitSet;
pub struct DatabaseSet;
pub struct SeedSet;

pub struct WalletBuilder<T, D: WalletType, DB: WalletDatabase> {
    mint_url: Option<cashu::MintUrl>,
    unit: Option<cashu::CurrencyUnit>,
    database: Option<DB>,
    seed: Option<[u8; 64]>,
    _marker: PhantomData<(T, D)>,
}

pub fn new_debit<DB: WalletDatabase>() -> WalletBuilder<Unconfigured, DebitWallet, DB> {
    WalletBuilder::<Unconfigured, DebitWallet, DB> {
        mint_url: None,
        unit: None,
        database: None,
        seed: None,
        _marker: PhantomData,
    }
}
pub fn new_credit<DB: WalletDatabase>() -> WalletBuilder<UnitSet, CreditWallet, DB> {
    WalletBuilder::<UnitSet, CreditWallet, DB> {
        mint_url: None,
        unit: Some(cashu::CurrencyUnit::Custom("crsat".into())),
        database: None,
        seed: None,
        _marker: PhantomData,
    }
}

impl<T: WalletType, DB: WalletDatabase> WalletBuilder<UnitSet, T, DB> {
    pub fn set_mint_url(self, mint_url: cashu::MintUrl) -> WalletBuilder<MintSet, T, DB> {
        WalletBuilder {
            mint_url: Some(mint_url),
            _marker: PhantomData,
            ..self
        }
    }
}

impl<T: WalletType, DB: WalletDatabase> WalletBuilder<Unconfigured, T, DB> {
    pub fn set_unit(self, unit: cashu::CurrencyUnit) -> WalletBuilder<UnitSet, T, DB> {
        WalletBuilder {
            unit: Some(unit),
            _marker: PhantomData,
            ..self
        }
    }
}

impl<T: WalletType, DB: WalletDatabase> WalletBuilder<MintSet, T, DB> {
    pub fn set_database(self, db: DB) -> WalletBuilder<DatabaseSet, T, DB> {
        WalletBuilder {
            database: Some(db),
            _marker: PhantomData,
            ..self
        }
    }
}

impl<T: WalletType, DB: WalletDatabase> WalletBuilder<DatabaseSet, T, DB> {
    pub fn set_seed(self, seed: [u8; 64]) -> WalletBuilder<SeedSet, T, DB> {
        WalletBuilder {
            seed: Some(seed),
            _marker: PhantomData,
            ..self
        }
    }
}

impl<T: WalletType, DB: WalletDatabase> WalletBuilder<SeedSet, T, DB>
where
    Connector<T>: MintConnector,
{
    pub fn build(self) -> Wallet<T, DB> {
        let xpriv =
            Xpriv::new_master(bitcoin::Network::Bitcoin, self.seed.unwrap().as_ref()).unwrap();
        Wallet {
            xpriv,
            mint_url: self.mint_url.clone().unwrap(),
            unit: self.unit.unwrap(),
            connector: Connector::new(self.mint_url.unwrap().to_string()),
            db: self.database.unwrap(),
        }
    }
}
