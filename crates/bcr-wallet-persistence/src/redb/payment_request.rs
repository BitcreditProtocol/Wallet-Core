use crate::{
    PaymentRequestStoreApi,
    error::{Error, Result},
};
use async_trait::async_trait;
use bcr_common::{
    cashu::{Amount, CurrencyUnit},
    core::NodeId,
};
use bcr_wallet_core::types::{PaymentRequest, PaymentRequestDirection, PaymentRequestState};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition, TableError};
use std::sync::Arc;
use tokio::task::spawn_blocking;
use uuid::Uuid;

#[derive(Debug, Clone, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum PaymentRequestEntryDirection {
    Incoming,
    Outgoing,
}

impl From<PaymentRequestDirection> for PaymentRequestEntryDirection {
    fn from(value: PaymentRequestDirection) -> Self {
        match value {
            PaymentRequestDirection::Incoming => PaymentRequestEntryDirection::Incoming,
            PaymentRequestDirection::Outgoing => PaymentRequestEntryDirection::Outgoing,
        }
    }
}

impl From<PaymentRequestEntryDirection> for PaymentRequestDirection {
    fn from(value: PaymentRequestEntryDirection) -> Self {
        match value {
            PaymentRequestEntryDirection::Incoming => PaymentRequestDirection::Incoming,
            PaymentRequestEntryDirection::Outgoing => PaymentRequestDirection::Outgoing,
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum PaymentRequestEntryState {
    Pending,
    Paid { tx_id: Uuid },
    Canceled,
    Rejected,
}

impl From<PaymentRequestState> for PaymentRequestEntryState {
    fn from(value: PaymentRequestState) -> Self {
        match value {
            PaymentRequestState::Pending => PaymentRequestEntryState::Pending,
            PaymentRequestState::Paid { tx_id } => PaymentRequestEntryState::Paid { tx_id },
            PaymentRequestState::Canceled => PaymentRequestEntryState::Canceled,
            PaymentRequestState::Rejected => PaymentRequestEntryState::Rejected,
        }
    }
}

impl From<PaymentRequestEntryState> for PaymentRequestState {
    fn from(value: PaymentRequestEntryState) -> Self {
        match value {
            PaymentRequestEntryState::Pending => PaymentRequestState::Pending,
            PaymentRequestEntryState::Paid { tx_id } => PaymentRequestState::Paid { tx_id },
            PaymentRequestEntryState::Canceled => PaymentRequestState::Canceled,
            PaymentRequestEntryState::Rejected => PaymentRequestState::Rejected,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PaymentRequestEntry {
    pub id: Uuid,
    pub node_id: NodeId,
    pub amount: Amount,
    pub unit: CurrencyUnit,
    pub description: Option<String>,
    pub deadline: Option<u64>,
    pub created_at: u64,
    pub state: PaymentRequestEntryState,
    pub direction: PaymentRequestEntryDirection,
}

impl From<PaymentRequest> for PaymentRequestEntry {
    fn from(value: PaymentRequest) -> Self {
        Self {
            id: value.id,
            node_id: value.node_id,
            amount: value.amount,
            unit: value.unit,
            description: value.description,
            deadline: value.deadline,
            created_at: value.created_at,
            state: value.state.into(),
            direction: value.direction.into(),
        }
    }
}

impl From<PaymentRequestEntry> for PaymentRequest {
    fn from(value: PaymentRequestEntry) -> Self {
        Self {
            id: value.id,
            node_id: value.node_id,
            amount: value.amount,
            unit: value.unit,
            description: value.description,
            deadline: value.deadline,
            created_at: value.created_at,
            state: value.state.into(),
            direction: value.direction.into(),
        }
    }
}

///////////////////////////////////////////// PaymentRequestDB
pub struct PaymentRequestDB {
    db: Arc<Database>,
    payment_requests_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
}

impl PaymentRequestDB {
    const PAYMENT_REQUESTS_DB_NAME: &'static str = "pending_incoming_payment_requests";

    pub fn new(db: Arc<Database>, wallet_id: &str) -> Result<Self> {
        // Leak once to get static string, because of dynamically generated table names
        let payment_requests_table_name: &'static str =
            Box::leak(format!("{wallet_id}_{}", Self::PAYMENT_REQUESTS_DB_NAME).into_boxed_str());

        let payment_requests_table = TableDefinition::new(payment_requests_table_name);

        Ok(Self {
            db,
            payment_requests_table,
        })
    }

    fn add_payment_request_sync(
        db: Arc<Database>,
        payment_requests_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        entry: PaymentRequestEntry,
    ) -> Result<()> {
        let id = entry.id;
        let write_txn = db.begin_write()?;

        {
            let mut table = write_txn.open_table(payment_requests_table)?;
            let old_value = table.get(id.as_bytes().as_slice())?.map(|v| v.value());

            if old_value.is_some() {
                return Err(Error::PaymentRequestAlreadyExists(id.to_string()));
            }
            let mut serialized = Vec::new();
            ciborium::into_writer(&entry, &mut serialized)?;
            table.insert(id.as_bytes().as_slice(), serialized)?;
        }

        write_txn.commit()?;
        Ok(())
    }

    fn get_payment_request_sync(
        db: Arc<Database>,
        payment_requests_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        id: Uuid,
    ) -> Result<Option<PaymentRequestEntry>> {
        let read_txn = db.begin_read()?;

        match read_txn.open_table(payment_requests_table) {
            Ok(table) => {
                let entry = table.get(id.as_bytes().as_slice())?;
                match entry {
                    Some(e) => {
                        let payment_request: PaymentRequestEntry =
                            ciborium::from_reader(e.value().as_slice())?;
                        Ok(Some(payment_request))
                    }
                    None => Ok(None),
                }
            }
            Err(TableError::TableDoesNotExist(_)) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn list_payment_requests_sync(
        db: Arc<Database>,
        payment_requests_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        direction: PaymentRequestEntryDirection,
        states: &[PaymentRequestEntryState],
    ) -> Result<Vec<PaymentRequestEntry>> {
        let read_txn = db.begin_read()?;
        let all_states = states.is_empty();

        match read_txn.open_table(payment_requests_table) {
            Ok(table) => {
                let mut res = Vec::new();
                for (_, v) in table.range::<&[u8]>(..)?.flatten() {
                    let entry: PaymentRequestEntry = ciborium::from_reader(v.value().as_slice())?;
                    let state_matches = all_states
                        || states.iter().any(|s| {
                            // match without specifically matching on tx_id
                            matches!(
                                (s, &entry.state),
                                (
                                    PaymentRequestEntryState::Pending,
                                    PaymentRequestEntryState::Pending,
                                ) | (
                                    PaymentRequestEntryState::Paid { .. },
                                    PaymentRequestEntryState::Paid { .. },
                                ) | (
                                    PaymentRequestEntryState::Canceled,
                                    PaymentRequestEntryState::Canceled,
                                ) | (
                                    PaymentRequestEntryState::Rejected,
                                    PaymentRequestEntryState::Rejected,
                                )
                            )
                        });
                    if entry.direction == direction && state_matches {
                        res.push(entry);
                    }
                }
                Ok(res)
            }
            Err(TableError::TableDoesNotExist(_)) => Ok(vec![]),
            Err(e) => Err(e.into()),
        }
    }

    fn set_payment_request_state_sync(
        db: Arc<Database>,
        payment_requests_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        id: Uuid,
        state: PaymentRequestEntryState,
    ) -> Result<()> {
        let write_txn = db.begin_write()?;

        {
            let mut table = write_txn.open_table(payment_requests_table)?;
            let old_value = table.get(id.as_bytes().as_slice())?.map(|v| v.value());

            if let Some(old_value) = old_value {
                let mut entry: PaymentRequestEntry = ciborium::from_reader(old_value.as_slice())?;

                entry.state = state;

                let mut serialized = Vec::new();
                ciborium::into_writer(&entry, &mut serialized)?;
                table.insert(id.as_bytes().as_slice(), serialized)?;
            } else {
                return Err(Error::PaymentRequestNotFound(id.to_string()));
            }
        }

        write_txn.commit()?;
        Ok(())
    }

    fn delete_repo_sync(
        db: Arc<Database>,
        payment_requests_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
    ) -> Result<()> {
        let write_txn = db.begin_write()?;

        {
            if write_txn.open_table(payment_requests_table).is_ok() {
                write_txn.delete_table(payment_requests_table)?;
            }
        }

        write_txn.commit()?;
        Ok(())
    }
}

#[async_trait]
impl PaymentRequestStoreApi for PaymentRequestDB {
    async fn add_payment_request(&self, payment_request: PaymentRequest) -> Result<()> {
        let db_clone = self.db.clone();
        let table = self.payment_requests_table;
        let res = spawn_blocking(move || {
            Self::add_payment_request_sync(db_clone, table, payment_request.into())
        })
        .await??;
        Ok(res)
    }

    async fn get_payment_request(&self, id: Uuid) -> Result<Option<PaymentRequest>> {
        let db_clone = self.db.clone();
        let table = self.payment_requests_table;
        let res =
            spawn_blocking(move || Self::get_payment_request_sync(db_clone, table, id)).await??;
        Ok(res.map(|c| c.into()))
    }

    async fn list_payment_requests(
        &self,
        direction: PaymentRequestDirection,
        states: &[PaymentRequestState],
    ) -> Result<Vec<PaymentRequest>> {
        let db_clone = self.db.clone();
        let table = self.payment_requests_table;
        let states: Vec<PaymentRequestEntryState> =
            states.iter().map(|s| s.to_owned().into()).collect();
        let res = spawn_blocking(move || {
            Self::list_payment_requests_sync(db_clone, table, direction.into(), &states)
        })
        .await??;
        Ok(res.into_iter().map(|entry| entry.into()).collect())
    }

    async fn set_payment_request_state(&self, id: Uuid, state: PaymentRequestState) -> Result<()> {
        let db_clone = self.db.clone();
        let table = self.payment_requests_table;
        spawn_blocking(move || {
            Self::set_payment_request_state_sync(db_clone, table, id, state.into())
        })
        .await??;
        Ok(())
    }

    async fn delete_repo(&self) -> Result<()> {
        let db_clone = self.db.clone();
        let payment_requests_table = self.payment_requests_table;
        spawn_blocking(move || Self::delete_repo_sync(db_clone, payment_requests_table)).await?
    }
}

#[cfg(test)]
mod tests {
    use crate::{PaymentRequestStoreApi, error::Error, test_utils::tests::wallet_id};

    use super::*;
    use bcr_common::cashu::{Amount, CurrencyUnit};
    use bcr_wallet_core::types::{PaymentRequest, PaymentRequestDirection, PaymentRequestState};
    use chrono::Utc;
    use redb::{Builder, backends::InMemoryBackend};
    use std::{str::FromStr, sync::Arc};
    use uuid::Uuid;

    const NODE_ID_1: &str =
        "bitcrt03205b8dec12bc9e879f5b517aa32192a2550e88adcee3e54ec2c7294802568fef";

    fn get_db(wallet_id: &str) -> PaymentRequestDB {
        let in_mem = InMemoryBackend::new();
        let db = Arc::new(
            Builder::new()
                .create_with_backend(in_mem)
                .expect("can create in-memory redb"),
        );
        PaymentRequestDB::new(db, wallet_id).expect("can create PaymentRequestDB")
    }

    fn test_payment_request() -> PaymentRequest {
        PaymentRequest {
            id: Uuid::new_v4(),
            node_id: NodeId::from_str(NODE_ID_1).unwrap(),
            amount: Amount::from(42u64),
            unit: CurrencyUnit::Sat,
            description: Some("some description".to_string()),
            deadline: Some(Utc::now().timestamp() as u64 + 3600),
            created_at: Utc::now().timestamp() as u64,
            state: PaymentRequestState::Pending,
            direction: PaymentRequestDirection::Incoming,
        }
    }

    #[tokio::test]
    async fn test_get_missing_returns_none() {
        let repo = get_db(&wallet_id());

        let res = repo
            .get_payment_request(Uuid::new_v4())
            .await
            .expect("get_payment_request works");

        assert_eq!(res, None);
    }

    #[tokio::test]
    async fn test_list_empty() {
        let repo = get_db(&wallet_id());

        let incoming = repo
            .list_payment_requests(PaymentRequestDirection::Incoming, &[])
            .await
            .expect("list_payment_requests works");
        assert!(incoming.is_empty());

        let outgoing = repo
            .list_payment_requests(PaymentRequestDirection::Outgoing, &[])
            .await
            .expect("list_payment_requests works");
        assert!(outgoing.is_empty());
    }

    #[tokio::test]
    async fn test_add_and_get_payment_request() {
        let repo = get_db(&wallet_id());

        let payment_request = test_payment_request();
        let id = payment_request.id;

        repo.add_payment_request(payment_request.clone())
            .await
            .expect("add_payment_request works");

        let loaded = repo
            .get_payment_request(id)
            .await
            .expect("get_payment_request works");

        assert_eq!(loaded, Some(payment_request));
    }

    #[tokio::test]
    async fn test_add_duplicate_returns_error() {
        let repo = get_db(&wallet_id());

        let payment_request = test_payment_request();
        let id = payment_request.id;

        repo.add_payment_request(payment_request.clone())
            .await
            .expect("add_payment_request works");

        let err = repo.add_payment_request(payment_request).await.unwrap_err();

        match err {
            Error::PaymentRequestAlreadyExists(err_id) => {
                assert_eq!(err_id, id.to_string());
            }
            other => panic!("expected PaymentRequestAlreadyExists, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_list_filters_by_direction() {
        let repo = get_db(&wallet_id());

        let incoming = test_payment_request();

        let mut outgoing = test_payment_request();
        outgoing.direction = PaymentRequestDirection::Outgoing;

        repo.add_payment_request(incoming.clone()).await.unwrap();
        repo.add_payment_request(outgoing.clone()).await.unwrap();

        let incoming_res = repo
            .list_payment_requests(PaymentRequestDirection::Incoming, &[])
            .await
            .unwrap();
        assert_eq!(incoming_res, vec![incoming]);

        let outgoing_res = repo
            .list_payment_requests(PaymentRequestDirection::Outgoing, &[])
            .await
            .unwrap();
        assert_eq!(outgoing_res, vec![outgoing]);
    }

    #[tokio::test]
    async fn test_list_filters_by_state() {
        let repo = get_db(&wallet_id());

        let pending = test_payment_request();

        let mut canceled = test_payment_request();
        canceled.state = PaymentRequestState::Canceled;

        let mut rejected = test_payment_request();
        rejected.state = PaymentRequestState::Rejected;

        repo.add_payment_request(pending.clone()).await.unwrap();
        repo.add_payment_request(canceled.clone()).await.unwrap();
        repo.add_payment_request(rejected.clone()).await.unwrap();

        let pending_res = repo
            .list_payment_requests(
                PaymentRequestDirection::Incoming,
                &[PaymentRequestState::Pending],
            )
            .await
            .unwrap();
        assert_eq!(pending_res, vec![pending]);

        let canceled_or_rejected = repo
            .list_payment_requests(
                PaymentRequestDirection::Incoming,
                &[PaymentRequestState::Canceled, PaymentRequestState::Rejected],
            )
            .await
            .unwrap();

        assert_eq!(canceled_or_rejected.len(), 2);
        assert!(canceled_or_rejected.contains(&canceled));
        assert!(canceled_or_rejected.contains(&rejected));
    }

    #[tokio::test]
    async fn test_list_empty_states_returns_all_for_direction() {
        let repo = get_db(&wallet_id());

        let pending = test_payment_request();

        let mut canceled = test_payment_request();
        canceled.state = PaymentRequestState::Canceled;

        let mut outgoing = test_payment_request();
        outgoing.direction = PaymentRequestDirection::Outgoing;

        repo.add_payment_request(pending.clone()).await.unwrap();
        repo.add_payment_request(canceled.clone()).await.unwrap();
        repo.add_payment_request(outgoing).await.unwrap();

        let res = repo
            .list_payment_requests(PaymentRequestDirection::Incoming, &[])
            .await
            .unwrap();

        assert_eq!(res.len(), 2);
        assert!(res.contains(&pending));
        assert!(res.contains(&canceled));
    }

    #[tokio::test]
    async fn test_set_payment_request_state() {
        let repo = get_db(&wallet_id());

        let payment_request = test_payment_request();
        let id = payment_request.id;

        repo.add_payment_request(payment_request).await.unwrap();

        let tx_id = Uuid::new_v4();
        repo.set_payment_request_state(id, PaymentRequestState::Paid { tx_id })
            .await
            .expect("set_payment_request_state works");

        let loaded = repo.get_payment_request(id).await.unwrap().unwrap();
        assert_eq!(loaded.state, PaymentRequestState::Paid { tx_id });
    }

    #[tokio::test]
    async fn test_set_payment_request_state_missing_returns_error() {
        let repo = get_db(&wallet_id());

        let id = Uuid::new_v4();

        let err = repo
            .set_payment_request_state(id, PaymentRequestState::Canceled)
            .await
            .unwrap_err();

        match err {
            Error::PaymentRequestNotFound(err_id) => {
                assert_eq!(err_id, id.to_string());
            }
            other => panic!("expected PaymentRequestNotFound, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_delete_repo_removes_all_payment_requests() {
        let repo = get_db(&wallet_id());

        let payment_request = test_payment_request();
        let id = payment_request.id;

        repo.add_payment_request(payment_request).await.unwrap();

        assert!(repo.get_payment_request(id).await.unwrap().is_some());

        repo.delete_repo().await.expect("delete_repo works");

        assert_eq!(repo.get_payment_request(id).await.unwrap(), None);

        let res = repo
            .list_payment_requests(PaymentRequestDirection::Incoming, &[])
            .await
            .unwrap();
        assert!(res.is_empty());
    }
}
