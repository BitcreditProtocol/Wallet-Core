use crate::{
    PendingPaymentRequestStoreApi,
    error::{Error, Result},
};
use async_trait::async_trait;
use bcr_common::{
    cashu::{Amount, CurrencyUnit},
    core::NodeId,
};
use bcr_wallet_core::types::PendingPaymentRequest;
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition, TableError};
use std::sync::Arc;
use tokio::task::spawn_blocking;
use uuid::Uuid;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PendingPaymentRequestEntry {
    pub id: Uuid,
    pub node_id: NodeId,
    pub amount: Amount,
    pub unit: CurrencyUnit,
    pub description: Option<String>,
    pub deadline: Option<u64>,
    pub created_at: u64,
}

impl From<PendingPaymentRequest> for PendingPaymentRequestEntry {
    fn from(value: PendingPaymentRequest) -> Self {
        Self {
            id: value.id,
            node_id: value.node_id,
            amount: value.amount,
            unit: value.unit,
            description: value.description,
            deadline: value.deadline,
            created_at: value.created_at,
        }
    }
}

impl From<PendingPaymentRequestEntry> for PendingPaymentRequest {
    fn from(value: PendingPaymentRequestEntry) -> Self {
        Self {
            id: value.id,
            node_id: value.node_id,
            amount: value.amount,
            unit: value.unit,
            description: value.description,
            deadline: value.deadline,
            created_at: value.created_at,
        }
    }
}

///////////////////////////////////////////// PendingPaymentRequestDB
pub struct PendingPaymentRequestDB {
    db: Arc<Database>,
    pending_payment_requests_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
}

impl PendingPaymentRequestDB {
    const PENDING_PAYMENT_REQUESTS_DB_NAME: &'static str = "pending_payment_requests";

    pub fn new(db: Arc<Database>, wallet_id: &str) -> Result<Self> {
        // Leak once to get static string, because of dynamically generated table names
        let pending_payment_requests_name: &'static str = Box::leak(
            format!("{wallet_id}_{}", Self::PENDING_PAYMENT_REQUESTS_DB_NAME).into_boxed_str(),
        );

        let pending_payment_requests_table = TableDefinition::new(pending_payment_requests_name);

