use crate::error::Error;
use crate::{TransactionRepository, error::Result};
use async_trait::async_trait;
use bcr_common::cashu::{CurrencyUnit, MintUrl, nut01 as cdk01};
use bcr_common::cdk_common::wallet::{Transaction, TransactionDirection, TransactionId};
use bcr_common::wire::borsh::{
    deserialize_cashu_amount, deserialize_from_str, deserialize_vec_of_strs, serialize_as_str,
    serialize_cashu_amount, serialize_vec_of_strs,
};
use borsh::{BorshDeserialize, BorshSerialize};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition, TableError};
use std::{collections::HashMap, sync::Arc};
use tokio::task::spawn_blocking;

/// StoredTransaction is a versioned, borsh-serialized transaction
#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub(super) enum StoredTransaction {
    V1(StoredTransactionPayloadV1),
}

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub(super) struct StoredTransactionPayloadV1 {
    pub tx_id: uuid::Uuid,
    #[borsh(
        serialize_with = "serialize_as_str",
        deserialize_with = "deserialize_from_str"
    )]
    pub mint_url: MintUrl,
    #[borsh(
        serialize_with = "serialize_as_str",
        deserialize_with = "deserialize_from_str"
    )]
    pub direction: TransactionDirection,
    #[borsh(
        serialize_with = "serialize_cashu_amount",
        deserialize_with = "deserialize_cashu_amount"
    )]
    pub amount: bcr_common::cashu::Amount,
    #[borsh(
        serialize_with = "serialize_cashu_amount",
        deserialize_with = "deserialize_cashu_amount"
    )]
    pub fee: bcr_common::cashu::Amount,
    #[borsh(
        serialize_with = "serialize_as_str",
        deserialize_with = "deserialize_from_str"
    )]
    pub unit: CurrencyUnit,
    #[borsh(
        serialize_with = "serialize_vec_of_strs",
        deserialize_with = "deserialize_vec_of_strs"
    )]
    pub ys: Vec<cdk01::PublicKey>,
    pub timestamp: u64,
    pub memo: Option<String>,
    pub metadata: HashMap<String, String>,
    pub quote_id: Option<String>,
}

impl std::convert::From<(Transaction, uuid::Uuid)> for StoredTransactionPayloadV1 {
    fn from((tx, tx_id): (Transaction, uuid::Uuid)) -> Self {
        StoredTransactionPayloadV1 {
            tx_id,
            mint_url: tx.mint_url,
            direction: tx.direction,
            amount: tx.amount,
            fee: tx.fee,
            unit: tx.unit,
            ys: tx.ys,
            timestamp: tx.timestamp,
            memo: tx.memo,
            metadata: tx.metadata,
            quote_id: tx.quote_id,
        }
    }
}

impl std::convert::From<StoredTransactionPayloadV1> for Transaction {
    fn from(entry: StoredTransactionPayloadV1) -> Self {
        Transaction {
            mint_url: entry.mint_url,
            direction: entry.direction,
            amount: entry.amount,
            fee: entry.fee,
            unit: entry.unit,
            ys: entry.ys,
            timestamp: entry.timestamp,
            memo: entry.memo,
            metadata: entry.metadata,
            quote_id: entry.quote_id,
            payment_request: None,
            payment_proof: None,
            payment_method: None,
            saga_id: None,
        }
    }
}

pub(super) fn to_stored_tx_v1(tx: Transaction, tx_id: uuid::Uuid) -> Result<StoredTransaction> {
    let payload = StoredTransactionPayloadV1::from((tx, tx_id));
    Ok(StoredTransaction::V1(payload))
}

