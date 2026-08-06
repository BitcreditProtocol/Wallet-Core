use bcr_common::{
    cashu::{self, Amount, CurrencyUnit, KeySetInfo, MintUrl},
    cdk_common::wallet::TransactionDirection,
    core::NodeId,
};
use bitcoin::{address::NetworkUnchecked, secp256k1};
use chrono::{DateTime, Datelike, Utc};
use nostr::{RelayUrl, event::EventId};
use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
};
use uuid::Uuid;

use crate::event::ContactPaymentRequestPayload;

pub type Seed = [u8; 64];

pub type PaymentResultCallback = Arc<dyn Fn(Option<Uuid>) + Send + Sync + 'static>;
pub type PendingPaymentSubscriptionCallback = Arc<dyn Fn(Uuid) + Send + Sync + 'static>;

#[derive(Default, Debug, Clone)]
pub struct SendSummary {
    pub request_id: Uuid,
    pub amount: Amount,
    pub unit: CurrencyUnit,
    pub fees: TransactionFees,
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
    pub fees: TransactionFees,
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
    Paid { tx_id: Uuid },
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

/// A transaction in our wallet
#[derive(Debug, Clone)]
pub struct Transaction {
    pub id: Uuid,
    pub mint_url: MintUrl,
    pub ys: Vec<cashu::PublicKey>,
    pub amount: cashu::Amount,
    pub fees: TransactionFees,
    pub unit: CurrencyUnit,
    pub tstamp: u64,
    pub direction: TransactionDirection,
    pub memo: Option<String>,
    pub payment_type: PaymentType,
    pub status: TransactionStatus,
    pub btc_tx_id: Option<bitcoin::Txid>,
    pub quote_id: Option<Uuid>,
    pub nostr_event_id: Option<EventId>,
    pub contact_node_id: Option<NodeId>,
    pub payment_request_id: Option<Uuid>,
    pub linked_txs: Vec<TransactionLink>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct TransactionFees {
    pub swap: cashu::Amount,
    pub network: cashu::Amount,
    pub melt: cashu::Amount,
}

impl TransactionFees {
    pub fn sum(&self) -> cashu::Amount {
        self.swap + self.network + self.melt
    }
}

/// A link to another transaction with a reason, e.g. linking a payment transaction with a reclaim of the payment
#[derive(Debug, Clone)]
pub struct TransactionLink {
    pub tx_id: Uuid,
    pub reason: TransactionLinkReason,
}

#[derive(strum::EnumString, strum::Display, Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionLinkReason {
    Reclaim,
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
    pub fees: TransactionFees,
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

impl std::convert::From<SendSummary> for PaymentSummary {
    fn from(value: SendSummary) -> Self {
        Self {
            request_id: value.request_id,
            unit: value.unit,
            amount: value.amount,
            fees: value.fees,
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
                && tx.tstamp < from
            {
                return false;
            }

            if let Some(to) = range.to
                && tx.tstamp > to
            {
                return false;
            }
        }

        if let Some(direction) = self.direction
            && tx.direction != direction
        {
            return false;
        }

        if !self.payment_types.is_empty() && !self.payment_types.contains(&tx.payment_type) {
            return false;
        }

        if !self.statuses.is_empty() && !self.statuses.contains(&tx.status) {
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
        id: Uuid,
    },

    #[strum_discriminants(default)]
    TimeDesc {
        tstamp: u64,
        id: Uuid,
    },

    AmountAsc {
        amount: Amount,
        id: Uuid,
    },
    AmountDesc {
        amount: Amount,
        id: Uuid,
    },
}

impl TransactionCursor {
    pub fn matches_sort(&self, sort: TransactionSort) -> bool {
        TransactionSort::from(self) == sort
    }

    pub fn from_tx(tx: &Transaction, sort: TransactionSort) -> Self {
        match sort {
            TransactionSort::TimeAsc => Self::TimeAsc {
                tstamp: tx.tstamp,
                id: tx.id,
            },
            TransactionSort::TimeDesc => Self::TimeDesc {
                tstamp: tx.tstamp,
                id: tx.id,
            },
            TransactionSort::AmountAsc => Self::AmountAsc {
                amount: tx.amount,
                id: tx.id,
            },
            TransactionSort::AmountDesc => Self::AmountDesc {
                amount: tx.amount,
                id: tx.id,
            },
        }
    }

