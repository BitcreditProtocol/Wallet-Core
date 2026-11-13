use crate::{
    MintMeltRepository,
    error::{Error, Result},
};
use async_trait::async_trait;
use bcr_common::{
    cashu::{CurrencyUnit, nut00 as cdk00},
    ecash,
    wire::borsh::{deserialize_from_str, serialize_as_str},
};
use bcr_wallet_core::{borsh::PremintEntry, crypto, types::MintSummary};
use bitcoin::address::NetworkUnchecked;
use borsh::{
    BorshDeserialize, BorshSerialize,
    io::{Error as BorshError, ErrorKind, Read, Write},
};
use redb::{Database, ReadableDatabase, TableDefinition, TableError};
use std::{fmt::Display, str::FromStr, sync::Arc};
use tokio::task::spawn_blocking;
use uuid::Uuid;

fn serialize_option_as_str<T: Display>(
    value: &Option<T>,
    writer: &mut impl Write,
) -> core::result::Result<(), BorshError> {
    BorshSerialize::serialize(&value.as_ref().map(ToString::to_string), writer)
}

fn serialize_address(
    address: &bitcoin::Address<NetworkUnchecked>,
    writer: &mut impl Write,
) -> core::result::Result<(), BorshError> {
    BorshSerialize::serialize(&address.assume_checked_ref().to_string(), writer)
}

fn deserialize_option_from_str<T: FromStr>(
    reader: &mut impl Read,
) -> core::result::Result<Option<T>, BorshError>
where
    T::Err: Display,
{
    Option::<String>::deserialize_reader(reader)?
        .map(|s| {
            s.parse()
                .map_err(|e| BorshError::new(ErrorKind::InvalidData, format!("{e}")))
        })
        .transpose()
}

///////////////////////////////////////////// Stored envelopes
#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub(super) struct EncryptedPayloadV1 {
    ciphertext: Vec<u8>,
}

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub(super) enum StoredMeltEntry {
    V1(EncryptedPayloadV1),
}

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub(super) enum StoredMintEntry {
    V1(EncryptedPayloadV1),
}

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub(super) enum StoredMeltCommitmentEntry {
    V1(EncryptedPayloadV1),
}

fn encrypt_payload<T: BorshSerialize>(
    payload: &T,
    keys: &bitcoin::secp256k1::Keypair,
) -> Result<EncryptedPayloadV1> {
    let encoded = borsh::to_vec(payload).map_err(|e| Error::BorshSerialization(e.to_string()))?;
    let ciphertext = crypto::encrypt_ecies(&encoded, &keys.public_key())?;
    Ok(EncryptedPayloadV1 { ciphertext })
}

fn decrypt_payload<T: BorshDeserialize>(
    encrypted: &EncryptedPayloadV1,
    keys: &bitcoin::secp256k1::Keypair,
) -> Result<T> {
    let decrypted = crypto::decrypt_ecies(&encrypted.ciphertext, &keys.secret_key())?;
    borsh::from_slice(&decrypted).map_err(|e| Error::BorshSerialization(e.to_string()))
}

fn to_bytes<T: BorshSerialize>(stored: &T) -> Result<Vec<u8>> {
    borsh::to_vec(stored).map_err(|e| Error::BorshSerialization(e.to_string()))
}

fn from_bytes<T: BorshDeserialize>(bytes: &[u8]) -> Result<T> {
    borsh::from_slice(bytes).map_err(|e| Error::BorshSerialization(e.to_string()))
}

///////////////////////////////////////////// MeltEntry
#[derive(Debug, Clone, Default, BorshSerialize, BorshDeserialize)]
pub(super) struct MeltEntry {
    pub quote_id: String,
    pub premints: Vec<PremintEntry>,
    #[borsh(
        serialize_with = "serialize_option_as_str",
        deserialize_with = "deserialize_option_from_str"
    )]
    pub kid: Option<ecash::Id>,
}

pub(super) fn premint_entries(premints: Vec<cdk00::PreMint>) -> Vec<PremintEntry> {
    premints
        .into_iter()
        .map(|premint| PremintEntry {
            blinded_message: premint.blinded_message,
            secret: premint.secret,
            secret_key: premint.r,
            amount: premint.amount,
        })
        .collect()
}

fn premints_from_entries(entries: Vec<PremintEntry>) -> Vec<cdk00::PreMint> {
    entries
        .into_iter()
        .map(|entry| cdk00::PreMint {
            blinded_message: entry.blinded_message,
            secret: entry.secret,
            r: entry.secret_key,
            amount: entry.amount,
        })
        .collect()
}

