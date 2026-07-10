use crate::{
    error::{Error, Result},
    redb::{migration::WalletStorageNamespace, pocket},
};
use bcr_common::cashu::{
    nut00 as cdk00, nut01 as cdk01, nut02 as cdk02, nut07 as cdk07, nut12 as cdk12, secret::Secret,
};
use redb::{ReadableTable, TableDefinition};

///////////////////////////////////////////////////////////////// MIGRATION 0001
const MIGRATION_0001_ADD_VERSIONED_ENVELOPES_AND_ENCRYPTION: &str =
    "0001_add_versioned_envelopes_and_encryption";

pub(super) fn migration_name_for_wallet(wallet_id: &str) -> String {
    format!(
        "{}_{}",
        MIGRATION_0001_ADD_VERSIONED_ENVELOPES_AND_ENCRYPTION, wallet_id
    )
}

pub(super) fn migration_0001_add_proof_envelope_and_encryption(
    txn: &redb::WriteTransaction,
    namespace: &WalletStorageNamespace,
) -> Result<()> {
    migrate_proof_table_to_envelope_and_encryption(txn, &namespace.proof_table, &namespace.keys)?;

    Ok(())
}

// Fetch old proofs
// convert to cdk proofs
// put in V1 envelope and encrypt
// Store new proofs
fn migrate_proof_table_to_envelope_and_encryption(
    txn: &redb::WriteTransaction,
    table_name: &str,
    keys: &bitcoin::secp256k1::Keypair,
) -> Result<()> {
    let table_def: TableDefinition<&[u8], Vec<u8>> = TableDefinition::new(table_name);

    let mut table = match txn.open_table(table_def) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => {
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    };

    let mut migrated_proofs: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    for item in table.iter()? {
        let (k, v) = item?;
        let proof: ProofEntry = ciborium::from_reader(v.value().as_slice())?;
        let state = proof.state;
        let cdk_proof: cdk00::Proof = proof.into();
        let stored_proof_v1 = pocket::to_stored_proof_v1(cdk_proof, Some(state), keys.to_owned())?;

        let migrated_proof_bytes = borsh::to_vec(&stored_proof_v1)
            .map_err(|e| Error::BorshSerialization(e.to_string()))?;
        migrated_proofs.push((k.value().to_vec(), migrated_proof_bytes));
    }

    for (key, new_proof) in migrated_proofs.iter() {
        table.insert(key.as_slice(), new_proof)?;
    }

    Ok(())
}

///////////////////////////////////////////// pre-0001 ProofEntry for the 0001 migration
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
struct ProofEntry {
    y: cdk01::PublicKey,
    amount: bcr_common::cashu::Amount,
    keyset_id: cdk02::Id,
    secret: Secret,
    c: cdk01::PublicKey,
    witness: Option<cdk00::Witness>,
    dleq: Option<cdk12::ProofDleq>,
    state: cdk07::State,
}

impl std::convert::From<cdk00::Proof> for ProofEntry {
    fn from(proof: cdk00::Proof) -> Self {
        let y = proof.y().expect("Hash to curve should not fail");
        ProofEntry {
            y,
            amount: proof.amount,
            keyset_id: proof.keyset_id,
            secret: proof.secret,
            c: proof.c,
            witness: proof.witness,
            dleq: proof.dleq,
            state: cdk07::State::Unspent,
        }
    }
}

