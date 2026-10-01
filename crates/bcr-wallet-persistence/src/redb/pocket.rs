use crate::{
    MigrationJournalEntry, MigrationJournalHeader, MigrationJournalRepository,
    MigrationJournalState, PocketRepository, SwapCommitmentRecord,
    error::{Error, Result},
};
use async_trait::async_trait;
use bcr_common::wire::borsh::{
    deserialize_cashu_amount, deserialize_from_str, deserialize_optionproofdleq,
    deserialize_optionproofwitness, deserialize_vec_of_strs, deserialize_vecof_blindedmessage,
    serialize_as_str, serialize_cashu_amount, serialize_optionproofdleq,
    serialize_optionproofwitness, serialize_vec_of_strs, serialize_vecof_blindedmessage,
};
use bcr_common::{
    cashu::{
        self, CurrencyUnit, nut00 as cdk00, nut01 as cdk01, nut07 as cdk07, nut12 as cdk12,
        secret::Secret,
    },
    ecash,
};
use bcr_wallet_core::{
    borsh::{deserialize_premints, serialize_premints},
    crypto,
    types::{ForeignMintProof, ForeignMintProofReason},
};
use bitcoin::secp256k1;
use borsh::{BorshDeserialize, BorshSerialize};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition, TableError};
use std::{collections::HashMap, sync::Arc};
use tokio::task::spawn_blocking;

/// Foreign Mint Proof Key is a composite of the clowder_id and the proof y
const COMPRESSED_PUBLIC_KEY_LEN: usize = 33;
const FOREIGN_MINT_PROOF_KEY_LEN: usize = COMPRESSED_PUBLIC_KEY_LEN * 2;

type ForeignMintProofKey = [u8; FOREIGN_MINT_PROOF_KEY_LEN];

fn foreign_mint_proof_key(
    clowder_id: &secp256k1::PublicKey,
    y: &cdk01::PublicKey,
) -> ForeignMintProofKey {
    let mut key = [0u8; FOREIGN_MINT_PROOF_KEY_LEN];
    key[..COMPRESSED_PUBLIC_KEY_LEN].copy_from_slice(&clowder_id.serialize());
    key[COMPRESSED_PUBLIC_KEY_LEN..].copy_from_slice(y.to_bytes().as_slice());
    key
}

/// StoredForeignMintProof is a versioned, encrypted, borsh-serialized foreign mint proof
#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub(super) enum StoredForeignMintProof {
    V1(EncryptedForeignMintProofPayloadV1),
}

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub(super) struct EncryptedForeignMintProofPayloadV1 {
    pub ciphertext: Vec<u8>,
}

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub(super) struct StoredForeignMintProofPayloadV1 {
    #[borsh(
        serialize_with = "serialize_as_str",
        deserialize_with = "deserialize_from_str"
    )]
    pub clowder_id: secp256k1::PublicKey,
    pub proof: StoredProofPayloadV1,
    pub reason: ForeignMintProofReasonV1,
}

impl From<ForeignMintProof> for StoredForeignMintProofPayloadV1 {
    fn from(value: ForeignMintProof) -> Self {
        Self {
            clowder_id: value.clowder_id,
            proof: value.proof.into(),
            reason: value.reason.into(),
        }
    }
}

impl From<StoredForeignMintProofPayloadV1> for ForeignMintProof {
    fn from(value: StoredForeignMintProofPayloadV1) -> Self {
        Self {
            clowder_id: value.clowder_id,
            proof: value.proof.into(),
            reason: value.reason.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum ForeignMintProofReasonV1 {
    MintOffline,
    WalletOffline,
}

impl From<ForeignMintProofReason> for ForeignMintProofReasonV1 {
    fn from(value: ForeignMintProofReason) -> Self {
        match value {
            ForeignMintProofReason::MintOffline => ForeignMintProofReasonV1::MintOffline,
            ForeignMintProofReason::WalletOffline => ForeignMintProofReasonV1::WalletOffline,
        }
    }
}

impl From<ForeignMintProofReasonV1> for ForeignMintProofReason {
    fn from(value: ForeignMintProofReasonV1) -> Self {
        match value {
            ForeignMintProofReasonV1::MintOffline => ForeignMintProofReason::MintOffline,
            ForeignMintProofReasonV1::WalletOffline => ForeignMintProofReason::WalletOffline,
        }
    }
}

pub(super) fn to_stored_foreign_mint_proof_v1(
    proof: ForeignMintProof,
    keys: bitcoin::secp256k1::Keypair,
) -> Result<StoredForeignMintProof> {
    let payload = StoredForeignMintProofPayloadV1::from(proof);
    let encoded = borsh::to_vec(&payload).map_err(|e| Error::BorshSerialization(e.to_string()))?;
    let encrypted = crypto::encrypt_ecies(&encoded, &keys.public_key())?;
    Ok(StoredForeignMintProof::V1(
        EncryptedForeignMintProofPayloadV1 {
            ciphertext: encrypted,
        },
    ))
}

pub(super) fn from_stored_foreign_mint_proof_v1(
    proof: StoredForeignMintProof,
    keys: bitcoin::secp256k1::Keypair,
) -> Result<ForeignMintProof> {
    let StoredForeignMintProof::V1(encrypted_payload) = proof;
    let decrypted = crypto::decrypt_ecies(&encrypted_payload.ciphertext, &keys.secret_key())?;
    let decoded: StoredForeignMintProofPayloadV1 =
        borsh::from_slice(&decrypted).map_err(|e| Error::BorshSerialization(e.to_string()))?;
    Ok(decoded.into())
}

/// StoredCommitment is a versioned, encrypted, borsh-serialized commitment
#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub(super) enum StoredCommitment {
    V1(EncryptedCommitmentPayloadV1),
}

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub(super) struct EncryptedCommitmentPayloadV1 {
    pub ciphertext: Vec<u8>,
}

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub(super) struct StoredCommitmentPayloadV1 {
    #[borsh(
        serialize_with = "serialize_vec_of_strs",
        deserialize_with = "deserialize_vec_of_strs"
    )]
    inputs: Vec<cashu::PublicKey>,
    #[borsh(
        serialize_with = "serialize_vecof_blindedmessage",
        deserialize_with = "deserialize_vecof_blindedmessage"
    )]
    outputs: Vec<cashu::BlindedMessage>,
    expiry: u64,
    #[borsh(
        serialize_with = "serialize_as_str",
        deserialize_with = "deserialize_from_str"
    )]
    commitment: secp256k1::schnorr::Signature,
    ephemeral_secret: Vec<u8>,
    body_content: String,
    #[borsh(
        serialize_with = "serialize_as_str",
        deserialize_with = "deserialize_from_str"
    )]
    wallet_key: cashu::PublicKey,
    #[borsh(
        serialize_with = "serialize_premints",
        deserialize_with = "deserialize_premints"
    )]
    premints: HashMap<ecash::Id, cdk00::PreMintSecrets>,
}

