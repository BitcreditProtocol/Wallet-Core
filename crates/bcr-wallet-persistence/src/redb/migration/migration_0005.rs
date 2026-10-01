use crate::{
    error::{Error, Result},
    redb::{migration::WalletStorageNamespace, transaction::StoredTransaction},
};
use redb::{ReadableTable, TableDefinition};

///////////////////////////////////////////////////////////////// MIGRATION 0005
const MIGRATION_0005_PAYMENT_TYPE_RENAME: &str = "0005_payment_type_rename";

pub(super) fn migration_name_for_wallet(wallet_id: &str) -> String {
    format!("{}_{}", MIGRATION_0005_PAYMENT_TYPE_RENAME, wallet_id)
}

pub(super) fn migration_0005_payment_type_rename(
    txn: &redb::WriteTransaction,
    namespace: &WalletStorageNamespace,
) -> Result<()> {
    tracing::info!("Migrating payment type strings..");
    canonicalize_payment_types(txn, &namespace.transaction_table)?;
    tracing::info!("Migrated payment type strings.");

    Ok(())
}

fn canonicalize_payment_types(txn: &redb::WriteTransaction, table_name: &str) -> Result<()> {
    let table_def: TableDefinition<&[u8], Vec<u8>> = TableDefinition::new(table_name);

    let mut table = match txn.open_table(table_def) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => {
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    };

    let mut rewritten: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    for item in table.iter()? {
        let (k, v) = item?;
        let deserialized: StoredTransaction = borsh::from_slice(v.value().as_slice())
            .map_err(|e| Error::BorshSerialization(e.to_string()))?;
        let reserialized =
            borsh::to_vec(&deserialized).map_err(|e| Error::BorshSerialization(e.to_string()))?;
        rewritten.push((k.value().to_vec(), reserialized));
    }

    for (id, bytes) in rewritten {
        table.insert(id.as_slice(), bytes)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::redb::transaction::{
        StoredTransactionPayloadV1, StoredTransactionPayloadV2, TransactionFeesV1,
    };
    use bcr_common::cashu::{Amount, CurrencyUnit, MintUrl};
    use bcr_common::cdk_common::wallet::TransactionDirection;
    use bcr_wallet_core::types::{PaymentType, TransactionStatus};
    use redb::{Builder, ReadableDatabase, backends::InMemoryBackend};
    use std::str::FromStr;
    use uuid::Uuid;

    fn test_database() -> std::sync::Arc<redb::Database> {
        let backend = InMemoryBackend::new();
        std::sync::Arc::new(
            Builder::new()
                .create_with_backend(backend)
                .expect("create in-memory database"),
        )
    }

    fn borsh_string(s: &str) -> Vec<u8> {
        borsh::to_vec(&s.to_string()).expect("serialize string")
    }

    fn downgrade_payment_type_to_cdk18(bytes: &mut Vec<u8>) {
        let old = borsh_string("PaymentRequest");
        let new = borsh_string("Cdk18");
        let pos = bytes
            .windows(old.len())
            .position(|w| w == old.as_slice())
            .expect("payment_type bytes present in serialized payload");
        bytes.splice(pos..pos + old.len(), new);
    }

    fn v1_payload() -> StoredTransactionPayloadV1 {
        StoredTransactionPayloadV1 {
            id: Uuid::new_v4(),
            mint_url: MintUrl::from_str("https://example.com").expect("valid mint url"),
            ys: vec![],
            amount: Amount::from(42u64),
            fees: Amount::ZERO,
            unit: CurrencyUnit::Sat,
            tstamp: 1_750_000_000,
            direction: TransactionDirection::Outgoing,
            memo: None,
            payment_type: PaymentType::PaymentRequest,
            status: TransactionStatus::Settled,
            btc_tx_id: None,
            quote_id: None,
            nostr_event_id: None,
            contact_node_id: None,
            payment_request_id: None,
            linked_txs: vec![],
        }
    }

    fn v2_payload() -> StoredTransactionPayloadV2 {
        StoredTransactionPayloadV2 {
            id: Uuid::new_v4(),
            mint_url: MintUrl::from_str("https://example.com").expect("valid mint url"),
            ys: vec![],
            amount: Amount::from(42u64),
            fees: TransactionFeesV1 {
                swap: Amount::ZERO,
                network: Amount::ZERO,
                melt: Amount::ZERO,
            },
            unit: CurrencyUnit::Sat,
            tstamp: 1_750_000_000,
            direction: TransactionDirection::Outgoing,
            memo: None,
            payment_type: PaymentType::PaymentRequest,
            status: TransactionStatus::Settled,
            btc_tx_id: None,
            quote_id: None,
            nostr_event_id: None,
            contact_node_id: None,
            payment_request_id: None,
            linked_txs: vec![],
        }
    }

    #[test]
    fn migrates_legacy_cdk18_string_in_v1_and_v2_rows() {
        let db = test_database();
        let table_name = "wallet-1_transactions";
        let table_def: TableDefinition<&[u8], Vec<u8>> = TableDefinition::new(table_name);

        let v1_id = Uuid::new_v4();
        let mut v1_bytes = borsh::to_vec(&StoredTransaction::V1(v1_payload())).unwrap();
        downgrade_payment_type_to_cdk18(&mut v1_bytes);

        let v2_id = Uuid::new_v4();
        let mut v2_bytes = borsh::to_vec(&StoredTransaction::V2(v2_payload())).unwrap();
        downgrade_payment_type_to_cdk18(&mut v2_bytes);

        assert!(v1_bytes.windows(5).any(|w| w == b"Cdk18"));
        assert!(v2_bytes.windows(5).any(|w| w == b"Cdk18"));

        {
            let write_txn = db.begin_write().unwrap();
            {
                let mut table = write_txn.open_table(table_def).unwrap();
                table.insert(v1_id.as_bytes().as_slice(), v1_bytes).unwrap();
                table.insert(v2_id.as_bytes().as_slice(), v2_bytes).unwrap();
            }
            write_txn.commit().unwrap();
        }

        {
            let write_txn = db.begin_write().unwrap();
            canonicalize_payment_types(&write_txn, table_name).expect("migration succeeds");
            write_txn.commit().unwrap();
        }

        let read_txn = db.begin_read().unwrap();
        let table = read_txn.open_table(table_def).unwrap();

        for id in [v1_id, v2_id] {
            let bytes = table
                .get(id.as_bytes().as_slice())
                .unwrap()
                .unwrap()
                .value();
            let stored: StoredTransaction = borsh::from_slice(&bytes).unwrap();
            let payment_type = match stored {
                StoredTransaction::V1(p) => p.payment_type,
                StoredTransaction::V2(p) => p.payment_type,
            };
            assert_eq!(payment_type, PaymentType::PaymentRequest);
            assert!(
                !bytes.windows(5).any(|w| w == b"Cdk18"),
                "Cdk18 string must be rewritten to PaymentRequest"
            );
        }
    }

    #[tokio::test]
    async fn list_txs_succeeds_after_migrating_legacy_cdk18_rows() {
        use crate::TransactionRepository;
        use crate::redb::transaction::TransactionDB;

        let db = test_database();
        let wallet_id = "wallet-list-txs";
        let table_name = TransactionDB::transaction_table_name(wallet_id);

        let tx_id = Uuid::new_v4();
        let mut bytes = borsh::to_vec(&StoredTransaction::V2(v2_payload())).unwrap();
        downgrade_payment_type_to_cdk18(&mut bytes);

        {
            let table_def: TableDefinition<&[u8], Vec<u8>> =
                TableDefinition::new(table_name.as_str());
            let write_txn = db.begin_write().unwrap();
            {
                let mut table = write_txn.open_table(table_def).unwrap();
                table.insert(tx_id.as_bytes().as_slice(), bytes).unwrap();
            }
            write_txn.commit().unwrap();
        }

        {
            let write_txn = db.begin_write().unwrap();
            canonicalize_payment_types(&write_txn, &table_name).expect("migration succeeds");
            write_txn.commit().unwrap();
        }

        let repo = TransactionDB::new(db, wallet_id).expect("create TransactionDB");
        let txs = repo
            .list_txs()
            .await
            .expect("list_txs no longer hard-fails on a legacy Cdk18 row");
        assert_eq!(txs.len(), 1);
        assert_eq!(txs[0].payment_type, PaymentType::PaymentRequest);
    }
}
