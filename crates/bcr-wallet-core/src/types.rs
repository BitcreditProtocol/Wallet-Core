// ----- standard library imports
// ----- extra library imports
use cashu::{Amount, CurrencyUnit, MintUrl, nut18 as cdk18};
use uuid::Uuid;
// ----- local imports

// ----- end imports

pub struct RedemptionSummary {
    pub tstamp: u64,
    pub amount: Amount,
}

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
    pub amount: Amount,
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
    pub mnemonic: bip39::Mnemonic,
}

#[derive(Default, Clone)]
pub struct PocketMeltSummary {
    pub request_id: Uuid,
    pub amount: Amount,
    pub fees: Amount,
    pub reserved_fees: Amount,
    pub expiry: u64,
}

impl PocketMeltSummary {
    pub fn new() -> Self {
        Self {
            request_id: Uuid::new_v4(),
            ..Default::default()
        }
    }
}

pub enum PaymentType {
    Cdk18(cdk18::PaymentRequest),
    Bolt11(cashu::Bolt11Invoice),
}
impl PaymentType {
    pub fn memo(&self) -> Option<String> {
        match self {
            PaymentType::Cdk18(req) => req.description.clone(),
            PaymentType::Bolt11(invoice) => Some(invoice.description().to_string()),
        }
    }
}

pub struct WalletPaymentSummary {
    pub request_id: Uuid,
    pub unit: CurrencyUnit,
    pub amount: Amount,
    pub fees: Amount,
    pub reserved_fees: Amount,
    pub expiry: u64,
    pub internal_rid: Uuid,
    pub details: PaymentType,
}
