use crate::{
    error::{Error, Result},
    redb::pocket::PocketDB,
};
use bcr_common::cashu::CurrencyUnit;
use borsh::{BorshDeserialize, BorshSerialize};
use chrono::Utc;
use redb::{Database, ReadableTable, TableDefinition};
use std::sync::Arc;

mod migration_0001;
mod migration_0002;

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub struct AppliedMigration {
    id: String,
    applied_at: u64,
}

const MIGRATIONS_TABLE: TableDefinition<&str, Vec<u8>> = TableDefinition::new("migrations");

pub async fn migrate_purse(db: Arc<Database>) -> Result<()> {
    tokio::task::spawn_blocking(move || migrate_purse_sync(db))
        .await
        .map_err(|e| Error::Custom(format!("migration task failed: {e}")))?
}

fn migrate_purse_sync(db: Arc<Database>) -> Result<()> {
    tracing::info!("Checking DB migrations for purse ..");
    let write_txn = db.begin_write()?;

    let applied = load_applied_migrations(&write_txn)?;

    let migration_0002_id_for_purse = migration_0002::migration_name_for_purse();
    if !applied.contains(&migration_0002_id_for_purse) {
        tracing::info!("Applying Migration 0002 for purse");
        migration_0002::migration_0002_add_purse_envelope(&write_txn)?;
        mark_migration_applied(&write_txn, &migration_0002_id_for_purse)?;
        tracing::info!("Applied Migration 0002 for purse");
    }

    write_txn.commit()?;
    tracing::info!("Finished DB migrations for purse");
    Ok(())
}

pub async fn migrate_wallet(db: Arc<Database>, namespace: WalletStorageNamespace) -> Result<()> {
    tokio::task::spawn_blocking(move || migrate_wallet_sync(db, namespace))
        .await
        .map_err(|e| Error::Custom(format!("migration task failed: {e}")))?
}

fn migrate_wallet_sync(db: Arc<Database>, namespace: WalletStorageNamespace) -> Result<()> {
    tracing::info!(
        "Checking DB migrations for wallet {}..",
        &namespace.wallet_id
    );
    let write_txn = db.begin_write()?;

    let applied = load_applied_migrations(&write_txn)?;

    let migration_0001_id_for_wallet =
        migration_0001::migration_name_for_wallet(&namespace.wallet_id);
    if !applied.contains(&migration_0001_id_for_wallet) {
        tracing::info!(
            "Applying Migration 0001 for wallet {}",
            &namespace.wallet_id
        );
        migration_0001::migration_0001_add_proof_envelope_and_encryption(&write_txn, &namespace)?;
        mark_migration_applied(&write_txn, &migration_0001_id_for_wallet)?;
        tracing::info!("Applied Migration 0001 for wallet {}", &namespace.wallet_id);
    }

    write_txn.commit()?;
    tracing::info!(
        "Finished DB migrations for wallet {}.",
        &namespace.wallet_id
    );
    Ok(())
}

#[derive(Debug, Clone)]
/// Collected the storage namespace a wallet
pub struct WalletStorageNamespace {
    wallet_id: String,
    proof_table: String,
    keys: bitcoin::secp256k1::Keypair,
}

pub fn collect_wallet_namespace(
    wallet_id: String,
    unit: CurrencyUnit,
    keys: bitcoin::secp256k1::Keypair,
) -> WalletStorageNamespace {
    WalletStorageNamespace {
        wallet_id: wallet_id.to_string(),
        proof_table: PocketDB::proof_table_name(&wallet_id, &unit),
        keys,
    }
}

fn load_applied_migrations(
    txn: &redb::WriteTransaction,
) -> Result<std::collections::HashSet<String>> {
    let table = match txn.open_table(MIGRATIONS_TABLE) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => {
            return Ok(std::collections::HashSet::new());
        }
        Err(e) => return Err(e.into()),
    };

    let mut applied = std::collections::HashSet::new();
    for item in table.iter()? {
        let (key, _) = item?;
        applied.insert(key.value().to_string());
    }

    Ok(applied)
}

