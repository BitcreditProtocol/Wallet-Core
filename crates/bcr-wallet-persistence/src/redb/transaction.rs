use crate::error::Error;
use crate::{TransactionRepository, error::Result};
use async_trait::async_trait;
use bcr_common::cashu::{self, CurrencyUnit, MintUrl};
use bcr_common::cdk_common::wallet::TransactionDirection;
use bcr_common::core::NodeId;
use bcr_common::wire::borsh::{
    deserialize_cashu_amount, deserialize_from_str, deserialize_vec_of_strs, serialize_as_str,
    serialize_cashu_amount, serialize_vec_of_strs,
};
use bcr_wallet_core::types::{PaymentType, Transaction, TransactionStatus};
use borsh::{BorshDeserialize, BorshSerialize};
use nostr::event::EventId;
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition, TableError};
use std::{str::FromStr, sync::Arc};
use tokio::task::spawn_blocking;
use uuid::Uuid;

/// StoredTransaction is a versioned, borsh-serialized transaction
#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub(super) enum StoredTransaction {
    V1(StoredTransactionPayloadV1),
    V2(StoredTransactionPayloadV2),
}

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub struct StoredTransactionPayloadV1 {
    pub id: Uuid,
    #[borsh(
        serialize_with = "serialize_as_str",
        deserialize_with = "deserialize_from_str"
    )]
    pub mint_url: MintUrl,
    #[borsh(
        serialize_with = "serialize_vec_of_strs",
        deserialize_with = "deserialize_vec_of_strs"
    )]
    pub ys: Vec<cashu::PublicKey>,
    #[borsh(
        serialize_with = "serialize_cashu_amount",
        deserialize_with = "deserialize_cashu_amount"
    )]
    pub amount: cashu::Amount,
    #[borsh(
        serialize_with = "serialize_cashu_amount",
        deserialize_with = "deserialize_cashu_amount"
    )]
    pub fees: cashu::Amount,
    #[borsh(
        serialize_with = "serialize_as_str",
        deserialize_with = "deserialize_from_str"
    )]
    pub unit: CurrencyUnit,
    pub tstamp: u64,
    #[borsh(
        serialize_with = "serialize_as_str",
        deserialize_with = "deserialize_from_str"
    )]
    pub direction: TransactionDirection,
    pub memo: Option<String>,
    #[borsh(
        serialize_with = "serialize_as_str",
        deserialize_with = "deserialize_from_str"
    )]
    pub payment_type: PaymentType,
    #[borsh(
        serialize_with = "serialize_as_str",
        deserialize_with = "deserialize_from_str"
    )]
    pub status: TransactionStatus,
    pub btc_tx_id: Option<String>,
    pub quote_id: Option<Uuid>,
    pub nostr_event_id: Option<String>,
    pub contact_node_id: Option<NodeId>,
    pub payment_request_id: Option<Uuid>,
    pub linked_txs: Vec<TransactionLink>,
}

impl From<bcr_wallet_core::types::Transaction> for StoredTransactionPayloadV1 {
    fn from(value: bcr_wallet_core::types::Transaction) -> Self {
        Self {
            id: value.id,
            mint_url: value.mint_url,
            ys: value.ys,
            amount: value.amount,
            fees: value.fees.sum(),
            unit: value.unit,
            tstamp: value.tstamp,
            direction: value.direction,
            memo: value.memo,
            payment_type: value.payment_type,
            status: value.status,
            btc_tx_id: value.btc_tx_id.map(|bid| bid.to_string()),
            quote_id: value.quote_id,
            nostr_event_id: value.nostr_event_id.map(|nei| nei.to_string()),
            contact_node_id: value.contact_node_id,
            payment_request_id: value.payment_request_id,
            linked_txs: value.linked_txs.into_iter().map(|ltx| ltx.into()).collect(),
        }
    }
}