pub(super) fn to_stored_commitment_v1(
    record: SwapCommitmentRecord,
    keys: bitcoin::secp256k1::Keypair,
) -> Result<StoredCommitment> {
    let payload = StoredCommitmentPayloadV1 {
        inputs: record.inputs,
        outputs: record.outputs,
        expiry: record.expiry,
        commitment: record.commitment,
        ephemeral_secret: record.ephemeral_secret.secret_bytes().to_vec(),
        body_content: record.body_content,
        wallet_key: record.wallet_key,
        premints: record.premints,
    };
    let encoded = borsh::to_vec(&payload).map_err(|e| Error::BorshSerialization(e.to_string()))?;
    let encrypted = crypto::encrypt_ecies(&encoded, &keys.public_key())?;
    Ok(StoredCommitment::V1(EncryptedCommitmentPayloadV1 {
        ciphertext: encrypted,
    }))
}

pub(super) fn from_stored_commitment_v1(
    commitment: StoredCommitment,
    keys: bitcoin::secp256k1::Keypair,
) -> Result<SwapCommitmentRecord> {
    let StoredCommitment::V1(encrypted_payload) = commitment;
    let decrypted = crypto::decrypt_ecies(&encrypted_payload.ciphertext, &keys.secret_key())?;
    let c: StoredCommitmentPayloadV1 =
        borsh::from_slice(&decrypted).map_err(|e| Error::BorshSerialization(e.to_string()))?;

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
        premints: c.premints,
    })
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
    keyset_id: ecash::Id,
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
            keyset_id: proof.keyset_id.into(),
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
            keyset_id: entry.keyset_id.into(),
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

/// StoredCounter is a versioned, borsh-serialized wallet counter
#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub(super) enum StoredCounter {
    V1(StoredCounterPayloadV1),
}

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub(super) struct StoredCounterPayloadV1 {
    #[borsh(
        serialize_with = "serialize_as_str",
        deserialize_with = "deserialize_from_str"
    )]
    pub kid: ecash::Id,
    pub counter: u32,
}

const MIGRATION_JOURNAL_HEADER_KEY: &[u8] = &[];

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub(super) enum StoredJournalHeader {
    V1(EncryptedJournalPayloadV1),
}

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub(super) enum StoredJournalEntry {
    V1(EncryptedJournalPayloadV1),
}

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub(super) struct EncryptedJournalPayloadV1 {
    pub ciphertext: Vec<u8>,
}

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub(super) struct StoredJournalHeaderPayloadV1 {
    #[borsh(
        serialize_with = "serialize_as_str",
        deserialize_with = "deserialize_from_str"
    )]
    substitute_url: url::Url,
    #[borsh(
        serialize_with = "serialize_as_str",
        deserialize_with = "deserialize_from_str"
    )]
    substitute_clowder_id: secp256k1::PublicKey,
    #[borsh(
        serialize_with = "serialize_as_str",
        deserialize_with = "deserialize_from_str"
    )]
    alpha_id: secp256k1::PublicKey,
    evidence_digest: [u8; 32],
}

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub(super) struct StoredJournalEntryPayloadV1 {
    proof: StoredProofPayloadV1,
    exchange_key: [u8; 32],
    state: JournalStateV1,
}

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub(super) enum JournalStateV1 {
    Pending,
    Sent,
    Exchanged(Vec<StoredProofPayloadV1>),
    Swapped,
    Held,
    Reclaimed(u64),
}

impl From<MigrationJournalState> for JournalStateV1 {
    fn from(value: MigrationJournalState) -> Self {
        match value {
            MigrationJournalState::Pending => JournalStateV1::Pending,
            MigrationJournalState::Sent => JournalStateV1::Sent,
            MigrationJournalState::Exchanged(proofs) => {
                JournalStateV1::Exchanged(proofs.into_iter().map(Into::into).collect())
            }
            MigrationJournalState::Swapped => JournalStateV1::Swapped,
            MigrationJournalState::Held => JournalStateV1::Held,
            MigrationJournalState::Reclaimed(amount) => JournalStateV1::Reclaimed(amount.into()),
        }
    }
}

impl From<JournalStateV1> for MigrationJournalState {
    fn from(value: JournalStateV1) -> Self {
        match value {
            JournalStateV1::Pending => MigrationJournalState::Pending,
            JournalStateV1::Sent => MigrationJournalState::Sent,
            JournalStateV1::Exchanged(proofs) => {
                MigrationJournalState::Exchanged(proofs.into_iter().map(Into::into).collect())
            }
            JournalStateV1::Swapped => MigrationJournalState::Swapped,
            JournalStateV1::Held => MigrationJournalState::Held,
            JournalStateV1::Reclaimed(amount) => MigrationJournalState::Reclaimed(amount.into()),
        }
    }
}

fn encrypt_journal_payload(
    payload: &impl BorshSerialize,
    keys: bitcoin::secp256k1::Keypair,
) -> Result<EncryptedJournalPayloadV1> {
    let encoded = borsh::to_vec(payload).map_err(|e| Error::BorshSerialization(e.to_string()))?;
    let ciphertext = crypto::encrypt_ecies(&encoded, &keys.public_key())?;
    Ok(EncryptedJournalPayloadV1 { ciphertext })
}

fn decrypt_journal_payload<T: BorshDeserialize>(
    payload: EncryptedJournalPayloadV1,
    keys: bitcoin::secp256k1::Keypair,
) -> Result<T> {
    let decrypted = crypto::decrypt_ecies(&payload.ciphertext, &keys.secret_key())?;
    borsh::from_slice(&decrypted).map_err(|e| Error::BorshSerialization(e.to_string()))
}

fn encode_journal_header(
    header: MigrationJournalHeader,
    keys: bitcoin::secp256k1::Keypair,
) -> Result<Vec<u8>> {
    let payload = StoredJournalHeaderPayloadV1 {
        substitute_url: header.substitute_url,
        substitute_clowder_id: header.substitute_clowder_id,
        alpha_id: header.alpha_id,
        evidence_digest: header.evidence_digest,
    };
    let stored = StoredJournalHeader::V1(encrypt_journal_payload(&payload, keys)?);
    borsh::to_vec(&stored).map_err(|e| Error::BorshSerialization(e.to_string()))
}

fn decode_journal_header(
    value: &[u8],
    keys: bitcoin::secp256k1::Keypair,
) -> Result<MigrationJournalHeader> {
    let StoredJournalHeader::V1(encrypted) =
        borsh::from_slice(value).map_err(|e| Error::BorshSerialization(e.to_string()))?;
    let payload: StoredJournalHeaderPayloadV1 = decrypt_journal_payload(encrypted, keys)?;
    Ok(MigrationJournalHeader {
        substitute_url: payload.substitute_url,
        substitute_clowder_id: payload.substitute_clowder_id,
        alpha_id: payload.alpha_id,
        evidence_digest: payload.evidence_digest,
    })
}

