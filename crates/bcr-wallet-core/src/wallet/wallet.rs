// ----- standard library imports
// ----- extra library imports
use cashu::{CurrencyUnit, MintUrl};
use serde::{Deserialize, Serialize};
// ----- local modules
use crate::db::WalletDatabase;
use crate::mint::{Connector, MintConnector};
// ----- end imports

pub trait WalletType {}
pub struct CreditWallet {}
pub struct DebitWallet {}

impl WalletType for CreditWallet {}
impl WalletType for DebitWallet {}

pub struct Wallet<T: WalletType, DB: WalletDatabase>
where
    Connector<T>: MintConnector,
{
    pub mint_url: MintUrl,
    pub connector: Connector<T>,
    pub unit: CurrencyUnit,
    pub db: DB,
    pub xpriv: bitcoin::bip32::Xpriv,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WalletData {
    pub mint_url: MintUrl,
    pub mnemonic: [String; 15],
    pub is_credit: bool,
    pub is_active: bool,
    pub unit: String,
}
