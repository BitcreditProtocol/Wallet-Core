use crate::{
    error::{Error, Result},
    redb::{migration::WalletStorageNamespace, transaction::StoredTransaction},
};
use bcr_common::{
    cashu::{CurrencyUnit, MintUrl, nut01 as cdk01},
    cdk_common::wallet::TransactionDirection,
    core::NodeId,
};
use bcr_wallet_core::types::{PaymentType, Transaction, TransactionStatus};
use nostr::event::EventId;
use redb::{ReadableTable, TableDefinition};
use std::{collections::HashMap, str::FromStr};
use uuid::Uuid;

///////////////////////////////////////////////////////////////// MIGRATION 0003
const MIGRATION_0003_TX_REWORK: &str = "0003_tx_rework";

pub(super) fn migration_name_for_wallet(wallet_id: &str) -> String {
    format!("{}_{}", MIGRATION_0003_TX_REWORK, wallet_id)
}

pub(super) fn migration_0003_tx_rework(
    txn: &redb::WriteTransaction,
    namespace: &WalletStorageNamespace,
) -> Result<()> {
    tracing::info!("Migrating transactions..");
    migrate_transactions_to_envelope_and_new_data_model(txn, &namespace.transaction_table)?;
    tracing::info!("Migrated transactions.");

    Ok(())
}

// Fetch old transactions
// put in V1 envelope and new data model
// Delete old transactions because of the id change
// Store new transactions
fn migrate_transactions_to_envelope_and_new_data_model(
    txn: &redb::WriteTransaction,
    table_name: &str,
) -> Result<()> {
    let table_def: TableDefinition<&[u8], Vec<u8>> = TableDefinition::new(table_name);

    let mut table = match txn.open_table(table_def) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => {
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    };

    let mut old_keys = Vec::new();
    let mut migrated_transactions: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    for item in table.iter()? {
        // discard old ID
        let (k, v) = item?;
        let old_key = k.value().to_vec();

        let old_tx: TransactionEntry = ciborium::from_reader(v.value().as_slice())?;
        let new_tx: Transaction = old_tx.try_into()?;
        let new_tx_id = new_tx.id;
        let stored_tx_v1 = StoredTransaction::V1(new_tx.into());

        let migrated_tx_bytes =
            borsh::to_vec(&stored_tx_v1).map_err(|e| Error::BorshSerialization(e.to_string()))?;

        old_keys.push(old_key);
        migrated_transactions.push((new_tx_id.as_bytes().to_vec(), migrated_tx_bytes));
    }

    // Remove legacy entries before adding new ones
    for old_key in &old_keys {
        table.remove(old_key.as_slice())?;
    }

    // Add new entries
    for (id, new_transaction) in migrated_transactions.iter() {
        table.insert(id.as_slice(), new_transaction)?;
    }

    Ok(())
}

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

impl TryFrom<TransactionEntry> for Transaction {
    type Error = Error;