pub(super) fn from_stored_tx_v1(tx: StoredTransaction) -> Result<StoredTransactionPayloadV1> {
    let StoredTransaction::V1(payload) = tx;
    Ok(payload)
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
    ) -> Result<uuid::Uuid> {
        let id = uuid::Uuid::new_v4();
        let entry = to_stored_tx_v1(tx, id)?;
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
        tx_id: uuid::Uuid,
    ) -> Result<Option<StoredTransaction>> {
        let read_txn = db.begin_read()?;

        match read_txn.open_table(tx_table) {
            Ok(table) => {
                let entry = table.get(tx_id.as_bytes().as_slice())?;
                match entry {
                    Some(e) => {
                        let deserialized: StoredTransaction =
                            borsh::from_slice(e.value().as_slice())
                                .map_err(|e| Error::BorshSerialization(e.to_string()))?;
                        Ok(Some(deserialized))
                    }
                    None => Ok(None),
                }
            }
            Err(TableError::TableDoesNotExist(_)) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn load_tx_by_ys_sync(
        db: Arc<Database>,
        tx_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        tx_id: TransactionId,
    ) -> Result<Option<StoredTransaction>> {
        let read_txn = db.begin_read()?;

        match read_txn.open_table(tx_table) {
            Ok(table) => {
                for (_, v) in table.range::<&[u8]>(..)?.flatten() {
                    let deserialized: StoredTransaction =
                        borsh::from_slice(v.value().as_slice())
                            .map_err(|e| Error::BorshSerialization(e.to_string()))?;
                    let tx = from_stored_tx_v1(deserialized.clone())?;
                    let tx_ys_id = TransactionId::new(tx.ys);
                    if tx_id == tx_ys_id {
                        return Ok(Some(deserialized));
                    }
                }
                Ok(None)
            }
            Err(TableError::TableDoesNotExist(_)) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn delete_tx_sync(
        db: Arc<Database>,
        tx_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        tx_id: uuid::Uuid,
    ) -> Result<()> {
        let write_txn = db.begin_write()?;

        {
            let mut table = write_txn.open_table(tx_table)?;
            table.remove(tx_id.as_bytes().as_slice())?;
        }

        write_txn.commit()?;
        Ok(())
    }

    fn list_tx_ids_sync(
        db: Arc<Database>,
        tx_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
    ) -> Result<Vec<uuid::Uuid>> {
        let read_txn = db.begin_read()?;

        match read_txn.open_table(tx_table) {
            Ok(table) => {
                let mut res = Vec::new();
                for (k, _) in table.range::<&[u8]>(..)?.flatten() {
                    let tx_id = uuid::Uuid::from_slice(k.value().to_vec().as_slice())?;
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
    ) -> Result<Vec<StoredTransaction>> {
        let read_txn = db.begin_read()?;

        match read_txn.open_table(tx_table) {
            Ok(table) => {
                let mut res = Vec::new();
                for (_, v) in table.range::<&[u8]>(..)?.flatten() {
                    let deserialized: StoredTransaction =
                        borsh::from_slice(v.value().as_slice())
                            .map_err(|e| Error::BorshSerialization(e.to_string()))?;
                    res.push(deserialized);
                }
                Ok(res)
            }
            Err(TableError::TableDoesNotExist(_)) => Ok(vec![]),
            Err(e) => Err(e.into()),
        }
    }

    fn update_meta_sync(
        db: Arc<Database>,
        tx_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        tx_id: uuid::Uuid,
        k: String,
        v: String,
    ) -> Result<Option<String>> {
        let write_txn = db.begin_write()?;
        let old_v = {
            let mut table = write_txn.open_table(tx_table)?;
            let old_value = table.get(tx_id.as_bytes().as_slice())?.map(|v| v.value());

            if let Some(old_value) = old_value {
                let deserialized: StoredTransaction = borsh::from_slice(&old_value)
                    .map_err(|e| Error::BorshSerialization(e.to_string()))?;
                let mut tx = from_stored_tx_v1(deserialized)?;
                let old = tx.metadata.insert(k, v);

                let serialized = borsh::to_vec(&StoredTransaction::V1(tx))
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

    fn update_memo_sync(
        db: Arc<Database>,
        tx_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        tx_id: uuid::Uuid,
        new_memo: Option<String>,
    ) -> Result<Option<String>> {
        let write_txn = db.begin_write()?;
        let old_v = {
            let mut table = write_txn.open_table(tx_table)?;
            let old_value = table.get(tx_id.as_bytes().as_slice())?.map(|v| v.value());

            if let Some(old_value) = old_value {
                let deserialized: StoredTransaction = borsh::from_slice(&old_value)
                    .map_err(|e| Error::BorshSerialization(e.to_string()))?;
                let mut tx = from_stored_tx_v1(deserialized)?;
                let old = tx.memo.clone();
                tx.memo = new_memo;

                let serialized = borsh::to_vec(&StoredTransaction::V1(tx))
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

    fn update_fee_sync(
        db: Arc<Database>,
        tx_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        tx_id: uuid::Uuid,
        fee_to_add: bcr_common::cashu::Amount,
    ) -> Result<()> {
        let write_txn = db.begin_write()?;

        {
            let mut table = write_txn.open_table(tx_table)?;
            let old_value = table.get(tx_id.as_bytes().as_slice())?.map(|v| v.value());

            if let Some(old_value) = old_value {
                let deserialized: StoredTransaction = borsh::from_slice(&old_value)
                    .map_err(|e| Error::BorshSerialization(e.to_string()))?;
                let mut tx = from_stored_tx_v1(deserialized)?;
                tx.fee += fee_to_add;

                let serialized = borsh::to_vec(&StoredTransaction::V1(tx))
                    .map_err(|e| Error::BorshSerialization(e.to_string()))?;
                table.insert(tx_id.as_bytes().as_slice(), serialized)?;
            }
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
    async fn store_tx(&self, tx: Transaction) -> Result<uuid::Uuid> {
        let db_clone = self.db.clone();
        let table = self.transaction_table;
        spawn_blocking(move || Self::store_tx_sync(db_clone, table, tx)).await?
    }

    async fn load_tx(&self, tx_id: uuid::Uuid) -> Result<Transaction> {
        let db_clone = self.db.clone();
        let table = self.transaction_table;
        let res = spawn_blocking(move || Self::load_tx_sync(db_clone, table, tx_id)).await??;
        let entry = res.ok_or(Error::TransactionNotFound(tx_id))?;
        Ok(from_stored_tx_v1(entry)?.into())
    }

    async fn load_tx_by_ys(&self, ys: Vec<cdk01::PublicKey>) -> Result<(uuid::Uuid, Transaction)> {
        let db_clone = self.db.clone();
        let table = self.transaction_table;
        let tx_id = TransactionId::new(ys);
        let res =
            spawn_blocking(move || Self::load_tx_by_ys_sync(db_clone, table, tx_id)).await??;
        let entry = res.ok_or(Error::TransactionNotFoundForYs(tx_id))?;
        let pl = from_stored_tx_v1(entry)?;

        Ok((pl.tx_id, pl.into()))
    }

    async fn delete_tx(&self, tx_id: uuid::Uuid) -> Result<()> {
        let db_clone = self.db.clone();
        let table = self.transaction_table;
        spawn_blocking(move || Self::delete_tx_sync(db_clone, table, tx_id)).await??;
        Ok(())
    }

    async fn list_tx_ids(&self) -> Result<Vec<uuid::Uuid>> {
        let db_clone = self.db.clone();
        let table = self.transaction_table;
        spawn_blocking(move || Self::list_tx_ids_sync(db_clone, table)).await?
    }

    async fn list_txs(&self) -> Result<Vec<Transaction>> {
        let db_clone = self.db.clone();
        let table = self.transaction_table;
        let res = spawn_blocking(move || Self::list_txs_sync(db_clone, table)).await??;
        let mapped: Result<Vec<Transaction>> = res
            .into_iter()
            .map(|entry| {
                let pl = from_stored_tx_v1(entry)?;
                Ok(pl.into())
            })
            .collect();

        Ok(mapped?)
    }

    async fn update_metadata(
        &self,
        tx_id: uuid::Uuid,
        k: String,
        v: String,
    ) -> Result<Option<String>> {
        let db_clone = self.db.clone();
        let table = self.transaction_table;
        spawn_blocking(move || Self::update_meta_sync(db_clone, table, tx_id, k, v)).await?
    }

    async fn update_memo(
        &self,
        tx_id: uuid::Uuid,
        new_memo: Option<String>,
    ) -> Result<Option<String>> {
        let db_clone = self.db.clone();
        let table = self.transaction_table;
        spawn_blocking(move || Self::update_memo_sync(db_clone, table, tx_id, new_memo)).await?
    }

    async fn update_fee(
        &self,
        tx_id: uuid::Uuid,
        fee_to_add: bcr_common::cashu::Amount,
    ) -> Result<()> {
        let db_clone = self.db.clone();
        let table = self.transaction_table;
        spawn_blocking(move || Self::update_fee_sync(db_clone, table, tx_id, fee_to_add)).await?
    }

    async fn delete_repo(&self) -> Result<()> {
        let db_clone = self.db.clone();
        let table = self.transaction_table;
        spawn_blocking(move || Self::delete_repo(db_clone, table)).await?
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use crate::{
        error::Error,
        test_utils::tests::{test_other_pub_key, test_pub_key, wallet_id},
    };
    use bcr_wallet_core::types::{PAYMENT_TYPE_METADATA_KEY, PaymentType};

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
        let mut metadata = HashMap::new();
        metadata.insert(
            PAYMENT_TYPE_METADATA_KEY.to_string(),
            PaymentType::Token.to_string(),
        );

        Transaction {
            mint_url,
            direction: TransactionDirection::Outgoing,
            amount: Amount::from(42u64),
            fee: Amount::ZERO,
            unit: CurrencyUnit::Sat,

            ys: vec![cdk01::PublicKey::from(test_pub_key())],

            timestamp: Utc::now().timestamp() as u64,
            memo: Some("some memo".to_string()),
            metadata,
            quote_id: None,
            payment_request: None,
            payment_proof: None,
            payment_method: None,
            saga_id: None,
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

        let tx_id = uuid::Uuid::new_v4();

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

        assert_eq!(loaded, tx);
    }

    #[tokio::test]
    async fn test_list_after_inserts() {
        let repo = get_db(&wallet_id());

        let mut tx1 = test_tx();
        tx1.ys = vec![cdk01::PublicKey::from(test_pub_key())];

        let mut tx2 = test_tx();
        tx2.ys = vec![cdk01::PublicKey::from(test_other_pub_key())];

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

        let tx_id = uuid::Uuid::new_v4();

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
    async fn test_update_metadata_missing_returns_none() {
        let repo = get_db(&wallet_id());

        let tx_id = uuid::Uuid::new_v4();

        let old = repo
            .update_metadata(tx_id, "new".to_string(), "value".to_string())
            .await
            .expect("update_metadata works");
        assert_eq!(old, None);
    }

    #[tokio::test]
    async fn test_update_metadata_insert_and_overwrite() {
        let repo = get_db(&wallet_id());

        let tx = test_tx();
        let tx_id = repo.store_tx(tx).await.unwrap();

        // no value for key before - returns None
        let old = repo
            .update_metadata(tx_id, "tag".to_string(), "first".to_string())
            .await
            .unwrap();
        assert_eq!(old, None);

        // overwrite value for key - returns old key
        let old = repo
            .update_metadata(tx_id, "tag".to_string(), "second".to_string())
            .await
            .unwrap();
        assert_eq!(old, Some("first".to_string()));

        let loaded = repo.load_tx(tx_id).await.unwrap();
        assert_eq!(
            loaded.metadata.get("tag").cloned(),
            Some("second".to_string())
        );
    }

    #[tokio::test]
    async fn test_update_fee() {
        let repo = get_db(&wallet_id());

        let tx = test_tx();
        let tx_id = repo.store_tx(tx).await.unwrap();

        repo.update_fee(tx_id, bcr_common::cashu::Amount::ONE)
            .await
            .unwrap();

        let loaded = repo.load_tx(tx_id).await.unwrap();
        assert_eq!(loaded.fee, bcr_common::cashu::Amount::ONE,);
    }

    #[tokio::test]
    async fn test_delete_removes() {
        let repo = get_db(&wallet_id());

        let tx = test_tx();
        let tx_id = repo.store_tx(tx).await.unwrap();

        repo.delete_tx(tx_id).await.unwrap();

        let err = repo.load_tx(tx_id).await.unwrap_err();
        match err {
            Error::TransactionNotFound(id) => assert_eq!(id, tx_id),
            other => panic!("expected TransactionNotFound, got: {other:?}"),
        }
    }
}
