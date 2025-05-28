// ----- standard library imports
// ----- extra library imports
use cashu::{CurrencyUnit, MintUrl};
// ----- local modules
use super::connector::{Connector, MintConnector};
use crate::db::WalletDatabase;
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
    pub seed: [u8; 32],
}
