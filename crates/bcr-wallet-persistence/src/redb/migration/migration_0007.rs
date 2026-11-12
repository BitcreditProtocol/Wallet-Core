use crate::{
    error::Result,
    redb::{
        migration::WalletStorageNamespace,
        payment_request::{
            PaymentRequestEntry, PaymentRequestEntryDirection, PaymentRequestEntryHistoryEntry,
            PaymentRequestEntryOrigin, PaymentRequestEntryState, encode_payment_request,
        },
    },
};
use bcr_common::{
    cashu::{Amount, CurrencyUnit},
    core::NodeId,
};
use redb::{ReadableTable, TableDefinition};
use uuid::Uuid;

///////////////////////////////////////////////////////////////// MIGRATION 0007
const MIGRATION_0007_PAYMENT_REQUEST_ENVELOPE: &str = "0007_payment_request_envelope";

pub(super) fn migration_name_for_wallet(wallet_id: &str) -> String {
    format!("{}_{}", MIGRATION_0007_PAYMENT_REQUEST_ENVELOPE, wallet_id)
}

pub(super) fn migration_0007_payment_request_envelope(
    txn: &redb::WriteTransaction,
    namespace: &WalletStorageNamespace,
) -> Result<()> {
    tracing::info!("Migrating payment request table..");
    let table_def: TableDefinition<&[u8], Vec<u8>> =
        TableDefinition::new(&namespace.payment_request_table);
    let mut table = match txn.open_table(table_def) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(()),
        Err(e) => return Err(e.into()),
    };

    let mut migrated: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    for item in table.iter()? {
        let (k, v) = item?;
        let legacy: LegacyPaymentRequestEntry = ciborium::from_reader(v.value().as_slice())?;
        migrated.push((k.value().to_vec(), encode_payment_request(&legacy.into())?));
    }
    for (key, value) in migrated {
        table.insert(key.as_slice(), value)?;
    }
    tracing::info!("Migrated payment request table.");
    Ok(())
}