fn encode_journal_entry(
    entry: MigrationJournalEntry,
    keys: bitcoin::secp256k1::Keypair,
) -> Result<Vec<u8>> {
    let payload = StoredJournalEntryPayloadV1 {
        proof: entry.proof.into(),
        exchange_key: entry.exchange_key.to_secret_bytes(),
        state: entry.state.into(),
    };
    let stored = StoredJournalEntry::V1(encrypt_journal_payload(&payload, keys)?);
    borsh::to_vec(&stored).map_err(|e| Error::BorshSerialization(e.to_string()))
}

fn decode_journal_entry(
    value: &[u8],
    keys: bitcoin::secp256k1::Keypair,
) -> Result<MigrationJournalEntry> {
    let StoredJournalEntry::V1(encrypted) =
        borsh::from_slice(value).map_err(|e| Error::BorshSerialization(e.to_string()))?;
    let payload: StoredJournalEntryPayloadV1 = decrypt_journal_payload(encrypted, keys)?;
    Ok(MigrationJournalEntry {
        proof: payload.proof.into(),
        exchange_key: cashu::SecretKey::from_slice(&payload.exchange_key)?,
        state: payload.state.into(),
    })
}

///////////////////////////////////////////// PocketDB
pub struct PocketDB {
    db: Arc<Database>,
    proof_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
    counter_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
    commitment_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
    foreign_mint_proof_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
    migration_journal_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
    keys: bitcoin::secp256k1::Keypair,
}

impl PocketDB {
    const PROOF_BASE_DB_NAME: &'static str = "proofs";
    const FOREIGN_MINT_PROOF_BASE_DB_NAME: &'static str = "foreign_mint_proofs";
    const COUNTER_BASE_DB_NAME: &'static str = "counters";
    const COMMITMENT_BASE_DB_NAME: &'static str = "commitments";
    const MIGRATION_JOURNAL_BASE_DB_NAME: &'static str = "migration_journal";

    pub fn proof_table_name(wallet_id: &str, unit: &CurrencyUnit) -> String {
        format!("{wallet_id}_{unit}_{}", Self::PROOF_BASE_DB_NAME)
    }

    pub fn counter_table_name(wallet_id: &str, unit: &CurrencyUnit) -> String {
        format!("{wallet_id}_{unit}_{}", Self::COUNTER_BASE_DB_NAME)
    }

    pub fn commitment_table_name(wallet_id: &str, unit: &CurrencyUnit) -> String {
        format!("{wallet_id}_{unit}_{}", Self::COMMITMENT_BASE_DB_NAME)
    }

    pub fn foreign_mint_proof_table_name(wallet_id: &str, unit: &CurrencyUnit) -> String {
        format!(
            "{wallet_id}_{unit}_{}",
            Self::FOREIGN_MINT_PROOF_BASE_DB_NAME
        )
    }