impl TryFrom<StoredTransactionPayloadV1> for bcr_wallet_core::types::Transaction {
    type Error = Error;
    fn try_from(value: StoredTransactionPayloadV1) -> Result<Self> {
        // melt is melt fee, otherwise swap fee
        let fees = match value.payment_type {
            PaymentType::OnChain => {
                if value.direction == TransactionDirection::Outgoing {
                    bcr_wallet_core::types::TransactionFees {
                        melt: value.fees,
                        ..Default::default()
                    }
                } else {
                    bcr_wallet_core::types::TransactionFees {
                        swap: value.fees,
                        ..Default::default()
                    }
                }
            }
            _ => bcr_wallet_core::types::TransactionFees {
                swap: value.fees,
                ..Default::default()
            },
        };
        Ok(Self {
            id: value.id,
            mint_url: value.mint_url,
            ys: value.ys,
            amount: value.amount,
            fees,
            unit: value.unit,
            tstamp: value.tstamp,
            direction: value.direction,
            memo: value.memo,
            payment_type: value.payment_type,
            status: value.status,
            btc_tx_id: value
                .btc_tx_id
                .map(|bid| bitcoin::Txid::from_str(&bid))
                .transpose()
                .map_err(|e| Error::InvalidBtcTxId(e.to_string()))?,
            quote_id: value.quote_id,
            nostr_event_id: value
                .nostr_event_id
                .map(|nei| EventId::from_str(&nei))
                .transpose()
                .map_err(|e| Error::InvalidNostrEventId(e.to_string()))?,
            contact_node_id: value.contact_node_id,
            payment_request_id: value.payment_request_id,
            linked_txs: value.linked_txs.into_iter().map(|ltx| ltx.into()).collect(),
        })
    }
}

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub struct StoredTransactionPayloadV2 {
    pub id: Uuid,
    #[borsh(
        serialize_with = "serialize_as_str",
        deserialize_with = "deserialize_from_str"
    )]
    pub mint_url: MintUrl,
    #[borsh(
        serialize_with = "serialize_vec_of_strs",
        deserialize_with = "deserialize_vec_of_strs"
    )]
    pub ys: Vec<cashu::PublicKey>,
    #[borsh(
        serialize_with = "serialize_cashu_amount",
        deserialize_with = "deserialize_cashu_amount"
    )]
    pub amount: cashu::Amount,
    pub fees: TransactionFeesV1,
    #[borsh(
        serialize_with = "serialize_as_str",
        deserialize_with = "deserialize_from_str"
    )]
    pub unit: CurrencyUnit,
    pub tstamp: u64,
    #[borsh(
        serialize_with = "serialize_as_str",
        deserialize_with = "deserialize_from_str"
    )]
    pub direction: TransactionDirection,
    pub memo: Option<String>,
    #[borsh(
        serialize_with = "serialize_as_str",
        deserialize_with = "deserialize_from_str"
    )]
    pub payment_type: PaymentType,
    #[borsh(
        serialize_with = "serialize_as_str",
        deserialize_with = "deserialize_from_str"
    )]
    pub status: TransactionStatus,
    pub btc_tx_id: Option<String>,
    pub quote_id: Option<Uuid>,
    pub nostr_event_id: Option<String>,
    pub contact_node_id: Option<NodeId>,
    pub payment_request_id: Option<Uuid>,
    pub linked_txs: Vec<TransactionLink>,
}

impl From<bcr_wallet_core::types::Transaction> for StoredTransactionPayloadV2 {
    fn from(value: bcr_wallet_core::types::Transaction) -> Self {
        Self {
            id: value.id,
            mint_url: value.mint_url,
            ys: value.ys,
            amount: value.amount,
            fees: value.fees.into(),
            unit: value.unit,
            tstamp: value.tstamp,
            direction: value.direction,
            memo: value.memo,
            payment_type: value.payment_type,
            status: value.status,
            btc_tx_id: value.btc_tx_id.map(|bid| bid.to_string()),
            quote_id: value.quote_id,
            nostr_event_id: value.nostr_event_id.map(|nei| nei.to_string()),
            contact_node_id: value.contact_node_id,
            payment_request_id: value.payment_request_id,
            linked_txs: value.linked_txs.into_iter().map(|ltx| ltx.into()).collect(),
        }
    }
}

