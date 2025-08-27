// ----- standard library imports
use std::{collections::HashMap, str::FromStr};
// ----- extra library imports
use cashu::{Amount, CurrencyUnit, MintUrl};
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
    pub unit: CurrencyUnit,
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

#[derive(strum::EnumString, strum::Display)]
pub enum PaymentType {
    NotApplicable,
    Token,
    Cdk18,
    Lightning,
}

pub struct PaymentSummary {
    pub request_id: Uuid,
    pub unit: CurrencyUnit,
    pub amount: Amount,
    pub fees: Amount,
    pub reserved_fees: Amount,
    pub expiry: u64,
    pub ptype: PaymentType,
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
pub fn get_payment_type(metas: &HashMap<String, String>) -> PaymentType {
    let Some(ptype) = metas.get(PAYMENT_TYPE_METADATA_KEY) else {
        return PaymentType::NotApplicable;
    };
    PaymentType::from_str(ptype).unwrap_or(PaymentType::NotApplicable)
}

impl std::convert::From<SendSummary> for PaymentSummary {
    fn from(value: SendSummary) -> Self {
        Self {
            request_id: value.request_id,
            unit: value.unit,
            amount: value.amount,
            fees: value.send_fees + value.swap_fees,
            reserved_fees: Amount::ZERO,
            expiry: 0,
            ptype: PaymentType::Token,
        }
    }
}

impl std::convert::From<MeltSummary> for PaymentSummary {
    fn from(value: MeltSummary) -> Self {
        Self {
            request_id: value.request_id,
            unit: value.unit,
            amount: value.amount,
            fees: value.fees,
            reserved_fees: value.reserved_fees,
            expiry: value.expiry,
            ptype: PaymentType::Lightning,
        }
    }
}