fn mark_migration_applied(txn: &redb::WriteTransaction, migration_id: &str) -> Result<()> {
    let mut table = txn.open_table(MIGRATIONS_TABLE)?;

    let record = AppliedMigration {
        id: migration_id.to_string(),
        applied_at: Utc::now().timestamp() as u64,
    };

    let serialized =
        borsh::to_vec(&record).map_err(|e| Error::BorshSerialization(e.to_string()))?;
    table.insert(migration_id, serialized)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use redb::{Builder, backends::InMemoryBackend};

    fn test_database() -> Arc<Database> {
        let backend = InMemoryBackend::new();
        Arc::new(
            Builder::new()
                .create_with_backend(backend)
                .expect("create in-memory database"),
        )
    }

    fn test_keypair() -> bitcoin::secp256k1::Keypair {
        bitcoin::secp256k1::Keypair::new_global(&mut bitcoin::secp256k1::rand::thread_rng())
    }

    fn test_namespace(wallet_id: &str) -> WalletStorageNamespace {
        collect_wallet_namespace(wallet_id.to_string(), CurrencyUnit::Sat, test_keypair())
    }

    #[test]
    fn collect_wallet_namespace_builds_expected_values() {
        let wallet_id = "wallet-1";
        let keys = test_keypair();
        let namespace = collect_wallet_namespace(wallet_id.to_string(), CurrencyUnit::Sat, keys);

        assert_eq!(namespace.wallet_id, wallet_id);
        assert_eq!(
            namespace.proof_table,
            PocketDB::proof_table_name(wallet_id, &CurrencyUnit::Sat)
        );
        assert_eq!(namespace.keys.public_key(), keys.public_key());
        assert_eq!(namespace.keys.secret_key(), keys.secret_key());
    }

    #[test]
    fn load_applied_migrations_is_empty_for_new_database() {
        let db = test_database();
        let write_txn = db.begin_write().expect("begin write transaction");
        let applied = load_applied_migrations(&write_txn).expect("load applied migrations");

        assert!(applied.is_empty());
    }

    #[test]
    fn mark_migration_applied_persists_migration() {
        let db = test_database();
        let migration_id = "migration-0001-wallet-1";

        {
            let write_txn = db.begin_write().expect("begin write transaction");
            mark_migration_applied(&write_txn, migration_id).expect("mark migration as applied");
            write_txn.commit().expect("commit migration record");
        }

        let write_txn = db.begin_write().expect("begin write transaction");
        let applied = load_applied_migrations(&write_txn).expect("load applied migrations");

        assert_eq!(applied.len(), 1);
        assert!(applied.contains(migration_id));
    }

    #[test]
    fn multiple_migrations_are_loaded() {
        let db = test_database();
        let first = "migration-0001-wallet-1";
        let second = "migration-0001-wallet-2";

        {
            let write_txn = db.begin_write().expect("begin write transaction");
            mark_migration_applied(&write_txn, first).expect("mark first migration");
            mark_migration_applied(&write_txn, second).expect("mark second migration");
            write_txn.commit().expect("commit migration records");
        }

        let write_txn = db.begin_write().expect("begin write transaction");
        let applied = load_applied_migrations(&write_txn).expect("load applied migrations");

        assert_eq!(applied.len(), 2);
        assert!(applied.contains(first));
        assert!(applied.contains(second));
    }

    #[tokio::test]
    async fn migrate_records_migration_for_wallet() {
        let db = test_database();
        let namespace = test_namespace("wallet-1");
        let expected_id = migration_0001::migration_name_for_wallet(&namespace.wallet_id);

        migrate_wallet(db.clone(), namespace)
            .await
            .expect("migration succeeds");

        let write_txn = db.begin_write().expect("begin write transaction");
        let applied = load_applied_migrations(&write_txn).expect("load applied migrations");

        assert!(applied.contains(&expected_id));
    }

    #[tokio::test]
    async fn migrations_are_scoped_per_wallet() {
        let db = test_database();
        let first_namespace = test_namespace("wallet-1");
        let second_namespace = test_namespace("wallet-2");

        let first_id = migration_0001::migration_name_for_wallet(&first_namespace.wallet_id);
        let second_id = migration_0001::migration_name_for_wallet(&second_namespace.wallet_id);

        assert_ne!(first_id, second_id);

        migrate_wallet(db.clone(), first_namespace)
            .await
            .expect("first wallet migration succeeds");

        migrate_wallet(db.clone(), second_namespace)
            .await
            .expect("second wallet migration succeeds");

        let write_txn = db.begin_write().expect("begin write transaction");
        let applied = load_applied_migrations(&write_txn).expect("load applied migrations");

        assert!(applied.contains(&first_id));
        assert!(applied.contains(&second_id));
    }
}