impl TryFrom<StoredTransactionPayloadV2> for bcr_wallet_core::types::Transaction {
    type Error = Error;
    fn try_from(value: StoredTransactionPayloadV2) -> Result<Self> {
        Ok(Self {
            id: value.id,
            mint_url: value.mint_url,
            ys: value.ys,
            amount: value.amount,
            fees: value.fees.into(),
            unit: value.unit,
            tstamp: value.tstamp,
            direction: value.direction,
            memo: value.memo,
            payment_type: value.payment_type,
            status: value.status,
            btc_tx_id: value
                .btc_tx_id
                .map(|bid| bitcoin::Txid::from_str(&bid))
                .transpose()
                .map_err(|e| Error::InvalidBtcTxId(e.to_string()))?,
            quote_id: value.quote_id,
            nostr_event_id: value
                .nostr_event_id
                .map(|nei| EventId::from_str(&nei))
                .transpose()
                .map_err(|e| Error::InvalidNostrEventId(e.to_string()))?,
            contact_node_id: value.contact_node_id,
            payment_request_id: value.payment_request_id,
            linked_txs: value.linked_txs.into_iter().map(|ltx| ltx.into()).collect(),
        })
    }
}

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub struct TransactionFeesV1 {
    #[borsh(
        serialize_with = "serialize_cashu_amount",
        deserialize_with = "deserialize_cashu_amount"
    )]
    pub swap: cashu::Amount,
    #[borsh(
        serialize_with = "serialize_cashu_amount",
        deserialize_with = "deserialize_cashu_amount"
    )]
    pub network: cashu::Amount,
    #[borsh(
        serialize_with = "serialize_cashu_amount",
        deserialize_with = "deserialize_cashu_amount"
    )]
    pub melt: cashu::Amount,
}

impl From<bcr_wallet_core::types::TransactionFees> for TransactionFeesV1 {
    fn from(value: bcr_wallet_core::types::TransactionFees) -> Self {
        Self {
            swap: value.swap,
            network: value.network,
            melt: value.melt,
        }
    }
}

impl From<TransactionFeesV1> for bcr_wallet_core::types::TransactionFees {
    fn from(value: TransactionFeesV1) -> Self {
        Self {
            swap: value.swap,
            network: value.network,
            melt: value.melt,
        }
    }
}

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub struct TransactionLink {
    pub tx_id: Uuid,
    pub reason: TransactionLinkReason,
}

impl From<bcr_wallet_core::types::TransactionLink> for TransactionLink {
    fn from(value: bcr_wallet_core::types::TransactionLink) -> Self {
        Self {
            tx_id: value.tx_id,
            reason: value.reason.into(),
        }
    }
}

