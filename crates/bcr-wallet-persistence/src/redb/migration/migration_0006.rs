use crate::{
    error::{Error, Result},
    redb::{
        migration::WalletStorageNamespace,
        pocket::{PocketDB, StoredProof, from_stored_proof_v1},
    },
};
use redb::{ReadableTable, TableDefinition};

///////////////////////////////////////////////////////////////// MIGRATION 0006
const MIGRATION_0006_PROOF_INDEX: &str = "0006_proof_index";

pub(super) fn migration_name_for_wallet(wallet_id: &str) -> String {
    format!("{}_{}", MIGRATION_0006_PROOF_INDEX, wallet_id)
}

pub(super) fn migration_0006_proof_index(
    txn: &redb::WriteTransaction,
    namespace: &WalletStorageNamespace,
) -> Result<()> {
    tracing::info!("Building proof state index..");
    rebuild_proof_index(
        txn,
        &namespace.proof_table,
        &namespace.proof_index_table,
        &namespace.keys,
    )?;
    tracing::info!("Built proof state index.");

    Ok(())
}

fn rebuild_proof_index(
    txn: &redb::WriteTransaction,
    proof_table_name: &str,
    index_table_name: &str,
    keys: &bitcoin::secp256k1::Keypair,
) -> Result<()> {
    let proof_def: TableDefinition<&[u8], Vec<u8>> = TableDefinition::new(proof_table_name);
    let index_def: TableDefinition<&[u8], ()> = TableDefinition::new(index_table_name);

    txn.delete_table(index_def)?;

    let proofs = match txn.open_table(proof_def) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(()),
        Err(e) => return Err(e.into()),
    };

    let mut index = txn.open_table(index_def)?;
    for item in proofs.iter()? {
        let (k, v) = item?;
        let y = bcr_common::cashu::PublicKey::from_slice(k.value())?;
        let stored: StoredProof = borsh::from_slice(v.value().as_slice())
            .map_err(|e| Error::BorshSerialization(e.to_string()))?;
        let (_, state) = from_stored_proof_v1(stored, *keys)?;
        index.insert(PocketDB::proof_index_key(state, &y).as_slice(), ())?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PocketRepository;
    use crate::redb::migration::collect_wallet_namespace;
    use crate::test_utils::tests::wallet_id;
    use bcr_common::{
        cashu::{Amount, CurrencyUnit},
        core_tests,
    };
    use redb::{Builder, Database, ReadableDatabase, backends::InMemoryBackend};
    use std::sync::Arc;

    fn proof() -> bcr_common::cashu::Proof {
        let (_, keyset) = core_tests::generate_random_ecash_keyset();
        core_tests::generate_random_ecash_proofs(&keyset, &[Amount::from(16u64)])[0].clone()
    }

    fn index_keys(db: &Arc<Database>, name: &str) -> Vec<Vec<u8>> {
        let def: TableDefinition<&[u8], ()> = TableDefinition::new(name);
        let read = db.begin_read().expect("read");
        match read.open_table(def) {
            Ok(table) => table
                .iter()
                .expect("iter")
                .map(|i| i.expect("row").0.value().to_vec())
                .collect(),
            Err(redb::TableError::TableDoesNotExist(_)) => vec![],
            Err(e) => panic!("{e}"),
        }
    }

    fn migrate(db: &Arc<Database>, namespace: &WalletStorageNamespace) {
        let txn = db.begin_write().expect("write");
        migration_0006_proof_index(&txn, namespace).expect("migrate");
        txn.commit().expect("commit");
    }

    fn drop_index(db: &Arc<Database>, name: &str) {
        let def: TableDefinition<&[u8], ()> = TableDefinition::new(name);
        let txn = db.begin_write().expect("write");
        txn.delete_table(def).expect("drop index");
        txn.commit().expect("commit");
    }

    #[tokio::test]
    async fn proof_index_backfilled_by_migration() {
        let db = Arc::new(
            Builder::new()
                .create_with_backend(InMemoryBackend::new())
                .expect("db"),
        );
        let keys =
            bitcoin::secp256k1::Keypair::new_global(&mut bitcoin::secp256k1::rand::thread_rng());
        let unit = CurrencyUnit::Sat;
        let wid = wallet_id();
        let namespace = collect_wallet_namespace(wid.clone(), unit.clone(), keys);
        let index_name = PocketDB::proof_index_table_name(&wid, &unit);

        migrate(&db, &namespace);
        assert!(index_keys(&db, &index_name).is_empty());

        let repo = PocketDB::new(db.clone(), &wid, &unit, keys).expect("repo");
        let unspent = repo.store_new(proof()).await.unwrap();
        let held = repo.store_pendingspent(proof()).await.unwrap();
        let spent = repo.store_new(proof()).await.unwrap();
        repo.mark_as_pendingspent(vec![spent]).await.unwrap();
        repo.mark_pending_as_spent(spent).await.unwrap();

        let mut expected = vec![
            PocketDB::proof_index_key(bcr_common::cashu::State::Unspent, &unspent),
            PocketDB::proof_index_key(bcr_common::cashu::State::PendingSpent, &held),
            PocketDB::proof_index_key(bcr_common::cashu::State::Spent, &spent),
        ];
        expected.sort();

        drop_index(&db, &index_name);
        assert!(index_keys(&db, &index_name).is_empty());

        migrate(&db, &namespace);
        assert_eq!(index_keys(&db, &index_name), expected);

        migrate(&db, &namespace);
        assert_eq!(index_keys(&db, &index_name), expected);
    }
}
