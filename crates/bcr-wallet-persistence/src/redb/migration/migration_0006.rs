use crate::{
    error::Result,
    redb::{
        migration::WalletStorageNamespace,
        nostr::{
            NostrEventOffsetEntry, NostrQueuedMessageEntry, encode_event_offset,
            encode_queued_message,
        },
    },
};
use nostr::types::Timestamp;
use redb::{ReadableTable, TableDefinition};

///////////////////////////////////////////////////////////////// MIGRATION 0006
const MIGRATION_0006_NOSTR_ENVELOPE: &str = "0006_nostr_envelope";

pub(super) fn migration_name_for_wallet(wallet_id: &str) -> String {
    format!("{}_{}", MIGRATION_0006_NOSTR_ENVELOPE, wallet_id)
}

pub(super) fn migration_0006_nostr_envelope(
    txn: &redb::WriteTransaction,
    namespace: &WalletStorageNamespace,
) -> Result<()> {
    tracing::info!("Migrating nostr tables..");
    migrate_table(txn, &namespace.offset_entry_table, |bytes| {
        let legacy: LegacyOffsetEntry = ciborium::from_reader(bytes)?;
        encode_event_offset(&legacy.into())
    })?;
    for table_name in [
        &namespace.queued_message_table,
        &namespace.dead_letter_table,
    ] {
        migrate_table(txn, table_name, |bytes| {
            let legacy: LegacyQueuedMessageEntry = ciborium::from_reader(bytes)?;
            encode_queued_message(&legacy.into(), &namespace.keys)
        })?;
    }
    tracing::info!("Migrated nostr tables.");
    Ok(())
}

fn migrate_table(
    txn: &redb::WriteTransaction,
    table_name: &str,
    convert: impl Fn(&[u8]) -> Result<Vec<u8>>,
) -> Result<()> {
    let table_def: TableDefinition<&[u8], Vec<u8>> = TableDefinition::new(table_name);
    let mut table = match txn.open_table(table_def) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(()),
        Err(e) => return Err(e.into()),
    };

    let mut migrated: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    for item in table.iter()? {
        let (k, v) = item?;
        match convert(v.value().as_slice()) {
            Ok(converted) => migrated.push((k.value().to_vec(), converted)),
            Err(e) => tracing::warn!("Leaving undecodable legacy row in {table_name}: {e}"),
        }
    }
    for (key, value) in migrated {
        table.insert(key.as_slice(), value)?;
    }
    Ok(())
}

///////////////////////////////////////////// Legacy entries
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
struct LegacyOffsetEntry {
    event_id: String,
    time: Timestamp,
    success: bool,
}

