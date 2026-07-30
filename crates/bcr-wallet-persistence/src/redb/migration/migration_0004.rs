use crate::{
    error::{Error, Result},
    redb::{migration::WalletStorageNamespace, transaction::StoredTransaction},
};
use bcr_wallet_core::types::Transaction;
use redb::{ReadableTable, TableDefinition};

///////////////////////////////////////////////////////////////// MIGRATION 0004
const MIGRATION_0004_TX_FEES: &str = "0004_tx_fees";

pub(super) fn migration_name_for_wallet(wallet_id: &str) -> String {
    format!("{}_{}", MIGRATION_0004_TX_FEES, wallet_id)
}

pub(super) fn migration_0004_tx_fees(
    txn: &redb::WriteTransaction,
    namespace: &WalletStorageNamespace,
) -> Result<()> {
    tracing::info!("Migrating transaction fees..");
    migrate_transaction_fees(txn, &namespace.transaction_table)?;
    tracing::info!("Migrated transaction fees.");

    Ok(())
}

// Fetch v1 transactions
// transform to v2
// put in V2 envelope
// Store new transactions
fn migrate_transaction_fees(txn: &redb::WriteTransaction, table_name: &str) -> Result<()> {
    let table_def: TableDefinition<&[u8], Vec<u8>> = TableDefinition::new(table_name);

    let mut table = match txn.open_table(table_def) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => {
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    };

    let mut migrated_transactions: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    for item in table.iter()? {
        let (_, v) = item?;

        let deserialized: StoredTransaction = borsh::from_slice(v.value().as_slice())
            .map_err(|e| Error::BorshSerialization(e.to_string()))?;
        let StoredTransaction::V1(old_tx) = deserialized else {
            continue;
        };
        let new_tx: Transaction = old_tx.try_into()?;
        let new_tx_id = new_tx.id;
        let stored_tx_v2 = StoredTransaction::V2(new_tx.into());

        let migrated_tx_bytes =
            borsh::to_vec(&stored_tx_v2).map_err(|e| Error::BorshSerialization(e.to_string()))?;

        migrated_transactions.push((new_tx_id.as_bytes().to_vec(), migrated_tx_bytes));
    }

    // Add new entries
    for (id, new_transaction) in migrated_transactions.iter() {
        table.insert(id.as_slice(), new_transaction)?;
    }

    Ok(())
}
