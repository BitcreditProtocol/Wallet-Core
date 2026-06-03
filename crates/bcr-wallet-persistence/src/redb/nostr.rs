use crate::{NostrEventOffset, NostrEventOffsetStoreApi, error::Result};
use async_trait::async_trait;
use nostr::types::Timestamp;
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition, TableError};
use std::sync::Arc;
use tokio::task::spawn_blocking;

///////////////////////////////////////////// NostrEventOffsetEntry
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct NostrEventOffsetEntry {
    pub event_id: String,
    pub time: Timestamp,
    pub success: bool,
}

impl From<NostrEventOffsetEntry> for NostrEventOffset {
    fn from(value: NostrEventOffsetEntry) -> Self {
        Self {
            event_id: value.event_id,
            time: value.time,
            success: value.success,
        }
    }
}

impl From<NostrEventOffset> for NostrEventOffsetEntry {
    fn from(value: NostrEventOffset) -> Self {
        Self {
            event_id: value.event_id,
            time: value.time,
            success: value.success,
        }
    }
}

///////////////////////////////////////////// NostrDB
pub struct NostrDB {
    db: Arc<Database>,
    last_offset_table: TableDefinition<'static, &'static [u8], u64>,
    offset_entry_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
}

impl NostrDB {
    const OFFSET_ENTRY_DB_NAME: &'static str = "offset_entry";
    const LAST_OFFSET_DB_NAME: &'static str = "last_offset";
    const CURRENT_OFFSET_UNIQUE_ID: &'static str = "current_offset";

    pub fn new(db: Arc<Database>, wallet_id: &str) -> Result<Self> {
        // Leak once to get static string, because of dynamically generated table names
        let offset_entry_name: &'static str =
            Box::leak(format!("{wallet_id}_{}", Self::OFFSET_ENTRY_DB_NAME).into_boxed_str());
        // Leak once to get static string, because of dynamically generated table names
        let last_offset_name: &'static str =
            Box::leak(format!("{wallet_id}_{}", Self::LAST_OFFSET_DB_NAME).into_boxed_str());

        let offset_entry_table = TableDefinition::new(offset_entry_name);
        let last_offset_table = TableDefinition::new(last_offset_name);

        Ok(Self {
            db,
            offset_entry_table,
            last_offset_table,
        })
    }

    fn current_offset_sync(
        db: Arc<Database>,
        last_offset_table: TableDefinition<'static, &'static [u8], u64>,
    ) -> Result<Timestamp> {
        let read_txn = db.begin_read()?;

        match read_txn.open_table(last_offset_table) {
            Ok(table) => {
                let entry = table.get(Self::CURRENT_OFFSET_UNIQUE_ID.as_bytes())?;
                match entry {
                    Some(t) => Ok(Timestamp::from(t.value())),
                    None => Ok(Timestamp::zero()),
                }
            }
            Err(TableError::TableDoesNotExist(_)) => Ok(Timestamp::zero()),
            Err(e) => Err(e.into()),
        }
    }

