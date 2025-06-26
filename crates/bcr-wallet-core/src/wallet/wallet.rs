// ----- standard library imports
// ----- extra library imports
use cashu::{CurrencyUnit, MintUrl};
// ----- local imports

// ----- end imports

pub trait WalletType {}
pub struct CreditWallet {}
pub struct DebitWallet {}

impl WalletType for CreditWallet {}
impl WalletType for DebitWallet {}

pub struct Wallet<T, DB, C> {
    pub mint_url: MintUrl,
    pub connector: C,
    pub unit: CurrencyUnit,
    pub db: DB,
    pub xpriv: bitcoin::bip32::Xpriv,
    pub _phantom: std::marker::PhantomData<T>,
}
