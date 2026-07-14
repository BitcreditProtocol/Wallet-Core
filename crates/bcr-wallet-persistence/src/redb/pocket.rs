use crate::{
    PocketRepository, SwapCommitmentRecord,
    error::{Error, Result},
};
use async_trait::async_trait;
use bcr_common::cashu::{
    self, CurrencyUnit, nut00 as cdk00, nut01 as cdk01, nut02 as cdk02, nut07 as cdk07,
    nut12 as cdk12, secret::Secret,
};
use bcr_common::wire::borsh::{
    deserialize_cashu_amount, deserialize_from_str, deserialize_optionproofdleq,
    deserialize_optionproofwitness, serialize_as_str, serialize_cashu_amount,
    serialize_optionproofdleq, serialize_optionproofwitness,
};
use bcr_wallet_core::crypto;
use bitcoin::secp256k1;
use borsh::{BorshDeserialize, BorshSerialize};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition, TableError};
use std::{collections::HashMap, sync::Arc};
use tokio::task::spawn_blocking;

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

fn premints_to_storage(premints: HashMap<cashu::Id, cdk00::PreMintSecrets>) -> PremintStorage {
    premints
        .into_iter()
        .map(|(kid, ps)| {
            let tuples = ps
                .secrets
                .into_iter()
                .map(|p| (p.blinded_message, p.secret, p.r, p.amount))
                .collect();
            (kid, tuples)
        })
        .collect()
}

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

/// StoredProof is a versioned, encrypted, borsh-serialized
#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub(super) enum StoredProof {
    V1(EncryptedProofPayloadV1),
}

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub(super) struct EncryptedProofPayloadV1 {
    pub ciphertext: Vec<u8>,
}

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub(super) struct StoredProofPayloadV1 {
    #[borsh(
        serialize_with = "serialize_as_str",
        deserialize_with = "deserialize_from_str"
    )]
    y: cdk01::PublicKey,
    #[borsh(
        serialize_with = "serialize_cashu_amount",
        deserialize_with = "deserialize_cashu_amount"
    )]
    amount: bcr_common::cashu::Amount,
    #[borsh(
        serialize_with = "serialize_as_str",
        deserialize_with = "deserialize_from_str"
    )]
    keyset_id: cdk02::Id,
    #[borsh(
        serialize_with = "serialize_as_str",
        deserialize_with = "deserialize_from_str"
    )]
    secret: Secret,
    #[borsh(
        serialize_with = "serialize_as_str",
        deserialize_with = "deserialize_from_str"
    )]
    c: cdk01::PublicKey,
    #[borsh(
        serialize_with = "serialize_optionproofwitness",
        deserialize_with = "deserialize_optionproofwitness"
    )]
    witness: Option<cdk00::Witness>,
    #[borsh(
        serialize_with = "serialize_optionproofdleq",
        deserialize_with = "deserialize_optionproofdleq"
    )]
    dleq: Option<cdk12::ProofDleq>,
    #[borsh(
        serialize_with = "serialize_as_str",
        deserialize_with = "deserialize_from_str"
    )]
    state: cdk07::State,
}