        Ok(Self {
            db,
            pending_payment_requests_table,
        })
    }

    fn add_pending_payment_request_sync(
        db: Arc<Database>,
        pending_payment_requests_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        entry: PendingPaymentRequestEntry,
    ) -> Result<()> {
        let id = entry.id;
        let write_txn = db.begin_write()?;

        {
            let mut table = write_txn.open_table(pending_payment_requests_table)?;
            let old_value = table.get(id.as_bytes().as_slice())?.map(|v| v.value());

            if old_value.is_some() {
                return Err(Error::PendingPaymentRequestAlreadyExists(id.to_string()));
            }
            let mut serialized = Vec::new();
            ciborium::into_writer(&entry, &mut serialized)?;
            table.insert(id.as_bytes().as_slice(), serialized)?;
        }

        write_txn.commit()?;
        Ok(())
    }

    fn get_pending_payment_request_sync(
        db: Arc<Database>,
        pending_payment_requests_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        id: Uuid,
    ) -> Result<Option<PendingPaymentRequestEntry>> {
        let read_txn = db.begin_read()?;

        match read_txn.open_table(pending_payment_requests_table) {
            Ok(table) => {
                let entry = table.get(id.as_bytes().as_slice())?;
                match entry {
                    Some(e) => {
                        let pending_payment_request: PendingPaymentRequestEntry =
                            ciborium::from_reader(e.value().as_slice())?;
                        Ok(Some(pending_payment_request))
                    }
                    None => Ok(None),
                }
            }
            Err(TableError::TableDoesNotExist(_)) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn list_pending_payment_requests_sync(
        db: Arc<Database>,
        pending_payment_requests_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
    ) -> Result<Vec<PendingPaymentRequestEntry>> {
        let read_txn = db.begin_read()?;

        match read_txn.open_table(pending_payment_requests_table) {
            Ok(table) => {
                let mut res = Vec::new();
                for (_, v) in table.range::<&[u8]>(..)?.flatten() {
                    let entry: PendingPaymentRequestEntry =
                        ciborium::from_reader(v.value().as_slice())?;
                    res.push(entry);
                }
                Ok(res)
            }
            Err(TableError::TableDoesNotExist(_)) => Ok(vec![]),
            Err(e) => Err(e.into()),
        }
    }

    fn delete_pending_payment_request_sync(
        db: Arc<Database>,
        pending_payment_requests_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        id: Uuid,
    ) -> Result<()> {
        let write_txn = db.begin_write()?;

        {
            let mut table = write_txn.open_table(pending_payment_requests_table)?;
            table.remove(id.as_bytes().as_slice())?;
        }

        write_txn.commit()?;
        Ok(())
    }

    fn delete_repo_sync(
        db: Arc<Database>,
        pending_payment_requests_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
    ) -> Result<()> {
        let write_txn = db.begin_write()?;

        {
            if write_txn.open_table(pending_payment_requests_table).is_ok() {
                write_txn.delete_table(pending_payment_requests_table)?;
            }
        }

        write_txn.commit()?;
        Ok(())
    }
}

#[async_trait]
impl PendingPaymentRequestStoreApi for PendingPaymentRequestDB {
    async fn add_pending_payment_request(
        &self,
        pending_payment_request: PendingPaymentRequest,
    ) -> Result<()> {
        let db_clone = self.db.clone();
        let table = self.pending_payment_requests_table;
        let res = spawn_blocking(move || {
            Self::add_pending_payment_request_sync(db_clone, table, pending_payment_request.into())
        })
        .await??;
        Ok(res)
    }

    async fn get_pending_payment_request(&self, id: Uuid) -> Result<Option<PendingPaymentRequest>> {
        let db_clone = self.db.clone();
        let table = self.pending_payment_requests_table;
        let res =
            spawn_blocking(move || Self::get_pending_payment_request_sync(db_clone, table, id))
                .await??;
        Ok(res.map(|c| c.into()))
    }

    async fn list_pending_payment_requests(&self) -> Result<Vec<PendingPaymentRequest>> {
        let db_clone = self.db.clone();
        let table = self.pending_payment_requests_table;
        let res = spawn_blocking(move || Self::list_pending_payment_requests_sync(db_clone, table))
            .await??;
        Ok(res.into_iter().map(|entry| entry.into()).collect())
    }

    async fn delete_pending_payment_request(&self, id: Uuid) -> Result<()> {
        let db_clone = self.db.clone();
        let table = self.pending_payment_requests_table;
        spawn_blocking(move || Self::delete_pending_payment_request_sync(db_clone, table, id))
            .await??;
        Ok(())
    }

    async fn delete_repo(&self) -> Result<()> {
        let db_clone = self.db.clone();
        let pending_payment_requests_table = self.pending_payment_requests_table;
        spawn_blocking(move || Self::delete_repo_sync(db_clone, pending_payment_requests_table))
            .await?
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;
    use redb::{Builder, backends::InMemoryBackend};
    use uuid::Uuid;

    const NODE_ID_1: &str =
        "bitcrt03205b8dec12bc9e879f5b517aa32192a2550e88adcee3e54ec2c7294802568fef";

    const NODE_ID_2: &str =
        "bitcrt0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";

    fn get_db(wallet_id: &str) -> PendingPaymentRequestDB {
        let in_mem = InMemoryBackend::new();
        let db = Arc::new(
            Builder::new()
                .create_with_backend(in_mem)
                .expect("can create in-memory redb"),
        );

        PendingPaymentRequestDB::new(db, wallet_id).expect("can create PendingPaymentRequestDB")
    }

    fn pending_payment_request(
        id: Uuid,
        node_id: &str,
        amount: u64,
        description: Option<&str>,
        deadline: Option<u64>,
        created_at: u64,
    ) -> PendingPaymentRequest {
        PendingPaymentRequest {
            id,
            node_id: NodeId::from_str(node_id).unwrap(),
            amount: Amount::from(amount),
            unit: CurrencyUnit::Sat,
            description: description.map(ToString::to_string),
            deadline,
            created_at,
        }
    }

    #[tokio::test]
    async fn test_get_pending_payment_request_empty_returns_none() {
        let repo = get_db("wallet-empty-pending-payment-request");

        let result = repo
            .get_pending_payment_request(Uuid::new_v4())
            .await
            .expect("get_pending_payment_request works");

        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_list_pending_payment_requests_empty_returns_empty_vec() {
        let repo = get_db("wallet-empty-pending-payment-request-list");

        let result = repo
            .list_pending_payment_requests()
            .await
            .expect("list_pending_payment_requests works");

        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn test_add_pending_payment_request_can_be_retrieved() {
        let repo = get_db("wallet-store-pending-payment-request");
        let id = Uuid::new_v4();

        let request = pending_payment_request(id, NODE_ID_1, 100, Some("Coffee"), None, 999);

        repo.add_pending_payment_request(request.clone())
            .await
            .expect("add_pending_payment_request works");

        let result = repo
            .get_pending_payment_request(id)
            .await
            .expect("get_pending_payment_request works")
            .expect("pending payment request exists");

        assert_eq!(result.id, request.id);
        assert_eq!(result.node_id, request.node_id);
        assert_eq!(result.amount, request.amount);
        assert_eq!(result.description, request.description);
        assert_eq!(result.deadline, request.deadline);
        assert_eq!(result.created_at, request.created_at);
    }

    #[tokio::test]
    async fn test_get_pending_payment_request_returns_none_for_unknown_id() {
        let repo = get_db("wallet-missing-pending-payment-request");

        repo.add_pending_payment_request(pending_payment_request(
            Uuid::new_v4(),
            NODE_ID_1,
            100,
            Some("Coffee"),
            None,
            999,
        ))
        .await
        .expect("add_pending_payment_request works");

        let result = repo
            .get_pending_payment_request(Uuid::new_v4())
            .await
            .expect("get_pending_payment_request works");

        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_add_pending_payment_request_rejects_duplicate_id() {
        let repo = get_db("wallet-duplicate-pending-payment-request");
        let id = Uuid::new_v4();

        let first = pending_payment_request(id, NODE_ID_1, 100, Some("Coffee"), None, 999);

        let duplicate = pending_payment_request(id, NODE_ID_2, 200, Some("Tea"), None, 1000);

        repo.add_pending_payment_request(first.clone())
            .await
            .expect("add first pending payment request works");

        let result = repo.add_pending_payment_request(duplicate).await;

        assert!(matches!(
            result,
            Err(Error::PendingPaymentRequestAlreadyExists(_))
        ));

        let stored = repo
            .get_pending_payment_request(id)
            .await
            .expect("get_pending_payment_request works")
            .expect("original pending payment request still exists");

        assert_eq!(stored.id, first.id);
        assert_eq!(stored.node_id, first.node_id);
        assert_eq!(stored.amount, first.amount);
        assert_eq!(stored.description, first.description);
        assert_eq!(stored.deadline, first.deadline);
        assert_eq!(stored.created_at, first.created_at);
    }

    #[tokio::test]
    async fn test_list_pending_payment_requests_returns_all_requests() {
        let repo = get_db("wallet-list-pending-payment-requests");

        let id_1 = Uuid::new_v4();
        let id_2 = Uuid::new_v4();

        repo.add_pending_payment_request(pending_payment_request(
            id_1,
            NODE_ID_1,
            100,
            Some("Coffee"),
            Some(2000),
            999,
        ))
        .await
        .expect("store first pending payment request works");

        repo.add_pending_payment_request(pending_payment_request(
            id_2,
            NODE_ID_2,
            200,
            Some("Tea"),
            None,
            1000,
        ))
        .await
        .expect("store second pending payment request works");

        let mut result = repo
            .list_pending_payment_requests()
            .await
            .expect("list_pending_payment_requests works");

        result.sort_by_key(|request| request.amount);

        assert_eq!(result.len(), 2);

        assert_eq!(result[0].id, id_1);
        assert_eq!(result[0].node_id, NodeId::from_str(NODE_ID_1).unwrap());
        assert_eq!(result[0].amount, Amount::from(100));
        assert_eq!(result[0].description, Some("Coffee".to_string()));
        assert_eq!(result[0].deadline, Some(2000));
        assert_eq!(result[0].created_at, 999);

        assert_eq!(result[1].id, id_2);
        assert_eq!(result[1].node_id, NodeId::from_str(NODE_ID_2).unwrap());
        assert_eq!(result[1].amount, Amount::from(200));
        assert_eq!(result[1].description, Some("Tea".to_string()));
        assert_eq!(result[1].deadline, None);
        assert_eq!(result[1].created_at, 1000);
    }

    #[tokio::test]
    async fn test_delete_pending_payment_request_removes_existing_request() {
        let repo = get_db("wallet-delete-pending-payment-request");
        let id = Uuid::new_v4();

        repo.add_pending_payment_request(pending_payment_request(
            id,
            NODE_ID_1,
            100,
            Some("Coffee"),
            None,
            999,
        ))
        .await
        .expect("add_pending_payment_request works");

        repo.delete_pending_payment_request(id)
            .await
            .expect("delete_pending_payment_request works");

        let result = repo
            .get_pending_payment_request(id)
            .await
            .expect("get_pending_payment_request works");

        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_delete_pending_payment_request_ignores_missing_request() {
        let repo = get_db("wallet-delete-missing-pending-payment-request");

        repo.delete_pending_payment_request(Uuid::new_v4())
            .await
            .expect("delete_pending_payment_request ignores missing requests");
    }

    #[tokio::test]
    async fn test_list_pending_payment_requests_after_delete_excludes_deleted_request() {
        let repo = get_db("wallet-list-after-delete-pending-payment-request");

        let id_1 = Uuid::new_v4();
        let id_2 = Uuid::new_v4();

        repo.add_pending_payment_request(pending_payment_request(
            id_1,
            NODE_ID_1,
            100,
            Some("Coffee"),
            None,
            999,
        ))
        .await
        .expect("store first pending payment request works");

        repo.add_pending_payment_request(pending_payment_request(
            id_2,
            NODE_ID_2,
            200,
            Some("Tea"),
            None,
            1000,
        ))
        .await
        .expect("store second pending payment request works");

        repo.delete_pending_payment_request(id_1)
            .await
            .expect("delete_pending_payment_request works");

        let result = repo
            .list_pending_payment_requests()
            .await
            .expect("list_pending_payment_requests works");

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, id_2);
    }

    #[tokio::test]
    async fn test_delete_repo_removes_all_pending_payment_requests() {
        let repo = get_db("wallet-delete-pending-payment-request-repo");

        repo.add_pending_payment_request(pending_payment_request(
            Uuid::new_v4(),
            NODE_ID_1,
            100,
            Some("Coffee"),
            None,
            999,
        ))
        .await
        .expect("store first pending payment request works");

        repo.add_pending_payment_request(pending_payment_request(
            Uuid::new_v4(),
            NODE_ID_2,
            200,
            Some("Tea"),
            None,
            1_600_000_001,
        ))
        .await
        .expect("store second pending payment request works");

        repo.delete_repo().await.expect("delete_repo works");

        let result = repo
            .list_pending_payment_requests()
            .await
            .expect("list_pending_payment_requests works");

        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn test_wallets_are_isolated() {
        let in_mem = InMemoryBackend::new();
        let db = Arc::new(
            Builder::new()
                .create_with_backend(in_mem)
                .expect("can create in-memory redb"),
        );

        let repo_a =
            PendingPaymentRequestDB::new(db.clone(), "wallet-a").expect("can create repo a");
        let repo_b =
            PendingPaymentRequestDB::new(db.clone(), "wallet-b").expect("can create repo b");

        let id = Uuid::new_v4();

        repo_a
            .add_pending_payment_request(pending_payment_request(
                id,
                NODE_ID_1,
                100,
                Some("Coffee"),
                None,
                999,
            ))
            .await
            .expect("store pending payment request in repo a works");

        assert!(
            repo_a
                .get_pending_payment_request(id)
                .await
                .expect("get_pending_payment_request repo a works")
                .is_some()
        );

        assert!(
            repo_b
                .get_pending_payment_request(id)
                .await
                .expect("get_pending_payment_request repo b works")
                .is_none()
        );

        assert_eq!(
            repo_a
                .list_pending_payment_requests()
                .await
                .expect("list_pending_payment_requests repo a works")
                .len(),
            1
        );

        assert!(
            repo_b
                .list_pending_payment_requests()
                .await
                .expect("list_pending_payment_requests repo b works")
                .is_empty()
        );
    }
}