    fn is_processed_sync(
        db: Arc<Database>,
        offset_entry_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        event_id: String,
    ) -> Result<bool> {
        let read_txn = db.begin_read()?;

        match read_txn.open_table(offset_entry_table) {
            Ok(table) => {
                let entry = table.get(event_id.as_bytes())?;
                match entry {
                    Some(e) => {
                        let _entry: NostrEventOffsetEntry =
                            ciborium::from_reader(e.value().as_slice())?;
                        Ok(true)
                    }
                    None => Ok(false),
                }
            }
            Err(TableError::TableDoesNotExist(_)) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    fn add_event_sync(
        db: Arc<Database>,
        offset_entry_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        last_offset_table: TableDefinition<'static, &'static [u8], u64>,
        entry: NostrEventOffsetEntry,
    ) -> Result<()> {
        let id = entry.event_id.clone();
        let write_txn = db.begin_write()?;

        {
            let mut table = write_txn.open_table(offset_entry_table)?;

            let mut serialized = Vec::new();
            ciborium::into_writer(&entry, &mut serialized)?;
            table.insert(id.as_bytes(), serialized)?;
        }

        {
            let mut table = write_txn.open_table(last_offset_table)?;
            let offset = table
                .get(Self::CURRENT_OFFSET_UNIQUE_ID.as_bytes())?
                .map(|v| v.value())
                .unwrap_or_default();
            let next = offset.max(entry.time.as_u64());
            table.insert(Self::CURRENT_OFFSET_UNIQUE_ID.as_bytes(), next)?;
        }

        write_txn.commit()?;
        Ok(())
    }

    fn delete_repo(
        db: Arc<Database>,
        offset_entry_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        last_offset_table: TableDefinition<'static, &'static [u8], u64>,
    ) -> Result<()> {
        let write_txn = db.begin_write()?;

        {
            if write_txn.open_table(offset_entry_table).is_ok() {
                write_txn.delete_table(offset_entry_table)?;
            }

            if write_txn.open_table(last_offset_table).is_ok() {
                write_txn.delete_table(last_offset_table)?;
            }
        }

        write_txn.commit()?;
        Ok(())
    }
}

#[async_trait]
impl NostrEventOffsetStoreApi for NostrDB {
    async fn current_offset(&self) -> Result<Timestamp> {
        let db_clone = self.db.clone();
        let table = self.last_offset_table;
        let res = spawn_blocking(move || Self::current_offset_sync(db_clone, table)).await??;
        Ok(res)
    }

    async fn is_processed(&self, event_id: &str) -> Result<bool> {
        let db_clone = self.db.clone();
        let table = self.offset_entry_table;
        let event_id_clone = event_id.to_owned();
        let res = spawn_blocking(move || Self::is_processed_sync(db_clone, table, event_id_clone))
            .await??;
        Ok(res)
    }

    async fn add_event(&self, data: NostrEventOffset) -> Result<()> {
        let db_clone = self.db.clone();
        let last_offset_table = self.last_offset_table;
        let entry_table = self.offset_entry_table;
        let res = spawn_blocking(move || {
            Self::add_event_sync(db_clone, entry_table, last_offset_table, data.into())
        })
        .await??;
        Ok(res)
    }

    async fn delete_repo(&self) -> Result<()> {
        let db_clone = self.db.clone();
        let last_offset_table = self.last_offset_table;
        let offset_entry_table = self.offset_entry_table;
        spawn_blocking(move || Self::delete_repo(db_clone, offset_entry_table, last_offset_table))
            .await?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use redb::{Builder, backends::InMemoryBackend};

    fn get_db(wallet_id: &str) -> NostrDB {
        let in_mem = InMemoryBackend::new();
        let db = Arc::new(
            Builder::new()
                .create_with_backend(in_mem)
                .expect("can create in-memory redb"),
        );

        NostrDB::new(db, wallet_id).expect("can create NostrDB")
    }

    fn offset(event_id: &str, time: u64, success: bool) -> NostrEventOffset {
        NostrEventOffset {
            event_id: event_id.to_string(),
            time: Timestamp::from(time),
            success,
        }
    }

    #[tokio::test]
    async fn test_current_offset_empty() {
        let repo = get_db("wallet-empty-offset");
        let current = repo.current_offset().await.expect("current_offset works");
        assert_eq!(current, Timestamp::zero());
    }

    #[tokio::test]
    async fn test_is_processed_missing_returns_false() {
        let repo = get_db("wallet-missing-event");
        let processed = repo
            .is_processed("missing-event-id")
            .await
            .expect("is_processed works");
        assert!(!processed);
    }

    #[tokio::test]
    async fn test_add_event_marks_event_as_processed() {
        let repo = get_db("wallet-processed");
        repo.add_event(offset("event-1", 100, true))
            .await
            .expect("add_event works");
        let processed = repo
            .is_processed("event-1")
            .await
            .expect("is_processed works");

        assert!(processed);
    }

    #[tokio::test]
    async fn test_add_event_does_not_mark_other_events_as_processed() {
        let repo = get_db("wallet-other-events");
        repo.add_event(offset("event-1", 100, true))
            .await
            .expect("add_event works");
        let processed = repo
            .is_processed("event-2")
            .await
            .expect("is_processed works");
        assert!(!processed);
    }

    #[tokio::test]
    async fn test_add_event_updates_current_offset() {
        let repo = get_db("wallet-updates-offset");
        repo.add_event(offset("event-1", 100, true))
            .await
            .expect("add_event works");
        let current = repo.current_offset().await.expect("current_offset works");
        assert_eq!(current, Timestamp::from(100));
    }

    #[tokio::test]
    async fn test_current_offset_keeps_highest_timestamp() {
        let repo = get_db("wallet-max-offset");
        repo.add_event(offset("event-1", 100, true))
            .await
            .expect("add_event event-1 works");
        repo.add_event(offset("event-2", 50, true))
            .await
            .expect("add_event event-2 works");
        repo.add_event(offset("event-3", 150, true))
            .await
            .expect("add_event event-3 works");
        let current = repo.current_offset().await.expect("current_offset works");
        assert_eq!(current, Timestamp::from(150));
    }

    #[tokio::test]
    async fn test_failed_event_is_still_processed_and_updates_offset() {
        let repo = get_db("wallet-failed-event");
        repo.add_event(offset("event-1", 100, true))
            .await
            .expect("add_event event-1 works");
        repo.add_event(offset("event-2", 200, false))
            .await
            .expect("add_event event-2 works");
        let processed = repo
            .is_processed("event-2")
            .await
            .expect("is_processed works");
        let current = repo.current_offset().await.expect("current_offset works");
        assert!(processed);
        assert_eq!(current, Timestamp::from(200));
    }

    #[tokio::test]
    async fn test_wallets_are_isolated() {
        let in_mem = InMemoryBackend::new();
        let db = Arc::new(
            Builder::new()
                .create_with_backend(in_mem)
                .expect("can create in-memory redb"),
        );

        let repo_a = NostrDB::new(db.clone(), "wallet-a").expect("can create repo a");
        let repo_b = NostrDB::new(db.clone(), "wallet-b").expect("can create repo b");
        repo_a
            .add_event(offset("event-1", 100, true))
            .await
            .expect("add_event repo a works");
        assert!(
            repo_a
                .is_processed("event-1")
                .await
                .expect("is_processed repo a works")
        );
        assert!(
            !repo_b
                .is_processed("event-1")
                .await
                .expect("is_processed repo b works")
        );
        assert_eq!(
            repo_a.current_offset().await.expect("repo a offset works"),
            Timestamp::from(100)
        );
        assert_eq!(
            repo_b.current_offset().await.expect("repo b offset works"),
            Timestamp::zero()
        );
    }
}
