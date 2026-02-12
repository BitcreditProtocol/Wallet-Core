use crate::error::Result;
use bcr_wallet_core::types::JobState;
use redb::{Database, ReadableDatabase, TableDefinition, TableError};
use std::sync::Arc;
use tokio::task::spawn_blocking;

///////////////////////////////////////////// JobState
const JOBS_TABLE: TableDefinition<&[u8], Vec<u8>> = TableDefinition::new("jobs");

///////////////////////////////////////////// JobsDB
pub struct JobsDB {
    db: Arc<Database>,
}

impl JobsDB {
    const JOBS_MAIN_ID: &'static str = "main";

    pub fn new(db: Arc<Database>) -> Result<Self> {
        Ok(Self { db })
    }

    fn store_sync(&self, job_state: JobState) -> Result<()> {
        let write_txn = self.db.begin_write()?;

        {
            let mut table = write_txn.open_table(JOBS_TABLE)?;

            let mut serialized = Vec::new();
            ciborium::into_writer(&job_state, &mut serialized)?;
            table.insert(Self::JOBS_MAIN_ID.as_bytes(), serialized)?;
        }

        write_txn.commit()?;
        Ok(())
    }

    fn load_sync(&self) -> Result<JobState> {
        let read_txn = self.db.begin_read()?;

        match read_txn.open_table(JOBS_TABLE) {
            Ok(table) => {
                let entry = table.get(Self::JOBS_MAIN_ID.as_bytes())?;
                match entry {
                    Some(e) => {
                        let job_state: JobState = ciborium::from_reader(e.value().as_slice())?;
                        Ok(job_state)
                    }
                    None => Ok(JobState::default()),
                }
            }
            Err(TableError::TableDoesNotExist(_)) => Ok(JobState::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn store(self: Arc<Self>, job_state: JobState) -> Result<()> {
        spawn_blocking(move || self.store_sync(job_state)).await?
    }

    pub async fn load(self: Arc<Self>) -> Result<JobState> {
        spawn_blocking(move || self.load_sync()).await?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use redb::{Builder, backends::InMemoryBackend};

    fn get_db() -> JobsDB {
        let in_mem_db = InMemoryBackend::new();
        JobsDB {
            db: Arc::new(
                Builder::new()
                    .create_with_backend(in_mem_db)
                    .expect("can create in-memory redb"),
            ),
        }
    }

    #[tokio::test]
    async fn test_store_load() {
        let now = Utc::now();
        let db = Arc::new(get_db());
        let mut default_state = db.clone().load().await.expect("load works");
        assert_eq!(default_state, JobState::default());

        default_state.last_run = now;

        db.clone()
            .store(default_state)
            .await
            .expect("can store job state");
        let updated_state = db.load().await.expect("load works");
        assert_eq!(updated_state.last_run, now);
    }
}