fn convert_melt_entry_from(qid: String, premints: Option<cdk00::PreMintSecrets>) -> MeltEntry {
    match premints {
        None => MeltEntry {
            quote_id: qid,
            ..Default::default()
        },
        Some(cdk00::PreMintSecrets { secrets, keyset_id }) => MeltEntry {
            quote_id: qid,
            premints: premint_entries(secrets),
            kid: Some(keyset_id.into()),
        },
    }
}

fn convert_melt_entry_to(entry: MeltEntry) -> (String, Option<cdk00::PreMintSecrets>) {
    let MeltEntry {
        quote_id,
        premints,
        kid,
    } = entry;
    let premints = kid.map(|keyset_id| cdk00::PreMintSecrets {
        secrets: premints_from_entries(premints),
        keyset_id: keyset_id.into(),
    });
    (quote_id, premints)
}

pub(super) fn encode_melt_entry(
    entry: &MeltEntry,
    keys: &bitcoin::secp256k1::Keypair,
) -> Result<Vec<u8>> {
    to_bytes(&StoredMeltEntry::V1(encrypt_payload(entry, keys)?))
}

fn decode_melt_entry(bytes: &[u8], keys: &bitcoin::secp256k1::Keypair) -> Result<MeltEntry> {
    let StoredMeltEntry::V1(encrypted) = from_bytes(bytes)?;
    decrypt_payload(&encrypted, keys)
}

///////////////////////////////////////////// MintEntry
#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub(super) struct MintEntry {
    pub quote_id: Uuid,
    pub amount_sat: u64,
    #[borsh(
        serialize_with = "serialize_address",
        deserialize_with = "deserialize_from_str"
    )]
    pub address: bitcoin::Address<NetworkUnchecked>,
    pub expiry: u64,
    pub premints: Vec<PremintEntry>,
    #[borsh(
        serialize_with = "serialize_as_str",
        deserialize_with = "deserialize_from_str"
    )]
    pub kid: ecash::Id,
    pub content: String,
    pub commitment: [u8; 64],
    pub ephemeral_secret: [u8; 32],
}

fn convert_mint_entry_from(
    quote_id: Uuid,
    amount: bitcoin::Amount,
    address: bitcoin::Address<NetworkUnchecked>,
    expiry: u64,
    premints: cdk00::PreMintSecrets,
    content: String,
    commitment: bitcoin::secp256k1::schnorr::Signature,
    ephemeral_secret: bitcoin::secp256k1::SecretKey,
) -> MintEntry {
    let cdk00::PreMintSecrets { secrets, keyset_id } = premints;
    MintEntry {
        quote_id,
        amount_sat: amount.to_sat(),
        address,
        expiry,
        premints: premint_entries(secrets),
        kid: keyset_id.into(),
        content,
        commitment: commitment.serialize(),
        ephemeral_secret: ephemeral_secret.secret_bytes(),
    }
}

fn convert_mint_entry_to(entry: MintEntry) -> Result<crate::MintRecord> {
    let summary = MintSummary {
        quote_id: entry.quote_id,
        amount: bitcoin::Amount::from_sat(entry.amount_sat),
        address: entry.address,
        expiry: entry.expiry,
    };
    let ephemeral_secret = bitcoin::secp256k1::SecretKey::from_slice(&entry.ephemeral_secret)
        .map_err(|e| Error::Custom(format!("invalid ephemeral secret: {e}")))?;
    let commitment = bitcoin::secp256k1::schnorr::Signature::from_slice(&entry.commitment)
        .map_err(|e| Error::Custom(format!("invalid commitment signature: {e}")))?;
    Ok(crate::MintRecord {
        summary,
        premint: cdk00::PreMintSecrets {
            secrets: premints_from_entries(entry.premints),
            keyset_id: entry.kid.into(),
        },
        content: entry.content,
        commitment,
        ephemeral_secret,
    })
}

pub(super) fn encode_mint_entry(
    entry: &MintEntry,
    keys: &bitcoin::secp256k1::Keypair,
) -> Result<Vec<u8>> {
    to_bytes(&StoredMintEntry::V1(encrypt_payload(entry, keys)?))
}

fn decode_mint_entry(bytes: &[u8], keys: &bitcoin::secp256k1::Keypair) -> Result<MintEntry> {
    let StoredMintEntry::V1(encrypted) = from_bytes(bytes)?;
    decrypt_payload(&encrypted, keys)
}

///////////////////////////////////////////// MeltCommitmentEntry
#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub(super) struct MeltCommitmentEntry {
    pub quote_id: Uuid,
    pub expiry: u64,
    pub commitment: [u8; 64],
    pub ephemeral_secret: [u8; 32],
    pub body_content: String,
}

