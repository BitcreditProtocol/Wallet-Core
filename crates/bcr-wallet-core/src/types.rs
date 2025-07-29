// ----- standard library imports
// ----- extra library imports
use bitcoin::bip32 as btc32;
use cashu::{Amount, CurrencyUnit, MintUrl};
use uuid::Uuid;
// ----- local imports

// ----- end imports

#[derive(Default)]
pub struct SendSummary {
    pub request_id: Uuid,
    pub unit: CurrencyUnit,
    pub swap_fees: Amount,
    pub send_fees: Amount,
}

#[derive(Clone)]
pub struct WalletSendSummary {
    pub request_id: Uuid,
    pub amount: Amount,
    pub unit: CurrencyUnit,
    pub internal_rid: Uuid,
}

#[derive(Default, Clone)]
pub struct PocketSendSummary {
    pub request_id: Uuid,
    pub swap_fees: Amount,
    pub send_fees: Amount,
}
impl PocketSendSummary {
    pub fn new() -> Self {
        Self {
            request_id: Uuid::new_v4(),
            ..Default::default()
        }
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct WalletConfig {
    pub wallet_id: String,
    pub name: String,
    pub network: bitcoin::Network,

    pub mint: MintUrl,
    pub debit: CurrencyUnit,
    pub credit: Option<CurrencyUnit>,
    pub master: btc32::Xpriv,
}