impl From<LegacyOffsetEntry> for NostrEventOffsetEntry {
    fn from(value: LegacyOffsetEntry) -> Self {
        Self {
            event_id: value.event_id,
            time: value.time,
            success: value.success,
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
struct LegacyQueuedMessageEntry {
    id: String,
    recipient: Option<String>,
    payload: String,
    #[serde(with = "time::serde::rfc3339")]
    created: time::OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    last_try: time::OffsetDateTime,
    num_retries: i32,
    max_retries: i32,
    #[serde(with = "time::serde::rfc3339")]
    processing_started_at: time::OffsetDateTime,
}

impl From<LegacyQueuedMessageEntry> for NostrQueuedMessageEntry {
    fn from(value: LegacyQueuedMessageEntry) -> Self {
        Self {
            id: value.id,
            recipient: value.recipient,
            payload: value.payload,
            created: value.created,
            last_try: value.last_try,
            num_retries: value.num_retries,
            max_retries: value.max_retries,
            processing_started_at: value.processing_started_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        NostrQueuedMessage, NostrRepository,
        redb::{
            migration::{collect_wallet_namespace, migrate_wallet},
            nostr::NostrDB,
        },
    };
    use bcr_common::cashu::CurrencyUnit;
    use redb::{Builder, Database, ReadableDatabase, backends::InMemoryBackend};
    use std::sync::Arc;

    const WALLET_ID: &str = "wallet-1";
    const SECRET_PAYLOAD: &str = "super-secret-queued-payload";

    fn test_database() -> Arc<Database> {
        Arc::new(
            Builder::new()
                .create_with_backend(InMemoryBackend::new())
                .expect("create in-memory database"),
        )
    }

    fn test_keypair() -> bitcoin::secp256k1::Keypair {
        bitcoin::secp256k1::Keypair::new_global(&mut bitcoin::secp256k1::rand::thread_rng())
    }

    fn legacy_message(id: &str, retries: i32) -> LegacyQueuedMessageEntry {
        LegacyQueuedMessageEntry {
            id: id.to_string(),
            recipient: Some("recipient".to_string()),
            payload: SECRET_PAYLOAD.to_string(),
            created: time::OffsetDateTime::from_unix_timestamp(1_000).unwrap(),
            last_try: time::OffsetDateTime::from_unix_timestamp(0).unwrap(),
            num_retries: retries,
            max_retries: 5,
            processing_started_at: time::OffsetDateTime::from_unix_timestamp(0).unwrap(),
        }
    }

    fn insert_legacy<T: serde::Serialize>(
        db: &Arc<Database>,
        table_name: &str,
        key: &str,
        value: &T,
    ) {
        let mut bytes = Vec::new();
        ciborium::into_writer(value, &mut bytes).unwrap();
        let table_def: TableDefinition<&[u8], Vec<u8>> = TableDefinition::new(table_name);
        let txn = db.begin_write().unwrap();
        {
            let mut table = txn.open_table(table_def).unwrap();
            table.insert(key.as_bytes(), bytes).unwrap();
        }
        txn.commit().unwrap();
    }

    fn raw_values(db: &Arc<Database>, table_name: &str) -> Vec<Vec<u8>> {
        let table_def: TableDefinition<&[u8], Vec<u8>> = TableDefinition::new(table_name);
        let txn = db.begin_read().unwrap();
        let table = txn.open_table(table_def).unwrap();
        table
            .iter()
            .unwrap()
            .map(|item| item.unwrap().1.value())
            .collect()
    }

    fn seed_legacy(db: &Arc<Database>) {
        insert_legacy(
            db,
            &NostrDB::offset_entry_table_name(WALLET_ID),
            "event-1",
            &LegacyOffsetEntry {
                event_id: "event-1".to_string(),
                time: Timestamp::from(100),
                success: true,
            },
        );
        insert_legacy(
            db,
            &NostrDB::queued_message_table_name(WALLET_ID),
            "queued-1",
            &legacy_message("queued-1", 1),
        );
        insert_legacy(
            db,
            &NostrDB::dead_letter_table_name(WALLET_ID),
            "dead-1",
            &legacy_message("dead-1", 5),
        );
    }

    #[tokio::test]
    async fn migration_0006_converts_legacy_rows_and_hides_payload() {
        let db = test_database();
        let keys = test_keypair();
        seed_legacy(&db);

        let namespace = collect_wallet_namespace(WALLET_ID.to_string(), CurrencyUnit::Sat, keys);
        migrate_wallet(db.clone(), namespace)
            .await
            .expect("migration succeeds");

        for table in [
            NostrDB::queued_message_table_name(WALLET_ID),
            NostrDB::dead_letter_table_name(WALLET_ID),
        ] {
            for raw in raw_values(&db, &table) {
                assert!(
                    !raw.windows(SECRET_PAYLOAD.len())
                        .any(|w| w == SECRET_PAYLOAD.as_bytes())
                );
            }
        }

        let repo = NostrDB::new(db.clone(), WALLET_ID, keys).unwrap();
        assert!(repo.is_processed("event-1").await.unwrap());
        assert_eq!(repo.current_offset().await.unwrap(), Timestamp::zero());
        let messages = repo.get_retry_messages(10).await.unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].id, "queued-1");
        assert_eq!(messages[0].recipient.as_deref(), Some("recipient"));
        assert_eq!(messages[0].payload, SECRET_PAYLOAD);

        // the migrated queue entry keeps its retry count: one more failure of 4 left
        for _ in 0..4 {
            repo.fail_retry("queued-1").await.unwrap();
        }
        assert!(repo.get_retry_messages(10).await.unwrap().is_empty());
        let dead = raw_values(&db, &NostrDB::dead_letter_table_name(WALLET_ID));
        assert_eq!(dead.len(), 2);
    }

    #[tokio::test]
    async fn migration_0006_is_a_noop_on_second_open_and_empty_database() {
        let db = test_database();
        let keys = test_keypair();
        let namespace = collect_wallet_namespace(WALLET_ID.to_string(), CurrencyUnit::Sat, keys);
        migrate_wallet(db.clone(), namespace.clone())
            .await
            .expect("empty migration succeeds");

        seed_legacy(&db);
        let other_db = test_database();
        seed_legacy(&other_db);
        migrate_wallet(other_db.clone(), namespace.clone())
            .await
            .expect("first migration succeeds");
        let before = raw_values(&other_db, &NostrDB::queued_message_table_name(WALLET_ID));
        migrate_wallet(other_db.clone(), namespace)
            .await
            .expect("second migration succeeds");
        let after = raw_values(&other_db, &NostrDB::queued_message_table_name(WALLET_ID));
        assert_eq!(before, after);
    }

    #[tokio::test]
    async fn migration_0006_keeps_a_corrupted_row_and_succeeds() {
        let db = test_database();
        let keys = test_keypair();
        let table_def: TableDefinition<&[u8], Vec<u8>> = TableDefinition::new(Box::leak(
            NostrDB::queued_message_table_name(WALLET_ID).into_boxed_str(),
        ));
        let txn = db.begin_write().unwrap();
        {
            let mut table = txn.open_table(table_def).unwrap();
            table.insert(b"bad".as_slice(), vec![0xff, 0x01]).unwrap();
        }
        txn.commit().unwrap();

        let namespace = collect_wallet_namespace(WALLET_ID.to_string(), CurrencyUnit::Sat, keys);
        migrate_wallet(db.clone(), namespace).await.unwrap();
        assert_eq!(
            raw_values(&db, &NostrDB::queued_message_table_name(WALLET_ID)),
            vec![vec![0xff, 0x01]]
        );
    }

    #[tokio::test]
    async fn new_messages_roundtrip_after_migration() {
        let db = test_database();
        let keys = test_keypair();
        let namespace = collect_wallet_namespace(WALLET_ID.to_string(), CurrencyUnit::Sat, keys);
        migrate_wallet(db.clone(), namespace).await.unwrap();
        let repo = NostrDB::new(db, WALLET_ID, keys).unwrap();
        repo.add_retry_message(
            NostrQueuedMessage {
                id: "m".to_string(),
                recipient: None,
                payload: "p".to_string(),
            },
            3,
        )
        .await
        .unwrap();
        assert_eq!(repo.get_retry_messages(1).await.unwrap()[0].payload, "p");
    }
}
