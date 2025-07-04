// ----- standard library imports
use std::marker::PhantomData;
// ----- extra library imports
use bitcoin::bip32::Xpriv;
use cdk::wallet::HttpClient;
// ----- local modules
use crate::db::WalletDatabase;
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
pub fn new_credit<DB: WalletDatabase>() -> WalletBuilder<Unconfigured, CreditWallet, DB> {
    WalletBuilder::<Unconfigured, CreditWallet, DB> {
        mint_url: None,
        unit: None,
        // unit: Some(cashu::CurrencyUnit::Custom("crsat".into())),
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
            database: self.database,
            seed: self.seed,
            unit: self.unit,
        }
    }
}

impl<T: WalletType, DB: WalletDatabase> WalletBuilder<Unconfigured, T, DB> {
    pub fn set_unit(self, unit: cashu::CurrencyUnit) -> WalletBuilder<UnitSet, T, DB> {
        WalletBuilder {
            unit: Some(unit),
            _marker: PhantomData,
            seed: self.seed,
            database: self.database,
            mint_url: self.mint_url,
        }
    }
}

impl<T: WalletType, DB: WalletDatabase> WalletBuilder<MintSet, T, DB> {
    pub fn set_database(self, db: DB) -> WalletBuilder<DatabaseSet, T, DB> {
        WalletBuilder {
            database: Some(db),
            _marker: PhantomData,
            unit: self.unit,
            mint_url: self.mint_url,
            seed: self.seed,
        }
    }
}

impl<T: WalletType, DB: WalletDatabase> WalletBuilder<DatabaseSet, T, DB> {
    pub fn set_seed(self, seed: [u8; 64]) -> WalletBuilder<SeedSet, T, DB> {
        WalletBuilder {
            seed: Some(seed),
            _marker: PhantomData,
            mint_url: self.mint_url,
            database: self.database,
            unit: self.unit,
        }
    }
}

impl<T, DB> WalletBuilder<SeedSet, T, DB>
where
    T: WalletType,
    DB: WalletDatabase,
{
    pub fn build(self) -> Wallet<T, DB, HttpClient> {
        let xpriv =
            Xpriv::new_master(bitcoin::Network::Bitcoin, self.seed.unwrap().as_ref()).unwrap();
        let mint_url = self.mint_url.unwrap();
        Wallet {
            xpriv,
            mint_url: mint_url.clone(),
            unit: self.unit.unwrap(),
            connector: HttpClient::new(mint_url),
            db: self.database.unwrap(),
            _phantom: std::marker::PhantomData,
        }
    }
}
