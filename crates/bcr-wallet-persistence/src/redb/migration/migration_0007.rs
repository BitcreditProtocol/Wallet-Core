use crate::{
    error::{Error, Result},
    redb::{
        migration::WalletStorageNamespace,
        pocket::{PocketDB, StoredProof, from_stored_proof_v1},
        transaction::{StoredTransaction, TransactionDB},
    },
};
use redb::{ReadableTable, TableDefinition};
use uuid::Uuid;

///////////////////////////////////////////////////////////////// MIGRATION 0007
const MIGRATION_0007_INDICES: &str = "0007_indices";

pub(super) fn migration_name_for_wallet(namespace: &WalletStorageNamespace) -> String {
    format!("{}_{}", MIGRATION_0007_INDICES, namespace.proof_index_table)
}

pub(super) fn migration_0007_indices(
    txn: &redb::WriteTransaction,
    namespace: &WalletStorageNamespace,
) -> Result<()> {
    tracing::info!("Building proof and transaction indices..");
    rebuild_index(
        txn,
        &namespace.proof_table,
        &namespace.proof_index_table,
        |k, v| {
            let y = bcr_common::cashu::PublicKey::from_slice(k)?;
            let stored: StoredProof =
                borsh::from_slice(v).map_err(|e| Error::BorshSerialization(e.to_string()))?;
            let (_, state) = from_stored_proof_v1(stored, namespace.keys)?;
            Ok(PocketDB::proof_index_key(state, &y))
        },
    )?;
    rebuild_index(
        txn,
        &namespace.transaction_table,
        &namespace.transaction_index_table,
        |k, v| {
            let id =
                Uuid::from_slice(k).map_err(|e| Error::InvalidTransactionData(e.to_string()))?;
            let stored: StoredTransaction =
                borsh::from_slice(v).map_err(|e| Error::BorshSerialization(e.to_string()))?;
            Ok(TransactionDB::transaction_index_key(stored.tstamp(), &id).to_vec())
        },
    )?;
    tracing::info!("Built proof and transaction indices.");

    Ok(())
}

fn rebuild_index(
    txn: &redb::WriteTransaction,
    table_name: &str,
    index_table_name: &str,
    index_key: impl Fn(&[u8], &[u8]) -> Result<Vec<u8>>,
) -> Result<()> {
    let table_def: TableDefinition<&[u8], Vec<u8>> = TableDefinition::new(table_name);
    let index_def: TableDefinition<&[u8], ()> = TableDefinition::new(index_table_name);

    txn.delete_table(index_def)?;

    let table = match txn.open_table(table_def) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(()),
        Err(e) => return Err(e.into()),
    };

    let mut index = txn.open_table(index_def)?;
    for item in table.iter()? {
        let (k, v) = item?;
        index.insert(index_key(k.value(), v.value().as_slice())?.as_slice(), ())?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        PocketRepository, TransactionRepository,
        redb::{migration::collect_wallet_namespace, transaction::tests::test_tx},
        test_utils::tests::wallet_id,
    };
    use bcr_common::{
        cashu::{Amount, CurrencyUnit},
        core_tests,
    };
    use redb::{Builder, backends::InMemoryBackend};
    use std::sync::Arc;

    fn proof() -> bcr_common::cashu::Proof {
        let (_, keyset) = core_tests::generate_random_ecash_keyset();
        core_tests::generate_random_ecash_proofs(&keyset, &[Amount::from(16u64)])[0].clone()
    }

    #[tokio::test]
    async fn indices_backfilled_by_migration() {
        let db = Arc::new(
            Builder::new()
                .create_with_backend(InMemoryBackend::new())
                .expect("db"),
        );
        let keys =
            bitcoin::secp256k1::Keypair::new_global(&mut bitcoin::secp256k1::rand::thread_rng());
        let wid = wallet_id();
        let namespace = collect_wallet_namespace(wid.clone(), CurrencyUnit::Sat, keys);
        let proofs = PocketDB::new(db.clone(), &wid, &CurrencyUnit::Sat, keys).expect("repo");
        let unspent = proofs.store_new(proof()).await.unwrap();
        let pending = proofs.store_pendingspent(proof()).await.unwrap();
        let txs = TransactionDB::new(db.clone(), &wid).expect("repo");
        for (id, tstamp) in [(1u128, 200u64), (2, 100), (3, 50)] {
            let mut tx = test_tx();
            tx.id = Uuid::from_u128(id);
            tx.tstamp = tstamp;
            txs.store_tx(tx).await.unwrap();
        }
        let tx_ids = || async {
            let listed = txs.list_txs_by_time(false, None, Default::default(), usize::MAX);
            let ids = listed.await.unwrap().into_iter().map(|tx| tx.id.as_u128());
            ids.collect::<Vec<_>>()
        };

        let txn = db.begin_write().expect("write");
        for name in [
            &namespace.proof_index_table,
            &namespace.transaction_index_table,
        ] {
            let index_def: TableDefinition<&[u8], ()> = TableDefinition::new(name);
            txn.delete_table(index_def).expect("drop index");
        }
        txn.commit().expect("commit");
        assert!(proofs.list_unspent().await.unwrap().is_empty());
        assert!(tx_ids().await.is_empty());

        for _ in 0..2 {
            let txn = db.begin_write().expect("write");
            migration_0007_indices(&txn, &namespace).expect("migrate");
            txn.commit().expect("commit");
            let listed = proofs.list_unspent().await.unwrap();
            assert_eq!(listed.into_keys().collect::<Vec<_>>(), vec![unspent]);
            let listed = proofs.list_pending().await.unwrap();
            assert_eq!(listed.into_keys().collect::<Vec<_>>(), vec![pending]);
            assert_eq!(tx_ids().await, vec![3, 2, 1]);
        }
    }
}
