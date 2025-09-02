// ----- standard library imports
use std::{collections::HashMap, str::FromStr};
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
    pub amount: Amount,
    pub unit: CurrencyUnit,
    pub swap_fees: Amount,
    pub send_fees: Amount,
}
impl SendSummary {
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
pub struct MeltSummary {
    pub request_id: Uuid,
    pub amount: Amount,
    pub fees: Amount,
    pub reserved_fees: Amount,
    pub expiry: u64,
}

impl MeltSummary {
    pub fn new() -> Self {
        Self {
            request_id: Uuid::new_v4(),
            ..Default::default()
        }
    }
}

#[derive(strum::EnumDiscriminants)]
#[strum_discriminants(derive(strum::EnumString, strum::Display))]
pub enum PaymentType {
    NotApplicable,
    Token,
    Cdk18(cdk18::PaymentRequest),
    Bolt11(cashu::Bolt11Invoice),
}
impl PaymentType {
    pub fn memo(&self) -> Option<String> {
        match self {
            PaymentType::Token => None,
            PaymentType::NotApplicable => None,
            PaymentType::Cdk18(req) => req.description.clone(),
            PaymentType::Bolt11(invoice) => Some(invoice.description().to_string()),
        }
    }
}

pub struct PaymentSummary {
    pub request_id: Uuid,
    pub unit: CurrencyUnit,
    pub amount: Amount,
    pub fees: Amount,
    pub reserved_fees: Amount,
    pub expiry: u64,
    pub internal_rid: Uuid,
    pub details: PaymentType,
}

#[derive(strum::Display, strum::EnumString, Default)]
pub enum TransactionStatus {
    #[default]
    NotApplicable,
    Pending,
    CashedIn,
    Canceled,
}
pub const TRANSACTION_STATUS_METADATA_KEY: &str = "transaction_status";
pub fn get_transaction_status(metas: &HashMap<String, String>) -> TransactionStatus {
    let Some(status) = metas.get(TRANSACTION_STATUS_METADATA_KEY) else {
        return TransactionStatus::default();
    };
    TransactionStatus::from_str(status).unwrap_or_default()
}

pub const PAYMENT_TYPE_METADATA_KEY: &str = "payment_type";
pub fn get_payment_type(metas: &HashMap<String, String>) -> PaymentTypeDiscriminants {
    let Some(ptype) = metas.get(PAYMENT_TYPE_METADATA_KEY) else {
        return PaymentTypeDiscriminants::NotApplicable;
    };
    PaymentTypeDiscriminants::from_str(ptype).unwrap_or(PaymentTypeDiscriminants::NotApplicable)
}