    pub fn migration_journal_table_name(wallet_id: &str, unit: &CurrencyUnit) -> String {
        format!(
            "{wallet_id}_{unit}_{}",
            Self::MIGRATION_JOURNAL_BASE_DB_NAME
        )
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
        let foreign_mint_proof_name: &'static str =
            Box::leak(Self::foreign_mint_proof_table_name(wallet_id, unit).into_boxed_str());
        let migration_journal_name: &'static str =
            Box::leak(Self::migration_journal_table_name(wallet_id, unit).into_boxed_str());

        let proof_table = TableDefinition::new(proof_name);
        let counter_table = TableDefinition::new(counter_name);
        let commitment_table = TableDefinition::new(commitment_name);
        let foreign_mint_proof_table = TableDefinition::new(foreign_mint_proof_name);
        let migration_journal_table = TableDefinition::new(migration_journal_name);

        Ok(Self {
            db,
            proof_table,
            counter_table,
            commitment_table,
            foreign_mint_proof_table,
            migration_journal_table,
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
            if table.get(y.to_bytes().as_slice())?.is_none() {
                let serialized =
                    borsh::to_vec(&entry).map_err(|e| Error::BorshSerialization(e.to_string()))?;
                table.insert(y.to_bytes().as_slice(), serialized)?;
            }
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
        ys: &[cdk01::PublicKey],
        old_state_set: &[cdk07::State],
        new_state: cdk07::State,
    ) -> Result<Vec<cdk00::Proof>> {
        let write_txn = db.begin_write()?;
        let mut proofs = Vec::with_capacity(ys.len());
        {
            let mut table = write_txn.open_table(proof_table)?;
            for y in ys {
                let Some(old_value) = table.get(y.to_bytes().as_slice())?.map(|v| v.value()) else {
                    return Err(Error::ProofNotFound(*y));
                };
                let deserialized: StoredProof = borsh::from_slice(old_value.as_slice())
                    .map_err(|e| Error::BorshSerialization(e.to_string()))?;
                let (proof, proof_state) = from_stored_proof_v1(deserialized, keys)?;

                if !old_state_set.contains(&proof_state) {
                    return Err(Error::InvalidProofState(*y));
                }

                let entry = to_stored_proof_v1(proof.clone(), Some(new_state), keys)?;
                let serialized =
                    borsh::to_vec(&entry).map_err(|e| Error::BorshSerialization(e.to_string()))?;

                table.insert(y.to_bytes().as_slice(), serialized)?;
                proofs.push(proof);
            }
        }

        write_txn.commit()?;
        Ok(proofs)
    }

    fn load_counter_sync(
        db: Arc<Database>,
        counter_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        kid: ecash::Id,
    ) -> Result<StoredCounter> {
        let read_txn = db.begin_read()?;

        match read_txn.open_table(counter_table) {
            Ok(table) => {
                let entry = table.get(kid.to_bytes().as_slice())?;
                match entry {
                    Some(e) => {
                        let deserialized: StoredCounter =
                            borsh::from_slice(e.value().as_slice())
                                .map_err(|e| Error::BorshSerialization(e.to_string()))?;
                        Ok(deserialized)
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
        kid: ecash::Id,
    ) -> Result<StoredCounter> {
        let write_txn = db.begin_write()?;

        let entry = {
            let mut table = write_txn.open_table(counter_table)?;
            let existing = table.get(kid.to_bytes().as_slice())?.map(|v| v.value());
            match existing {
                Some(existing) => borsh::from_slice(&existing)
                    .map_err(|e| Error::BorshSerialization(e.to_string()))?,
                None => {
                    let entry = StoredCounter::V1(StoredCounterPayloadV1 { kid, counter: 0 });
                    let serialized = borsh::to_vec(&entry)
                        .map_err(|e| Error::BorshSerialization(e.to_string()))?;
                    table.insert(kid.to_bytes().as_slice(), serialized)?;
                    entry
                }
            }
        };

        write_txn.commit()?;
        Ok(entry)
    }

    fn increment_counter_sync(
        db: Arc<Database>,
        counter_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        old: StoredCounter,
        new: StoredCounter,
    ) -> Result<()> {
        let StoredCounter::V1(old) = old;
        let StoredCounter::V1(new) = new;
        if old.kid != new.kid {
            return Err(Error::CounterKidMismatch);
        }

        let write_txn = db.begin_write()?;
        {
            let mut table = write_txn.open_table(counter_table)?;
            let old_value = table.get(old.kid.to_bytes().as_slice())?.map(|v| v.value());

            if let Some(old_value) = old_value {
                let deserialized: StoredCounter = borsh::from_slice(&old_value)
                    .map_err(|e| Error::BorshSerialization(e.to_string()))?;
                let StoredCounter::V1(old_counter) = deserialized;

                if old_counter.kid != old.kid {
                    return Err(Error::CounterKidMismatch);
                }
                if old_counter.counter != old.counter {
                    return Err(Error::CounterConflict(old.kid));
                }

                let serialized = borsh::to_vec(&StoredCounter::V1(new))
                    .map_err(|e| Error::BorshSerialization(e.to_string()))?;
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
        keys: bitcoin::secp256k1::Keypair,
    ) -> Result<()> {
        let commitment = record.commitment;
        let entry = to_stored_commitment_v1(record, keys)?;
        let write_txn = db.begin_write()?;

        {
            let mut table = write_txn.open_table(commitment_table)?;
            let serialized =
                borsh::to_vec(&entry).map_err(|e| Error::BorshSerialization(e.to_string()))?;

            table.insert(commitment.serialize().as_slice(), serialized)?;
        }

        write_txn.commit()?;
        Ok(())
    }

    fn load_commitment_sync(
        db: Arc<Database>,
        commitment_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        commitment: secp256k1::schnorr::Signature,
        keys: bitcoin::secp256k1::Keypair,
    ) -> Result<SwapCommitmentRecord> {
        let read_txn = db.begin_read()?;

        match read_txn.open_table(commitment_table) {
            Ok(table) => {
                let entry = table.get(commitment.serialize().as_slice())?;
                match entry {
                    Some(e) => {
                        let deserialized: StoredCommitment =
                            borsh::from_slice(e.value().as_slice())
                                .map_err(|e| Error::BorshSerialization(e.to_string()))?;
                        let record = from_stored_commitment_v1(deserialized, keys)?;
                        Ok(record)
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
        keys: bitcoin::secp256k1::Keypair,
    ) -> Result<Vec<SwapCommitmentRecord>> {
        let read_txn = db.begin_read()?;

        match read_txn.open_table(commitment_table) {
            Ok(table) => {
                let mut res = Vec::new();
                for (_, v) in table.range::<&[u8]>(..)?.flatten() {
                    let deserialized: StoredCommitment = borsh::from_slice(v.value().as_slice())
                        .map_err(|e| Error::BorshSerialization(e.to_string()))?;
                    let record = from_stored_commitment_v1(deserialized, keys)?;
                    res.push(record);
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

    fn store_foreign_mint_proof_sync(
        db: Arc<Database>,
        foreign_mint_proof_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        keys: bitcoin::secp256k1::Keypair,
        foreign_mint_proof: ForeignMintProof,
    ) -> Result<cdk01::PublicKey> {
        let y = foreign_mint_proof.proof.y().expect("valid y");
        let key = foreign_mint_proof_key(&foreign_mint_proof.clowder_id, &y);
        let entry = to_stored_foreign_mint_proof_v1(foreign_mint_proof, keys)?;
        let write_txn = db.begin_write()?;

        {
            let mut table = write_txn.open_table(foreign_mint_proof_table)?;
            let serialized =
                borsh::to_vec(&entry).map_err(|e| Error::BorshSerialization(e.to_string()))?;
            table.insert(key.as_slice(), serialized)?;
        }

        write_txn.commit()?;
        Ok(y)
    }

    fn load_foreign_mint_proofs_sync(
        db: Arc<Database>,
        foreign_mint_proof_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        keys: bitcoin::secp256k1::Keypair,
    ) -> Result<Vec<ForeignMintProof>> {
        let read_txn = db.begin_read()?;
        match read_txn.open_table(foreign_mint_proof_table) {
            Ok(table) => {
                let mut res = Vec::new();
                for item in table.range::<&[u8]>(..)? {
                    let (_, v) = item?;
                    let deserialized: StoredForeignMintProof =
                        borsh::from_slice(v.value().as_slice())
                            .map_err(|e| Error::BorshSerialization(e.to_string()))?;
                    let fmp = from_stored_foreign_mint_proof_v1(deserialized, keys)?;
                    res.push(fmp)
                }
                Ok(res)
            }
            Err(TableError::TableDoesNotExist(_)) => Ok(vec![]),
            Err(e) => Err(e.into()),
        }
    }

    fn delete_foreign_mint_proofs_sync(
        db: Arc<Database>,
        foreign_mint_proof_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        clowder_id: secp256k1::PublicKey,
        ys: Vec<cdk01::PublicKey>,
    ) -> Result<()> {
        let write_txn = db.begin_write()?;

        {
            let mut table = write_txn.open_table(foreign_mint_proof_table)?;
            for y in ys.iter() {
                let key = foreign_mint_proof_key(&clowder_id, y);
                table.remove(key.as_slice())?;
            }
        }

        write_txn.commit()?;
        Ok(())
    }

    fn delete_repo(
        db: Arc<Database>,
        proof_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        commitment_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        counter_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        foreign_mint_proof_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        migration_journal_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
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

            if write_txn.open_table(foreign_mint_proof_table).is_ok() {
                write_txn.delete_table(foreign_mint_proof_table)?;
            }

            if write_txn.open_table(migration_journal_table).is_ok() {
                write_txn.delete_table(migration_journal_table)?;
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

    async fn mark_as_pendingspent(&self, ys: Vec<cdk01::PublicKey>) -> Result<Vec<cdk00::Proof>> {
        let db_clone = self.db.clone();
        let table = self.proof_table;
        let keys = self.keys;
        spawn_blocking(move || {
            Self::update_entry_state_sync(
                db_clone,
                table,
                keys,
                &ys,
                &[cdk07::State::Unspent],
                cdk07::State::PendingSpent,
            )
        })
        .await?
    }

    async fn mark_pending_as_spent(&self, y: cdk01::PublicKey) -> Result<cdk00::Proof> {
        let db_clone = self.db.clone();
        let table = self.proof_table;
        let keys = self.keys;
        let mut proofs = spawn_blocking(move || {
            Self::update_entry_state_sync(
                db_clone,
                table,
                keys,
                &[y],
                &[cdk07::State::Pending, cdk07::State::PendingSpent],
                cdk07::State::Spent,
            )
        })
        .await??;
        Ok(proofs.remove(0))
    }

    async fn revert_pendingspent_to_unspent(&self, y: cdk01::PublicKey) -> Result<cdk00::Proof> {
        let db_clone = self.db.clone();
        let table = self.proof_table;
        let keys = self.keys;
        let mut proofs = spawn_blocking(move || {
            Self::update_entry_state_sync(
                db_clone,
                table,
                keys,
                &[y],
                &[cdk07::State::PendingSpent],
                cdk07::State::Unspent,
            )
        })
        .await??;
        Ok(proofs.remove(0))
    }

    async fn counter(&self, kid: ecash::Id) -> Result<u32> {
        let db_clone = self.db.clone();
        let table = self.counter_table;
        let counter =
            spawn_blocking(move || Self::load_counter_sync(db_clone, table, kid)).await??;
        let StoredCounter::V1(counter) = counter;
        Ok(counter.counter)
    }

    async fn increment_counter(&self, kid: ecash::Id, old: u32, increment: u32) -> Result<()> {
        let db_clone = self.db.clone();
        let table = self.counter_table;
        let old_c = StoredCounterPayloadV1 { kid, counter: old };
        let old = StoredCounter::V1(old_c.clone());
        let new = StoredCounter::V1(StoredCounterPayloadV1 {
            kid,
            counter: old_c
                .counter
                .checked_add(increment)
                // TODO (future): how do we handle this? we can switch seeds / switch keyset, but if we hit u32::Max, this fails
                .ok_or(Error::CounterExhausted)?,
        });
        spawn_blocking(move || Self::increment_counter_sync(db_clone, table, old, new)).await?
    }

    async fn store_commitment(&self, record: crate::SwapCommitmentRecord) -> Result<()> {
        let db_clone = self.db.clone();
        let table = self.commitment_table;
        let keys = self.keys;
        spawn_blocking(move || Self::store_commitment_sync(db_clone, table, record, keys)).await?
    }

    async fn load_commitment(
        &self,
        commitment: secp256k1::schnorr::Signature,
    ) -> Result<SwapCommitmentRecord> {
        let db_clone = self.db.clone();
        let table = self.commitment_table;
        let keys = self.keys;
        spawn_blocking(move || Self::load_commitment_sync(db_clone, table, commitment, keys))
            .await?
    }

    async fn delete_commitment(&self, commitment: secp256k1::schnorr::Signature) -> Result<()> {
        let db_clone = self.db.clone();
        let table = self.commitment_table;
        spawn_blocking(move || Self::delete_commitment_sync(db_clone, table, commitment)).await?
    }

    async fn list_commitments(&self) -> Result<Vec<SwapCommitmentRecord>> {
        let db_clone = self.db.clone();
        let table = self.commitment_table;
        let keys = self.keys;
        spawn_blocking(move || Self::list_commitments_sync(db_clone, table, keys)).await?
    }

    async fn delete_repo(&self) -> Result<()> {
        let db_clone = self.db.clone();
        let proof_table = self.proof_table;
        let commitment_table = self.commitment_table;
        let counter_table = self.counter_table;
        let foreign_mint_proof_table = self.foreign_mint_proof_table;
        let migration_journal_table = self.migration_journal_table;
        spawn_blocking(move || {
            Self::delete_repo(
                db_clone,
                proof_table,
                commitment_table,
                counter_table,
                foreign_mint_proof_table,
                migration_journal_table,
            )
        })
        .await?
    }

    async fn store_foreign_mint_proof(
        &self,
        foreign_mint_proof: ForeignMintProof,
    ) -> Result<cdk01::PublicKey> {
        let db_clone = self.db.clone();
        let table = self.foreign_mint_proof_table;
        let keys = self.keys;
        let y = spawn_blocking(move || {
            Self::store_foreign_mint_proof_sync(db_clone, table, keys, foreign_mint_proof)
        })
        .await??;
        Ok(y)
    }

    async fn load_foreign_mint_proofs(&self) -> Result<Vec<ForeignMintProof>> {
        let db_clone = self.db.clone();
        let table = self.foreign_mint_proof_table;
        let keys = self.keys;
        let res =
            spawn_blocking(move || Self::load_foreign_mint_proofs_sync(db_clone, table, keys))
                .await??;
        Ok(res)
    }

    async fn delete_foreign_mint_proofs(
        &self,
        clowder_id: secp256k1::PublicKey,
        ys: Vec<cdk01::PublicKey>,
    ) -> Result<()> {
        let db_clone = self.db.clone();
        let table = self.foreign_mint_proof_table;
        spawn_blocking(move || {
            Self::delete_foreign_mint_proofs_sync(db_clone, table, clowder_id, ys)
        })
        .await??;
        Ok(())
    }
}

impl PocketDB {
    fn put_journal_sync(
        db: Arc<Database>,
        proof_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        journal_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        keys: bitcoin::secp256k1::Keypair,
        header: MigrationJournalHeader,
        entries: Vec<(cdk01::PublicKey, cashu::SecretKey)>,
    ) -> Result<()> {
        let write_txn = db.begin_write()?;
        {
            let mut proofs = write_txn.open_table(proof_table)?;
            let mut journal = write_txn.open_table(journal_table)?;
            let stored_header = journal
                .get(MIGRATION_JOURNAL_HEADER_KEY)?
                .map(|v| v.value());
            match stored_header {
                Some(stored) => {
                    if decode_journal_header(&stored, keys)? != header {
                        return Err(Error::MigrationJournalHeaderMismatch);
                    }
                }
                None => {
                    journal.insert(
                        MIGRATION_JOURNAL_HEADER_KEY,
                        encode_journal_header(header, keys)?,
                    )?;
                }
            }
            for (y, exchange_key) in entries {
                let key = y.to_bytes();
                if journal.get(key.as_slice())?.is_some() {
                    return Err(Error::ProofAlreadyJournaled(y));
                }
                let Some(stored) = proofs.remove(key.as_slice())?.map(|v| v.value()) else {
                    return Err(Error::ProofNotFound(y));
                };
                let deserialized: StoredProof = borsh::from_slice(&stored)
                    .map_err(|e| Error::BorshSerialization(e.to_string()))?;
                let (proof, proof_state) = from_stored_proof_v1(deserialized, keys)?;
                let state = match proof_state {
                    cdk07::State::Unspent => MigrationJournalState::Pending,
                    cdk07::State::PendingSpent => MigrationJournalState::Held,
                    _ => return Err(Error::InvalidProofState(y)),
                };
                let entry = MigrationJournalEntry {
                    proof,
                    exchange_key,
                    state,
                };
                journal.insert(key.as_slice(), encode_journal_entry(entry, keys)?)?;
            }
        }
        write_txn.commit()?;
        Ok(())
    }

    fn update_journal_sync(
        db: Arc<Database>,
        journal_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        keys: bitcoin::secp256k1::Keypair,
        y: cdk01::PublicKey,
        state: MigrationJournalState,
    ) -> Result<()> {
        let write_txn = db.begin_write()?;
        {
            let mut journal = write_txn.open_table(journal_table)?;
            let key = y.to_bytes();
            let Some(stored) = journal.get(key.as_slice())?.map(|v| v.value()) else {
                return Err(Error::MigrationJournalEntryNotFound(y));
            };
            let mut entry = decode_journal_entry(&stored, keys)?;
            entry.state = state;
            journal.insert(key.as_slice(), encode_journal_entry(entry, keys)?)?;
        }
        write_txn.commit()?;
        Ok(())
    }

    fn load_journal_sync(
        db: Arc<Database>,
        journal_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        keys: bitcoin::secp256k1::Keypair,
    ) -> Result<
        Option<(
            MigrationJournalHeader,
            HashMap<cdk01::PublicKey, MigrationJournalEntry>,
        )>,
    > {
        let read_txn = db.begin_read()?;
        let journal = match read_txn.open_table(journal_table) {
            Ok(table) => table,
            Err(TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let Some(stored_header) = journal.get(MIGRATION_JOURNAL_HEADER_KEY)? else {
            return Ok(None);
        };
        let header = decode_journal_header(&stored_header.value(), keys)?;
        let mut entries = HashMap::new();
        for item in journal.range::<&[u8]>(..)? {
            let (k, v) = item?;
            if k.value() == MIGRATION_JOURNAL_HEADER_KEY {
                continue;
            }
            let y = cdk01::PublicKey::from_slice(k.value())?;
            entries.insert(y, decode_journal_entry(&v.value(), keys)?);
        }
        Ok(Some((header, entries)))
    }

    fn clear_journal_sync(
        db: Arc<Database>,
        journal_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        keys: bitcoin::secp256k1::Keypair,
    ) -> Result<()> {
        let write_txn = db.begin_write()?;
        {
            let mut journal = write_txn.open_table(journal_table)?;
            let mut swapped = Vec::new();
            let mut remaining = 0;
            for item in journal.range::<&[u8]>(..)? {
                let (k, v) = item?;
                if k.value() == MIGRATION_JOURNAL_HEADER_KEY {
                    continue;
                }
                match decode_journal_entry(&v.value(), keys)?.state {
                    MigrationJournalState::Swapped => swapped.push(k.value().to_vec()),
                    _ => remaining += 1,
                }
            }
            for key in swapped {
                journal.remove(key.as_slice())?;
            }
            if remaining == 0 {
                journal.remove(MIGRATION_JOURNAL_HEADER_KEY)?;
            }
        }
        write_txn.commit()?;
        Ok(())
    }
}

#[async_trait]
impl MigrationJournalRepository for PocketDB {
    async fn put(
        &self,
        header: MigrationJournalHeader,
        entries: Vec<(cdk01::PublicKey, cashu::SecretKey)>,
    ) -> Result<()> {
        let db_clone = self.db.clone();
        let proof_table = self.proof_table;
        let journal_table = self.migration_journal_table;
        let keys = self.keys;
        spawn_blocking(move || {
            Self::put_journal_sync(db_clone, proof_table, journal_table, keys, header, entries)
        })
        .await?
    }

    async fn update(&self, y: cdk01::PublicKey, state: MigrationJournalState) -> Result<()> {
        let db_clone = self.db.clone();
        let table = self.migration_journal_table;
        let keys = self.keys;
        spawn_blocking(move || Self::update_journal_sync(db_clone, table, keys, y, state)).await?
    }

    async fn load(
        &self,
    ) -> Result<
        Option<(
            MigrationJournalHeader,
            HashMap<cdk01::PublicKey, MigrationJournalEntry>,
        )>,
    > {
        let db_clone = self.db.clone();
        let table = self.migration_journal_table;
        let keys = self.keys;
        spawn_blocking(move || Self::load_journal_sync(db_clone, table, keys)).await?
    }

    async fn clear(&self) -> Result<()> {
        let db_clone = self.db.clone();
        let table = self.migration_journal_table;
        let keys = self.keys;
        spawn_blocking(move || Self::clear_journal_sync(db_clone, table, keys)).await?
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
            .mark_as_pendingspent(vec![y])
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
            .mark_as_pendingspent(vec![y])
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

        let err = repo.mark_as_pendingspent(vec![y]).await.unwrap_err();
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
    async fn test_increment_counter_rejects_stale_old() {
        let repo = get_db(&wallet_id(), CurrencyUnit::Sat);
        let (_, mintkeyset) = core_tests::generate_random_ecash_keyset();
        let kid = mintkeyset.id;

        repo.counter(kid).await.expect("counter works");
        repo.increment_counter(kid, 0, 3)
            .await
            .expect("increment_counter works");

        let err = repo.increment_counter(kid, 0, 2).await.unwrap_err();
        assert!(matches!(err, Error::CounterConflict(k) if k == kid));
        assert_eq!(repo.counter(kid).await.expect("counter works"), 3);
    }

    #[tokio::test]
    async fn test_store_new_keeps_existing_state() {
        let repo = get_db(&wallet_id(), CurrencyUnit::Sat);
        let proof = test_proof();

        let y = repo.store_new(proof.clone()).await.unwrap();
        repo.mark_as_pendingspent(vec![y]).await.unwrap();
        repo.store_new(proof).await.unwrap();

        let (_, state) = repo.load_proof(y).await.unwrap();
        assert_eq!(state, cdk07::State::PendingSpent);
    }

    #[tokio::test]
    async fn test_mark_as_pendingspent_is_all_or_nothing() {
        let repo = get_db(&wallet_id(), CurrencyUnit::Sat);

        let unspent = repo.store_new(test_proof()).await.unwrap();
        let pending = repo.store_pendingspent(test_proof()).await.unwrap();

        let err = repo
            .mark_as_pendingspent(vec![unspent, pending])
            .await
            .unwrap_err();
        assert!(matches!(err, Error::InvalidProofState(k) if k == pending));

        let (_, state) = repo.load_proof(unspent).await.unwrap();
        assert_eq!(state, cdk07::State::Unspent);
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

    fn test_clowder_id() -> secp256k1::PublicKey {
        let keypair = secp256k1::Keypair::new_global(&mut secp256k1::rand::thread_rng());

        secp256k1::PublicKey::from_keypair(&keypair)
    }

    #[tokio::test]
    async fn test_store_foreign_mint_proof() {
        let repo = get_db(&wallet_id(), CurrencyUnit::Sat);

        let clowder_id = test_clowder_id();
        let proof = test_proof();
        let expected_y = proof.y().expect("proof has valid y");

        let stored_y = repo
            .store_foreign_mint_proof(ForeignMintProof {
                clowder_id,
                proof: proof.clone(),
                reason: ForeignMintProofReason::MintOffline,
            })
            .await
            .expect("store_foreign_mint_proof works");
        assert_eq!(stored_y, expected_y);

        let loaded = repo
            .load_foreign_mint_proofs()
            .await
            .expect("load_foreign_mint_proofs works");
        assert_eq!(loaded.len(), 1);

        let stored = &loaded[0];
        assert_eq!(stored.clowder_id, clowder_id);
        assert_eq!(stored.proof, proof);
        assert!(matches!(stored.reason, ForeignMintProofReason::MintOffline));
    }

    #[tokio::test]
    async fn test_load_foreign_mint_proofs() {
        let repo = get_db(&wallet_id(), CurrencyUnit::Sat);

        let clowder_id_a = test_clowder_id();
        let clowder_id_b = test_clowder_id();

        let proof_a1 = test_proof();
        let proof_a2 = test_proof();
        let proof_b1 = test_proof();

        repo.store_foreign_mint_proof(ForeignMintProof {
            clowder_id: clowder_id_a,
            proof: proof_a1.clone(),
            reason: ForeignMintProofReason::MintOffline,
        })
        .await
        .expect("store first foreign mint proof");

        repo.store_foreign_mint_proof(ForeignMintProof {
            clowder_id: clowder_id_a,
            proof: proof_a2.clone(),
            reason: ForeignMintProofReason::WalletOffline,
        })
        .await
        .expect("store second foreign mint proof");

        repo.store_foreign_mint_proof(ForeignMintProof {
            clowder_id: clowder_id_b,
            proof: proof_b1.clone(),
            reason: ForeignMintProofReason::MintOffline,
        })
        .await
        .expect("store third foreign mint proof");

        let loaded = repo
            .load_foreign_mint_proofs()
            .await
            .expect("load_foreign_mint_proofs works");

        assert_eq!(loaded.len(), 3);
        assert!(loaded.iter().any(|entry| {
            entry.clowder_id == clowder_id_a
                && entry.proof == proof_a1
                && matches!(entry.reason, ForeignMintProofReason::MintOffline)
        }));
        assert!(loaded.iter().any(|entry| {
            entry.clowder_id == clowder_id_a
                && entry.proof == proof_a2
                && matches!(entry.reason, ForeignMintProofReason::WalletOffline)
        }));
        assert!(loaded.iter().any(|entry| {
            entry.clowder_id == clowder_id_b
                && entry.proof == proof_b1
                && matches!(entry.reason, ForeignMintProofReason::MintOffline)
        }));
    }

    #[tokio::test]
    async fn test_delete_foreign_mint_proof() {
        let repo = get_db(&wallet_id(), CurrencyUnit::Sat);

        let clowder_id_a = test_clowder_id();
        let clowder_id_b = test_clowder_id();

        let proof = test_proof();
        let y = proof.y().unwrap();
        let proof_2 = test_proof();
        let y_2 = proof_2.y().unwrap();

        repo.store_foreign_mint_proof(ForeignMintProof {
            clowder_id: clowder_id_a,
            proof: proof.clone(),
            reason: ForeignMintProofReason::MintOffline,
        })
        .await
        .expect("store proof for first clowder");

        repo.store_foreign_mint_proof(ForeignMintProof {
            clowder_id: clowder_id_a,
            proof: proof_2.clone(),
            reason: ForeignMintProofReason::MintOffline,
        })
        .await
        .expect("store proof_2 for first clowder");

        repo.store_foreign_mint_proof(ForeignMintProof {
            clowder_id: clowder_id_b,
            proof: proof.clone(),
            reason: ForeignMintProofReason::WalletOffline,
        })
        .await
        .expect("store proof for second clowder");

        let loaded = repo
            .load_foreign_mint_proofs()
            .await
            .expect("load before delete");
        assert_eq!(loaded.len(), 3);

        repo.delete_foreign_mint_proofs(clowder_id_a, vec![y, y_2])
            .await
            .expect("delete_foreign_mint_proofs works");

        let loaded = repo
            .load_foreign_mint_proofs()
            .await
            .expect("load after delete");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].clowder_id, clowder_id_b);
        assert_eq!(loaded[0].proof, proof);
        assert!(matches!(
            loaded[0].reason,
            ForeignMintProofReason::WalletOffline
        ));

        repo.delete_foreign_mint_proofs(clowder_id_b, vec![y])
            .await
            .expect("delete remaining foreign mint proof");

        let loaded = repo
            .load_foreign_mint_proofs()
            .await
            .expect("load after deleting all records");
        assert!(loaded.is_empty());
    }

    fn journal_keys() -> secp256k1::Keypair {
        secp256k1::Keypair::from_seckey_slice(secp256k1::SECP256K1, &[0x0b; 32])
            .expect("valid keypair")
    }

    fn journal_secret(byte: u8) -> cashu::SecretKey {
        cashu::SecretKey::from_slice(&[byte; 32]).expect("valid secret key")
    }

    fn journal_header(evidence_digest: [u8; 32]) -> MigrationJournalHeader {
        MigrationJournalHeader {
            substitute_url: url::Url::parse("https://sub.example/").expect("valid url"),
            substitute_clowder_id: crate::test_utils::tests::test_pub_key(),
            alpha_id: crate::test_utils::tests::test_other_pub_key(),
            evidence_digest,
        }
    }

    fn journal_payloads(
        db: &Database,
        table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        keys: secp256k1::Keypair,
    ) -> Vec<(Vec<u8>, Vec<u8>, Vec<u8>)> {
        let read_txn = db.begin_read().expect("begin read");
        let table = read_txn.open_table(table).expect("journal table exists");
        table
            .range::<&[u8]>(..)
            .expect("range")
            .map(|item| {
                let (k, v) = item.expect("row");
                let raw = v.value();
                let encrypted = if k.value().is_empty() {
                    let StoredJournalHeader::V1(e) = borsh::from_slice(&raw).expect("header");
                    e
                } else {
                    let StoredJournalEntry::V1(e) = borsh::from_slice(&raw).expect("entry");
                    e
                };
                let plain = crypto::decrypt_ecies(&encrypted.ciphertext, &keys.secret_key())
                    .expect("decrypt");
                (k.value().to_vec(), raw, plain)
            })
            .collect()
    }

    async fn assert_pocket_is(repo: &PocketDB, ys: &[cdk01::PublicKey]) {
        let mut all = repo.list_all().await.expect("list_all");
        all.sort();
        let mut expected = ys.to_vec();
        expected.sort();
        assert_eq!(all, expected);
    }

    #[tokio::test]
    async fn migration_journal_worked_example() {
        let path =
            std::env::temp_dir().join(format!("migration_journal_{}.redb", uuid::Uuid::new_v4()));
        let keys = journal_keys();
        let open = |path: &std::path::Path| {
            let db = Arc::new(Database::create(path).expect("create file db"));
            PocketDB::new(db, "w1", &CurrencyUnit::Sat, keys).expect("PocketDB")
        };
        let repo = open(&path);

        let (_, keyset) = core_tests::generate_random_ecash_keyset();
        let amounts = [8u64, 4, 2, 1, 16, 32].map(Amount::from);
        let proofs = core_tests::generate_random_ecash_proofs(&keyset, &amounts);
        let [p1, p2, p3, p5, p9, b1] = proofs.try_into().expect("six proofs");
        let y1 = repo.store_new(p1.clone()).await.expect("store p1");
        let y2 = repo.store_pendingspent(p2.clone()).await.expect("store p2");
        let y3 = repo.store_new(p3.clone()).await.expect("store p3");
        repo.mark_as_pendingspent(vec![y3])
            .await
            .expect("reserve p3");
        repo.mark_pending_as_spent(y3).await.expect("spend p3");
        let y5 = repo.store_new(p5.clone()).await.expect("store p5");
        let y9 = p9.y().expect("y9");
        let (k1, k2, k3, k5, k9) = (
            journal_secret(0x21),
            journal_secret(0x22),
            journal_secret(0x23),
            journal_secret(0x25),
            journal_secret(0x29),
        );
        let h = journal_header([0x11; 32]);

        repo.put(h.clone(), vec![(y1, k1.clone()), (y2, k2.clone())])
            .await
            .expect("put works");
        assert_pocket_is(&repo, &[y3, y5]).await;
        let expected = HashMap::from([
            (
                y1,
                MigrationJournalEntry {
                    proof: p1.clone(),
                    exchange_key: k1.clone(),
                    state: MigrationJournalState::Pending,
                },
            ),
            (
                y2,
                MigrationJournalEntry {
                    proof: p2.clone(),
                    exchange_key: k2.clone(),
                    state: MigrationJournalState::Held,
                },
            ),
        ]);
        assert_eq!(
            repo.load().await.expect("load"),
            Some((h.clone(), expected.clone()))
        );

        repo.update(y1, MigrationJournalState::Sent)
            .await
            .expect("update works");
        let table = repo.migration_journal_table;
        let before = journal_payloads(&repo.db, table, keys);
        drop(repo);
        let repo = open(&path);
        let after = journal_payloads(&repo.db, table, keys);
        assert_eq!(before, after);
        assert_eq!(after.len(), 3);
        let mut expected = expected;
        expected.get_mut(&y1).expect("y1").state = MigrationJournalState::Sent;
        assert_eq!(
            repo.load().await.expect("load after reopen"),
            Some((h.clone(), expected.clone()))
        );
        for (_, _, plain) in after.iter().filter(|(k, _, _)| !k.is_empty()) {
            let payload: StoredJournalEntryPayloadV1 = borsh::from_slice(plain).expect("payload");
            assert_eq!(&borsh::to_vec(&payload).expect("encode"), plain);
        }

        let err = repo
            .put(h.clone(), vec![(y5, k5.clone()), (y9, k9)])
            .await
            .expect_err("unknown proof refused");
        assert!(matches!(err, Error::ProofNotFound(y) if y == y9));
        let err = repo
            .put(h.clone(), vec![(y3, k3)])
            .await
            .expect_err("spent proof refused");
        assert!(matches!(err, Error::InvalidProofState(y) if y == y3));
        let err = repo
            .put(journal_header([0x22; 32]), vec![(y5, k5)])
            .await
            .expect_err("other outage refused");
        assert!(matches!(err, Error::MigrationJournalHeaderMismatch));
        assert_pocket_is(&repo, &[y3, y5]).await;
        assert_eq!(
            repo.load_proof(y5).await.expect("y5").1,
            cdk07::State::Unspent
        );
        assert_eq!(journal_payloads(&repo.db, table, keys), after);

        repo.update(y1, MigrationJournalState::Exchanged(vec![b1.clone()]))
            .await
            .expect("exchanged");
        let Some((_, loaded)) = repo.load().await.expect("load") else {
            panic!("journal empty");
        };
        assert_eq!(
            loaded[&y1].state,
            MigrationJournalState::Exchanged(vec![b1])
        );
        repo.update(y1, MigrationJournalState::Swapped)
            .await
            .expect("swapped");
        repo.clear().await.expect("clear");
        expected.remove(&y1);
        assert_eq!(
            repo.load().await.expect("load"),
            Some((h.clone(), expected))
        );

        repo.update(y2, MigrationJournalState::Pending)
            .await
            .expect("pending");
        repo.update(y2, MigrationJournalState::Swapped)
            .await
            .expect("swapped");
        repo.clear().await.expect("clear");
        assert_eq!(repo.load().await.expect("load"), None);
        assert!(journal_payloads(&repo.db, table, keys).is_empty());

        drop(repo);
        std::fs::remove_file(&path).expect("remove db file");
    }

    #[tokio::test]
    async fn migration_journal_refuses_duplicate_and_missing_entries() {
        let repo = get_db(&wallet_id(), CurrencyUnit::Sat);
        let proof = test_proof();
        let y = repo.store_new(proof.clone()).await.expect("store");
        let h = journal_header([0x11; 32]);

        let err = repo
            .update(y, MigrationJournalState::Sent)
            .await
            .expect_err("missing entry refused");
        assert!(matches!(err, Error::MigrationJournalEntryNotFound(e) if e == y));

        repo.put(h.clone(), vec![(y, journal_secret(0x21))])
            .await
            .expect("put works");
        repo.store_new(proof).await.expect("store again");
        let err = repo
            .put(h, vec![(y, journal_secret(0x22))])
            .await
            .expect_err("journaled proof refused");
        assert!(matches!(err, Error::ProofAlreadyJournaled(e) if e == y));
        assert_pocket_is(&repo, &[y]).await;
        let Some((_, loaded)) = repo.load().await.expect("load") else {
            panic!("journal empty");
        };
        assert_eq!(loaded[&y].exchange_key, journal_secret(0x21));
    }

    #[tokio::test]
    async fn migration_journal_delete_repo_drops_journal() {
        let repo = get_db(&wallet_id(), CurrencyUnit::Sat);
        let y = repo.store_new(test_proof()).await.expect("store");
        repo.put(journal_header([0x11; 32]), vec![(y, journal_secret(0x21))])
            .await
            .expect("put works");
        repo.delete_repo().await.expect("delete_repo");
        assert_eq!(repo.load().await.expect("load"), None);
    }
}