    fn try_from(entry: TransactionEntry) -> Result<Self> {
        Ok(Transaction {
            id: Uuid::new_v4(),
            mint_url: entry.mint_url,
            direction: entry.direction,
            amount: entry.amount,
            fees: entry.fee,
            unit: entry.unit,
            ys: entry.ys,
            tstamp: entry.timestamp,
            memo: entry.memo,
            status: get_transaction_status(&entry.metadata),
            payment_type: get_payment_type(&entry.metadata),
            quote_id: entry
                .quote_id
                .map(|qid| Uuid::from_str(&qid))
                .transpose()
                .map_err(|e| Error::InvalidQuoteId(e.to_string()))?,
            payment_request_id: get_payment_request_id(&entry.metadata),
            nostr_event_id: get_nostr_event_id(&entry.metadata),
            contact_node_id: get_contact_node_id(&entry.metadata),
            btc_tx_id: get_btc_tx_id(&entry.metadata),
            linked_txs: vec![],
        })
    }
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

pub const NOSTR_EVENT_ID_METADATA_KEY: &str = "nostr_event_id";
pub fn get_nostr_event_id(metas: &HashMap<String, String>) -> Option<EventId> {
    let id = metas
        .get(NOSTR_EVENT_ID_METADATA_KEY)
        .or_else(|| metas.get("nostr::event_id"))?;
    EventId::from_str(id).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{redb::transaction::StoredTransactionPayloadV1, test_utils::tests::test_pub_key};
    use bcr_common::cashu::Amount;
    use redb::{
        Builder, Database, ReadableDatabase, ReadableTable, TableDefinition,
        backends::InMemoryBackend,
    };
    use std::{
        collections::{HashMap, HashSet},
        sync::Arc,
    };

    fn test_database() -> Arc<Database> {
        let backend = InMemoryBackend::new();
        Arc::new(
            Builder::new()
                .create_with_backend(backend)
                .expect("create in-memory database"),
        )
    }

    fn legacy_transaction(
        metadata: HashMap<String, String>,
        quote_id: Option<String>,
    ) -> TransactionEntry {
        TransactionEntry {
            tx_id: "legacy-id-that-is-discarded".to_string(),
            mint_url: MintUrl::from_str("https://example.com").expect("valid mint URL"),
            direction: TransactionDirection::Outgoing,
            amount: Amount::from(42_u64),
            fee: Amount::from(2_u64),
            unit: CurrencyUnit::Sat,
            ys: vec![cdk01::PublicKey::from(test_pub_key())],
            timestamp: 1_750_000_000,
            memo: Some("legacy transaction".to_string()),
            metadata,
            quote_id,
        }
    }

    fn insert_legacy_transaction(
        db: &Arc<Database>,
        table_name: &str,
        database_key: &[u8],
        transaction: &TransactionEntry,
    ) {
        let mut encoded = Vec::new();
        ciborium::into_writer(transaction, &mut encoded).expect("serialize legacy transaction");
        let table_def: TableDefinition<&[u8], Vec<u8>> = TableDefinition::new(table_name);
        let write_txn = db.begin_write().expect("begin write transaction");

        {
            let mut table = write_txn
                .open_table(table_def)
                .expect("open legacy transaction table");
            table
                .insert(database_key, encoded)
                .expect("insert legacy transaction");
        }
        write_txn.commit().expect("commit legacy transaction");
    }

    fn run_migration(db: &Arc<Database>, table_name: &str) {
        let write_txn = db.begin_write().expect("begin write transaction");
        migrate_transactions_to_envelope_and_new_data_model(&write_txn, table_name)
            .expect("migrate transactions");
        write_txn.commit().expect("commit migration");
    }

    fn load_migrated_transactions(
        db: &Arc<Database>,
        table_name: &str,
    ) -> Vec<(Vec<u8>, StoredTransactionPayloadV1)> {
        let table_def: TableDefinition<&[u8], Vec<u8>> = TableDefinition::new(table_name);
        let read_txn = db.begin_read().expect("begin read transaction");
        let table = read_txn
            .open_table(table_def)
            .expect("open migrated transaction table");
        let mut transactions = Vec::new();
        for item in table.iter().expect("iterate transaction table") {
            let (key, value) = item.expect("read transaction entry");
            let envelope: StoredTransaction = borsh::from_slice(value.value().as_slice())
                .expect("deserialize transaction envelope");
            let StoredTransaction::V1(payload) = envelope;
            transactions.push((key.value().to_vec(), payload));
        }
        transactions
    }

    fn assert_key_matches_payload_id(database_key: &[u8], payload: &StoredTransactionPayloadV1) {
        let key_id = Uuid::from_slice(database_key).expect("database key is a UUID");
        assert_eq!(key_id, payload.id);
        assert_eq!(database_key, payload.id.as_bytes());
    }

    fn assert_key_is_missing(db: &Arc<Database>, table_name: &str, database_key: &[u8]) {
        let table_def: TableDefinition<&[u8], Vec<u8>> = TableDefinition::new(table_name);
        let read_txn = db.begin_read().expect("begin read transaction");
        let table = read_txn
            .open_table(table_def)
            .expect("open transaction table");
        assert!(
            table
                .get(database_key)
                .expect("read transaction table")
                .is_none(),
            "legacy database key should have been removed"
        );
    }

    #[test]
    fn migration_succeeds_when_transaction_table_does_not_exist() {
        let db = test_database();
        let write_txn = db.begin_write().expect("begin write transaction");
        migrate_transactions_to_envelope_and_new_data_model(&write_txn, "missing-transactions")
            .expect("migration succeeds for missing table");
        write_txn.commit().expect("commit migration");
    }

    #[test]
    fn migration_replaces_legacy_key_with_new_transaction_id() {
        let db = test_database();
        let table_name = "transactions-key-replacement";
        let legacy_key = b"legacy-database-key";

        let legacy = legacy_transaction(HashMap::new(), None);
        insert_legacy_transaction(&db, table_name, legacy_key, &legacy);

        run_migration(&db, table_name);
        assert_key_is_missing(&db, table_name, legacy_key);

        let migrated = load_migrated_transactions(&db, table_name);
        assert_eq!(migrated.len(), 1);

        let (new_key, payload) = &migrated[0];
        assert_ne!(new_key.as_slice(), legacy_key);
        assert_key_matches_payload_id(new_key, payload);

        assert_ne!(
            payload.id.to_string(),
            legacy.tx_id,
            "legacy transaction ID must be discarded"
        );
    }

    #[test]
    fn migration_converts_legacy_transaction_to_v1_envelope() {
        let db = test_database();
        let table_name = "transactions-field-conversion";
        let legacy_key = b"legacy-transaction-key";

        let quote_id = Uuid::new_v4();
        let payment_request_id = Uuid::new_v4();

        let btc_tx_id = "11".repeat(32);
        let nostr_event_id = "22".repeat(32);

        let metadata = HashMap::from([
            (
                TRANSACTION_STATUS_METADATA_KEY.to_string(),
                TransactionStatus::Settled.to_string(),
            ),
            (
                PAYMENT_TYPE_METADATA_KEY.to_string(),
                PaymentType::Token.to_string(),
            ),
            (BTC_TX_ID_TYPE_METADATA_KEY.to_string(), btc_tx_id.clone()),
            (
                PAYMENT_REQUEST_ID_METADATA_KEY.to_string(),
                payment_request_id.to_string(),
            ),
            (
                NOSTR_EVENT_ID_METADATA_KEY.to_string(),
                nostr_event_id.clone(),
            ),
        ]);

        let legacy = legacy_transaction(metadata, Some(quote_id.to_string()));

        insert_legacy_transaction(&db, table_name, legacy_key, &legacy);
        run_migration(&db, table_name);
        assert_key_is_missing(&db, table_name, legacy_key);

        let migrated = load_migrated_transactions(&db, table_name);
        assert_eq!(migrated.len(), 1);

        let (new_key, payload) = &migrated[0];
        assert_key_matches_payload_id(new_key, payload);
        assert_eq!(payload.mint_url, legacy.mint_url);
        assert_eq!(payload.direction, legacy.direction);
        assert_eq!(payload.amount, legacy.amount);
        assert_eq!(payload.fees, legacy.fee);
        assert_eq!(payload.unit, legacy.unit);
        assert_eq!(payload.ys, legacy.ys);
        assert_eq!(payload.tstamp, legacy.timestamp);
        assert_eq!(payload.memo, legacy.memo);
        assert_eq!(payload.status, TransactionStatus::Settled);
        assert_eq!(payload.payment_type, PaymentType::Token);
        assert_eq!(payload.quote_id, Some(quote_id));
        assert_eq!(payload.payment_request_id, Some(payment_request_id));
        assert_eq!(payload.btc_tx_id.as_deref(), Some(btc_tx_id.as_str()));
        assert_eq!(
            payload.nostr_event_id.as_deref(),
            Some(nostr_event_id.as_str())
        );
        assert_eq!(payload.contact_node_id, None);
        assert!(payload.linked_txs.is_empty());
    }

    #[test]
    fn migration_replaces_multiple_legacy_entries() {
        let db = test_database();
        let table_name = "multiple-transactions";

        let first_key = b"first-legacy-key";
        let second_key = b"second-legacy-key";
        let third_key = b"third-legacy-key";

        let mut first = legacy_transaction(HashMap::new(), None);
        first.amount = Amount::from(1_u64);
        first.memo = Some("first".to_string());

        let mut second = legacy_transaction(HashMap::new(), None);
        second.amount = Amount::from(20_u64);
        second.memo = Some("second".to_string());

        let mut third = legacy_transaction(HashMap::new(), None);
        third.amount = Amount::from(u64::MAX);
        third.memo = Some("third".to_string());

        insert_legacy_transaction(&db, table_name, first_key, &first);
        insert_legacy_transaction(&db, table_name, second_key, &second);
        insert_legacy_transaction(&db, table_name, third_key, &third);

        run_migration(&db, table_name);

        assert_key_is_missing(&db, table_name, first_key);
        assert_key_is_missing(&db, table_name, second_key);
        assert_key_is_missing(&db, table_name, third_key);

        let migrated = load_migrated_transactions(&db, table_name);
        assert_eq!(migrated.len(), 3);

        let mut migrated_ids = HashSet::new();
        let mut migrated_memos = HashSet::new();
        for (database_key, payload) in &migrated {
            assert_key_matches_payload_id(database_key, payload);
            assert!(
                migrated_ids.insert(payload.id),
                "each migrated transaction must receive a unique ID"
            );
            migrated_memos.insert(payload.memo.clone());
        }
        assert_eq!(
            migrated_memos,
            HashSet::from([
                Some("first".to_string()),
                Some("second".to_string()),
                Some("third".to_string()),
            ])
        );
    }
}