impl From<TransactionLink> for bcr_wallet_core::types::TransactionLink {
    fn from(value: TransactionLink) -> Self {
        Self {
            tx_id: value.tx_id,
            reason: value.reason.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum TransactionLinkReason {
    Reclaim,
}

impl From<bcr_wallet_core::types::TransactionLinkReason> for TransactionLinkReason {
    fn from(value: bcr_wallet_core::types::TransactionLinkReason) -> Self {
        match value {
            bcr_wallet_core::types::TransactionLinkReason::Reclaim => {
                TransactionLinkReason::Reclaim
            }
        }
    }
}

impl From<TransactionLinkReason> for bcr_wallet_core::types::TransactionLinkReason {
    fn from(value: TransactionLinkReason) -> Self {
        match value {
            TransactionLinkReason::Reclaim => {
                bcr_wallet_core::types::TransactionLinkReason::Reclaim
            }
        }
    }
}

///////////////////////////////////////////// TransactionDB
pub struct TransactionDB {
    db: Arc<Database>,
    transaction_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
}

impl TransactionDB {
    const TRANSACTION_BASE_DB_NAME: &'static str = "transactions";

    pub fn transaction_table_name(wallet_id: &str) -> String {
        format!("{wallet_id}_{}", Self::TRANSACTION_BASE_DB_NAME)
    }

    pub fn new(db: Arc<Database>, wallet_id: &str) -> Result<Self> {
        // Leak once to get static string, because of dynamically generated table names
        let transaction_name: &'static str =
            Box::leak(Self::transaction_table_name(wallet_id).into_boxed_str());
        let transaction_table = TableDefinition::new(transaction_name);
        Ok(Self {
            db,
            transaction_table,
        })
    }

    fn store_tx_sync(
        db: Arc<Database>,
        tx_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        tx: Transaction,
    ) -> Result<Uuid> {
        let id = tx.id;
        let entry = StoredTransaction::V2(tx.into());
        let write_txn = db.begin_write()?;

        {
            let mut table = write_txn.open_table(tx_table)?;
            let serialized =
                borsh::to_vec(&entry).map_err(|e| Error::BorshSerialization(e.to_string()))?;

            table.insert(id.as_bytes().as_slice(), serialized)?;
        }

        write_txn.commit()?;
        Ok(id)
    }

    fn load_tx_sync(
        db: Arc<Database>,
        tx_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        tx_id: Uuid,
    ) -> Result<Option<Transaction>> {
        let read_txn = db.begin_read()?;

        match read_txn.open_table(tx_table) {
            Ok(table) => {
                let entry = table.get(tx_id.as_bytes().as_slice())?;
                match entry {
                    Some(e) => {
                        let deserialized: StoredTransaction =
                            borsh::from_slice(e.value().as_slice())
                                .map_err(|e| Error::BorshSerialization(e.to_string()))?;
                        let tx = stored_tx_data(deserialized)?;
                        Ok(Some(tx.try_into()?))
                    }
                    None => Ok(None),
                }
            }
            Err(TableError::TableDoesNotExist(_)) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn list_tx_ids_sync(
        db: Arc<Database>,
        tx_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
    ) -> Result<Vec<Uuid>> {
        let read_txn = db.begin_read()?;

        match read_txn.open_table(tx_table) {
            Ok(table) => {
                let mut res = Vec::new();
                for item in table.range::<&[u8]>(..)? {
                    let (k, _) = item?;
                    let tx_id = Uuid::from_slice(k.value().to_vec().as_slice())
                        .map_err(|e| Error::InvalidTransactionId(e.to_string()))?;
                    res.push(tx_id);
                }
                Ok(res)
            }
            Err(TableError::TableDoesNotExist(_)) => Ok(vec![]),
            Err(e) => Err(e.into()),
        }
    }

    fn list_txs_sync(
        db: Arc<Database>,
        tx_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
    ) -> Result<Vec<Transaction>> {
        let read_txn = db.begin_read()?;

        match read_txn.open_table(tx_table) {
            Ok(table) => {
                let mut res = Vec::new();
                for (_, v) in table.range::<&[u8]>(..)?.flatten() {
                    let deserialized: StoredTransaction =
                        borsh::from_slice(v.value().as_slice())
                            .map_err(|e| Error::BorshSerialization(e.to_string()))?;
                    let tx = stored_tx_data(deserialized)?;
                    res.push(tx.try_into()?);
                }
                Ok(res)
            }
            Err(TableError::TableDoesNotExist(_)) => Ok(vec![]),
            Err(e) => Err(e.into()),
        }
    }

    fn update_status_sync(
        db: Arc<Database>,
        tx_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        tx_id: Uuid,
        status: TransactionStatus,
    ) -> Result<Option<TransactionStatus>> {
        let write_txn = db.begin_write()?;
        let old_v = {
            let mut table = write_txn.open_table(tx_table)?;
            let old_value = table.get(tx_id.as_bytes().as_slice())?.map(|v| v.value());

            if let Some(old_value) = old_value {
                let deserialized: StoredTransaction = borsh::from_slice(&old_value)
                    .map_err(|e| Error::BorshSerialization(e.to_string()))?;
                let mut tx = stored_tx_data(deserialized)?;
                let old = tx.status;
                tx.status = status;

                let serialized = borsh::to_vec(&StoredTransaction::V2(tx))
                    .map_err(|e| Error::BorshSerialization(e.to_string()))?;
                table.insert(tx_id.as_bytes().as_slice(), serialized)?;
                Some(old)
            } else {
                None
            }
        };

        write_txn.commit()?;
        Ok(old_v)
    }

    fn update_memo_sync(
        db: Arc<Database>,
        tx_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        tx_id: Uuid,
        new_memo: Option<String>,
    ) -> Result<Option<String>> {
        let write_txn = db.begin_write()?;
        let old_v = {
            let mut table = write_txn.open_table(tx_table)?;
            let old_value = table.get(tx_id.as_bytes().as_slice())?.map(|v| v.value());

            if let Some(old_value) = old_value {
                let deserialized: StoredTransaction = borsh::from_slice(&old_value)
                    .map_err(|e| Error::BorshSerialization(e.to_string()))?;
                let mut tx = stored_tx_data(deserialized)?;

                let old = tx.memo.clone();
                tx.memo = new_memo;

                let serialized = borsh::to_vec(&StoredTransaction::V2(tx))
                    .map_err(|e| Error::BorshSerialization(e.to_string()))?;
                table.insert(tx_id.as_bytes().as_slice(), serialized)?;
                old
            } else {
                None
            }
        };

        write_txn.commit()?;
        Ok(old_v)
    }

    fn link_txs_sync(
        db: Arc<Database>,
        tx_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        tx_id_1: Uuid,
        tx_id_2: Uuid,
        reason: TransactionLinkReason,
    ) -> Result<()> {
        let write_txn = db.begin_write()?;

        {
            let mut table = write_txn.open_table(tx_table)?;
            let entry_1 = table
                .get(tx_id_1.as_bytes().as_slice())?
                .map(|v| v.value())
                .ok_or(Error::TransactionNotFound(tx_id_1))?;
            let entry_2 = table
                .get(tx_id_2.as_bytes().as_slice())?
                .map(|v| v.value())
                .ok_or(Error::TransactionNotFound(tx_id_2))?;

            let deserialized_1: StoredTransaction = borsh::from_slice(&entry_1)
                .map_err(|e| Error::BorshSerialization(e.to_string()))?;
            let mut tx_1 = stored_tx_data(deserialized_1)?;

            let deserialized_2: StoredTransaction = borsh::from_slice(&entry_2)
                .map_err(|e| Error::BorshSerialization(e.to_string()))?;
            let mut tx_2 = stored_tx_data(deserialized_2)?;

            let link_1_to_2 = TransactionLink {
                tx_id: tx_id_2,
                reason,
            };

            let link_2_to_1 = TransactionLink {
                tx_id: tx_id_1,
                reason,
            };

            tx_1.linked_txs.push(link_1_to_2);
            tx_2.linked_txs.push(link_2_to_1);

            let serialized_1 = borsh::to_vec(&StoredTransaction::V2(tx_1))
                .map_err(|e| Error::BorshSerialization(e.to_string()))?;
            table.insert(tx_id_1.as_bytes().as_slice(), serialized_1)?;

            let serialized_2 = borsh::to_vec(&StoredTransaction::V2(tx_2))
                .map_err(|e| Error::BorshSerialization(e.to_string()))?;
            table.insert(tx_id_2.as_bytes().as_slice(), serialized_2)?;
        }

        write_txn.commit()?;
        Ok(())
    }

    fn delete_repo(
        db: Arc<Database>,
        tx_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
    ) -> Result<()> {
        let write_txn = db.begin_write()?;

        {
            if write_txn.open_table(tx_table).is_ok() {
                write_txn.delete_table(tx_table)?;
            }
        }

        write_txn.commit()?;
        Ok(())
    }
}

#[async_trait]
impl TransactionRepository for TransactionDB {
    async fn store_tx(&self, tx: Transaction) -> Result<Uuid> {
        let db_clone = self.db.clone();
        let table = self.transaction_table;
        spawn_blocking(move || Self::store_tx_sync(db_clone, table, tx)).await?
    }

    async fn load_tx(&self, tx_id: Uuid) -> Result<Transaction> {
        let db_clone = self.db.clone();
        let table = self.transaction_table;
        let res = spawn_blocking(move || Self::load_tx_sync(db_clone, table, tx_id)).await??;
        let entry = res.ok_or(Error::TransactionNotFound(tx_id))?;
        Ok(entry)
    }

    async fn list_tx_ids(&self) -> Result<Vec<Uuid>> {
        let db_clone = self.db.clone();
        let table = self.transaction_table;
        spawn_blocking(move || Self::list_tx_ids_sync(db_clone, table)).await?
    }

    async fn list_txs(&self) -> Result<Vec<Transaction>> {
        let db_clone = self.db.clone();
        let table = self.transaction_table;
        let res = spawn_blocking(move || Self::list_txs_sync(db_clone, table)).await??;
        Ok(res)
    }

    async fn update_status(
        &self,
        tx_id: Uuid,
        status: TransactionStatus,
    ) -> Result<Option<TransactionStatus>> {
        let db_clone = self.db.clone();
        let table = self.transaction_table;
        spawn_blocking(move || Self::update_status_sync(db_clone, table, tx_id, status)).await?
    }

    async fn update_memo(&self, tx_id: Uuid, new_memo: Option<String>) -> Result<Option<String>> {
        let db_clone = self.db.clone();
        let table = self.transaction_table;
        spawn_blocking(move || Self::update_memo_sync(db_clone, table, tx_id, new_memo)).await?
    }

    async fn link_txs(
        &self,
        tx_id_1: Uuid,
        tx_id_2: Uuid,
        reason: bcr_wallet_core::types::TransactionLinkReason,
    ) -> Result<()> {
        let db_clone = self.db.clone();
        let table = self.transaction_table;
        spawn_blocking(move || {
            Self::link_txs_sync(db_clone, table, tx_id_1, tx_id_2, reason.into())
        })
        .await?
    }

    async fn delete_repo(&self) -> Result<()> {
        let db_clone = self.db.clone();
        let table = self.transaction_table;
        spawn_blocking(move || Self::delete_repo(db_clone, table)).await?
    }
}

// after migrations, everything has to be v2
fn stored_tx_data(stored_tx: StoredTransaction) -> Result<StoredTransactionPayloadV2> {
    match stored_tx {
        StoredTransaction::V1(_) => Err(Error::InvalidTransactionData("v1".to_string())),
        StoredTransaction::V2(data) => Ok(data),
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        error::Error,
        test_utils::tests::{test_other_pub_key, test_pub_key, wallet_id},
    };
    use bcr_wallet_core::types::PaymentType;

    use super::*;
    use bcr_common::cashu::Amount;
    use chrono::Utc;
    use redb::{Builder, backends::InMemoryBackend};

    fn get_db(wallet_id: &str) -> TransactionDB {
        let in_mem = InMemoryBackend::new();
        let db = Arc::new(
            Builder::new()
                .create_with_backend(in_mem)
                .expect("can create in-memory redb"),
        );
        TransactionDB::new(db, wallet_id).expect("can create TransactionDB")
    }

    fn test_tx() -> Transaction {
        let mint_url = MintUrl::from_str("https://example.com").expect("valid mint url");
        Transaction {
            id: Uuid::new_v4(),
            mint_url,
            direction: TransactionDirection::Outgoing,
            amount: Amount::from(42u64),
            fees: TransactionFeesV1 {
                swap: Amount::ZERO,
                network: Amount::ZERO,
                melt: Amount::ZERO,
            }
            .into(),
            unit: CurrencyUnit::Sat,
            ys: vec![cashu::PublicKey::from(test_pub_key())],
            tstamp: Utc::now().timestamp() as u64,
            memo: Some("some memo".to_string()),
            payment_type: PaymentType::Token,
            status: TransactionStatus::Pending,
            quote_id: None,
            btc_tx_id: None,
            nostr_event_id: None,
            contact_node_id: None,
            payment_request_id: None,
            linked_txs: vec![],
        }
    }

    #[tokio::test]
    async fn test_list_empty() {
        let repo = get_db(&wallet_id());
        assert!(repo.list_tx_ids().await.unwrap().is_empty(),);
        assert!(repo.list_txs().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_load_missing_returns_error() {
        let repo = get_db(&wallet_id());

        let tx = test_tx();
        let tx_id = tx.id;

        let err = repo.load_tx(tx_id).await.unwrap_err();
        match err {
            Error::TransactionNotFound(id) => assert_eq!(id, tx_id),
            other => panic!("expected TransactionNotFound, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_store_load_tx() {
        let repo = get_db(&wallet_id());

        let tx = test_tx();
        let tx_id = repo.store_tx(tx.clone()).await.expect("store_tx works");

        let loaded = repo.load_tx(tx_id).await.expect("load_tx works");

        assert_eq!(loaded.id, tx.id);
    }

    #[tokio::test]
    async fn test_list_after_inserts() {
        let repo = get_db(&wallet_id());

        let mut tx1 = test_tx();
        tx1.ys = vec![cashu::PublicKey::from(test_pub_key())];

        let mut tx2 = test_tx();
        tx2.ys = vec![cashu::PublicKey::from(test_other_pub_key())];

        let id1 = repo.store_tx(tx1.clone()).await.unwrap();
        let id2 = repo.store_tx(tx2.clone()).await.unwrap();

        let mut ids = repo.list_tx_ids().await.unwrap();
        ids.sort_by_key(|a| a.to_string());

        let mut expected_ids = vec![id1, id2];
        expected_ids.sort_by_key(|a| a.to_string());

        assert_eq!(ids, expected_ids);

        let txs = repo.list_txs().await.unwrap();
        assert_eq!(txs.len(), 2);
    }

    #[tokio::test]
    async fn test_update_memo_missing_returns_none() {
        let repo = get_db(&wallet_id());

        let tx = test_tx();
        let tx_id = tx.id;

        let old = repo
            .update_memo(tx_id, None)
            .await
            .expect("update_memo works");
        assert_eq!(old, None);
    }

    #[tokio::test]
    async fn test_update_memo_insert_and_overwrite() {
        let repo = get_db(&wallet_id());

        let tx = test_tx();
        let tx_id = repo.store_tx(tx).await.unwrap();

        // no value for memo before - returns set value
        let old = repo
            .update_memo(tx_id, Some("new memo".to_string()))
            .await
            .unwrap();
        assert_eq!(old, Some("some memo".to_owned()));

        // overwrite value for memo - returns old memo
        let old = repo
            .update_memo(tx_id, Some("different memo".to_string()))
            .await
            .unwrap();
        assert_eq!(old, Some("new memo".to_string()));

        let loaded = repo.load_tx(tx_id).await.unwrap();
        assert_eq!(loaded.memo, Some("different memo".to_string()));
    }

    #[tokio::test]
    async fn test_update_status_missing_returns_none() {
        let repo = get_db(&wallet_id());

        let tx = test_tx();
        let tx_id = tx.id;

        let old = repo
            .update_status(tx_id, TransactionStatus::Settled)
            .await
            .expect("update_status works");
        assert_eq!(old, None);
    }

    #[tokio::test]
    async fn test_update_status_insert_and_overwrite() {
        let repo = get_db(&wallet_id());

        let tx = test_tx();
        let tx_id = repo.store_tx(tx).await.unwrap();

        // no value for key before - returns None
        let old = repo
            .update_status(tx_id, TransactionStatus::Settled)
            .await
            .unwrap();
        assert_eq!(old, Some(TransactionStatus::Pending));

        // overwrite value for key - returns old key
        let old = repo
            .update_status(tx_id, TransactionStatus::Pending)
            .await
            .unwrap();
        assert_eq!(old, Some(TransactionStatus::Settled));

        let loaded = repo.load_tx(tx_id).await.unwrap();
        assert_eq!(loaded.status, TransactionStatus::Pending);
    }
}