impl std::convert::From<cdk00::Proof> for StoredProofPayloadV1 {
    fn from(proof: cdk00::Proof) -> Self {
        let y = proof.y().expect("Hash to curve should not fail");
        StoredProofPayloadV1 {
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

impl std::convert::From<StoredProofPayloadV1> for cdk00::Proof {
    fn from(entry: StoredProofPayloadV1) -> Self {
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

pub(super) fn to_stored_proof_v1(
    proof: cdk00::Proof,
    state: Option<cdk07::State>,
    keys: bitcoin::secp256k1::Keypair,
) -> Result<StoredProof> {
    let mut payload = StoredProofPayloadV1::from(proof);
    if let Some(state) = state {
        payload.state = state;
    }
    let encoded = borsh::to_vec(&payload).map_err(|e| Error::BorshSerialization(e.to_string()))?;
    let encrypted = crypto::encrypt_ecies(&encoded, &keys.public_key())?;
    Ok(StoredProof::V1(EncryptedProofPayloadV1 {
        ciphertext: encrypted,
    }))
}

pub(super) fn from_stored_proof_v1(
    proof: StoredProof,
    keys: bitcoin::secp256k1::Keypair,
) -> Result<(cdk00::Proof, cdk07::State)> {
    let StoredProof::V1(encrypted_payload) = proof;
    let decrypted = crypto::decrypt_ecies(&encrypted_payload.ciphertext, &keys.secret_key())?;
    let decoded: StoredProofPayloadV1 =
        borsh::from_slice(&decrypted).map_err(|e| Error::BorshSerialization(e.to_string()))?;
    let state = decoded.state;
    Ok((decoded.into(), state))
}

///////////////////////////////////////////// CounterEntry
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct CounterEntry {
    kid: cdk02::Id,
    counter: u32,
}

///////////////////////////////////////////// PocketDB
pub struct PocketDB {
    db: Arc<Database>,
    proof_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
    counter_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
    commitment_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
    keys: bitcoin::secp256k1::Keypair,
}

impl PocketDB {
    const PROOF_BASE_DB_NAME: &'static str = "proofs";
    const COUNTER_BASE_DB_NAME: &'static str = "counters";
    const COMMITMENT_BASE_DB_NAME: &'static str = "commitments";

    pub fn proof_table_name(wallet_id: &str, unit: &CurrencyUnit) -> String {
        format!("{wallet_id}_{unit}_{}", Self::PROOF_BASE_DB_NAME)
    }

    pub fn counter_table_name(wallet_id: &str, unit: &CurrencyUnit) -> String {
        format!("{wallet_id}_{unit}_{}", Self::COUNTER_BASE_DB_NAME)
    }

    pub fn commitment_table_name(wallet_id: &str, unit: &CurrencyUnit) -> String {
        format!("{wallet_id}_{unit}_{}", Self::COMMITMENT_BASE_DB_NAME)
    }

    pub fn new(
        db: Arc<Database>,
        wallet_id: &str,
        unit: &CurrencyUnit,
        keys: bitcoin::secp256k1::Keypair,
    ) -> Result<Self> {
        // Leak once to get static string, because of dynamically generated table names
        let proof_name: &'static str =
            Box::leak(Self::proof_table_name(wallet_id, unit).into_boxed_str());
        let counter_name: &'static str =
            Box::leak(Self::counter_table_name(wallet_id, unit).into_boxed_str());
        let commitment_name: &'static str =
            Box::leak(Self::commitment_table_name(wallet_id, unit).into_boxed_str());

        let proof_table = TableDefinition::new(proof_name);
        let counter_table = TableDefinition::new(counter_name);
        let commitment_table = TableDefinition::new(commitment_name);
        Ok(Self {
            db,
            proof_table,
            counter_table,
            commitment_table,
            keys,
        })
    }

    fn store_new_sync(
        db: Arc<Database>,
        proof_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        keys: bitcoin::secp256k1::Keypair,
        proof: cdk00::Proof,
    ) -> Result<cdk01::PublicKey> {
        let y = proof.y().expect("valid y");
        let entry = to_stored_proof_v1(proof, None, keys)?;

        let write_txn = db.begin_write()?;

        {
            let mut table = write_txn.open_table(proof_table)?;

            let serialized =
                borsh::to_vec(&entry).map_err(|e| Error::BorshSerialization(e.to_string()))?;
            table.insert(y.to_bytes().as_slice(), serialized)?;
        }

        write_txn.commit()?;
        Ok(y)
    }

    fn store_pendingspent_sync(
        db: Arc<Database>,
        proof_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        keys: bitcoin::secp256k1::Keypair,
        proof: cdk00::Proof,
    ) -> Result<cdk01::PublicKey> {
        let y = proof.y().expect("valid y");
        let entry = to_stored_proof_v1(proof, Some(cdk07::State::PendingSpent), keys)?;

        let write_txn = db.begin_write()?;

        {
            let mut table = write_txn.open_table(proof_table)?;
            let serialized =
                borsh::to_vec(&entry).map_err(|e| Error::BorshSerialization(e.to_string()))?;

            table.insert(y.to_bytes().as_slice(), serialized)?;
        }

        write_txn.commit()?;
        Ok(y)
    }

    fn load_proof_sync(
        db: Arc<Database>,
        proof_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        keys: bitcoin::secp256k1::Keypair,
        y: cdk01::PublicKey,
    ) -> Result<Option<(cdk00::Proof, cdk07::State)>> {
        let read_txn = db.begin_read()?;

        match read_txn.open_table(proof_table) {
            Ok(table) => {
                let entry = table.get(y.to_bytes().as_slice())?;
                match entry {
                    Some(e) => {
                        let deserialized: StoredProof = borsh::from_slice(e.value().as_slice())
                            .map_err(|e| Error::BorshSerialization(e.to_string()))?;
                        let (proof, state) = from_stored_proof_v1(deserialized, keys)?;
                        Ok(Some((proof, state)))
                    }
                    None => Ok(None),
                }
            }
            Err(TableError::TableDoesNotExist(_)) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn load_proofs_sync(
        db: Arc<Database>,
        proof_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        keys: bitcoin::secp256k1::Keypair,
        ys: Vec<cdk01::PublicKey>,
    ) -> Result<Vec<(cdk00::Proof, cdk07::State)>> {
        let read_txn = db.begin_read()?;
        match read_txn.open_table(proof_table) {
            Ok(table) => {
                let mut res = Vec::with_capacity(ys.len());
                for y in ys.iter() {
                    match table.get(y.to_bytes().as_slice())? {
                        Some(entry) => {
                            let deserialized: StoredProof =
                                borsh::from_slice(entry.value().as_slice())
                                    .map_err(|e| Error::BorshSerialization(e.to_string()))?;
                            let (proof, state) = from_stored_proof_v1(deserialized, keys)?;
                            res.push((proof, state))
                        }
                        None => {
                            return Err(Error::ProofNotFound(y.to_owned()));
                        }
                    }
                }
                Ok(res)
            }
            Err(TableError::TableDoesNotExist(_)) => Ok(vec![]),
            Err(e) => Err(e.into()),
        }
    }

    fn delete_proof_sync(
        db: Arc<Database>,
        proof_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        keys: bitcoin::secp256k1::Keypair,
        y: cdk01::PublicKey,
    ) -> Result<Option<(cdk00::Proof, cdk07::State)>> {
        let write_txn = db.begin_write()?;

        let old = {
            let mut table = write_txn.open_table(proof_table)?;
            match table.remove(y.to_bytes().as_slice())? {
                Some(old) => {
                    let deserialized: StoredProof = borsh::from_slice(old.value().as_slice())
                        .map_err(|e| Error::BorshSerialization(e.to_string()))?;
                    let (proof, state) = from_stored_proof_v1(deserialized, keys)?;
                    Some((proof, state))
                }
                None => None,
            }
        };

        write_txn.commit()?;
        Ok(old)
    }

    fn list_keys_sync(
        db: Arc<Database>,
        proof_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
    ) -> Result<Vec<cdk01::PublicKey>> {
        let read_txn = db.begin_read()?;

        match read_txn.open_table(proof_table) {
            Ok(table) => {
                let mut res = Vec::new();
                for item in table.range::<&[u8]>(..)? {
                    let (k, _) = item?;
                    let y = cdk01::PublicKey::from_slice(k.value().to_vec().as_slice())?;
                    res.push(y);
                }
                Ok(res)
            }
            Err(TableError::TableDoesNotExist(_)) => Ok(vec![]),
            Err(e) => Err(e.into()),
        }
    }

    fn list_sync(
        db: Arc<Database>,
        proof_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        keys: bitcoin::secp256k1::Keypair,
        state: Option<cdk07::State>,
    ) -> Result<Vec<(cdk00::Proof, cdk07::State)>> {
        let read_txn = db.begin_read()?;

        match read_txn.open_table(proof_table) {
            Ok(table) => {
                let mut res = Vec::new();
                for (_, v) in table.range::<&[u8]>(..)?.flatten() {
                    let deserialized: StoredProof = borsh::from_slice(v.value().as_slice())
                        .map_err(|e| Error::BorshSerialization(e.to_string()))?;
                    let (proof, proof_state) = from_stored_proof_v1(deserialized, keys)?;
                    if let Some(s) = state {
                        if s == proof_state {
                            res.push((proof, proof_state));
                        }
                    } else {
                        res.push((proof, proof_state))
                    }
                }
                Ok(res)
            }
            Err(TableError::TableDoesNotExist(_)) => Ok(vec![]),
            Err(e) => Err(e.into()),
        }
    }

    fn update_entry_state_sync(
        db: Arc<Database>,
        proof_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        keys: bitcoin::secp256k1::Keypair,
        y: cdk01::PublicKey,
        old_state_set: &[cdk07::State],
        new_state: cdk07::State,
    ) -> Result<(cdk00::Proof, cdk07::State)> {
        let write_txn = db.begin_write()?;
        let new_value = {
            let mut table = write_txn.open_table(proof_table)?;
            let old_value = table.get(y.to_bytes().as_slice())?.map(|v| v.value());

            if let Some(old_value) = old_value {
                let deserialized: StoredProof = borsh::from_slice(old_value.as_slice())
                    .map_err(|e| Error::BorshSerialization(e.to_string()))?;
                let (proof, proof_state) = from_stored_proof_v1(deserialized, keys)?;

                if !old_state_set.contains(&proof_state) {
                    return Err(Error::InvalidProofState(y));
                }

                let entry = to_stored_proof_v1(proof.clone(), Some(new_state), keys)?;
                let serialized =
                    borsh::to_vec(&entry).map_err(|e| Error::BorshSerialization(e.to_string()))?;

                table.insert(y.to_bytes().as_slice(), serialized)?;
                (proof, new_state)
            } else {
                return Err(Error::ProofNotFound(y));
            }
        };

        write_txn.commit()?;
        Ok(new_value)
    }

    fn load_counter_sync(
        db: Arc<Database>,
        counter_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        kid: cdk02::Id,
    ) -> Result<CounterEntry> {
        let read_txn = db.begin_read()?;

        match read_txn.open_table(counter_table) {
            Ok(table) => {
                let entry = table.get(kid.to_bytes().as_slice())?;
                match entry {
                    Some(e) => {
                        let counter: CounterEntry = ciborium::from_reader(e.value().as_slice())?;
                        Ok(counter)
                    }
                    None => Self::insert_counter_sync(db, counter_table, kid),
                }
            }
            Err(TableError::TableDoesNotExist(_)) => {
                Self::insert_counter_sync(db, counter_table, kid)
            }
            Err(e) => Err(e.into()),
        }
    }

    fn insert_counter_sync(
        db: Arc<Database>,
        counter_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        kid: cdk02::Id,
    ) -> Result<CounterEntry> {
        let entry = CounterEntry { kid, counter: 0 };
        let write_txn = db.begin_write()?;

        {
            let mut table = write_txn.open_table(counter_table)?;

            let mut serialized = Vec::new();
            ciborium::into_writer(&entry, &mut serialized)?;
            table.insert(kid.to_bytes().as_slice(), serialized)?;
        }

        write_txn.commit()?;
        Ok(entry)
    }

    fn increment_counter_sync(
        db: Arc<Database>,
        counter_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        old: CounterEntry,
        new: CounterEntry,
    ) -> Result<()> {
        if old.kid != new.kid {
            return Err(Error::CounterKidMismatch);
        }

        let write_txn = db.begin_write()?;
        {
            let mut table = write_txn.open_table(counter_table)?;
            let old_value = table.get(old.kid.to_bytes().as_slice())?.map(|v| v.value());

            if let Some(old_value) = old_value {
                let old_counter: CounterEntry = ciborium::from_reader(old_value.as_slice())?;

                if old_counter.kid != old.kid {
                    return Err(Error::CounterKidMismatch);
                }

                let mut serialized = Vec::new();
                ciborium::into_writer(&new, &mut serialized)?;
                table.insert(old.kid.to_bytes().as_slice(), serialized)?;
            } else {
                return Err(Error::CounterNotFound(old.kid));
            }
        }

        write_txn.commit()?;
        Ok(())
    }

    fn store_commitment_sync(
        db: Arc<Database>,
        commitment_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        record: crate::SwapCommitmentRecord,
    ) -> Result<()> {
        let commitment = record.commitment;
        let entry = Commitment {
            inputs: record.inputs,
            outputs: record.outputs,
            expiry: record.expiry,
            commitment,
            ephemeral_secret: record.ephemeral_secret.secret_bytes().to_vec(),
            body_content: record.body_content,
            wallet_key: record.wallet_key,
            premints: premints_to_storage(record.premints),
        };
        let write_txn = db.begin_write()?;

        {
            let mut table = write_txn.open_table(commitment_table)?;

            let mut serialized = Vec::new();
            ciborium::into_writer(&entry, &mut serialized)?;
            table.insert(commitment.serialize().as_slice(), serialized)?;
        }

        write_txn.commit()?;
        Ok(())
    }

    fn load_commitment_sync(
        db: Arc<Database>,
        commitment_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        commitment: secp256k1::schnorr::Signature,
    ) -> Result<SwapCommitmentRecord> {
        let read_txn = db.begin_read()?;

        match read_txn.open_table(commitment_table) {
            Ok(table) => {
                let entry = table.get(commitment.serialize().as_slice())?;
                match entry {
                    Some(e) => {
                        let c: Commitment = ciborium::from_reader(e.value().as_slice())?;
                        let secret = secp256k1::SecretKey::from_slice(&c.ephemeral_secret)
                            .map_err(|e| Error::Custom(format!("invalid ephemeral secret: {e}")))?;
                        Ok(SwapCommitmentRecord {
                            inputs: c.inputs,
                            outputs: c.outputs,
                            expiry: c.expiry,
                            commitment: c.commitment,
                            ephemeral_secret: secret,
                            body_content: c.body_content,
                            wallet_key: c.wallet_key,
                            premints: premints_from_storage(c.premints),
                        })
                    }
                    None => Err(Error::Custom(format!(
                        "commitment not found: {}",
                        commitment
                    ))),
                }
            }
            Err(TableError::TableDoesNotExist(_)) => Err(Error::Custom(format!(
                "commitment not found: {}",
                commitment
            ))),
            Err(e) => Err(e.into()),
        }
    }

    fn list_commitments_sync(
        db: Arc<Database>,
        commitment_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
    ) -> Result<Vec<SwapCommitmentRecord>> {
        let read_txn = db.begin_read()?;

        match read_txn.open_table(commitment_table) {
            Ok(table) => {
                let mut res = Vec::new();
                for (_, v) in table.range::<&[u8]>(..)?.flatten() {
                    let c: Commitment = ciborium::from_reader(v.value().as_slice())?;
                    let secret = secp256k1::SecretKey::from_slice(&c.ephemeral_secret)
                        .map_err(|e| Error::Custom(format!("invalid ephemeral secret: {e}")))?;
                    res.push(SwapCommitmentRecord {
                        inputs: c.inputs,
                        outputs: c.outputs,
                        expiry: c.expiry,
                        commitment: c.commitment,
                        ephemeral_secret: secret,
                        body_content: c.body_content,
                        wallet_key: c.wallet_key,
                        premints: premints_from_storage(c.premints),
                    });
                }
                Ok(res)
            }
            Err(TableError::TableDoesNotExist(_)) => Ok(vec![]),
            Err(e) => Err(e.into()),
        }
    }

    fn delete_commitment_sync(
        db: Arc<Database>,
        commitment_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        commitment: secp256k1::schnorr::Signature,
    ) -> Result<()> {
        let write_txn = db.begin_write()?;

        {
            let mut table = write_txn.open_table(commitment_table)?;
            table.remove(commitment.serialize().as_slice())?;
        }

        write_txn.commit()?;
        Ok(())
    }

    fn delete_repo(
        db: Arc<Database>,
        proof_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        commitment_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        counter_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
    ) -> Result<()> {
        let write_txn = db.begin_write()?;

        {
            if write_txn.open_table(proof_table).is_ok() {
                write_txn.delete_table(proof_table)?;
            }

            if write_txn.open_table(commitment_table).is_ok() {
                write_txn.delete_table(commitment_table)?;
            }

            if write_txn.open_table(counter_table).is_ok() {
                write_txn.delete_table(counter_table)?;
            }
        }

        write_txn.commit()?;
        Ok(())
    }
}

#[async_trait]
impl PocketRepository for PocketDB {
    async fn store_new(&self, proof: cdk00::Proof) -> Result<cdk01::PublicKey> {
        let db_clone = self.db.clone();
        let table = self.proof_table;
        let keys = self.keys;
        spawn_blocking(move || Self::store_new_sync(db_clone, table, keys, proof)).await?
    }

    async fn store_pendingspent(&self, proof: cdk00::Proof) -> Result<cdk01::PublicKey> {
        let db_clone = self.db.clone();
        let table = self.proof_table;
        let keys = self.keys;
        spawn_blocking(move || Self::store_pendingspent_sync(db_clone, table, keys, proof)).await?
    }

    async fn load_proof(&self, y: cdk01::PublicKey) -> Result<(cdk00::Proof, cdk07::State)> {
        let db_clone = self.db.clone();
        let table = self.proof_table;
        let keys = self.keys;
        let res = spawn_blocking(move || Self::load_proof_sync(db_clone, table, keys, y)).await??;
        let (proof, state) = res.ok_or(Error::ProofNotFound(y))?;
        Ok((proof, state))
    }

    async fn load_proofs(
        &self,
        ys: &[cdk01::PublicKey],
    ) -> Result<HashMap<cdk01::PublicKey, cdk00::Proof>> {
        let db_clone = self.db.clone();
        let ys_clone = ys.to_owned();
        let table = self.proof_table;
        let keys = self.keys;
        let res = spawn_blocking(move || Self::load_proofs_sync(db_clone, table, keys, ys_clone))
            .await??;
        Ok(res
            .into_iter()
            .map(|(entry, _)| (entry.y().expect("valid y"), entry))
            .collect())
    }

    async fn delete_proof(
        &self,
        y: cdk01::PublicKey,
    ) -> Result<Option<(cdk00::Proof, cdk07::State)>> {
        let db_clone = self.db.clone();
        let table = self.proof_table;
        let keys = self.keys;
        let res =
            spawn_blocking(move || Self::delete_proof_sync(db_clone, table, keys, y)).await??;
        Ok(res)
    }

    async fn list_unspent(&self) -> Result<HashMap<cdk01::PublicKey, cdk00::Proof>> {
        let db_clone = self.db.clone();
        let table = self.proof_table;
        let keys = self.keys;
        let list = spawn_blocking(move || {
            Self::list_sync(db_clone, table, keys, Some(cdk07::State::Unspent))
        })
        .await??;
        Ok(list
            .into_iter()
            .map(|(entry, _)| (entry.y().expect("valid y"), entry))
            .collect())
    }

    async fn list_spent(&self) -> Result<HashMap<cdk01::PublicKey, cdk00::Proof>> {
        let db_clone = self.db.clone();
        let table = self.proof_table;
        let keys = self.keys;
        let list = spawn_blocking(move || {
            Self::list_sync(db_clone, table, keys, Some(cdk07::State::Spent))
        })
        .await??;
        Ok(list
            .into_iter()
            .map(|(entry, _)| (entry.y().expect("valid y"), entry))
            .collect())
    }

    async fn list_pending(&self) -> Result<HashMap<cdk01::PublicKey, cdk00::Proof>> {
        let db_clone = self.db.clone();
        let db_clone_two = self.db.clone();
        let table = self.proof_table;
        let keys = self.keys;
        let pending: HashMap<cdk01::PublicKey, cdk00::Proof> = spawn_blocking(move || {
            Self::list_sync(db_clone, table, keys, Some(cdk07::State::Pending))
        })
        .await??
        .into_iter()
        .map(|(entry, _)| (entry.y().expect("valid y"), entry))
        .collect();
        let mut pending_spent: HashMap<cdk01::PublicKey, cdk00::Proof> =
            spawn_blocking(move || {
                Self::list_sync(db_clone_two, table, keys, Some(cdk07::State::PendingSpent))
            })
            .await??
            .into_iter()
            .map(|(entry, _)| (entry.y().expect("valid y"), entry))
            .collect();

        pending_spent.extend(pending);
        Ok(pending_spent)
    }

    async fn list_reserved(&self) -> Result<HashMap<cdk01::PublicKey, cdk00::Proof>> {
        let db_clone = self.db.clone();
        let table = self.proof_table;
        let keys = self.keys;
        let list = spawn_blocking(move || {
            Self::list_sync(db_clone, table, keys, Some(cdk07::State::Reserved))
        })
        .await??;
        Ok(list
            .into_iter()
            .map(|(entry, _)| (entry.y().expect("valid y"), entry))
            .collect())
    }

    async fn list_all(&self) -> Result<Vec<cdk01::PublicKey>> {
        let db_clone = self.db.clone();
        let table = self.proof_table;
        spawn_blocking(move || Self::list_keys_sync(db_clone, table)).await?
    }

    async fn mark_as_pendingspent(&self, y: cdk01::PublicKey) -> Result<cdk00::Proof> {
        let db_clone = self.db.clone();
        let table = self.proof_table;
        let keys = self.keys;
        let (proof, _) = spawn_blocking(move || {
            Self::update_entry_state_sync(
                db_clone,
                table,
                keys,
                y,
                &[cdk07::State::Unspent],
                cdk07::State::PendingSpent,
            )
        })
        .await??;
        Ok(proof)
    }

    async fn mark_pending_as_spent(&self, y: cdk01::PublicKey) -> Result<cdk00::Proof> {
        let db_clone = self.db.clone();
        let table = self.proof_table;
        let keys = self.keys;
        let (proof, _) = spawn_blocking(move || {
            Self::update_entry_state_sync(
                db_clone,
                table,
                keys,
                y,
                &[cdk07::State::Pending, cdk07::State::PendingSpent],
                cdk07::State::Spent,
            )
        })
        .await??;
        Ok(proof)
    }

    async fn revert_pendingspent_to_unspent(&self, y: cdk01::PublicKey) -> Result<cdk00::Proof> {
        let db_clone = self.db.clone();
        let table = self.proof_table;
        let keys = self.keys;
        let (proof, _) = spawn_blocking(move || {
            Self::update_entry_state_sync(
                db_clone,
                table,
                keys,
                y,
                &[cdk07::State::PendingSpent],
                cdk07::State::Unspent,
            )
        })
        .await??;
        Ok(proof)
    }

    async fn counter(&self, kid: bcr_common::cashu::Id) -> Result<u32> {
        let db_clone = self.db.clone();
        let table = self.counter_table;
        let counter =
            spawn_blocking(move || Self::load_counter_sync(db_clone, table, kid)).await??;
        Ok(counter.counter)
    }

    async fn increment_counter(
        &self,
        kid: bcr_common::cashu::Id,
        old: u32,
        increment: u32,
    ) -> Result<()> {
        let db_clone = self.db.clone();
        let table = self.counter_table;
        let old = CounterEntry { kid, counter: old };
        let new = CounterEntry {
            kid,
            counter: old.counter + increment,
        };
        spawn_blocking(move || Self::increment_counter_sync(db_clone, table, old, new)).await?
    }

    async fn store_commitment(&self, record: crate::SwapCommitmentRecord) -> Result<()> {
        let db_clone = self.db.clone();
        let table = self.commitment_table;
        spawn_blocking(move || Self::store_commitment_sync(db_clone, table, record)).await?
    }

    async fn load_commitment(
        &self,
        commitment: secp256k1::schnorr::Signature,
    ) -> Result<SwapCommitmentRecord> {
        let db_clone = self.db.clone();
        let table = self.commitment_table;
        spawn_blocking(move || Self::load_commitment_sync(db_clone, table, commitment)).await?
    }

    async fn delete_commitment(&self, commitment: secp256k1::schnorr::Signature) -> Result<()> {
        let db_clone = self.db.clone();
        let table = self.commitment_table;
        spawn_blocking(move || Self::delete_commitment_sync(db_clone, table, commitment)).await?
    }

    async fn list_commitments(&self) -> Result<Vec<SwapCommitmentRecord>> {
        let db_clone = self.db.clone();
        let table = self.commitment_table;
        spawn_blocking(move || Self::list_commitments_sync(db_clone, table)).await?
    }

    async fn delete_repo(&self) -> Result<()> {
        let db_clone = self.db.clone();
        let proof_table = self.proof_table;
        let commitment_table = self.commitment_table;
        let counter_table = self.counter_table;
        spawn_blocking(move || {
            Self::delete_repo(db_clone, proof_table, commitment_table, counter_table)
        })
        .await?
    }
}

#[cfg(test)]
mod tests {
    use crate::error::Error;
    use crate::test_utils::tests::wallet_id;

    use super::*;
    use bcr_common::{
        cashu::{self, Amount},
        core_tests,
    };
    use redb::{Builder, backends::InMemoryBackend};

    fn get_db(wallet_id: &str, unit: CurrencyUnit) -> PocketDB {
        let in_mem = InMemoryBackend::new();
        let db = Arc::new(
            Builder::new()
                .create_with_backend(in_mem)
                .expect("can create in-memory redb"),
        );
        let keypair = secp256k1::Keypair::new_global(&mut secp256k1::rand::thread_rng());
        PocketDB::new(db, wallet_id, &unit, keypair).expect("can create PocketDB")
    }

    fn test_proof() -> cdk00::Proof {
        let (_, keyset) = core_tests::generate_random_ecash_keyset();
        let amounts = [Amount::from(16u64)];
        let proofs = core_tests::generate_random_ecash_proofs(&keyset, &amounts);
        proofs[0].clone()
    }

    #[tokio::test]
    async fn test_store_load_unspent() {
        let repo = get_db(&wallet_id(), CurrencyUnit::Sat);

        let proof = test_proof();
        let y = repo
            .store_new(proof.clone())
            .await
            .expect("store_new works");

        let (loaded, state) = repo.load_proof(y).await.expect("load_proof works");
        assert_eq!(state, cdk07::State::Unspent);
        assert_eq!(loaded, proof);

        let unspent = repo.list_unspent().await.expect("list_unspent works");
        assert!(unspent.contains_key(&y));
    }

    #[tokio::test]
    async fn test_store_load_pendingspent() {
        let repo = get_db(&wallet_id(), CurrencyUnit::Sat);

        let proof = test_proof();
        let y = repo
            .store_pendingspent(proof)
            .await
            .expect("store_pendingspent works");

        let (_loaded, state) = repo.load_proof(y).await.expect("load_proof works");
        assert_eq!(state, cdk07::State::PendingSpent);

        let pending = repo.list_pending().await.expect("list_pending works");
        assert!(pending.contains_key(&y));
    }

    #[tokio::test]
    async fn test_list_and_delete() {
        let repo = get_db(&wallet_id(), CurrencyUnit::Sat);

        let y1 = repo.store_new(test_proof()).await.unwrap();
        let _y2 = repo.store_new(test_proof()).await.unwrap();

        let all = repo.list_all().await.expect("list_all works");
        assert_eq!(all.len(), 2);

        let unspent = repo.list_unspent().await.expect("list_unspent works");
        assert_eq!(unspent.len(), 2);

        let deleted = repo.delete_proof(y1).await.expect("delete_proof works");
        assert!(deleted.is_some());

        let deleted2 = repo.delete_proof(y1).await.expect("delete_proof works");
        assert!(deleted2.is_none());

        let err = repo.load_proof(y1).await.unwrap_err();
        match err {
            Error::ProofNotFound(k) => assert_eq!(k, y1),
            other => panic!("expected ProofNotFound, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_mark_as_pendingspent() {
        let repo = get_db(&wallet_id(), CurrencyUnit::Sat);

        let y = repo.store_new(test_proof()).await.unwrap();
        let _proof = repo
            .mark_as_pendingspent(y)
            .await
            .expect("mark_as_pendingspent works");

        let (_loaded, state) = repo.load_proof(y).await.unwrap();
        assert_eq!(state, cdk07::State::PendingSpent);

        let pending = repo.list_pending().await.unwrap();
        assert!(pending.contains_key(&y));

        let unspent = repo.list_unspent().await.unwrap();
        assert!(!unspent.contains_key(&y));
    }

    #[tokio::test]
    async fn test_mark_as_spent() {
        let repo = get_db(&wallet_id(), CurrencyUnit::Sat);

        let y = repo.store_new(test_proof()).await.unwrap();
        let _proof = repo
            .mark_as_pendingspent(y)
            .await
            .expect("mark_as_pendingspent works");
        let _proof = repo
            .mark_pending_as_spent(y)
            .await
            .expect("mark_pending_as_spent works");

        let (_loaded, state) = repo.load_proof(y).await.unwrap();
        assert_eq!(state, cdk07::State::Spent);

        let pending = repo.list_pending().await.unwrap();
        assert!(!pending.contains_key(&y));

        let spent = repo.list_spent().await.unwrap();
        assert!(spent.contains_key(&y));
    }

    #[tokio::test]
    async fn test_mark_as_pendingspent_invalid_state_errors() {
        let repo = get_db(&wallet_id(), CurrencyUnit::Sat);

        let y = repo.store_pendingspent(test_proof()).await.unwrap();

        let err = repo.mark_as_pendingspent(y).await.unwrap_err();
        match err {
            Error::InvalidProofState(k) => assert_eq!(k, y),
            other => panic!("expected InvalidProofState, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_counter_initializes_and_increments() {
        let repo = get_db(&wallet_id(), CurrencyUnit::Sat);
        let (_, mintkeyset) = core_tests::generate_random_ecash_keyset();
        let kid = mintkeyset.id;

        let c0 = repo.counter(kid).await.expect("counter works");
        assert_eq!(c0, 0);

        repo.increment_counter(kid, 0, 3)
            .await
            .expect("increment_counter works");

        let c1 = repo.counter(kid).await.expect("counter works");
        assert_eq!(c1, 3);

        repo.increment_counter(kid, 3, 2)
            .await
            .expect("increment_counter works");

        let c2 = repo.counter(kid).await.expect("counter works");
        assert_eq!(c2, 5);
    }

    #[tokio::test]
    async fn test_store_load_delete_commitment() {
        let repo = get_db(&wallet_id(), CurrencyUnit::Sat);

        let key = cashu::SecretKey::generate();
        let sig = key.sign(&[0u8; 32]).unwrap();
        let ephemeral_keypair =
            secp256k1::Keypair::new_global(&mut bitcoin::secp256k1::rand::thread_rng());
        let ephemeral_secret = secp256k1::SecretKey::from_keypair(&ephemeral_keypair);
        let wallet_key =
            cashu::PublicKey::from(secp256k1::PublicKey::from_keypair(&ephemeral_keypair));

        repo.store_commitment(crate::SwapCommitmentRecord {
            inputs: vec![],
            outputs: vec![],
            expiry: 1000u64,
            commitment: sig,
            ephemeral_secret,
            body_content: "test_content".to_string(),
            wallet_key,
            premints: HashMap::new(),
        })
        .await
        .expect("store_commitment works");

        let record = repo
            .load_commitment(sig)
            .await
            .expect("load_commitment works");
        assert_eq!(record.expiry, 1000u64);
        assert_eq!(record.body_content, "test_content");
        assert!(record.premints.is_empty());

        repo.delete_commitment(sig)
            .await
            .expect("delete_commitment works");
        assert!(repo.load_commitment(sig).await.is_err());
    }
}
