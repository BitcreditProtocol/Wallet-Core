// ----- standard library imports
// ----- extra library imports
use cashu::{Amount, CurrencyUnit};
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
