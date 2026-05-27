use bcr_common::{
    cashu::{self, Amount, CurrencyUnit, KeySetInfo},
    cdk_common::wallet::{Transaction, TransactionDirection, TransactionId},
};
use bitcoin::{address::NetworkUnchecked, secp256k1};
use nostr_sdk::RelayUrl;
use std::{collections::HashMap, str::FromStr, sync::Arc};
use uuid::Uuid;

pub type Seed = [u8; 64];

pub type PaymentResultCallback = Arc<dyn Fn(Option<TransactionId>) + Send + Sync + 'static>;

#[derive(Default, Debug, Clone)]
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
    pub mint: url::Url,
    pub mint_keyset_infos: HashMap<cashu::Id, KeySetInfo>,
    pub clowder_id: secp256k1::PublicKey,
    pub debit: CurrencyUnit,
    pub pub_key: secp256k1::PublicKey,
    pub betas: Vec<url::Url>,
    pub nostr_relays: Vec<RelayUrl>,
}

#[derive(Default, Debug, Clone)]
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

#[derive(Debug, Clone)]
pub struct MintSummary {
    pub quote_id: Uuid,
    pub amount: bitcoin::Amount,
    pub address: bitcoin::Address<NetworkUnchecked>,
    pub expiry: u64,
}

#[derive(strum::EnumString, strum::Display, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PaymentType {
    #[default]
    NotApplicable,
    Token,
    Cdk18,
    OnChain,
    Swap,
}

#[derive(Debug, Clone)]
pub struct PaymentSummary {
    pub request_id: Uuid,
    pub unit: CurrencyUnit,
    pub amount: Amount,
    pub fees: Amount,
    pub reserved_fees: Amount,
    pub expiry: u64,
    pub ptype: PaymentType,
}

#[derive(strum::Display, strum::EnumString, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TransactionStatus {
    #[default]
    NotApplicable,
    Pending,
    Settled,
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

pub const BTC_ALPHA_TX_ID_TYPE_METADATA_KEY: &str = "btc_alpha_tx_id";
pub fn get_btc_alpha_tx_id(metas: &HashMap<String, String>) -> Option<bitcoin::Txid> {
    let tx_id = metas.get(BTC_ALPHA_TX_ID_TYPE_METADATA_KEY)?;
    bitcoin::Txid::from_str(tx_id).ok()
}

pub const BTC_BETA_TX_ID_TYPE_METADATA_KEY: &str = "btc_beta_tx_id";
pub fn get_btc_beta_tx_id(metas: &HashMap<String, String>) -> Option<bitcoin::Txid> {
    let tx_id = metas.get(BTC_BETA_TX_ID_TYPE_METADATA_KEY)?;
    bitcoin::Txid::from_str(tx_id).ok()
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
            ptype: PaymentType::OnChain,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct TransactionFilters {
    pub payment_types: Vec<PaymentType>,
    pub statuses: Vec<TransactionStatus>,
    pub direction: Option<TransactionDirection>,
    pub time_range: Option<TimeRange>,
}

impl TransactionFilters {
    pub fn matches_tx(&self, tx: &Transaction) -> bool {
        if let Some(range) = self.time_range {
            if let Some(from) = range.from
                && tx.timestamp < from
            {
                return false;
            }

            if let Some(to) = range.to
                && tx.timestamp > to
            {
                return false;
            }
        }

        if let Some(direction) = self.direction
            && tx.direction != direction
        {
            return false;
        }

        let payment_type = get_payment_type(&tx.metadata);
        if !self.payment_types.is_empty() && !self.payment_types.contains(&payment_type) {
            return false;
        }

        let status = get_transaction_status(&tx.metadata);
        if !self.statuses.is_empty() && !self.statuses.contains(&status) {
            return false;
        }

        true
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeRange {
    pub from: Option<u64>,
    pub to: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, strum::EnumDiscriminants)]
#[strum_discriminants(
    name(TransactionSort),
    derive(Default, Hash, serde::Serialize, serde::Deserialize)
)]
pub enum TransactionCursor {
    TimeAsc {
        tstamp: u64,
        id: TransactionId,
    },

    #[strum_discriminants(default)]
    TimeDesc {
        tstamp: u64,
        id: TransactionId,
    },

    AmountAsc {
        amount: Amount,
        id: TransactionId,
    },
    AmountDesc {
        amount: Amount,
        id: TransactionId,
    },
}

impl TransactionCursor {
    pub fn matches_sort(&self, sort: TransactionSort) -> bool {
        TransactionSort::from(self) == sort
    }

    pub fn from_tx(tx: &Transaction, sort: TransactionSort) -> Self {
        match sort {
            TransactionSort::TimeAsc => Self::TimeAsc {
                tstamp: tx.timestamp,
                id: tx.id(),
            },
            TransactionSort::TimeDesc => Self::TimeDesc {
                tstamp: tx.timestamp,
                id: tx.id(),
            },
            TransactionSort::AmountAsc => Self::AmountAsc {
                amount: tx.amount,
                id: tx.id(),
            },
            TransactionSort::AmountDesc => Self::AmountDesc {
                amount: tx.amount,
                id: tx.id(),
            },
        }
    }

    // check if the given transaction is after the given cursor, falling back to comparing transaction ids
    pub fn tx_is_after(&self, tx: &Transaction) -> bool {
        match self {
            Self::TimeAsc { tstamp, id } => {
                tx.timestamp > *tstamp || (tx.timestamp == *tstamp && tx.id() > *id)
            }
            Self::TimeDesc { tstamp, id } => {
                tx.timestamp < *tstamp || (tx.timestamp == *tstamp && tx.id() < *id)
            }
            Self::AmountAsc { amount, id } => {
                tx.amount > *amount || (tx.amount == *amount && tx.id() > *id)
            }
            Self::AmountDesc { amount, id } => {
                tx.amount < *amount || (tx.amount == *amount && tx.id() < *id)
            }
        }
    }
}
