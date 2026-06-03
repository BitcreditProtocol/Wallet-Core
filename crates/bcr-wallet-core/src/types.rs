use bcr_common::{
    cashu::{self, Amount, CurrencyUnit, KeySetInfo},
    cdk_common::wallet::{Transaction, TransactionDirection, TransactionId},
    core::NodeId,
};
use bitcoin::{address::NetworkUnchecked, secp256k1};
use chrono::{DateTime, Datelike, Utc};
use nostr::RelayUrl;
use std::{
    collections::{BTreeMap, HashMap},
    str::FromStr,
    sync::Arc,
};
use uuid::Uuid;

use crate::event::ContactPaymentRequestPayload;

pub type Seed = [u8; 64];

pub type PaymentResultCallback = Arc<dyn Fn(Option<TransactionId>) + Send + Sync + 'static>;
pub type PendingPaymentSubscriptionCallback = Arc<dyn Fn(Uuid) + Send + Sync + 'static>;

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

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum PaymentRequestState {
    Pending,
    Paid { tx_id: TransactionId },
    Canceled,
    Rejected,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum PaymentRequestDirection {
    Incoming,
    Outgoing,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct PaymentRequest {
    pub id: Uuid,
    pub node_id: NodeId,
    pub amount: Amount,
    pub unit: CurrencyUnit,
    pub description: Option<String>,
    pub deadline: Option<u64>,
    pub created_at: u64,
    pub state: PaymentRequestState,
    pub direction: PaymentRequestDirection,
}

impl PaymentRequest {
    pub fn new_incoming(
        node_id: NodeId,
        amount: Amount,
        unit: CurrencyUnit,
        description: Option<String>,
        deadline: Option<u64>,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            node_id,
            amount,
            unit,
            description,
            deadline,
            created_at: Utc::now().timestamp() as u64,
            state: PaymentRequestState::Pending,
            direction: PaymentRequestDirection::Incoming,
        }
    }

    pub fn new_outgoing(
        node_id: NodeId,
        amount: Amount,
        unit: CurrencyUnit,
        description: Option<String>,
        deadline: Option<u64>,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            node_id,
            amount,
            unit,
            description,
            deadline,
            created_at: Utc::now().timestamp() as u64,
            state: PaymentRequestState::Pending,
            direction: PaymentRequestDirection::Outgoing,
        }
    }
}

impl From<ContactPaymentRequestPayload> for PaymentRequest {
    fn from(value: ContactPaymentRequestPayload) -> Self {
        Self {
            id: value.id,
            node_id: value.sender,
            amount: value.amount,
            unit: value.unit,
            description: value.memo,
            deadline: value.deadline,
            created_at: value.created_at,
            state: PaymentRequestState::Pending,
            direction: PaymentRequestDirection::Incoming,
        }
    }
}

#[derive(strum::EnumString, strum::Display, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PaymentType {
    #[default]
    NotApplicable,
    Token,
    Cdk18,
    OnChain,
    Swap,
    Contact,
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

pub const BTC_TX_ID_TYPE_METADATA_KEY: &str = "btc_tx_id";
pub fn get_btc_tx_id(metas: &HashMap<String, String>) -> Option<bitcoin::Txid> {
    let tx_id = metas.get(BTC_TX_ID_TYPE_METADATA_KEY)?;
    bitcoin::Txid::from_str(tx_id).ok()
}

pub const CONTACT_NODE_ID_METADATA_KEY: &str = "contact_node_id";
pub fn get_contact_node_id(metas: &HashMap<String, String>) -> Option<NodeId> {
    let node_id = metas.get(CONTACT_NODE_ID_METADATA_KEY)?;
    NodeId::from_str(node_id).ok()
}

pub const PAYMENT_REQUEST_ID_METADATA_KEY: &str = "payment_request_id";
pub fn get_payment_request_id(metas: &HashMap<String, String>) -> Option<Uuid> {
    let id = metas.get(PAYMENT_REQUEST_ID_METADATA_KEY)?;
    Uuid::from_str(id).ok()
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

#[derive(Debug, Clone)]
pub struct ListTransactionsResult {
    pub txs: Vec<Transaction>,
    pub next_cursor: Option<TransactionCursor>,
    pub fees_by_month: Vec<FeesByMonth>,
}

#[derive(Debug, Clone)]
pub struct FeesByMonth {
    pub year: i32,
    pub month: u32,
    pub fees: Amount,
}

// Sums up fees per month for a given set of transactions
pub fn extract_fees_per_month(transactions: &[Transaction]) -> Vec<FeesByMonth> {
    let mut fees_by_month: BTreeMap<(i32, u32), Amount> = BTreeMap::new();

    for tx in transactions {
        let Some(dt) = DateTime::<Utc>::from_timestamp(tx.timestamp as i64, 0) else {
            continue;
        };

        let year = dt.year();
        let month = dt.month();

        fees_by_month
            .entry((year, month))
            .and_modify(|fees| *fees += tx.fee)
            .or_insert(tx.fee);
    }

    fees_by_month
        .into_iter()
        .rev()
        .map(|((year, month), fees)| FeesByMonth { year, month, fees })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn ts(year: i32, month: u32, day: u32, hour: u32, min: u32, sec: u32) -> u64 {
        Utc.with_ymd_and_hms(year, month, day, hour, min, sec)
            .unwrap()
            .timestamp() as u64
    }

    fn tx(timestamp: u64, fee: Amount) -> Transaction {
        Transaction {
            mint_url: cashu::MintUrl::from_str("https://mint.example").unwrap(),
            direction: TransactionDirection::Incoming,
            amount: Amount::from(0),
            fee,
            unit: CurrencyUnit::Sat,
            ys: vec![],
            timestamp,
            memo: None,
            metadata: HashMap::new(),
            quote_id: None,
            payment_request: None,
            payment_proof: None,
            payment_method: None,
            saga_id: None,
        }
    }

    #[test]
    fn test_empty_vec() {
        let result = extract_fees_per_month(&[]);
        assert!(result.is_empty());
    }

    #[test]
    fn test_fees_by_month() {
        let transactions = vec![
            tx(ts(2025, 2, 1, 12, 0, 0), Amount::from(10)),
            tx(ts(2025, 2, 15, 12, 0, 0), Amount::from(20)),
            tx(ts(2025, 3, 1, 12, 0, 0), Amount::from(5)),
        ];
        let result = extract_fees_per_month(&transactions);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].year, 2025);
        assert_eq!(result[0].month, 3);
        assert_eq!(result[0].fees, Amount::from(5));
        assert_eq!(result[1].year, 2025);
        assert_eq!(result[1].month, 2);
        assert_eq!(result[1].fees, Amount::from(30));
    }

    #[test]
    fn sorts_by_year_and_month_descending() {
        let transactions = vec![
            tx(ts(2024, 12, 15, 12, 0, 0), Amount::from(1)),
            tx(ts(2025, 1, 15, 12, 0, 0), Amount::from(2)),
            tx(ts(2025, 3, 15, 12, 0, 0), Amount::from(3)),
            tx(ts(2025, 2, 15, 12, 0, 0), Amount::from(4)),
        ];
        let result = extract_fees_per_month(&transactions);
        let year_months: Vec<(i32, u32)> =
            result.iter().map(|item| (item.year, item.month)).collect();
        assert_eq!(
            year_months,
            vec![(2025, 3), (2025, 2), (2025, 1), (2024, 12),]
        );
    }
}
