use crate::{NostrEventOffset, NostrQueuedMessage, NostrRepository, error::Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
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

///////////////////////////////////////////// NostrQueuedMessageEntry
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct NostrQueuedMessageEntry {
    pub id: String,
    pub recipient: Option<String>,
    pub payload: String,
    pub created: DateTime<Utc>,
    pub last_try: DateTime<Utc>,
    pub num_retries: i32,
    pub max_retries: i32,
    pub processing_started_at: DateTime<Utc>,
}

impl From<NostrQueuedMessageEntry> for NostrQueuedMessage {
    fn from(value: NostrQueuedMessageEntry) -> Self {
        Self {
            id: value.id,
            recipient: value.recipient,
            payload: value.payload,
        }
    }
}

impl NostrQueuedMessageEntry {
    fn from(value: NostrQueuedMessage, max_retries: i32) -> Self {
        Self {
            id: value.id,
            recipient: value.recipient,
            payload: value.payload,
            created: Utc::now(),
            last_try: DateTime::from_timestamp(0, 0).expect("valid"),
            num_retries: 0,
            max_retries,
            processing_started_at: DateTime::from_timestamp(0, 0).expect("valid"),
        }
    }
}
///////////////////////////////////////////// NostrDB
pub struct NostrDB {
    db: Arc<Database>,
    last_offset_table: TableDefinition<'static, &'static [u8], u64>,
    offset_entry_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
    queued_message_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
    dead_letter_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
}

impl NostrDB {
    const OFFSET_ENTRY_DB_NAME: &'static str = "offset_entry";
    const LAST_OFFSET_DB_NAME: &'static str = "last_offset";
    const QUEUED_MESSAGE_DB_NAME: &'static str = "queued_message";
    const DEAD_LETTER_DB_NAME: &'static str = "dead_letter";
    const CURRENT_OFFSET_UNIQUE_ID: &'static str = "current_offset";
    const NOSTR_QUEUE_PROCESSING_TIMEOUT_SECS: i64 = 60;

    pub fn new(db: Arc<Database>, wallet_id: &str) -> Result<Self> {
        // Leak once to get static string, because of dynamically generated table names
        let offset_entry_name: &'static str =
            Box::leak(format!("{wallet_id}_{}", Self::OFFSET_ENTRY_DB_NAME).into_boxed_str());
        // Leak once to get static string, because of dynamically generated table names
        let last_offset_name: &'static str =
            Box::leak(format!("{wallet_id}_{}", Self::LAST_OFFSET_DB_NAME).into_boxed_str());
        // Leak once to get static string, because of dynamically generated table names
        let queued_message_name: &'static str =
            Box::leak(format!("{wallet_id}_{}", Self::QUEUED_MESSAGE_DB_NAME).into_boxed_str());
        // Leak once to get static string, because of dynamically generated table names
        let dead_letter_name: &'static str =
            Box::leak(format!("{wallet_id}_{}", Self::DEAD_LETTER_DB_NAME).into_boxed_str());

        let offset_entry_table = TableDefinition::new(offset_entry_name);
        let last_offset_table = TableDefinition::new(last_offset_name);
        let queued_message_table = TableDefinition::new(queued_message_name);
        let dead_letter_table = TableDefinition::new(dead_letter_name);

        Ok(Self {
            db,
            offset_entry_table,
            last_offset_table,
            queued_message_table,
            dead_letter_table,
        })
    }

    ///////////////////////////////////////////// Event Offset
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

    ///////////////////////////////////////////// Event Offset
    fn add_retry_message_sync(
        db: Arc<Database>,
        queued_message_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        entry: NostrQueuedMessageEntry,
    ) -> Result<()> {
        let id = entry.id.clone();
        let write_txn = db.begin_write()?;

        {
            let mut table = write_txn.open_table(queued_message_table)?;

            let mut serialized = Vec::new();
            ciborium::into_writer(&entry, &mut serialized)?;
            table.insert(id.as_bytes(), serialized)?;
        }

        write_txn.commit()?;
        Ok(())
    }

    fn get_retry_messages_sync(
        db: Arc<Database>,
        queued_message_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        limit: u64,
    ) -> Result<Vec<NostrQueuedMessageEntry>> {
        let now = Utc::now();
        let retry_before =
            now - chrono::Duration::seconds(Self::NOSTR_QUEUE_PROCESSING_TIMEOUT_SECS);

        let write_txn = db.begin_write()?;

        let res = {
            let mut table = write_txn.open_table(queued_message_table)?;
            let mut res = Vec::new();
            for (_, v) in table.range::<&[u8]>(..)?.flatten() {
                let entry: NostrQueuedMessageEntry = ciborium::from_reader(v.value().as_slice())?;
                if entry.processing_started_at < retry_before {
                    res.push(entry);
                }
            }

            res.sort_by_key(|msg| msg.last_try);
            res.truncate(limit as usize);

            // set processing_started_at to avoid retrying before the backoff time
            for to_update in res.iter_mut() {
                to_update.processing_started_at = Utc::now();
                let mut serialized = Vec::new();
                ciborium::into_writer(&to_update, &mut serialized)?;
                table.insert(to_update.id.as_bytes(), serialized)?;
            }

            res
        };

        write_txn.commit()?;

        Ok(res)
    }

    fn fail_retry_sync(
        db: Arc<Database>,
        queued_message_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        dead_letter_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        id: String,
    ) -> Result<()> {
        let write_txn = db.begin_write()?;

        {
            let mut table = write_txn.open_table(queued_message_table)?;
            let old_value = table.get(id.as_bytes())?.map(|v| v.value());

            if let Some(old_value) = old_value {
                let mut entry: NostrQueuedMessageEntry =
                    ciborium::from_reader(old_value.as_slice())?;

                entry.num_retries += 1;
                if entry.num_retries >= entry.max_retries {
                    // remove from retry entries
                    table.remove(id.as_bytes())?;

                    // add to dead letter table for book keeping
                    let mut table = write_txn.open_table(dead_letter_table)?;

                    let mut serialized = Vec::new();
                    ciborium::into_writer(&entry, &mut serialized)?;
                    table.insert(id.as_bytes(), serialized)?;
                } else {
                    entry.last_try = Utc::now();
                    entry.processing_started_at = DateTime::from_timestamp(0, 0).expect("valid");

                    let mut serialized = Vec::new();
                    ciborium::into_writer(&entry, &mut serialized)?;
                    table.insert(id.as_bytes(), serialized)?;
                }
            }
        }

        write_txn.commit()?;
        Ok(())
    }

    fn succeed_retry_sync(
        db: Arc<Database>,
        queued_message_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        id: String,
    ) -> Result<()> {
        let write_txn = db.begin_write()?;

        {
            let mut table = write_txn.open_table(queued_message_table)?;
            table.remove(id.as_bytes())?;
        }

        write_txn.commit()?;
        Ok(())
    }

    ///////////////////////////////////////////// Delete Repo
    fn delete_repo_sync(
        db: Arc<Database>,
        offset_entry_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        last_offset_table: TableDefinition<'static, &'static [u8], u64>,
        queued_message_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        dead_letter_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
    ) -> Result<()> {
        let write_txn = db.begin_write()?;

        {
            if write_txn.open_table(offset_entry_table).is_ok() {
                write_txn.delete_table(offset_entry_table)?;
            }

            if write_txn.open_table(last_offset_table).is_ok() {
                write_txn.delete_table(last_offset_table)?;
            }

            if write_txn.open_table(queued_message_table).is_ok() {
                write_txn.delete_table(queued_message_table)?;
            }

            if write_txn.open_table(dead_letter_table).is_ok() {
                write_txn.delete_table(dead_letter_table)?;
            }
        }

        write_txn.commit()?;
        Ok(())
    }
}

#[async_trait]
impl NostrRepository for NostrDB {
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
        let queued_message_table = self.queued_message_table;
        let dead_letter_table = self.dead_letter_table;
        spawn_blocking(move || {
            Self::delete_repo_sync(
                db_clone,
                offset_entry_table,
                last_offset_table,
                queued_message_table,
                dead_letter_table,
            )
        })
        .await?
    }

    async fn add_retry_message(&self, message: NostrQueuedMessage, max_retries: i32) -> Result<()> {
        let db_clone = self.db.clone();
        let table = self.queued_message_table;
        let res = spawn_blocking(move || {
            Self::add_retry_message_sync(
                db_clone,
                table,
                NostrQueuedMessageEntry::from(message, max_retries),
            )
        })
        .await??;
        Ok(res)
    }

    async fn get_retry_messages(&self, limit: u64) -> Result<Vec<NostrQueuedMessage>> {
        let db_clone = self.db.clone();
        let table = self.queued_message_table;
        let res =
            spawn_blocking(move || Self::get_retry_messages_sync(db_clone, table, limit)).await??;
        Ok(res.into_iter().map(|entry| entry.into()).collect())
    }

    async fn fail_retry(&self, id: &str) -> Result<()> {
        let db_clone = self.db.clone();
        let table = self.queued_message_table;
        let dead_letter_table = self.dead_letter_table;
        let id_clone = id.to_owned();
        let res = spawn_blocking(move || {
            Self::fail_retry_sync(db_clone, table, dead_letter_table, id_clone)
        })
        .await??;
        Ok(res)
    }

    async fn succeed_retry(&self, id: &str) -> Result<()> {
        let db_clone = self.db.clone();
        let table = self.queued_message_table;
        let id_clone = id.to_owned();
        let res =
            spawn_blocking(move || Self::succeed_retry_sync(db_clone, table, id_clone)).await??;
        Ok(res)
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

    // Retry Queue Tests
    fn queued_message(id: &str, recipient: Option<&str>, payload: &str) -> NostrQueuedMessage {
        NostrQueuedMessage {
            id: id.to_string(),
            recipient: recipient.map(|s| s.to_string()),
            payload: payload.to_string(),
        }
    }

    fn read_queued_entry(repo: &NostrDB, id: &str) -> Option<NostrQueuedMessageEntry> {
        let read_txn = repo.db.begin_read().expect("can begin read txn");
        let table = read_txn
            .open_table(repo.queued_message_table)
            .expect("queued message table exists");

        table
            .get(id.as_bytes())
            .expect("can read queued message")
            .map(|value| {
                ciborium::from_reader(value.value().as_slice())
                    .expect("can deserialize queued message")
            })
    }

    fn read_dead_letter_entry(repo: &NostrDB, id: &str) -> Option<NostrQueuedMessageEntry> {
        let read_txn = repo.db.begin_read().expect("can begin read txn");
        let table = read_txn
            .open_table(repo.dead_letter_table)
            .expect("dead letter table exists");

        table
            .get(id.as_bytes())
            .expect("can read dead letter message")
            .map(|value| {
                ciborium::from_reader(value.value().as_slice())
                    .expect("can deserialize dead letter message")
            })
    }

    #[tokio::test]
    async fn test_add_retry_message_can_be_retrieved() {
        let repo = get_db("wallet-retry-add");

        repo.add_retry_message(queued_message("msg-1", Some("recipient-1"), "payload-1"), 3)
            .await
            .expect("add_retry_message works");

        let messages = repo
            .get_retry_messages(10)
            .await
            .expect("get_retry_messages works");

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].id, "msg-1");
        assert_eq!(messages[0].recipient.as_deref(), Some("recipient-1"));
        assert_eq!(messages[0].payload, "payload-1");
    }

    #[tokio::test]
    async fn test_get_retry_messages_respects_limit() {
        let repo = get_db("wallet-retry-limit");

        repo.add_retry_message(queued_message("msg-1", None, "payload-1"), 3)
            .await
            .expect("add msg-1 works");
        repo.add_retry_message(queued_message("msg-2", None, "payload-2"), 3)
            .await
            .expect("add msg-2 works");

        let messages = repo
            .get_retry_messages(1)
            .await
            .expect("get_retry_messages works");

        assert_eq!(messages.len(), 1);
    }

    #[tokio::test]
    async fn test_get_retry_messages_locks_messages_until_timeout() {
        let repo = get_db("wallet-retry-lock");

        repo.add_retry_message(queued_message("msg-1", None, "payload-1"), 3)
            .await
            .expect("add_retry_message works");

        let first = repo
            .get_retry_messages(10)
            .await
            .expect("first get_retry_messages works");
        let second = repo
            .get_retry_messages(10)
            .await
            .expect("second get_retry_messages works");

        assert_eq!(first.len(), 1);
        assert_eq!(first[0].id, "msg-1");
        assert!(second.is_empty());
    }

    #[tokio::test]
    async fn test_fail_retry_keeps_message_before_max_retries() {
        let repo = get_db("wallet-retry-fail-keeps");

        repo.add_retry_message(queued_message("msg-1", None, "payload-1"), 3)
            .await
            .expect("add_retry_message works");

        let _ = repo
            .get_retry_messages(10)
            .await
            .expect("get_retry_messages works");

        repo.fail_retry("msg-1").await.expect("fail_retry works");

        let entry = read_queued_entry(&repo, "msg-1").expect("message remains queued");
        assert_eq!(entry.num_retries, 1);
        assert_eq!(entry.max_retries, 3);
        assert!(entry.last_try > DateTime::from_timestamp(0, 0).expect("valid"));
        assert_eq!(
            entry.processing_started_at,
            DateTime::from_timestamp(0, 0).expect("valid")
        );

        let messages = repo
            .get_retry_messages(10)
            .await
            .expect("get_retry_messages works after fail");

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].id, "msg-1");
    }

    #[tokio::test]
    async fn test_fail_retry_moves_message_to_dead_letter_at_max_retries() {
        let repo = get_db("wallet-retry-dead-letter");

        repo.add_retry_message(queued_message("msg-1", None, "payload-1"), 1)
            .await
            .expect("add_retry_message works");

        repo.fail_retry("msg-1").await.expect("fail_retry works");

        assert!(read_queued_entry(&repo, "msg-1").is_none());

        let dead_letter =
            read_dead_letter_entry(&repo, "msg-1").expect("message moved to dead letter table");

        assert_eq!(dead_letter.id, "msg-1");
        assert_eq!(dead_letter.payload, "payload-1");
        assert_eq!(dead_letter.num_retries, 1);
        assert_eq!(dead_letter.max_retries, 1);
    }

    #[tokio::test]
    async fn test_succeed_retry_removes_message_from_queue() {
        let repo = get_db("wallet-retry-success");

        repo.add_retry_message(queued_message("msg-1", None, "payload-1"), 3)
            .await
            .expect("add_retry_message works");

        repo.succeed_retry("msg-1")
            .await
            .expect("succeed_retry works");

        let messages = repo
            .get_retry_messages(10)
            .await
            .expect("get_retry_messages works");

        assert!(messages.is_empty());
        assert!(read_queued_entry(&repo, "msg-1").is_none());
    }
}
