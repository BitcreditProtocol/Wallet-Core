use crate::{
    PaymentRequestStoreApi,
    error::{Error, Result},
};
use async_trait::async_trait;
use bcr_common::{
    cashu::{Amount, CurrencyUnit},
    core::NodeId,
};
use bcr_wallet_core::types::{
    PaymentRequest, PaymentRequestActionOrigin, PaymentRequestDirection,
    PaymentRequestHistoryEntry, PaymentRequestState, PaymentRequestTransition,
    PaymentRequestTransitionOutcome,
};
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

#[derive(Debug, Clone, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum PaymentRequestEntryOrigin {
    Local,
    Remote { event_id: String },
}

impl From<PaymentRequestActionOrigin> for PaymentRequestEntryOrigin {
    fn from(value: PaymentRequestActionOrigin) -> Self {
        match value {
            PaymentRequestActionOrigin::Local => PaymentRequestEntryOrigin::Local,
            PaymentRequestActionOrigin::Remote { event_id } => {
                PaymentRequestEntryOrigin::Remote { event_id }
            }
        }
    }
}

impl From<PaymentRequestEntryOrigin> for PaymentRequestActionOrigin {
    fn from(value: PaymentRequestEntryOrigin) -> Self {
        match value {
            PaymentRequestEntryOrigin::Local => PaymentRequestActionOrigin::Local,
            PaymentRequestEntryOrigin::Remote { event_id } => {
                PaymentRequestActionOrigin::Remote { event_id }
            }
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PaymentRequestEntryHistoryEntry {
    pub state: PaymentRequestEntryState,
    pub applied: bool,
    pub actor: Option<NodeId>,
    pub at: u64,
    pub origin: PaymentRequestEntryOrigin,
    pub reason: Option<String>,
}

impl From<PaymentRequestHistoryEntry> for PaymentRequestEntryHistoryEntry {
    fn from(value: PaymentRequestHistoryEntry) -> Self {
        Self {
            state: value.state.into(),
            applied: value.applied,
            actor: value.actor,
            at: value.at,
            origin: value.origin.into(),
            reason: value.reason,
        }
    }
}

impl From<PaymentRequestEntryHistoryEntry> for PaymentRequestHistoryEntry {
    fn from(value: PaymentRequestEntryHistoryEntry) -> Self {
        Self {
            state: value.state.into(),
            applied: value.applied,
            actor: value.actor,
            at: value.at,
            origin: value.origin.into(),
            reason: value.reason,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PaymentRequestEntry {
    pub id: Uuid,
    #[serde(default)]
    pub node_id: Option<NodeId>,
    pub amount: Amount,
    pub unit: CurrencyUnit,
    pub description: Option<String>,
    pub deadline: Option<u64>,
    pub created_at: u64,
    pub state: PaymentRequestEntryState,
    pub direction: PaymentRequestEntryDirection,
    /// Absent in rows written by older versions.
    #[serde(default)]
    pub history: Vec<PaymentRequestEntryHistoryEntry>,
    #[serde(default)]
    pub tombstone: bool,
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
            history: value.history.into_iter().map(Into::into).collect(),
            tombstone: value.tombstone,
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
            history: value.history.into_iter().map(Into::into).collect(),
            tombstone: value.tombstone,
        }
    }
}

fn apply_transition(
    entry: &mut PaymentRequestEntry,
    transition: PaymentRequestTransition,
) -> PaymentRequestTransitionOutcome {
    use PaymentRequestEntryState::*;
    let target: PaymentRequestEntryState = transition.target_state.into();
    if entry
        .history
        .iter()
        .any(|h| h.state == target && h.actor == transition.actor)
    {
        return PaymentRequestTransitionOutcome::AlreadyApplied;
    }
    let applied = match (&entry.state, &target) {
        (Pending, Paid { .. } | Canceled | Rejected) | (Canceled | Rejected, Paid { .. }) => true,
        (a, b) if a == b => return PaymentRequestTransitionOutcome::AlreadyApplied,
        _ => false,
    };
    if applied {
        entry.state = target.clone();
    }
    entry.history.push(PaymentRequestEntryHistoryEntry {
        state: target,
        applied,
        actor: transition.actor,
        at: transition.at,
        origin: transition.origin.into(),
        reason: transition.reason,
    });
    if applied {
        PaymentRequestTransitionOutcome::Applied
    } else {
        PaymentRequestTransitionOutcome::Conflicted
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

            let to_store = match old_value {
                None => entry,
                Some(old_value) => {
                    let existing: PaymentRequestEntry =
                        ciborium::from_reader(old_value.as_slice())?;
                    if !existing.tombstone {
                        return Err(Error::PaymentRequestAlreadyExists(id.to_string()));
                    }
                    if existing.node_id == entry.node_id && existing.direction == entry.direction {
                        PaymentRequestEntry {
                            state: existing.state,
                            history: existing.history,
                            ..entry
                        }
                    } else {
                        tracing::warn!(
                            "Dropping tombstone for payment request {id}: actor {:?} does not match the request's real sender {:?}",
                            existing.node_id,
                            entry.node_id
                        );
                        entry
                    }
                }
            };

            let mut serialized = Vec::new();
            ciborium::into_writer(&to_store, &mut serialized)?;
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
                        if payment_request.tombstone {
                            Ok(None)
                        } else {
                            Ok(Some(payment_request))
                        }
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
                    if !entry.tombstone && entry.direction == direction && state_matches {
                        res.push(entry);
                    }
                }
                res.sort_by_key(|entry| (entry.created_at, entry.id));
                Ok(res)
            }
            Err(TableError::TableDoesNotExist(_)) => Ok(vec![]),
            Err(e) => Err(e.into()),
        }
    }

    fn apply_payment_request_transition_sync(
        db: Arc<Database>,
        payment_requests_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        id: Uuid,
        counterparty: Option<(NodeId, PaymentRequestDirection)>,
        transition: PaymentRequestTransition,
    ) -> Result<Option<PaymentRequestTransitionOutcome>> {
        let write_txn = db.begin_write()?;
        let outcome;

        {
            let mut table = write_txn.open_table(payment_requests_table)?;
            let stored = table
                .get(id.as_bytes().as_slice())?
                .map(|v| ciborium::from_reader::<PaymentRequestEntry, _>(v.value().as_slice()))
                .transpose()?;

            let mut entry = match (stored, &counterparty) {
                (Some(entry), Some(_)) => entry,
                (Some(entry), None) if !entry.tombstone => entry,
                (None, Some((node_id, direction))) => PaymentRequest::new_tombstone(
                    id,
                    node_id.clone(),
                    direction.clone(),
                    transition.at,
                )
                .into(),
                _ => return Err(Error::PaymentRequestNotFound(id.to_string())),
            };
            if let Some((node_id, direction)) = counterparty
                && (entry.node_id.as_ref() != Some(&node_id) || entry.direction != direction.into())
            {
                tracing::warn!(
                    "Dropping transition for payment request {id}: {node_id} is not its counterparty"
                );
                return Ok(None);
            }

            let result = apply_transition(&mut entry, transition);
            if result != PaymentRequestTransitionOutcome::AlreadyApplied {
                let mut serialized = Vec::new();
                ciborium::into_writer(&entry, &mut serialized)?;
                table.insert(id.as_bytes().as_slice(), serialized)?;
            }
            outcome = (!entry.tombstone).then_some(result);
        }

        write_txn.commit()?;
        Ok(outcome)
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

    async fn apply_payment_request_transition(
        &self,
        id: Uuid,
        transition: PaymentRequestTransition,
    ) -> Result<PaymentRequestTransitionOutcome> {
        let db_clone = self.db.clone();
        let table = self.payment_requests_table;
        spawn_blocking(move || {
            Self::apply_payment_request_transition_sync(db_clone, table, id, None, transition)
        })
        .await??
        .ok_or(Error::PaymentRequestNotFound(id.to_string()))
    }

    async fn apply_remote_payment_request_transition(
        &self,
        id: Uuid,
        counterparty: NodeId,
        direction: PaymentRequestDirection,
        transition: PaymentRequestTransition,
    ) -> Result<Option<PaymentRequestTransitionOutcome>> {
        let db_clone = self.db.clone();
        let table = self.payment_requests_table;
        spawn_blocking(move || {
            Self::apply_payment_request_transition_sync(
                db_clone,
                table,
                id,
                Some((counterparty, direction)),
                transition,
            )
        })
        .await?
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

    #[test]
    fn entry_stored_with_a_required_node_id_reads_as_some() {
        #[derive(serde::Serialize)]
        struct LegacyEntry {
            id: Uuid,
            node_id: NodeId,
            amount: Amount,
            unit: CurrencyUnit,
            description: Option<String>,
            deadline: Option<u64>,
            created_at: u64,
            state: PaymentRequestEntryState,
            direction: PaymentRequestEntryDirection,
        }
        let node_id = NodeId::from_str(NODE_ID_1).unwrap();
        let mut bytes = Vec::new();
        ciborium::into_writer(
            &LegacyEntry {
                id: Uuid::new_v4(),
                node_id: node_id.clone(),
                amount: Amount::from(42u64),
                unit: CurrencyUnit::Sat,
                description: None,
                deadline: None,
                created_at: 1,
                state: PaymentRequestEntryState::Pending,
                direction: PaymentRequestEntryDirection::Incoming,
            },
            &mut bytes,
        )
        .unwrap();

        let entry: PaymentRequestEntry = ciborium::from_reader(bytes.as_slice()).unwrap();
        assert_eq!(entry.node_id, Some(node_id));
    }

    fn test_payment_request() -> PaymentRequest {
        PaymentRequest {
            id: Uuid::new_v4(),
            node_id: Some(NodeId::from_str(NODE_ID_1).unwrap()),
            amount: Amount::from(42u64),
            unit: CurrencyUnit::Sat,
            description: Some("some description".to_string()),
            deadline: Some(time::OffsetDateTime::now_utc().unix_timestamp() as u64 + 3600),
            created_at: time::OffsetDateTime::now_utc().unix_timestamp() as u64,
            state: PaymentRequestState::Pending,
            direction: PaymentRequestDirection::Incoming,
            history: Vec::new(),
            tombstone: false,
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

    fn local_transition(target_state: PaymentRequestState) -> PaymentRequestTransition {
        PaymentRequestTransition {
            target_state,
            actor: Some(NodeId::from_str(NODE_ID_1).unwrap()),
            at: 123,
            origin: PaymentRequestActionOrigin::Local,
            reason: None,
        }
    }

    #[tokio::test]
    async fn test_apply_transition_pending_to_paid_applies_and_records_history() {
        let repo = get_db(&wallet_id());

        let payment_request = test_payment_request();
        let id = payment_request.id;

        repo.add_payment_request(payment_request).await.unwrap();

        let tx_id = Uuid::new_v4();
        let outcome = repo
            .apply_payment_request_transition(
                id,
                local_transition(PaymentRequestState::Paid { tx_id }),
            )
            .await
            .expect("apply_payment_request_transition works");
        assert_eq!(outcome, PaymentRequestTransitionOutcome::Applied);

        let loaded = repo.get_payment_request(id).await.unwrap().unwrap();
        assert_eq!(loaded.state, PaymentRequestState::Paid { tx_id });
        assert_eq!(loaded.history.len(), 1);
        assert!(loaded.history[0].applied);
        assert_eq!(loaded.history[0].state, PaymentRequestState::Paid { tx_id });
    }

    #[tokio::test]
    async fn test_apply_transition_missing_returns_error() {
        let repo = get_db(&wallet_id());

        let id = Uuid::new_v4();

        let err = repo
            .apply_payment_request_transition(id, local_transition(PaymentRequestState::Canceled))
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
    async fn test_apply_transition_paid_wins_over_canceled() {
        let repo = get_db(&wallet_id());
        let payment_request = test_payment_request();
        let id = payment_request.id;
        repo.add_payment_request(payment_request).await.unwrap();

        repo.apply_payment_request_transition(id, local_transition(PaymentRequestState::Canceled))
            .await
            .unwrap();

        let tx_id = Uuid::new_v4();
        let outcome = repo
            .apply_payment_request_transition(
                id,
                local_transition(PaymentRequestState::Paid { tx_id }),
            )
            .await
            .unwrap();
        assert_eq!(outcome, PaymentRequestTransitionOutcome::Applied);

        let loaded = repo.get_payment_request(id).await.unwrap().unwrap();
        assert_eq!(loaded.state, PaymentRequestState::Paid { tx_id });
        assert_eq!(loaded.history.len(), 2);
    }

    #[tokio::test]
    async fn test_apply_transition_late_cancel_after_paid_is_conflicted_not_applied() {
        let repo = get_db(&wallet_id());
        let payment_request = test_payment_request();
        let id = payment_request.id;
        repo.add_payment_request(payment_request).await.unwrap();

        let tx_id = Uuid::new_v4();
        repo.apply_payment_request_transition(
            id,
            local_transition(PaymentRequestState::Paid { tx_id }),
        )
        .await
        .unwrap();

        let outcome = repo
            .apply_payment_request_transition(id, local_transition(PaymentRequestState::Canceled))
            .await
            .unwrap();
        assert_eq!(outcome, PaymentRequestTransitionOutcome::Conflicted);

        let loaded = repo.get_payment_request(id).await.unwrap().unwrap();
        assert_eq!(loaded.state, PaymentRequestState::Paid { tx_id });
        assert_eq!(loaded.history.len(), 2);
        assert!(loaded.history[0].applied);
        assert!(!loaded.history[1].applied);
        assert_eq!(loaded.history[1].state, PaymentRequestState::Canceled);
    }

    #[tokio::test]
    async fn test_apply_transition_duplicate_is_idempotent_no_op() {
        let repo = get_db(&wallet_id());
        let payment_request = test_payment_request();
        let id = payment_request.id;
        repo.add_payment_request(payment_request).await.unwrap();

        repo.apply_payment_request_transition(id, local_transition(PaymentRequestState::Rejected))
            .await
            .unwrap();
        let outcome = repo
            .apply_payment_request_transition(id, local_transition(PaymentRequestState::Rejected))
            .await
            .unwrap();
        assert_eq!(outcome, PaymentRequestTransitionOutcome::AlreadyApplied);

        let loaded = repo.get_payment_request(id).await.unwrap().unwrap();
        assert_eq!(loaded.history.len(), 1);
    }

    #[tokio::test]
    async fn test_apply_transition_remote_history_round_trips_actor_time_origin_reason() {
        let repo = get_db(&wallet_id());
        let payment_request = test_payment_request();
        let id = payment_request.id;
        repo.add_payment_request(payment_request).await.unwrap();

        let actor = NodeId::from_str(NODE_ID_1).unwrap();
        let origin = PaymentRequestActionOrigin::Remote {
            event_id: "evt-1".to_string(),
        };
        repo.apply_payment_request_transition(
            id,
            PaymentRequestTransition {
                target_state: PaymentRequestState::Canceled,
                actor: Some(actor.clone()),
                at: 1_700_000_123,
                origin: origin.clone(),
                reason: Some("no longer needed".to_string()),
            },
        )
        .await
        .unwrap();

        let loaded = repo.get_payment_request(id).await.unwrap().unwrap();
        assert_eq!(loaded.state, PaymentRequestState::Canceled);
        assert_eq!(loaded.history.len(), 1);
        let entry = &loaded.history[0];
        assert_eq!(entry.state, PaymentRequestState::Canceled);
        assert!(entry.applied);
        assert_eq!(entry.actor, Some(actor));
        assert_eq!(entry.at, 1_700_000_123);
        assert_eq!(entry.origin, origin);
        assert_eq!(entry.reason, Some("no longer needed".to_string()));
    }

    #[tokio::test]
    async fn test_apply_transition_same_remote_event_twice_records_it_once() {
        let repo = get_db(&wallet_id());
        let payment_request = test_payment_request();
        let id = payment_request.id;
        repo.add_payment_request(payment_request).await.unwrap();
        repo.apply_payment_request_transition(
            id,
            local_transition(PaymentRequestState::Paid {
                tx_id: Uuid::new_v4(),
            }),
        )
        .await
        .unwrap();

        let remote_cancel = PaymentRequestTransition {
            target_state: PaymentRequestState::Canceled,
            actor: Some(NodeId::from_str(NODE_ID_1).unwrap()),
            at: 123,
            origin: PaymentRequestActionOrigin::Remote {
                event_id: "evt-1".to_string(),
            },
            reason: None,
        };
        let first = repo
            .apply_payment_request_transition(id, remote_cancel.clone())
            .await
            .unwrap();
        let second = repo
            .apply_payment_request_transition(id, remote_cancel.clone())
            .await
            .unwrap();
        let retried = repo
            .apply_payment_request_transition(
                id,
                PaymentRequestTransition {
                    origin: PaymentRequestActionOrigin::Remote {
                        event_id: "evt-2".to_string(),
                    },
                    ..remote_cancel
                },
            )
            .await
            .unwrap();
        assert_eq!(first, PaymentRequestTransitionOutcome::Conflicted);
        assert_eq!(second, PaymentRequestTransitionOutcome::AlreadyApplied);
        assert_eq!(retried, PaymentRequestTransitionOutcome::AlreadyApplied);

        let loaded = repo.get_payment_request(id).await.unwrap().unwrap();
        assert_eq!(loaded.history.len(), 2);
    }

    #[tokio::test]
    async fn test_tombstone_is_hidden_from_get_and_list_until_the_real_request_arrives() {
        let repo = get_db(&wallet_id());
        let id = Uuid::new_v4();
        let actor = NodeId::from_str(NODE_ID_1).unwrap();

        let tombstone =
            PaymentRequest::new_tombstone(id, actor, PaymentRequestDirection::Incoming, 1);
        repo.add_payment_request(tombstone).await.unwrap();

        assert_eq!(repo.get_payment_request(id).await.unwrap(), None);
        let listed = repo
            .list_payment_requests(PaymentRequestDirection::Incoming, &[])
            .await
            .unwrap();
        assert!(listed.is_empty());
    }

    fn remote_cancel(event_id: &str) -> PaymentRequestTransition {
        PaymentRequestTransition {
            target_state: PaymentRequestState::Canceled,
            actor: Some(NodeId::from_str(NODE_ID_1).unwrap()),
            at: 2,
            origin: PaymentRequestActionOrigin::Remote {
                event_id: event_id.to_string(),
            },
            reason: None,
        }
    }

    #[tokio::test]
    async fn test_tombstone_with_mismatched_actor_is_dropped() {
        let repo = get_db(&wallet_id());
        let id = Uuid::new_v4();
        let forged_actor = NodeId::from_str(
            "bitcrt0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798",
        )
        .unwrap();
        repo.apply_remote_payment_request_transition(
            id,
            forged_actor.clone(),
            PaymentRequestDirection::Incoming,
            PaymentRequestTransition {
                actor: Some(forged_actor),
                ..remote_cancel("evt1")
            },
        )
        .await
        .unwrap();

        let mut real_request = test_payment_request();
        real_request.id = id;
        repo.add_payment_request(real_request).await.unwrap();

        let loaded = repo.get_payment_request(id).await.unwrap().unwrap();
        assert!(!loaded.tombstone);
        assert_eq!(loaded.state, PaymentRequestState::Pending);
        assert!(loaded.history.is_empty());
    }

    #[tokio::test]
    async fn test_remote_transition_for_unknown_id_is_tombstoned_and_idempotent() {
        let repo = get_db(&wallet_id());
        let id = Uuid::new_v4();
        let actor = NodeId::from_str(NODE_ID_1).unwrap();

        for event_id in ["evt1", "evt1", "evt2"] {
            let outcome = repo
                .apply_remote_payment_request_transition(
                    id,
                    actor.clone(),
                    PaymentRequestDirection::Incoming,
                    remote_cancel(event_id),
                )
                .await
                .unwrap();
            assert_eq!(outcome, None);
        }
        assert_eq!(repo.get_payment_request(id).await.unwrap(), None);

        let mut real_request = test_payment_request();
        real_request.id = id;
        real_request.node_id = Some(actor);
        repo.add_payment_request(real_request).await.unwrap();
        let loaded = repo.get_payment_request(id).await.unwrap().unwrap();
        assert_eq!(loaded.state, PaymentRequestState::Canceled);
        assert_eq!(loaded.history.len(), 1);
    }

    #[tokio::test]
    async fn test_remote_transition_applies_only_for_the_counterparty_and_direction() {
        let repo = get_db(&wallet_id());
        let payment_request = test_payment_request();
        let id = payment_request.id;
        let counterparty = payment_request.node_id.clone().unwrap();
        repo.add_payment_request(payment_request).await.unwrap();
        let other = NodeId::from_str(
            "bitcrt0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798",
        )
        .unwrap();

        for (node_id, direction) in [
            (other, PaymentRequestDirection::Incoming),
            (counterparty.clone(), PaymentRequestDirection::Outgoing),
        ] {
            let outcome = repo
                .apply_remote_payment_request_transition(
                    id,
                    node_id,
                    direction,
                    remote_cancel("evt1"),
                )
                .await
                .unwrap();
            assert_eq!(outcome, None);
        }
        let loaded = repo.get_payment_request(id).await.unwrap().unwrap();
        assert_eq!(loaded.state, PaymentRequestState::Pending);
        assert!(loaded.history.is_empty());

        let outcome = repo
            .apply_remote_payment_request_transition(
                id,
                counterparty,
                PaymentRequestDirection::Incoming,
                remote_cancel("evt1"),
            )
            .await
            .unwrap();
        assert_eq!(outcome, Some(PaymentRequestTransitionOutcome::Applied));
        let loaded = repo.get_payment_request(id).await.unwrap().unwrap();
        assert_eq!(loaded.state, PaymentRequestState::Canceled);
    }

    #[tokio::test]
    async fn test_local_transition_on_tombstone_is_not_found_and_not_written() {
        let repo = get_db(&wallet_id());
        let id = Uuid::new_v4();
        let actor = NodeId::from_str(NODE_ID_1).unwrap();
        repo.add_payment_request(PaymentRequest::new_tombstone(
            id,
            actor.clone(),
            PaymentRequestDirection::Incoming,
            1,
        ))
        .await
        .unwrap();

        let err = repo
            .apply_payment_request_transition(id, local_transition(PaymentRequestState::Rejected))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::PaymentRequestNotFound(_)));

        let mut real_request = test_payment_request();
        real_request.id = id;
        real_request.node_id = Some(actor);
        repo.add_payment_request(real_request).await.unwrap();
        let loaded = repo.get_payment_request(id).await.unwrap().unwrap();
        assert_eq!(loaded.state, PaymentRequestState::Pending);
        assert!(loaded.history.is_empty());
    }

    #[tokio::test]
    async fn test_tombstone_with_mismatched_direction_is_dropped() {
        let repo = get_db(&wallet_id());
        let id = Uuid::new_v4();
        let actor = NodeId::from_str(NODE_ID_1).unwrap();
        repo.apply_remote_payment_request_transition(
            id,
            actor.clone(),
            PaymentRequestDirection::Outgoing,
            PaymentRequestTransition {
                target_state: PaymentRequestState::Rejected,
                ..remote_cancel("evt1")
            },
        )
        .await
        .unwrap();

        let mut real_request = test_payment_request();
        real_request.id = id;
        real_request.node_id = Some(actor);
        repo.add_payment_request(real_request).await.unwrap();

        let loaded = repo.get_payment_request(id).await.unwrap().unwrap();
        assert_eq!(loaded.state, PaymentRequestState::Pending);
        assert!(loaded.history.is_empty());
    }

    #[tokio::test]
    async fn test_old_cbor_entry_without_history_field_deserializes_with_defaults() {
        #[derive(serde::Serialize)]
        struct OldPaymentRequestEntry {
            id: Uuid,
            node_id: NodeId,
            amount: Amount,
            unit: CurrencyUnit,
            description: Option<String>,
            deadline: Option<u64>,
            created_at: u64,
            state: PaymentRequestEntryState,
            direction: PaymentRequestEntryDirection,
        }

        let old = OldPaymentRequestEntry {
            id: Uuid::new_v4(),
            node_id: NodeId::from_str(NODE_ID_1).unwrap(),
            amount: Amount::from(1u64),
            unit: CurrencyUnit::Sat,
            description: None,
            deadline: None,
            created_at: 1,
            state: PaymentRequestEntryState::Pending,
            direction: PaymentRequestEntryDirection::Incoming,
        };

        let mut serialized = Vec::new();
        ciborium::into_writer(&old, &mut serialized).unwrap();
        let deserialized: PaymentRequestEntry =
            ciborium::from_reader(serialized.as_slice()).unwrap();

        assert!(deserialized.history.is_empty());
        assert!(!deserialized.tombstone);
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