fn convert_melt_commitment_from(record: crate::MeltCommitmentRecord) -> MeltCommitmentEntry {
    MeltCommitmentEntry {
        quote_id: record.quote_id,
        expiry: record.expiry,
        commitment: record.commitment.serialize(),
        ephemeral_secret: record.ephemeral_secret.secret_bytes(),
        body_content: record.body_content,
    }
}

fn convert_melt_commitment_to(entry: MeltCommitmentEntry) -> Result<crate::MeltCommitmentRecord> {
    let ephemeral_secret = bitcoin::secp256k1::SecretKey::from_slice(&entry.ephemeral_secret)
        .map_err(|e| Error::Custom(format!("invalid ephemeral secret: {e}")))?;
    let commitment = bitcoin::secp256k1::schnorr::Signature::from_slice(&entry.commitment)
        .map_err(|e| Error::Custom(format!("invalid commitment signature: {e}")))?;
    Ok(crate::MeltCommitmentRecord {
        quote_id: entry.quote_id,
        expiry: entry.expiry,
        commitment,
        ephemeral_secret,
        body_content: entry.body_content,
    })
}

pub(super) fn encode_melt_commitment_entry(
    entry: &MeltCommitmentEntry,
    keys: &bitcoin::secp256k1::Keypair,
) -> Result<Vec<u8>> {
    to_bytes(&StoredMeltCommitmentEntry::V1(encrypt_payload(
        entry, keys,
    )?))
}

fn decode_melt_commitment_entry(
    bytes: &[u8],
    keys: &bitcoin::secp256k1::Keypair,
) -> Result<MeltCommitmentEntry> {
    let StoredMeltCommitmentEntry::V1(encrypted) = from_bytes(bytes)?;
    decrypt_payload(&encrypted, keys)
}

///////////////////////////////////////////// MintMeltDB
pub struct MintMeltDB {
    db: Arc<Database>,
    keys: bitcoin::secp256k1::Keypair,
    melt_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
    mint_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
    melt_commitment_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
}

impl MintMeltDB {
    const MELT_BASE_DB_NAME: &'static str = "melts";
    const MINT_BASE_DB_NAME: &'static str = "mints";
    const MELT_COMMITMENT_BASE_DB_NAME: &'static str = "melt_commitments";

    pub(super) fn melt_table_name(wallet_id: &str, unit: &CurrencyUnit) -> String {
        format!("{wallet_id}_{unit}_{}", Self::MELT_BASE_DB_NAME)
    }

    pub(super) fn mint_table_name(wallet_id: &str, unit: &CurrencyUnit) -> String {
        format!("{wallet_id}_{unit}_{}", Self::MINT_BASE_DB_NAME)
    }

    pub(super) fn melt_commitment_table_name(wallet_id: &str, unit: &CurrencyUnit) -> String {
        format!("{wallet_id}_{unit}_{}", Self::MELT_COMMITMENT_BASE_DB_NAME)
    }

    pub fn new(
        db: Arc<Database>,
        wallet_id: &str,
        unit: &CurrencyUnit,
        keys: bitcoin::secp256k1::Keypair,
    ) -> Result<Self> {
        // Leak once to get static string, because of dynamically generated table names
        let melt_name: &'static str =
            Box::leak(Self::melt_table_name(wallet_id, unit).into_boxed_str());
        let mint_name: &'static str =
            Box::leak(Self::mint_table_name(wallet_id, unit).into_boxed_str());
        let melt_commitment_name: &'static str =
            Box::leak(Self::melt_commitment_table_name(wallet_id, unit).into_boxed_str());
        let melt_table = TableDefinition::new(melt_name);
        let mint_table = TableDefinition::new(mint_name);
        let melt_commitment_table = TableDefinition::new(melt_commitment_name);
        Ok(MintMeltDB {
            db,
            keys,
            melt_table,
            mint_table,
            melt_commitment_table,
        })
    }

    // melt
    fn store_melt_sync(
        db: Arc<Database>,
        melt_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        keys: bitcoin::secp256k1::Keypair,
        melt: MeltEntry,
    ) -> Result<String> {
        let id = melt.quote_id.clone();
        let write_txn = db.begin_write()?;

        {
            let mut table = write_txn.open_table(melt_table)?;

            table.insert(id.as_bytes(), encode_melt_entry(&melt, &keys)?)?;
        }

        write_txn.commit()?;
        Ok(id)
    }