///////////////////////////////////////////// Legacy entries
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
enum LegacyDirection {
    Incoming,
    Outgoing,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
enum LegacyState {
    Pending,
    Paid { tx_id: Uuid },
    Canceled,
    Rejected,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
enum LegacyOrigin {
    Local,
    Remote { event_id: String },
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
struct LegacyHistoryEntry {
    state: LegacyState,
    applied: bool,
    actor: Option<NodeId>,
    at: u64,
    origin: LegacyOrigin,
    reason: Option<String>,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
struct LegacyPaymentRequestEntry {
    id: Uuid,
    #[serde(default)]
    node_id: Option<NodeId>,
    amount: Amount,
    unit: CurrencyUnit,
    description: Option<String>,
    deadline: Option<u64>,
    created_at: u64,
    state: LegacyState,
    direction: LegacyDirection,
    #[serde(default)]
    history: Vec<LegacyHistoryEntry>,
    #[serde(default)]
    tombstone: bool,
}

impl From<LegacyDirection> for PaymentRequestEntryDirection {
    fn from(value: LegacyDirection) -> Self {
        match value {
            LegacyDirection::Incoming => Self::Incoming,
            LegacyDirection::Outgoing => Self::Outgoing,
        }
    }
}

impl From<LegacyState> for PaymentRequestEntryState {
    fn from(value: LegacyState) -> Self {
        match value {
            LegacyState::Pending => Self::Pending,
            LegacyState::Paid { tx_id } => Self::Paid { tx_id },
            LegacyState::Canceled => Self::Canceled,
            LegacyState::Rejected => Self::Rejected,
        }
    }
}

impl From<LegacyOrigin> for PaymentRequestEntryOrigin {
    fn from(value: LegacyOrigin) -> Self {
        match value {
            LegacyOrigin::Local => Self::Local,
            LegacyOrigin::Remote { event_id } => Self::Remote { event_id },
        }
    }
}

impl From<LegacyHistoryEntry> for PaymentRequestEntryHistoryEntry {
    fn from(value: LegacyHistoryEntry) -> Self {
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

impl From<LegacyPaymentRequestEntry> for PaymentRequestEntry {
    fn from(value: LegacyPaymentRequestEntry) -> Self {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        PaymentRequestStoreApi,
        redb::{
            migration::{collect_wallet_namespace, migrate_wallet},
            payment_request::PaymentRequestDB,
        },
    };
    use bcr_wallet_core::types::{
        PaymentRequest, PaymentRequestActionOrigin, PaymentRequestDirection,
        PaymentRequestHistoryEntry, PaymentRequestState,
    };
    use redb::{Builder, Database, backends::InMemoryBackend};
    use std::{str::FromStr, sync::Arc};

    const WALLET_ID: &str = "wallet-1";
    const NODE_ID: &str =
        "bitcrt03205b8dec12bc9e879f5b517aa32192a2550e88adcee3e54ec2c7294802568fef";

    fn test_database() -> Arc<Database> {
        Arc::new(
            Builder::new()
                .create_with_backend(InMemoryBackend::new())
                .expect("create in-memory database"),
        )
    }

    fn namespace() -> WalletStorageNamespace {
        collect_wallet_namespace(
            WALLET_ID.to_string(),
            CurrencyUnit::Sat,
            bitcoin::secp256k1::Keypair::new_global(&mut bitcoin::secp256k1::rand::thread_rng()),
        )
    }

    fn insert_legacy<T: serde::Serialize>(db: &Arc<Database>, id: Uuid, value: &T) {
        let mut bytes = Vec::new();
        ciborium::into_writer(value, &mut bytes).unwrap();
        let table_def: TableDefinition<&[u8], Vec<u8>> =
            TableDefinition::new(&namespace().payment_request_table);
        let txn = db.begin_write().unwrap();
        {
            let mut table = txn.open_table(table_def).unwrap();
            table.insert(id.as_bytes().as_slice(), bytes).unwrap();
        }
        txn.commit().unwrap();
    }

    fn legacy_entry(
        state: LegacyState,
        direction: LegacyDirection,
        history: Vec<LegacyHistoryEntry>,
    ) -> LegacyPaymentRequestEntry {
        LegacyPaymentRequestEntry {
            id: Uuid::new_v4(),
            node_id: Some(NodeId::from_str(NODE_ID).unwrap()),
            amount: Amount::from(42u64),
            unit: CurrencyUnit::Sat,
            description: Some("lunch".to_string()),
            deadline: Some(2_000),
            created_at: 1_000,
            state,
            direction,
            history,
            tombstone: false,
        }
    }

    fn expected(legacy: &LegacyPaymentRequestEntry) -> PaymentRequest {
        PaymentRequestEntry::from(legacy.clone()).into()
    }

    #[tokio::test]
    async fn migration_0007_keeps_every_state_direction_and_origin() {
        let db = test_database();
        let tx_id = Uuid::new_v4();
        let states = [
            LegacyState::Pending,
            LegacyState::Paid { tx_id },
            LegacyState::Canceled,
            LegacyState::Rejected,
        ];
        let origins = [
            LegacyOrigin::Local,
            LegacyOrigin::Remote {
                event_id: "event-1".to_string(),
            },
        ];
        let mut legacy = Vec::new();
        for direction in [LegacyDirection::Incoming, LegacyDirection::Outgoing] {
            for state in &states {
                let history = origins
                    .iter()
                    .map(|origin| LegacyHistoryEntry {
                        state: state.clone(),
                        applied: true,
                        actor: Some(NodeId::from_str(NODE_ID).unwrap()),
                        at: 1_500,
                        origin: origin.clone(),
                        reason: Some("because".to_string()),
                    })
                    .collect();
                let entry = legacy_entry(state.clone(), direction.clone(), history);
                insert_legacy(&db, entry.id, &entry);
                legacy.push(entry);
            }
        }

        migrate_wallet(db.clone(), namespace()).await.unwrap();

        let repo = PaymentRequestDB::new(db, WALLET_ID).unwrap();
        for entry in &legacy {
            let loaded = repo.get_payment_request(entry.id).await.unwrap().unwrap();
            assert_eq!(loaded, expected(entry));
        }
        let paid = repo
            .list_payment_requests(
                PaymentRequestDirection::Incoming,
                &[PaymentRequestState::Paid { tx_id }],
            )
            .await
            .unwrap();
        assert_eq!(paid.len(), 1);
        assert_eq!(paid[0].state, PaymentRequestState::Paid { tx_id });
        assert_eq!(
            paid[0].history[1],
            PaymentRequestHistoryEntry {
                state: PaymentRequestState::Paid { tx_id },
                applied: true,
                actor: Some(NodeId::from_str(NODE_ID).unwrap()),
                at: 1_500,
                origin: PaymentRequestActionOrigin::Remote {
                    event_id: "event-1".to_string()
                },
                reason: Some("because".to_string()),
            }
        );
    }

    #[tokio::test]
    async fn migration_0007_applies_serde_defaults_to_rows_without_history_and_tombstone() {
        #[derive(serde::Serialize)]
        struct OldEntry {
            id: Uuid,
            node_id: NodeId,
            amount: Amount,
            unit: CurrencyUnit,
            description: Option<String>,
            deadline: Option<u64>,
            created_at: u64,
            state: LegacyState,
            direction: LegacyDirection,
        }
        #[derive(serde::Serialize)]
        struct OlderEntry {
            id: Uuid,
            amount: Amount,
            unit: CurrencyUnit,
            description: Option<String>,
            deadline: Option<u64>,
            created_at: u64,
            state: LegacyState,
            direction: LegacyDirection,
        }
        let db = test_database();
        let tx_id = Uuid::new_v4();
        let old_id = Uuid::new_v4();
        let older_id = Uuid::new_v4();
        insert_legacy(
            &db,
            old_id,
            &OldEntry {
                id: old_id,
                node_id: NodeId::from_str(NODE_ID).unwrap(),
                amount: Amount::from(7u64),
                unit: CurrencyUnit::Sat,
                description: None,
                deadline: None,
                created_at: 3,
                state: LegacyState::Paid { tx_id },
                direction: LegacyDirection::Outgoing,
            },
        );
        insert_legacy(
            &db,
            older_id,
            &OlderEntry {
                id: older_id,
                amount: Amount::from(9u64),
                unit: CurrencyUnit::Sat,
                description: Some("d".to_string()),
                deadline: Some(5),
                created_at: 4,
                state: LegacyState::Canceled,
                direction: LegacyDirection::Incoming,
            },
        );

        migrate_wallet(db.clone(), namespace()).await.unwrap();

        let repo = PaymentRequestDB::new(db, WALLET_ID).unwrap();
        let old = repo.get_payment_request(old_id).await.unwrap().unwrap();
        assert_eq!(old.state, PaymentRequestState::Paid { tx_id });
        assert_eq!(old.node_id, Some(NodeId::from_str(NODE_ID).unwrap()));
        assert_eq!(old.direction, PaymentRequestDirection::Outgoing);
        assert!(old.history.is_empty());
        assert!(!old.tombstone);
        let older = repo.get_payment_request(older_id).await.unwrap().unwrap();
        assert_eq!(older.state, PaymentRequestState::Canceled);
        assert_eq!(older.node_id, None);
        assert!(older.history.is_empty());
        assert!(!older.tombstone);
    }

    #[tokio::test]
    async fn migration_0007_keeps_tombstones() {
        let db = test_database();
        let mut entry = legacy_entry(LegacyState::Pending, LegacyDirection::Incoming, vec![]);
        entry.tombstone = true;
        insert_legacy(&db, entry.id, &entry);

        migrate_wallet(db.clone(), namespace()).await.unwrap();

        let repo = PaymentRequestDB::new(db, WALLET_ID).unwrap();
        assert_eq!(repo.get_payment_request(entry.id).await.unwrap(), None);
    }

    #[tokio::test]
    async fn migration_0007_on_empty_table_succeeds() {
        let db = test_database();

        migrate_wallet(db.clone(), namespace()).await.unwrap();

        let repo = PaymentRequestDB::new(db, WALLET_ID).unwrap();
        let listed = repo
            .list_payment_requests(PaymentRequestDirection::Incoming, &[])
            .await
            .unwrap();
        assert!(listed.is_empty());
    }

    #[tokio::test]
    async fn migration_0007_second_open_leaves_rows_unchanged() {
        let db = test_database();
        let entry = legacy_entry(LegacyState::Rejected, LegacyDirection::Outgoing, vec![]);
        insert_legacy(&db, entry.id, &entry);

        migrate_wallet(db.clone(), namespace()).await.unwrap();
        migrate_wallet(db.clone(), namespace()).await.unwrap();

        let repo = PaymentRequestDB::new(db, WALLET_ID).unwrap();
        let loaded = repo.get_payment_request(entry.id).await.unwrap().unwrap();
        assert_eq!(loaded, expected(&entry));
    }
}
