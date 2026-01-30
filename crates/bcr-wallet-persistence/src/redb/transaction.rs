use crate::error::Error;
use crate::{TransactionRepository, error::Result};
use async_trait::async_trait;
use bcr_common::cashu::{CurrencyUnit, MintUrl, nut01 as cdk01};
use bcr_common::cdk::wallet::types::{Transaction, TransactionDirection, TransactionId};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition, TableError};
use std::{collections::HashMap, str::FromStr, sync::Arc};
use tokio::task::spawn_blocking;

///////////////////////////////////////////// TransactionEntry
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
struct TransactionEntry {
    pub tx_id: String,
    pub mint_url: MintUrl,
    pub direction: TransactionDirection,
    pub amount: bcr_common::cashu::Amount,
    pub fee: bcr_common::cashu::Amount,
    pub unit: CurrencyUnit,
    pub ys: Vec<cdk01::PublicKey>,
    pub timestamp: u64,
    pub memo: Option<String>,
    pub metadata: HashMap<String, String>,
    pub quote_id: Option<String>,
}

impl std::convert::From<Transaction> for TransactionEntry {
    fn from(tx: Transaction) -> Self {
        let tx_id = TransactionId::new(tx.ys.clone());
        TransactionEntry {
            tx_id: tx_id.to_string(),
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
impl std::convert::From<TransactionEntry> for Transaction {
    fn from(entry: TransactionEntry) -> Self {
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

    pub fn new(db: Arc<Database>, wallet_id: &str) -> Result<Self> {
        // Leak once to get static string, because of dynamically generated table names
        let transaction_name: &'static str =
            Box::leak(format!("{wallet_id}_{}", Self::TRANSACTION_BASE_DB_NAME).into_boxed_str());
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
    ) -> Result<TransactionId> {
        let id = tx.id();
        let entry: TransactionEntry = tx.into();
        let write_txn = db.begin_write()?;

        {
            let mut table = write_txn.open_table(tx_table)?;

            let mut serialized = Vec::new();
            ciborium::into_writer(&entry, &mut serialized)?;
            table.insert(id.as_bytes().as_slice(), serialized)?;
        }

        write_txn.commit()?;
        Ok(id)
    }

    fn load_tx_sync(
        db: Arc<Database>,
        tx_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        tx_id: TransactionId,
    ) -> Result<Option<TransactionEntry>> {
        let read_txn = db.begin_read()?;

        match read_txn.open_table(tx_table) {
            Ok(table) => {
                let entry = table.get(tx_id.as_bytes().as_slice())?;
                match entry {
                    Some(e) => {
                        let tx: TransactionEntry = ciborium::from_reader(e.value().as_slice())?;
                        Ok(Some(tx))
                    }
                    None => Ok(None),
                }
            }
            Err(TableError::TableDoesNotExist(_)) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn delete_tx_sync(
        db: Arc<Database>,
        tx_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        tx_id: TransactionId,
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
    ) -> Result<Vec<TransactionId>> {
        let read_txn = db.begin_read()?;

        match read_txn.open_table(tx_table) {
            Ok(table) => {
                let mut res = Vec::new();
                for (_, v) in table.range::<&[u8]>(..)?.flatten() {
                    let tx: TransactionEntry = ciborium::from_reader(v.value().as_slice())?;
                    res.push(TransactionId::from_str(&tx.tx_id)?);
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
    ) -> Result<Vec<TransactionEntry>> {
        let read_txn = db.begin_read()?;

        match read_txn.open_table(tx_table) {
            Ok(table) => {
                let mut res = Vec::new();
                for (_, v) in table.range::<&[u8]>(..)?.flatten() {
                    let tx: TransactionEntry = ciborium::from_reader(v.value().as_slice())?;
                    res.push(tx);
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
        tx_id: TransactionId,
        k: String,
        v: String,
    ) -> Result<Option<String>> {
        let write_txn = db.begin_write()?;
        let old_v = {
            let mut table = write_txn.open_table(tx_table)?;
            let old_value = table.get(tx_id.as_bytes().as_slice())?.map(|v| v.value());

            if let Some(old_value) = old_value {
                let mut tx: TransactionEntry = ciborium::from_reader(old_value.as_slice())?;
                let old = tx.metadata.insert(k, v);

                let mut serialized = Vec::new();
                ciborium::into_writer(&tx, &mut serialized)?;
                table.insert(tx_id.as_bytes().as_slice(), serialized)?;
                old
            } else {
                None
            }
        };

        write_txn.commit()?;
        Ok(old_v)
    }
}

#[async_trait]
impl TransactionRepository for TransactionDB {
    async fn store_tx(&self, tx: Transaction) -> Result<TransactionId> {
        let db_clone = self.db.clone();
        let table = self.transaction_table;
        spawn_blocking(move || Self::store_tx_sync(db_clone, table, tx)).await?
    }

    async fn load_tx(&self, tx_id: TransactionId) -> Result<Transaction> {
        let db_clone = self.db.clone();
        let table = self.transaction_table;
        let res = spawn_blocking(move || Self::load_tx_sync(db_clone, table, tx_id)).await??;
        let entry = res.ok_or(Error::TransactionNotFound(tx_id))?;
        Ok(entry.into())
    }

    async fn delete_tx(&self, tx_id: TransactionId) -> Result<()> {
        let db_clone = self.db.clone();
        let table = self.transaction_table;
        spawn_blocking(move || Self::delete_tx_sync(db_clone, table, tx_id)).await??;
        Ok(())
    }

    async fn list_tx_ids(&self) -> Result<Vec<TransactionId>> {
        let db_clone = self.db.clone();
        let table = self.transaction_table;
        spawn_blocking(move || Self::list_tx_ids_sync(db_clone, table)).await?
    }

    async fn list_txs(&self) -> Result<Vec<Transaction>> {
        let db_clone = self.db.clone();
        let table = self.transaction_table;
        let res = spawn_blocking(move || Self::list_txs_sync(db_clone, table)).await??;
        Ok(res.into_iter().map(|entry| entry.into()).collect())
    }

    async fn update_metadata(
        &self,
        tx_id: TransactionId,
        k: String,
        v: String,
    ) -> Result<Option<String>> {
        let db_clone = self.db.clone();
        let table = self.transaction_table;
        spawn_blocking(move || Self::update_meta_sync(db_clone, table, tx_id, k, v)).await?
    }
}

#[cfg(test)]
mod tests {
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
        let tx_id = tx.id();

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
    async fn test_update_metadata_missing_returns_none() {
        let repo = get_db(&wallet_id());

        let tx = test_tx();
        let tx_id = tx.id();

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