    fn load_melt_sync(
        db: Arc<Database>,
        melt_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        keys: bitcoin::secp256k1::Keypair,
        qid: String,
    ) -> Result<Option<MeltEntry>> {
        let read_txn = db.begin_read()?;

        match read_txn.open_table(melt_table) {
            Ok(table) => {
                let entry = table.get(qid.as_bytes())?;
                match entry {
                    Some(e) => Ok(Some(decode_melt_entry(e.value().as_slice(), &keys)?)),
                    None => Ok(None),
                }
            }
            Err(TableError::TableDoesNotExist(_)) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn delete_melt_sync(
        db: Arc<Database>,
        melt_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        qid: String,
    ) -> Result<()> {
        let write_txn = db.begin_write()?;

        {
            let mut table = write_txn.open_table(melt_table)?;
            table.remove(qid.as_bytes())?;
        }

        write_txn.commit()?;
        Ok(())
    }

    fn list_melts_sync(
        db: Arc<Database>,
        melt_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        keys: bitcoin::secp256k1::Keypair,
    ) -> Result<Vec<String>> {
        let read_txn = db.begin_read()?;

        match read_txn.open_table(melt_table) {
            Ok(table) => {
                let mut res = Vec::new();
                for (_, v) in table.range::<&[u8]>(..)?.flatten() {
                    res.push(decode_melt_entry(v.value().as_slice(), &keys)?.quote_id);
                }
                Ok(res)
            }
            Err(TableError::TableDoesNotExist(_)) => Ok(vec![]),
            Err(e) => Err(e.into()),
        }
    }

    // mint
    fn store_mint_sync(
        db: Arc<Database>,
        mint_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        keys: bitcoin::secp256k1::Keypair,
        mint: MintEntry,
    ) -> Result<Uuid> {
        let write_txn = db.begin_write()?;

        {
            let mut table = write_txn.open_table(mint_table)?;

            table.insert(
                mint.quote_id.as_bytes().as_slice(),
                encode_mint_entry(&mint, &keys)?,
            )?;
        }

        write_txn.commit()?;
        Ok(mint.quote_id)
    }

    fn load_mint_sync(
        db: Arc<Database>,
        mint_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        keys: bitcoin::secp256k1::Keypair,
        qid: Uuid,
    ) -> Result<Option<MintEntry>> {
        let read_txn = db.begin_read()?;

        match read_txn.open_table(mint_table) {
            Ok(table) => {
                let entry = table.get(qid.as_bytes().as_slice())?;
                match entry {
                    Some(e) => Ok(Some(decode_mint_entry(e.value().as_slice(), &keys)?)),
                    None => Ok(None),
                }
            }
            Err(TableError::TableDoesNotExist(_)) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn delete_mint_sync(
        db: Arc<Database>,
        mint_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        qid: Uuid,
    ) -> Result<()> {
        let write_txn = db.begin_write()?;

        {
            let mut table = write_txn.open_table(mint_table)?;
            table.remove(qid.as_bytes().as_slice())?;
        }

        write_txn.commit()?;
        Ok(())
    }

    fn list_mints_sync(
        db: Arc<Database>,
        mint_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        keys: bitcoin::secp256k1::Keypair,
    ) -> Result<Vec<Uuid>> {
        let read_txn = db.begin_read()?;

        match read_txn.open_table(mint_table) {
            Ok(table) => {
                let mut res = Vec::new();
                for (_, v) in table.range::<&[u8]>(..)?.flatten() {
                    res.push(decode_mint_entry(v.value().as_slice(), &keys)?.quote_id);
                }
                Ok(res)
            }
            Err(TableError::TableDoesNotExist(_)) => Ok(vec![]),
            Err(e) => Err(e.into()),
        }
    }

    // melt commitment
    fn store_melt_commitment_sync(
        db: Arc<Database>,
        table_def: TableDefinition<'static, &'static [u8], Vec<u8>>,
        keys: bitcoin::secp256k1::Keypair,
        record: crate::MeltCommitmentRecord,
    ) -> Result<()> {
        let entry = convert_melt_commitment_from(record);
        let write_txn = db.begin_write()?;
        {
            let mut table = write_txn.open_table(table_def)?;
            table.insert(
                entry.quote_id.as_bytes().as_slice(),
                encode_melt_commitment_entry(&entry, &keys)?,
            )?;
        }
        write_txn.commit()?;
        Ok(())
    }

    fn load_melt_commitment_sync(
        db: Arc<Database>,
        table_def: TableDefinition<'static, &'static [u8], Vec<u8>>,
        keys: bitcoin::secp256k1::Keypair,
        quote_id: Uuid,
    ) -> Result<crate::MeltCommitmentRecord> {
        let read_txn = db.begin_read()?;
        match read_txn.open_table(table_def) {
            Ok(table) => {
                let entry = table.get(quote_id.as_bytes().as_slice())?;
                match entry {
                    Some(e) => convert_melt_commitment_to(decode_melt_commitment_entry(
                        e.value().as_slice(),
                        &keys,
                    )?),
                    None => Err(Error::MeltCommitmentNotFound(quote_id.to_string())),
                }
            }
            Err(TableError::TableDoesNotExist(_)) => {
                Err(Error::MeltCommitmentNotFound(quote_id.to_string()))
            }
            Err(e) => Err(e.into()),
        }
    }

    fn delete_melt_commitment_sync(
        db: Arc<Database>,
        table_def: TableDefinition<'static, &'static [u8], Vec<u8>>,
        quote_id: Uuid,
    ) -> Result<()> {
        let write_txn = db.begin_write()?;
        {
            let mut table = write_txn.open_table(table_def)?;
            table.remove(quote_id.as_bytes().as_slice())?;
        }
        write_txn.commit()?;
        Ok(())
    }

    fn list_melt_commitments_sync(
        db: Arc<Database>,
        table_def: TableDefinition<'static, &'static [u8], Vec<u8>>,
        keys: bitcoin::secp256k1::Keypair,
    ) -> Result<Vec<crate::MeltCommitmentRecord>> {
        let read_txn = db.begin_read()?;
        match read_txn.open_table(table_def) {
            Ok(table) => {
                let mut res = Vec::new();
                for (_, v) in table.range::<&[u8]>(..)?.flatten() {
                    res.push(convert_melt_commitment_to(decode_melt_commitment_entry(
                        v.value().as_slice(),
                        &keys,
                    )?)?);
                }
                Ok(res)
            }
            Err(TableError::TableDoesNotExist(_)) => Ok(vec![]),
            Err(e) => Err(e.into()),
        }
    }

    fn delete_repo(
        db: Arc<Database>,
        mint_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        melt_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        melt_commitment_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
    ) -> Result<()> {
        let write_txn = db.begin_write()?;

        {
            if write_txn.open_table(mint_table).is_ok() {
                write_txn.delete_table(mint_table)?;
            }

            if write_txn.open_table(melt_table).is_ok() {
                write_txn.delete_table(melt_table)?;
            }

            if write_txn.open_table(melt_commitment_table).is_ok() {
                write_txn.delete_table(melt_commitment_table)?;
            }
        }

        write_txn.commit()?;
        Ok(())
    }
}

#[async_trait]
impl MintMeltRepository for MintMeltDB {
    // melt
    async fn store_melt(
        &self,
        qid: String,
        premints: Option<cdk00::PreMintSecrets>,
    ) -> Result<String> {
        let db_clone = self.db.clone();
        let table = self.melt_table;
        let keys = self.keys;
        let entry = convert_melt_entry_from(qid, premints);
        spawn_blocking(move || Self::store_melt_sync(db_clone, table, keys, entry)).await?
    }

    async fn load_melt(&self, qid: String) -> Result<cdk00::PreMintSecrets> {
        let db_clone = self.db.clone();
        let table = self.melt_table;
        let keys = self.keys;
        let id_clone = qid.clone();
        let res =
            spawn_blocking(move || Self::load_melt_sync(db_clone, table, keys, id_clone)).await??;
        let entry = res.ok_or(Error::MeltNotFound(qid.clone()))?;
        let (qid, premints) = convert_melt_entry_to(entry);
        premints.ok_or(Error::MeltNotFound(qid))
    }

    async fn list_melts(&self) -> Result<Vec<String>> {
        let db_clone = self.db.clone();
        let table = self.melt_table;
        let keys = self.keys;
        spawn_blocking(move || Self::list_melts_sync(db_clone, table, keys)).await?
    }

    async fn delete_melt(&self, qid: String) -> Result<()> {
        let db_clone = self.db.clone();
        let table = self.melt_table;
        spawn_blocking(move || Self::delete_melt_sync(db_clone, table, qid)).await??;
        Ok(())
    }
    // mint
    async fn store_mint(
        &self,
        quote_id: Uuid,
        amount: bitcoin::Amount,
        address: bitcoin::Address<NetworkUnchecked>,
        expiry: u64,
        premints: cdk00::PreMintSecrets,
        content: String,
        commitment: bitcoin::secp256k1::schnorr::Signature,
        ephemeral_secret: bitcoin::secp256k1::SecretKey,
    ) -> Result<Uuid> {
        let db_clone = self.db.clone();
        let table = self.mint_table;
        let keys = self.keys;
        let entry = convert_mint_entry_from(
            quote_id,
            amount,
            address,
            expiry,
            premints,
            content,
            commitment,
            ephemeral_secret,
        );
        spawn_blocking(move || Self::store_mint_sync(db_clone, table, keys, entry)).await?
    }

    async fn load_mint(&self, qid: Uuid) -> Result<crate::MintRecord> {
        let db_clone = self.db.clone();
        let table = self.mint_table;
        let keys = self.keys;
        let res =
            spawn_blocking(move || Self::load_mint_sync(db_clone, table, keys, qid)).await??;
        let entry = res.ok_or(Error::MintNotFound(qid.clone().to_string()))?;
        convert_mint_entry_to(entry)
    }

    async fn list_mints(&self) -> Result<Vec<Uuid>> {
        let db_clone = self.db.clone();
        let table = self.mint_table;
        let keys = self.keys;
        spawn_blocking(move || Self::list_mints_sync(db_clone, table, keys)).await?
    }

    async fn delete_mint(&self, qid: Uuid) -> Result<()> {
        let db_clone = self.db.clone();
        let table = self.mint_table;
        spawn_blocking(move || Self::delete_mint_sync(db_clone, table, qid)).await??;
        Ok(())
    }
    // melt commitment
    async fn store_melt_commitment(&self, record: crate::MeltCommitmentRecord) -> Result<()> {
        let db_clone = self.db.clone();
        let table = self.melt_commitment_table;
        let keys = self.keys;
        spawn_blocking(move || Self::store_melt_commitment_sync(db_clone, table, keys, record))
            .await?
    }

    async fn load_melt_commitment(&self, quote_id: Uuid) -> Result<crate::MeltCommitmentRecord> {
        let db_clone = self.db.clone();
        let table = self.melt_commitment_table;
        let keys = self.keys;
        spawn_blocking(move || Self::load_melt_commitment_sync(db_clone, table, keys, quote_id))
            .await?
    }

    async fn delete_melt_commitment(&self, quote_id: Uuid) -> Result<()> {
        let db_clone = self.db.clone();
        let table = self.melt_commitment_table;
        spawn_blocking(move || Self::delete_melt_commitment_sync(db_clone, table, quote_id))
            .await??;
        Ok(())
    }

    async fn list_melt_commitments(&self) -> Result<Vec<crate::MeltCommitmentRecord>> {
        let db_clone = self.db.clone();
        let table = self.melt_commitment_table;
        let keys = self.keys;
        spawn_blocking(move || Self::list_melt_commitments_sync(db_clone, table, keys)).await?
    }

    async fn delete_repo(&self) -> Result<()> {
        let db_clone = self.db.clone();
        let mint_table = self.mint_table;
        let melt_table = self.melt_table;
        let melt_commitment_table = self.melt_commitment_table;
        spawn_blocking(move || {
            Self::delete_repo(db_clone, mint_table, melt_table, melt_commitment_table)
        })
        .await?
    }
}

#[cfg(test)]
mod tests {
    use crate::error::Error;
    use crate::test_utils::tests::{valid_payment_address_testnet, wallet_id};

    use super::*;
    use bcr_common::{
        cashu::{Amount, amount::SplitTarget},
        core_tests,
    };
    use redb::{Builder, backends::InMemoryBackend};

    fn dummy_commitment() -> (String, bitcoin::secp256k1::schnorr::Signature) {
        let content = "dGVzdA==".to_string(); // base64 "test"
        let sig = bitcoin::secp256k1::schnorr::Signature::from_slice(&[0xab; 64])
            .expect("valid signature bytes");
        (content, sig)
    }

    fn dummy_ephemeral_secret() -> bitcoin::secp256k1::SecretKey {
        bitcoin::secp256k1::SecretKey::from_slice(&[1u8; 32]).expect("valid secret")
    }

    fn test_keys() -> bitcoin::secp256k1::Keypair {
        bitcoin::secp256k1::Keypair::new_global(&mut bitcoin::secp256k1::rand::thread_rng())
    }

    fn get_db(wallet_id: &str, unit: CurrencyUnit) -> MintMeltDB {
        let in_mem = InMemoryBackend::new();
        let db = Arc::new(
            Builder::new()
                .create_with_backend(in_mem)
                .expect("can create in-memory redb"),
        );
        MintMeltDB::new(db, wallet_id, &unit, test_keys()).expect("can create MintMeltDB")
    }

    // melt

    #[tokio::test]
    async fn test_list_melts_empty() {
        let repo = get_db(&wallet_id(), CurrencyUnit::Sat);
        let melts = repo.list_melts().await.expect("list_melts works");
        assert!(melts.is_empty());
    }

    #[tokio::test]
    async fn test_load_melt_missing_returns_error() {
        let repo = get_db(&wallet_id(), CurrencyUnit::Sat);
        let qid = "missing-qid".to_string();
        let err = repo.load_melt(qid.clone()).await.unwrap_err();

        match err {
            Error::MeltNotFound(id) => assert_eq!(id, qid),
            other => panic!("expected Error::MeltNotFound, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_store_list_delete_melt() {
        let repo = get_db(&wallet_id(), CurrencyUnit::Sat);
        let qid = "qid-1".to_string();

        // create premint
        let amounts = [Amount::from(8u64)];
        let (_, mintkeyset) = core_tests::generate_random_ecash_keyset();
        let keyset = bcr_wallet_core::util::to_keyset(&mintkeyset, None);
        let premint = cdk00::PreMintSecrets::random(
            keyset.id.into(),
            amounts[0],
            &SplitTarget::None,
            &bcr_wallet_core::util::to_fee_and_amounts(&keyset),
        )
        .unwrap();

        let stored_id = repo
            .store_melt(qid.clone(), Some(premint.clone()))
            .await
            .expect("store_melt works");
        assert_eq!(stored_id, qid);

        let melts = repo.list_melts().await.expect("list_melts works");
        assert_eq!(melts, vec![qid.clone()]);

        let melt = repo.load_melt(qid.clone()).await.expect("load_melt works");
        assert_eq!(melt, premint);

        repo.delete_melt(qid.clone())
            .await
            .expect("delete_melt works");

        let melts = repo.list_melts().await.expect("list_melts works");
        assert!(melts.is_empty());

        // load should now error again
        let err = repo.load_melt(qid.clone()).await.unwrap_err();
        match err {
            Error::MeltNotFound(id) => assert_eq!(id, qid),
            other => panic!("expected Error::MeltNotFound, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_store_melt_with_none_premints_load_returns_not_found() {
        let repo = get_db(&wallet_id(), CurrencyUnit::Sat);
        let qid = "qid-none-premints".to_string();

        repo.store_melt(qid.clone(), None)
            .await
            .expect("store_melt works");

        let err = repo.load_melt(qid.clone()).await.unwrap_err();
        match err {
            Error::MeltNotFound(id) => assert_eq!(id, qid),
            other => panic!("expected Error::MeltNotFound, got: {other:?}"),
        }
    }

    // mint

    #[tokio::test]
    async fn test_list_mints_empty() {
        let repo = get_db(&wallet_id(), CurrencyUnit::Sat);
        let mints = repo.list_mints().await.expect("list_mints works");
        assert!(mints.is_empty());
    }

    #[tokio::test]
    async fn test_load_mint_missing_returns_error() {
        let repo = get_db(&wallet_id(), CurrencyUnit::Sat);
        let qid = Uuid::new_v4();
        let err = repo.load_mint(qid).await.unwrap_err();

        match err {
            Error::MintNotFound(id) => assert_eq!(id, qid.to_string()),
            other => panic!("expected Error::MintNotFound, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_store_load_mint() {
        let repo = get_db(&wallet_id(), CurrencyUnit::Sat);

        let qid = Uuid::new_v4();
        let amount = bitcoin::Amount::from_sat(12345);
        let address = valid_payment_address_testnet();
        let expiry = time::OffsetDateTime::now_utc().unix_timestamp() as u64;
        let (content, commitment) = dummy_commitment();

        let (_, mintkeyset) = core_tests::generate_random_ecash_keyset();
        let keyset = bcr_wallet_core::util::to_keyset(&mintkeyset, None);
        let premint = cdk00::PreMintSecrets::random(
            keyset.id.into(),
            Amount::from(12345u64),
            &SplitTarget::None,
            &bcr_wallet_core::util::to_fee_and_amounts(&keyset),
        )
        .unwrap();

        let stored = repo
            .store_mint(
                qid,
                amount,
                address.clone(),
                expiry,
                premint.clone(),
                content.clone(),
                commitment,
                dummy_ephemeral_secret(),
            )
            .await
            .expect("store_mint works");
        assert_eq!(stored, qid);

        let record = repo.load_mint(qid).await.expect("load_mint works");
        assert_eq!(record.summary.quote_id, qid);
        assert_eq!(record.summary.amount, amount);
        assert_eq!(record.summary.address, address);
        assert_eq!(record.summary.expiry, expiry);
        assert_eq!(record.premint, premint);
        assert_eq!(record.content, content);
        assert_eq!(record.commitment, commitment);
    }

    #[tokio::test]
    async fn test_list_mints_after_inserts() {
        let repo = get_db(&wallet_id(), CurrencyUnit::Sat);

        let q1 = Uuid::new_v4();
        let q2 = Uuid::new_v4();
        let (content, commitment) = dummy_commitment();

        let (_, mintkeyset) = core_tests::generate_random_ecash_keyset();
        let keyset = bcr_wallet_core::util::to_keyset(&mintkeyset, None);
        let premint1 = cdk00::PreMintSecrets::random(
            keyset.id.into(),
            Amount::from(1u64),
            &SplitTarget::None,
            &bcr_wallet_core::util::to_fee_and_amounts(&keyset),
        )
        .unwrap();
        let premint2 = cdk00::PreMintSecrets::random(
            keyset.id.into(),
            Amount::from(2u64),
            &SplitTarget::None,
            &bcr_wallet_core::util::to_fee_and_amounts(&keyset),
        )
        .unwrap();

        repo.store_mint(
            q1,
            bitcoin::Amount::from_sat(1),
            valid_payment_address_testnet(),
            111,
            premint1,
            content.clone(),
            commitment,
            dummy_ephemeral_secret(),
        )
        .await
        .expect("store_mint q1");
        repo.store_mint(
            q2,
            bitcoin::Amount::from_sat(2),
            valid_payment_address_testnet(),
            222,
            premint2,
            content.clone(),
            commitment,
            dummy_ephemeral_secret(),
        )
        .await
        .expect("store_mint q2");

        let mut ids = repo.list_mints().await.expect("list_mints works");
        ids.sort();

        let mut expected = vec![q1, q2];
        expected.sort();

        assert_eq!(ids, expected);
    }

    #[tokio::test]
    async fn test_delete_mint_removes_entry() {
        let repo = get_db(&wallet_id(), CurrencyUnit::Sat);

        let qid = Uuid::new_v4();
        let (content, commitment) = dummy_commitment();
        let (_, mintkeyset) = core_tests::generate_random_ecash_keyset();
        let keyset = bcr_wallet_core::util::to_keyset(&mintkeyset, None);
        let premint = cdk00::PreMintSecrets::random(
            keyset.id.into(),
            Amount::from(42u64),
            &SplitTarget::None,
            &bcr_wallet_core::util::to_fee_and_amounts(&keyset),
        )
        .unwrap();

        repo.store_mint(
            qid,
            bitcoin::Amount::from_sat(42),
            valid_payment_address_testnet(),
            999,
            premint,
            content,
            commitment,
            dummy_ephemeral_secret(),
        )
        .await
        .expect("store_mint works");

        repo.delete_mint(qid).await.expect("delete_mint works");

        let err = repo.load_mint(qid).await.unwrap_err();
        match err {
            Error::MintNotFound(id) => assert_eq!(id, qid.to_string()),
            other => panic!("expected Error::MintNotFound, got: {other:?}"),
        }
    }

    // melt commitment

    fn sample_melt_commitment_record(quote_id: Uuid, expiry: u64) -> crate::MeltCommitmentRecord {
        let (content, sig) = dummy_commitment();
        crate::MeltCommitmentRecord {
            quote_id,
            expiry,
            commitment: sig,
            ephemeral_secret: dummy_ephemeral_secret(),
            body_content: content,
        }
    }

    #[tokio::test]
    async fn test_list_melt_commitments_empty() {
        let repo = get_db(&wallet_id(), CurrencyUnit::Sat);
        let items = repo
            .list_melt_commitments()
            .await
            .expect("list_melt_commitments works");
        assert!(items.is_empty());
    }

    #[tokio::test]
    async fn test_load_melt_commitment_missing_returns_error() {
        let repo = get_db(&wallet_id(), CurrencyUnit::Sat);
        let qid = Uuid::new_v4();
        let err = repo.load_melt_commitment(qid).await.unwrap_err();
        match err {
            Error::MeltCommitmentNotFound(id) => assert_eq!(id, qid.to_string()),
            other => panic!("expected Error::MeltCommitmentNotFound, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_store_load_list_delete_melt_commitment() {
        let repo = get_db(&wallet_id(), CurrencyUnit::Sat);
        let qid = Uuid::new_v4();
        let record = sample_melt_commitment_record(qid, 1_234_567);

        repo.store_melt_commitment(record.clone())
            .await
            .expect("store_melt_commitment works");

        let loaded = repo
            .load_melt_commitment(qid)
            .await
            .expect("load_melt_commitment works");
        assert_eq!(loaded.quote_id, qid);
        assert_eq!(loaded.expiry, 1_234_567);
        assert_eq!(loaded.commitment, record.commitment);
        assert_eq!(
            loaded.ephemeral_secret.secret_bytes(),
            record.ephemeral_secret.secret_bytes()
        );
        assert_eq!(loaded.body_content, record.body_content);

        let items = repo
            .list_melt_commitments()
            .await
            .expect("list_melt_commitments works");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].quote_id, qid);

        repo.delete_melt_commitment(qid)
            .await
            .expect("delete_melt_commitment works");

        let err = repo.load_melt_commitment(qid).await.unwrap_err();
        match err {
            Error::MeltCommitmentNotFound(id) => assert_eq!(id, qid.to_string()),
            other => panic!("expected Error::MeltCommitmentNotFound, got: {other:?}"),
        }
        let items = repo
            .list_melt_commitments()
            .await
            .expect("list_melt_commitments works");
        assert!(items.is_empty());
    }
}