    // check if the given transaction is after the given cursor, falling back to comparing transaction ids
    pub fn tx_is_after(&self, tx: &Transaction) -> bool {
        match self {
            Self::TimeAsc { tstamp, id } => {
                tx.tstamp > *tstamp || (tx.tstamp == *tstamp && tx.id > *id)
            }
            Self::TimeDesc { tstamp, id } => {
                tx.tstamp < *tstamp || (tx.tstamp == *tstamp && tx.id < *id)
            }
            Self::AmountAsc { amount, id } => {
                tx.amount > *amount || (tx.amount == *amount && tx.id > *id)
            }
            Self::AmountDesc { amount, id } => {
                tx.amount < *amount || (tx.amount == *amount && tx.id < *id)
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
    pub fees: TransactionFees,
}

// Sums up fees per month for a given set of transactions
pub fn extract_fees_per_month(transactions: &[Transaction]) -> Vec<FeesByMonth> {
    let mut fees_by_month: BTreeMap<(i32, u32), TransactionFees> = BTreeMap::new();

    for tx in transactions {
        let Some(dt) = DateTime::<Utc>::from_timestamp(tx.tstamp as i64, 0) else {
            continue;
        };

        let year = dt.year();
        let month = dt.month();

        fees_by_month
            .entry((year, month))
            .and_modify(|fees| {
                fees.swap += tx.fees.swap;
                fees.network += tx.fees.network;
                fees.melt += tx.fees.melt;
            })
            .or_insert(tx.fees);
    }

    fees_by_month
        .into_iter()
        .rev()
        .map(|((year, month), fees)| FeesByMonth { year, month, fees })
        .collect()
}

#[derive(Debug, Clone)]
pub struct BtcTxStatus {
    pub tx_id: bitcoin::Txid,
    pub bitcoin_network: bitcoin::Network,
    pub receivers: Vec<BtcTxStatusReceiver>,
    pub fee: bitcoin::Amount,
    pub confirmations: u64,
    pub confirmation_tstamp: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct BtcTxStatusReceiver {
    pub address: bitcoin::Address<bitcoin::address::NetworkUnchecked>,
    pub amount: bitcoin::Amount,
}

#[derive(Debug, Clone)]
pub struct MeltEstimation {
    pub tx_vsize: u64,
    pub fee_rates: Vec<MeltFeeRateEstimate>,
    pub melt_fee: u64,
    pub melt_fee_ppk: u64,
}

#[derive(Debug, Clone)]
pub struct MeltFeeRateEstimate {
    pub target_blocks: u16,
    pub sat_per_vb: f32,
}

#[derive(Debug, Clone)]
pub struct ForeignMintProof {
    pub clowder_id: secp256k1::PublicKey,
    pub proof: cashu::Proof,
    pub reason: ForeignMintProofReason,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ForeignMintProofReason {
    MintOffline,
    WalletOffline,
}

#[derive(Debug, Clone)]
pub struct ClowderBeta {
    pub url: url::Url,
    pub clowder_id: secp256k1::PublicKey,
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;
    use chrono::{TimeZone, Utc};

    fn ts(year: i32, month: u32, day: u32, hour: u32, min: u32, sec: u32) -> u64 {
        Utc.with_ymd_and_hms(year, month, day, hour, min, sec)
            .unwrap()
            .timestamp() as u64
    }

    fn tx(timestamp: u64, fee: Amount) -> Transaction {
        Transaction {
            id: Uuid::new_v4(),
            mint_url: cashu::MintUrl::from_str("https://mint.example").unwrap(),
            direction: TransactionDirection::Incoming,
            amount: Amount::from(0),
            fees: TransactionFees {
                swap: fee,
                network: cashu::Amount::ZERO,
                melt: cashu::Amount::ZERO,
            },
            unit: CurrencyUnit::Sat,
            ys: vec![],
            tstamp: timestamp,
            memo: None,
            quote_id: None,
            payment_type: PaymentType::Token,
            status: TransactionStatus::Pending,
            payment_request_id: None,
            btc_tx_id: None,
            nostr_event_id: None,
            contact_node_id: None,
            linked_txs: vec![],
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
        assert_eq!(result[0].fees.swap, Amount::from(5));
        assert_eq!(result[1].year, 2025);
        assert_eq!(result[1].month, 2);
        assert_eq!(result[1].fees.swap, Amount::from(30));
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
