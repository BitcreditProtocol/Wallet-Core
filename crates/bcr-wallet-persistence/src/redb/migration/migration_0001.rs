use std::collections::HashMap;

use crate::{
    SwapCommitmentRecord,
    error::{Error, Result},
    redb::{
        migration::WalletStorageNamespace,
        pocket::{self, StoredCounter, StoredCounterPayloadV1},
    },
};
use bcr_common::cashu::{
    self, nut00 as cdk00, nut01 as cdk01, nut02 as cdk02, nut07 as cdk07, nut12 as cdk12,
    secret::Secret,
};
use bitcoin::secp256k1;
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

pub(super) fn migration_0001_envelope_and_encryption(
    txn: &redb::WriteTransaction,
    namespace: &WalletStorageNamespace,
) -> Result<()> {
    tracing::info!("Migrating proofs..");
    migrate_proof_table_to_envelope_and_encryption(txn, &namespace.proof_table, &namespace.keys)?;
    tracing::info!("Migrated proofs.");
    tracing::info!("Migrating counters..");
    migrate_counter_table_to_envelope(txn, &namespace.counter_table)?;
    tracing::info!("Migrated counters.");
    tracing::info!("Migrating commitments..");
    migrate_commitment_table_to_envelope_and_encryption(
        txn,
        &namespace.commitment_table,
        &namespace.keys,
    )?;
    tracing::info!("Migrated commitments.");

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

// Fetch old counters
// put in V1 envelope
// Store new counters
fn migrate_counter_table_to_envelope(txn: &redb::WriteTransaction, table_name: &str) -> Result<()> {
    let table_def: TableDefinition<&[u8], Vec<u8>> = TableDefinition::new(table_name);

    let mut table = match txn.open_table(table_def) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => {
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    };

    let mut migrated_counters: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    for item in table.iter()? {
        let (k, v) = item?;
        let counter: CounterEntry = ciborium::from_reader(v.value().as_slice())?;
        let stored_counter_v1 = StoredCounter::V1(StoredCounterPayloadV1 {
            kid: counter.kid,
            counter: counter.counter,
        });

        let migrated_counter_bytes = borsh::to_vec(&stored_counter_v1)
            .map_err(|e| Error::BorshSerialization(e.to_string()))?;
        migrated_counters.push((k.value().to_vec(), migrated_counter_bytes));
    }

    for (key, new_counter) in migrated_counters.iter() {
        table.insert(key.as_slice(), new_counter)?;
    }

    Ok(())
}

// Fetch old commitments
// convert to records
// put in V1 envelope and encrypt
// Store new commitments
fn migrate_commitment_table_to_envelope_and_encryption(
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

    let mut migrated_commitments: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    for item in table.iter()? {
        let (k, v) = item?;
        let c: Commitment = ciborium::from_reader(v.value().as_slice())?;
        let secret = secp256k1::SecretKey::from_slice(&c.ephemeral_secret)
            .map_err(|e| Error::Custom(format!("invalid ephemeral secret: {e}")))?;
        let record = SwapCommitmentRecord {
            inputs: c.inputs,
            outputs: c.outputs,
            expiry: c.expiry,
            commitment: c.commitment,
            ephemeral_secret: secret,
            body_content: c.body_content,
            wallet_key: c.wallet_key,
            premints: premints_from_storage(c.premints),
        };
        let stored_commitment_v1 = pocket::to_stored_commitment_v1(record, keys.to_owned())?;

        let migrated_commitment_bytes = borsh::to_vec(&stored_commitment_v1)
            .map_err(|e| Error::BorshSerialization(e.to_string()))?;
        migrated_commitments.push((k.value().to_vec(), migrated_commitment_bytes));
    }

    for (key, new_commitment) in migrated_commitments.iter() {
        table.insert(key.as_slice(), new_commitment)?;
    }

    Ok(())
}

///////////////////////////////////////////// CounterEntry
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct CounterEntry {
    kid: cdk02::Id,
    counter: u32,
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
///////////////////////////////////////////// Commitment
type PremintStorage = Vec<(
    cashu::Id,
    Vec<(
        cdk00::BlindedMessage,
        Secret,
        cdk01::SecretKey,
        cashu::Amount,
    )>,
)>;

fn premints_from_storage(stored: PremintStorage) -> HashMap<cashu::Id, cdk00::PreMintSecrets> {
    stored
        .into_iter()
        .map(|(kid, tuples)| {
            let secrets = tuples
                .into_iter()
                .map(|(bm, secret, r, amount)| cdk00::PreMint {
                    blinded_message: bm,
                    secret,
                    r,
                    amount,
                })
                .collect();
            (
                kid,
                cdk00::PreMintSecrets {
                    secrets,
                    keyset_id: kid,
                },
            )
        })
        .collect()
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
struct Commitment {
    inputs: Vec<cashu::PublicKey>,
    outputs: Vec<cashu::BlindedMessage>,
    expiry: u64,
    commitment: secp256k1::schnorr::Signature,
    ephemeral_secret: Vec<u8>,
    body_content: String,
    wallet_key: cashu::PublicKey,
    #[serde(default)]
    premints: PremintStorage,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::redb::{
        migration::collect_wallet_namespace,
        pocket::{StoredCommitment, StoredProof, from_stored_commitment_v1, from_stored_proof_v1},
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

        migrate_proof_table_to_envelope_and_encryption(
            &write_txn,
            &namespace.proof_table,
            &namespace.keys,
        )
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
        migrate_proof_table_to_envelope_and_encryption(
            &write_txn,
            &namespace.proof_table,
            &namespace.keys,
        )
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
        migrate_proof_table_to_envelope_and_encryption(
            &write_txn,
            &namespace.proof_table,
            &namespace.keys,
        )
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

        migrate_proof_table_to_envelope_and_encryption(
            &write_txn,
            &namespace.proof_table,
            &namespace.keys,
        )
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

        migrate_proof_table_to_envelope_and_encryption(
            &write_txn,
            &namespace.proof_table,
            &namespace.keys,
        )
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

        migrate_proof_table_to_envelope_and_encryption(
            &write_txn,
            &namespace.proof_table,
            &namespace.keys,
        )
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

    // counter
    fn insert_legacy_counter(
        db: &Arc<Database>,
        table_name: &str,
        key: &[u8],
        counter: CounterEntry,
    ) {
        let mut encoded = Vec::new();
        ciborium::into_writer(&counter, &mut encoded).expect("serialize legacy counter");
        let table_def: TableDefinition<&[u8], Vec<u8>> = TableDefinition::new(table_name);
        let write_txn = db.begin_write().expect("begin write transaction");
        {
            let mut table = write_txn
                .open_table(table_def)
                .expect("open legacy counter table");

            table.insert(key, encoded).expect("insert legacy counter");
        }
        write_txn.commit().expect("commit legacy counter");
    }

    fn load_migrated_counter(db: &Arc<Database>, table_name: &str, key: &[u8]) -> StoredCounter {
        let table_def: TableDefinition<&[u8], Vec<u8>> = TableDefinition::new(table_name);
        let read_txn = db.begin_read().expect("begin read transaction");
        let table = read_txn
            .open_table(table_def)
            .expect("open migrated counter table");
        let stored = table
            .get(key)
            .expect("read migrated counter")
            .expect("migrated counter exists");
        borsh::from_slice(stored.value().as_slice()).expect("deserialize stored counter envelope")
    }

    #[test]
    fn counter_migration_succeeds_when_table_does_not_exist() {
        let db = test_database();
        let keys = test_keypair();
        let namespace = namespace("wallet-1", keys);
        let write_txn = db.begin_write().expect("begin write transaction");
        migrate_counter_table_to_envelope(&write_txn, &namespace.counter_table)
            .expect("migration succeeds for missing counter table");
        write_txn.commit().expect("commit migration");
    }

    #[test]
    fn counter_migration_converts_legacy_counter_to_v1_envelope() {
        let db = test_database();
        let keys = test_keypair();
        let namespace = namespace("wallet-1", keys);
        let proof = test_proof();
        let expected_kid = proof.keyset_id;
        let expected_counter = 42;
        let database_key = b"counter-key";
        insert_legacy_counter(
            &db,
            &namespace.counter_table,
            database_key,
            CounterEntry {
                kid: expected_kid,
                counter: expected_counter,
            },
        );
        let write_txn = db.begin_write().expect("begin write transaction");
        migrate_counter_table_to_envelope(&write_txn, &namespace.counter_table)
            .expect("migrate legacy counter");
        write_txn.commit().expect("commit migration");
        let migrated = load_migrated_counter(&db, &namespace.counter_table, database_key);
        match migrated {
            StoredCounter::V1(payload) => {
                assert_eq!(payload.kid, expected_kid);
                assert_eq!(payload.counter, expected_counter);
            }
        }
    }

    #[test]
    fn counter_migration_preserves_database_key() {
        let db = test_database();
        let keys = test_keypair();
        let namespace = namespace("wallet-1", keys);
        let database_key = b"original-counter-key";
        let proof = test_proof();
        insert_legacy_counter(
            &db,
            &namespace.counter_table,
            database_key,
            CounterEntry {
                kid: proof.keyset_id,
                counter: 7,
            },
        );
        let write_txn = db.begin_write().expect("begin write transaction");
        migrate_counter_table_to_envelope(&write_txn, &namespace.counter_table)
            .expect("migrate legacy counter");
        write_txn.commit().expect("commit migration");
        let table_def: TableDefinition<&[u8], Vec<u8>> =
            TableDefinition::new(namespace.counter_table.as_str());
        let read_txn = db.begin_read().expect("begin read transaction");
        let table = read_txn.open_table(table_def).expect("open counter table");
        assert!(
            table
                .get(database_key.as_slice())
                .expect("read counter entry")
                .is_some()
        );
    }

    #[test]
    fn counter_migration_converts_multiple_counters() {
        let db = test_database();
        let keys = test_keypair();
        let namespace = namespace("wallet-1", keys);

        let first_kid = test_proof().keyset_id;
        let second_kid = test_proof().keyset_id;
        let third_kid = test_proof().keyset_id;

        insert_legacy_counter(
            &db,
            &namespace.counter_table,
            b"first",
            CounterEntry {
                kid: first_kid,
                counter: 1,
            },
        );

        insert_legacy_counter(
            &db,
            &namespace.counter_table,
            b"second",
            CounterEntry {
                kid: second_kid,
                counter: 20,
            },
        );

        insert_legacy_counter(
            &db,
            &namespace.counter_table,
            b"third",
            CounterEntry {
                kid: third_kid,
                counter: u32::MAX,
            },
        );

        let write_txn = db.begin_write().expect("begin write transaction");

        migrate_counter_table_to_envelope(&write_txn, &namespace.counter_table)
            .expect("migrate all legacy counters");

        write_txn.commit().expect("commit migration");

        let first = load_migrated_counter(&db, &namespace.counter_table, b"first");
        let second = load_migrated_counter(&db, &namespace.counter_table, b"second");
        let third = load_migrated_counter(&db, &namespace.counter_table, b"third");

        match first {
            StoredCounter::V1(payload) => {
                assert_eq!(payload.kid, first_kid);
                assert_eq!(payload.counter, 1);
            }
        }

        match second {
            StoredCounter::V1(payload) => {
                assert_eq!(payload.kid, second_kid);
                assert_eq!(payload.counter, 20);
            }
        }

        match third {
            StoredCounter::V1(payload) => {
                assert_eq!(payload.kid, third_kid);
                assert_eq!(payload.counter, u32::MAX);
            }
        }
    }

    // commitments
    fn test_commitment() -> Commitment {
        let secp = secp256k1::Secp256k1::new();
        let signing_keys = test_keypair();
        let ephemeral_secret = secp256k1::SecretKey::new(&mut secp256k1::rand::thread_rng());
        let message = secp256k1::Message::from_digest([42_u8; 32]);
        let signature = secp.sign_schnorr_no_aux_rand(&message, &signing_keys);
        let proof = test_proof();

        Commitment {
            inputs: Vec::new(),
            outputs: Vec::new(),
            expiry: 1_750_000_000,
            commitment: signature,
            ephemeral_secret: ephemeral_secret.secret_bytes().to_vec(),
            body_content: "test commitment body".to_string(),
            wallet_key: proof.c,
            premints: Vec::new(),
        }
    }

    fn insert_legacy_commitment(
        db: &Arc<Database>,
        table_name: &str,
        key: &[u8],
        commitment: &Commitment,
    ) {
        let mut encoded = Vec::new();
        ciborium::into_writer(commitment, &mut encoded).expect("serialize legacy commitment");
        let table_def: TableDefinition<&[u8], Vec<u8>> = TableDefinition::new(table_name);
        let write_txn = db.begin_write().expect("begin write transaction");

        {
            let mut table = write_txn
                .open_table(table_def)
                .expect("open legacy commitment table");

            table
                .insert(key, encoded)
                .expect("insert legacy commitment");
        }

        write_txn.commit().expect("commit legacy commitment");
    }

    fn load_migrated_commitment(
        db: &Arc<Database>,
        table_name: &str,
        key: &[u8],
        keys: bitcoin::secp256k1::Keypair,
    ) -> SwapCommitmentRecord {
        let table_def: TableDefinition<&[u8], Vec<u8>> = TableDefinition::new(table_name);
        let read_txn = db.begin_read().expect("begin read transaction");
        let table = read_txn
            .open_table(table_def)
            .expect("open migrated commitment table");

        let stored = table
            .get(key)
            .expect("read migrated commitment")
            .expect("migrated commitment exists");

        let envelope: StoredCommitment = borsh::from_slice(stored.value().as_slice())
            .expect("deserialize stored commitment envelope");

        from_stored_commitment_v1(envelope, keys).expect("decrypt migrated commitment")
    }

    fn assert_commitment_matches_record(legacy: &Commitment, migrated: &SwapCommitmentRecord) {
        assert_eq!(migrated.inputs, legacy.inputs);
        assert_eq!(migrated.outputs, legacy.outputs);
        assert_eq!(migrated.expiry, legacy.expiry);
        assert_eq!(migrated.commitment, legacy.commitment);
        assert_eq!(
            migrated.ephemeral_secret.secret_bytes(),
            legacy.ephemeral_secret.as_slice()
        );
        assert_eq!(migrated.body_content, legacy.body_content);
        assert_eq!(migrated.wallet_key, legacy.wallet_key);
        assert_eq!(
            migrated.premints,
            premints_from_storage(legacy.premints.clone())
        );
    }

    #[test]
    fn commitment_migration_succeeds_when_table_does_not_exist() {
        let db = test_database();
        let keys = test_keypair();
        let namespace = namespace("wallet-1", keys);
        let write_txn = db.begin_write().expect("begin write transaction");
        migrate_commitment_table_to_envelope_and_encryption(
            &write_txn,
            &namespace.commitment_table,
            &namespace.keys,
        )
        .expect("migration succeeds for missing commitment table");
        write_txn.commit().expect("commit migration");
    }

    #[test]
    fn commitment_migration_converts_legacy_commitment_to_encrypted_envelope() {
        let db = test_database();
        let keys = test_keypair();
        let namespace = namespace("wallet-1", keys);
        let database_key = b"commitment-key";
        let original = test_commitment();
        insert_legacy_commitment(&db, &namespace.commitment_table, database_key, &original);
        let write_txn = db.begin_write().expect("begin write transaction");
        migrate_commitment_table_to_envelope_and_encryption(
            &write_txn,
            &namespace.commitment_table,
            &namespace.keys,
        )
        .expect("migrate legacy commitment");
        write_txn.commit().expect("commit migration");
        let migrated =
            load_migrated_commitment(&db, &namespace.commitment_table, database_key, keys);
        assert_commitment_matches_record(&original, &migrated);
    }

    #[test]
    fn commitment_migration_encrypts_stored_payload() {
        let db = test_database();
        let keys = test_keypair();
        let namespace = namespace("wallet-1", keys);
        let database_key = b"encrypted-commitment";
        let original = test_commitment();

        insert_legacy_commitment(&db, &namespace.commitment_table, database_key, &original);

        let write_txn = db.begin_write().expect("begin write transaction");

        migrate_commitment_table_to_envelope_and_encryption(
            &write_txn,
            &namespace.commitment_table,
            &namespace.keys,
        )
        .expect("migrate legacy commitment");

        write_txn.commit().expect("commit migration");

        let table_def: TableDefinition<&[u8], Vec<u8>> =
            TableDefinition::new(namespace.commitment_table.as_str());

        let read_txn = db.begin_read().expect("begin read transaction");
        let table = read_txn
            .open_table(table_def)
            .expect("open migrated commitment table");

        let stored = table
            .get(database_key.as_slice())
            .expect("read migrated entry")
            .expect("migrated entry exists");

        let envelope: StoredCommitment =
            borsh::from_slice(stored.value().as_slice()).expect("deserialize stored commitment");

        match envelope {
            StoredCommitment::V1(payload) => {
                assert!(!payload.ciphertext.is_empty());
            }
        }
    }

    #[test]
    fn commitment_migration_converts_multiple_commitments() {
        let db = test_database();
        let keys = test_keypair();
        let namespace = namespace("wallet-1", keys);

        let mut first = test_commitment();
        first.expiry = 1;
        first.body_content = "first".to_string();

        let mut second = test_commitment();
        second.expiry = 2;
        second.body_content = "second".to_string();

        let mut third = test_commitment();
        third.expiry = u64::MAX;
        third.body_content = "third".to_string();

        insert_legacy_commitment(&db, &namespace.commitment_table, b"first", &first);
        insert_legacy_commitment(&db, &namespace.commitment_table, b"second", &second);
        insert_legacy_commitment(&db, &namespace.commitment_table, b"third", &third);

        let write_txn = db.begin_write().expect("begin write transaction");

        migrate_commitment_table_to_envelope_and_encryption(
            &write_txn,
            &namespace.commitment_table,
            &namespace.keys,
        )
        .expect("migrate all legacy commitments");

        write_txn.commit().expect("commit migration");

        let migrated_first =
            load_migrated_commitment(&db, &namespace.commitment_table, b"first", keys);
        let migrated_second =
            load_migrated_commitment(&db, &namespace.commitment_table, b"second", keys);
        let migrated_third =
            load_migrated_commitment(&db, &namespace.commitment_table, b"third", keys);

        assert_commitment_matches_record(&first, &migrated_first);
        assert_commitment_matches_record(&second, &migrated_second);
        assert_commitment_matches_record(&third, &migrated_third);
    }
}