impl std::convert::From<ProofEntry> for cdk00::Proof {
    fn from(entry: ProofEntry) -> Self {
        cdk00::Proof {
            amount: entry.amount,
            keyset_id: entry.keyset_id,
            secret: entry.secret,
            c: entry.c,
            witness: entry.witness,
            dleq: entry.dleq,
            p2pk_e: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::redb::{
        migration::collect_wallet_namespace,
        pocket::{StoredProof, from_stored_proof_v1},
    };
    use bcr_common::{
        cashu::{Amount, CurrencyUnit},
        core_tests,
    };
    use redb::{Builder, Database, ReadableDatabase, TableDefinition, backends::InMemoryBackend};
    use std::sync::Arc;

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

    fn test_proof() -> cdk00::Proof {
        let (_, keyset) = core_tests::generate_random_ecash_keyset();
        let proofs = core_tests::generate_random_ecash_proofs(&keyset, &[Amount::from(16_u64)]);

        proofs[0].clone()
    }

    fn namespace(wallet_id: &str, keys: bitcoin::secp256k1::Keypair) -> WalletStorageNamespace {
        collect_wallet_namespace(wallet_id.to_string(), CurrencyUnit::Sat, keys)
    }

    fn insert_legacy_proof(
        db: &Arc<Database>,
        table_name: &str,
        proof: cdk00::Proof,
        state: cdk07::State,
    ) -> cdk01::PublicKey {
        let y = proof.y().expect("proof has valid y");
        let mut legacy_entry = ProofEntry::from(proof);
        legacy_entry.state = state;
        let mut encoded = Vec::new();
        ciborium::into_writer(&legacy_entry, &mut encoded).expect("serialize legacy proof");
        let table_def: TableDefinition<&[u8], Vec<u8>> = TableDefinition::new(table_name);
        let write_txn = db.begin_write().expect("begin write transaction");
        {
            let mut table = write_txn
                .open_table(table_def)
                .expect("open legacy proof table");

            table
                .insert(y.to_bytes().as_slice(), encoded)
                .expect("insert legacy proof");
        }
        write_txn.commit().expect("commit legacy proof");
        y
    }

    fn load_migrated_proof(
        db: &Arc<Database>,
        table_name: &str,
        y: cdk01::PublicKey,
        keys: bitcoin::secp256k1::Keypair,
    ) -> (cdk00::Proof, cdk07::State) {
        let table_def: TableDefinition<&[u8], Vec<u8>> = TableDefinition::new(table_name);

        let read_txn = db.begin_read().expect("begin read transaction");
        let table = read_txn
            .open_table(table_def)
            .expect("open migrated proof table");

        let stored = table
            .get(y.to_bytes().as_slice())
            .expect("read migrated proof")
            .expect("migrated proof exists");

        let envelope: StoredProof = borsh::from_slice(stored.value().as_slice())
            .expect("deserialize stored proof envelope");

        from_stored_proof_v1(envelope, keys).expect("decrypt migrated proof")
    }

    #[test]
    fn migration_name_contains_migration_and_wallet_ids() {
        let wallet_id = "wallet-123";
        let migration_name = migration_name_for_wallet(wallet_id);

        assert_eq!(
            migration_name,
            "0001_add_versioned_envelopes_and_encryption_wallet-123"
        );
    }

    #[test]
    fn migration_name_is_different_for_each_wallet() {
        let first = migration_name_for_wallet("wallet-1");
        let second = migration_name_for_wallet("wallet-2");

        assert_ne!(first, second);
    }

    #[test]
    fn migration_succeeds_when_proof_table_does_not_exist() {
        let db = test_database();
        let keys = test_keypair();
        let namespace = namespace("wallet-1", keys);

        let write_txn = db.begin_write().expect("begin write transaction");

        migration_0001_add_proof_envelope_and_encryption(&write_txn, &namespace)
            .expect("migration succeeds for missing proof table");

        write_txn.commit().expect("commit migration");
    }

    #[test]
    fn migration_converts_legacy_proof_to_encrypted_envelope() {
        let db = test_database();
        let keys = test_keypair();
        let namespace = namespace("wallet-1", keys);

        let original = test_proof();
        let y = insert_legacy_proof(
            &db,
            &namespace.proof_table,
            original.clone(),
            cdk07::State::Unspent,
        );

        let write_txn = db.begin_write().expect("begin write transaction");
        migration_0001_add_proof_envelope_and_encryption(&write_txn, &namespace)
            .expect("migrate legacy proof");
        write_txn.commit().expect("commit migration");
        let (migrated, state) = load_migrated_proof(&db, &namespace.proof_table, y, keys);

        assert_eq!(migrated, original);
        assert_eq!(state, cdk07::State::Unspent);
    }

    #[test]
    fn migration_preserves_state() {
        let db = test_database();
        let keys = test_keypair();
        let namespace = namespace("wallet-1", keys);

        let original = test_proof();
        let y = insert_legacy_proof(
            &db,
            &namespace.proof_table,
            original.clone(),
            cdk07::State::PendingSpent,
        );
        let write_txn = db.begin_write().expect("begin write transaction");
        migration_0001_add_proof_envelope_and_encryption(&write_txn, &namespace)
            .expect("migrate legacy proof");
        write_txn.commit().expect("commit migration");
        let (migrated, state) = load_migrated_proof(&db, &namespace.proof_table, y, keys);

        assert_eq!(migrated, original);
        assert_eq!(state, cdk07::State::PendingSpent);
    }

    #[test]
    fn migration_preserves_database_key() {
        let db = test_database();
        let keys = test_keypair();
        let namespace = namespace("wallet-1", keys);

        let original = test_proof();
        let expected_y = original.y().expect("proof has valid y");

        insert_legacy_proof(&db, &namespace.proof_table, original, cdk07::State::Unspent);

        let write_txn = db.begin_write().expect("begin write transaction");

        migration_0001_add_proof_envelope_and_encryption(&write_txn, &namespace)
            .expect("migrate legacy proof");

        write_txn.commit().expect("commit migration");

        let table_def: TableDefinition<&[u8], Vec<u8>> =
            TableDefinition::new(namespace.proof_table.as_str());

        let read_txn = db.begin_read().expect("begin read transaction");
        let table = read_txn.open_table(table_def).expect("open proof table");

        assert!(
            table
                .get(expected_y.to_bytes().as_slice())
                .expect("read proof entry")
                .is_some()
        );
    }

    #[test]
    fn migration_encrypts_stored_proof_payload() {
        let db = test_database();
        let keys = test_keypair();
        let namespace = namespace("wallet-1", keys);

        let original = test_proof();
        let y = insert_legacy_proof(&db, &namespace.proof_table, original, cdk07::State::Unspent);

        let write_txn = db.begin_write().expect("begin write transaction");

        migration_0001_add_proof_envelope_and_encryption(&write_txn, &namespace)
            .expect("migrate legacy proof");

        write_txn.commit().expect("commit migration");

        let table_def: TableDefinition<&[u8], Vec<u8>> =
            TableDefinition::new(namespace.proof_table.as_str());

        let read_txn = db.begin_read().expect("begin read transaction");
        let table = read_txn
            .open_table(table_def)
            .expect("open migrated proof table");

        let stored = table
            .get(y.to_bytes().as_slice())
            .expect("read migrated entry")
            .expect("migrated entry exists");

        let envelope: StoredProof =
            borsh::from_slice(stored.value().as_slice()).expect("deserialize stored proof");

        match envelope {
            StoredProof::V1(payload) => {
                assert!(!payload.ciphertext.is_empty());
            }
        }
    }

    #[test]
    fn migration_converts_multiple_proofs() {
        let db = test_database();
        let keys = test_keypair();
        let namespace = namespace("wallet-1", keys);

        let first = test_proof();
        let second = test_proof();
        let third = test_proof();

        let first_y = insert_legacy_proof(
            &db,
            &namespace.proof_table,
            first.clone(),
            cdk07::State::Unspent,
        );

        let second_y = insert_legacy_proof(
            &db,
            &namespace.proof_table,
            second.clone(),
            cdk07::State::PendingSpent,
        );

        let third_y = insert_legacy_proof(
            &db,
            &namespace.proof_table,
            third.clone(),
            cdk07::State::Spent,
        );

        let write_txn = db.begin_write().expect("begin write transaction");

        migration_0001_add_proof_envelope_and_encryption(&write_txn, &namespace)
            .expect("migrate all legacy proofs");

        write_txn.commit().expect("commit migration");

        let (migrated_first, first_state) =
            load_migrated_proof(&db, &namespace.proof_table, first_y, keys);
        let (migrated_second, second_state) =
            load_migrated_proof(&db, &namespace.proof_table, second_y, keys);
        let (migrated_third, third_state) =
            load_migrated_proof(&db, &namespace.proof_table, third_y, keys);

        assert_eq!(migrated_first, first);
        assert_eq!(first_state, cdk07::State::Unspent);

        assert_eq!(migrated_second, second);
        assert_eq!(second_state, cdk07::State::PendingSpent);

        assert_eq!(migrated_third, third);
        assert_eq!(third_state, cdk07::State::Spent);
    }
}
