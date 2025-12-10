use crate::{
    TStamp,
    config::Settings,
    error::{Error, Result},
    persistence::Commitment,
    pocket::{PocketRepository, debit::MintMeltRepository},
    purse::PurseRepository,
    types::WalletConfig,
    wallet::TransactionRepository,
};
use async_trait::async_trait;
use cashu::{
    Amount, CurrencyUnit, MintUrl, nut00 as cdk00, nut01 as cdk01, nut02 as cdk02, nut07 as cdk07,
    nut12 as cdk12, secret::Secret,
};
use cdk::wallet::types::{Transaction, TransactionDirection, TransactionId};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition, TableError};
use std::{collections::HashMap, str::FromStr, sync::Arc};
use tokio::task::spawn_blocking;

///////////////////////////////////////////// ProofEntry
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
struct ProofEntry {
    y: cdk01::PublicKey,
    amount: cashu::Amount,
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
        }
    }
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
}

impl PocketDB {
    const PROOF_BASE_DB_NAME: &'static str = "proofs";
    const COUNTER_BASE_DB_NAME: &'static str = "counters";
    const COMMITMENT_BASE_DB_NAME: &'static str = "commitments";

    pub fn new(db: Arc<Database>, unit: &CurrencyUnit) -> Result<Self> {
        // Leak once to get static string, because of dynamically generated table names
        let proof_name: &'static str =
            Box::leak(format!("{unit}_{}", Self::PROOF_BASE_DB_NAME).into_boxed_str());
        let counter_name: &'static str =
            Box::leak(format!("{unit}_{}", Self::COUNTER_BASE_DB_NAME).into_boxed_str());
        let commitment_name: &'static str =
            Box::leak(format!("{unit}_{}", Self::COMMITMENT_BASE_DB_NAME).into_boxed_str());

        let proof_table = TableDefinition::new(proof_name);
        let counter_table = TableDefinition::new(counter_name);
        let commitment_table = TableDefinition::new(commitment_name);
        Ok(Self {
            db,
            proof_table,
            counter_table,
            commitment_table,
        })
    }

    fn store_new_sync(
        db: Arc<Database>,
        proof_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        proof: cdk00::Proof,
    ) -> Result<cdk01::PublicKey> {
        let entry = ProofEntry::from(proof);
        let y = entry.y;

        let write_txn = db.begin_write()?;

        {
            let mut table = write_txn.open_table(proof_table)?;

            let mut serialized = Vec::new();
            ciborium::into_writer(&entry, &mut serialized)?;
            table.insert(y.to_bytes().as_slice(), serialized)?;
        }

        write_txn.commit()?;
        Ok(y)
    }

    fn store_pendingspent_sync(
        db: Arc<Database>,
        proof_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        proof: cdk00::Proof,
    ) -> Result<cdk01::PublicKey> {
        let mut entry = ProofEntry::from(proof);
        entry.state = cdk07::State::PendingSpent;
        let y = entry.y;

        let write_txn = db.begin_write()?;

        {
            let mut table = write_txn.open_table(proof_table)?;

            let mut serialized = Vec::new();
            ciborium::into_writer(&entry, &mut serialized)?;
            table.insert(y.to_bytes().as_slice(), serialized)?;
        }

        write_txn.commit()?;
        Ok(y)
    }

    fn load_proof_sync(
        db: Arc<Database>,
        proof_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        y: cdk01::PublicKey,
    ) -> Result<Option<ProofEntry>> {
        let read_txn = db.begin_read()?;

        match read_txn.open_table(proof_table) {
            Ok(table) => {
                let entry = table.get(y.to_bytes().as_slice())?;
                match entry {
                    Some(e) => {
                        let proof: ProofEntry = ciborium::from_reader(e.value().as_slice())?;
                        Ok(Some(proof))
                    }
                    None => Ok(None),
                }
            }
            Err(TableError::TableDoesNotExist(_)) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn delete_proof_sync(
        db: Arc<Database>,
        proof_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        y: cdk01::PublicKey,
    ) -> Result<Option<ProofEntry>> {
        let write_txn = db.begin_write()?;

        let old = {
            let mut table = write_txn.open_table(proof_table)?;
            match table.remove(y.to_bytes().as_slice())? {
                Some(old) => {
                    let proof: ProofEntry = ciborium::from_reader(old.value().as_slice())?;
                    Some(proof)
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
                for (_, v) in table.range::<&[u8]>(..)?.flatten() {
                    let proof: ProofEntry = ciborium::from_reader(v.value().as_slice())?;
                    res.push(proof.y);
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
        state: Option<cdk07::State>,
    ) -> Result<Vec<ProofEntry>> {
        let read_txn = db.begin_read()?;

        match read_txn.open_table(proof_table) {
            Ok(table) => {
                let mut res = Vec::new();
                for (_, v) in table.range::<&[u8]>(..)?.flatten() {
                    let proof: ProofEntry = ciborium::from_reader(v.value().as_slice())?;
                    if let Some(s) = state {
                        if s == proof.state {
                            res.push(proof);
                        }
                    } else {
                        res.push(proof)
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
        y: cdk01::PublicKey,
        old_state_set: &[cdk07::State],
        new_state: cdk07::State,
    ) -> Result<ProofEntry> {
        let write_txn = db.begin_write()?;
        let new_value = {
            let mut table = write_txn.open_table(proof_table)?;
            let old_value = table.get(y.to_bytes().as_slice())?.map(|v| v.value());

            if let Some(old_value) = old_value {
                let mut proof: ProofEntry = ciborium::from_reader(old_value.as_slice())?;

                if !old_state_set.contains(&proof.state) {
                    return Err(Error::InvalidProofState(y));
                }

                proof.state = new_state;

                let mut serialized = Vec::new();
                ciborium::into_writer(&proof, &mut serialized)?;
                table.insert(y.to_bytes().as_slice(), serialized)?;
                proof
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
        inputs: Vec<cashu::PublicKey>,
        outputs: Vec<cashu::BlindedMessage>,
        expiration: TStamp,
        commitment: secp256k1::schnorr::Signature,
    ) -> Result<()> {
        let entry = Commitment {
            inputs,
            outputs,
            expiration,
            commitment,
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
}

#[async_trait]
impl PocketRepository for PocketDB {
    async fn store_new(&self, proof: cdk00::Proof) -> Result<cdk01::PublicKey> {
        let db_clone = self.db.clone();
        let table = self.proof_table;
        spawn_blocking(move || Self::store_new_sync(db_clone, table, proof)).await?
    }

    async fn store_pendingspent(&self, proof: cdk00::Proof) -> Result<cdk01::PublicKey> {
        let db_clone = self.db.clone();
        let table = self.proof_table;
        spawn_blocking(move || Self::store_pendingspent_sync(db_clone, table, proof)).await?
    }

    async fn load_proof(&self, y: cdk01::PublicKey) -> Result<(cdk00::Proof, cdk07::State)> {
        let db_clone = self.db.clone();
        let table = self.proof_table;
        let res = spawn_blocking(move || Self::load_proof_sync(db_clone, table, y)).await??;
        let proof = res.ok_or(Error::ProofNotFound(y))?;
        let state = proof.state;
        Ok((proof.into(), state))
    }

    async fn delete_proof(&self, y: cdk01::PublicKey) -> Result<Option<cdk00::Proof>> {
        let db_clone = self.db.clone();
        let table = self.proof_table;
        let proof = spawn_blocking(move || Self::delete_proof_sync(db_clone, table, y)).await??;
        Ok(proof.map(|p| p.into()))
    }

    async fn list_unspent(&self) -> Result<HashMap<cdk01::PublicKey, cdk00::Proof>> {
        let db_clone = self.db.clone();
        let table = self.proof_table;
        let list =
            spawn_blocking(move || Self::list_sync(db_clone, table, Some(cdk07::State::Unspent)))
                .await??;
        Ok(list
            .into_iter()
            .map(|entry| (entry.y, cdk00::Proof::from(entry)))
            .collect())
    }

    async fn list_pending(&self) -> Result<HashMap<cdk01::PublicKey, cdk00::Proof>> {
        let db_clone = self.db.clone();
        let db_clone_two = self.db.clone();
        let table = self.proof_table;
        let pending: HashMap<cdk01::PublicKey, cdk00::Proof> =
            spawn_blocking(move || Self::list_sync(db_clone, table, Some(cdk07::State::Pending)))
                .await??
                .into_iter()
                .map(|entry| (entry.y, cdk00::Proof::from(entry)))
                .collect();
        let mut pending_spent: HashMap<cdk01::PublicKey, cdk00::Proof> =
            spawn_blocking(move || {
                Self::list_sync(db_clone_two, table, Some(cdk07::State::PendingSpent))
            })
            .await??
            .into_iter()
            .map(|entry| (entry.y, cdk00::Proof::from(entry)))
            .collect();

        pending_spent.extend(pending);
        Ok(pending_spent)
    }

    async fn list_reserved(&self) -> Result<HashMap<cdk01::PublicKey, cdk00::Proof>> {
        let db_clone = self.db.clone();
        let table = self.proof_table;
        let list =
            spawn_blocking(move || Self::list_sync(db_clone, table, Some(cdk07::State::Reserved)))
                .await??;
        Ok(list
            .into_iter()
            .map(|entry| (entry.y, cdk00::Proof::from(entry)))
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
        let proof = spawn_blocking(move || {
            Self::update_entry_state_sync(
                db_clone,
                table,
                y,
                &[cdk07::State::Unspent],
                cdk07::State::PendingSpent,
            )
        })
        .await??;
        Ok(proof.into())
    }

    async fn counter(&self, kid: cashu::Id) -> Result<u32> {
        let db_clone = self.db.clone();
        let table = self.counter_table;
        let counter =
            spawn_blocking(move || Self::load_counter_sync(db_clone, table, kid)).await??;
        Ok(counter.counter)
    }

    async fn increment_counter(&self, kid: cashu::Id, old: u32, increment: u32) -> Result<()> {
        let db_clone = self.db.clone();
        let table = self.counter_table;
        let old = CounterEntry { kid, counter: old };
        let new = CounterEntry {
            kid,
            counter: old.counter + increment,
        };
        spawn_blocking(move || Self::increment_counter_sync(db_clone, table, old, new)).await?
    }

    async fn store_commitment(
        &self,
        inputs: Vec<cashu::PublicKey>,
        outputs: Vec<cashu::BlindedMessage>,
        expiration: TStamp,
        commitment: secp256k1::schnorr::Signature,
    ) -> Result<()> {
        let db_clone = self.db.clone();
        let table = self.commitment_table;
        spawn_blocking(move || {
            Self::store_commitment_sync(db_clone, table, inputs, outputs, expiration, commitment)
        })
        .await?
    }
}

///////////////////////////////////////////// TransactionEntry
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
struct TransactionEntry {
    pub tx_id: String,
    pub mint_url: MintUrl,
    pub direction: TransactionDirection,
    pub amount: cashu::Amount,
    pub fee: cashu::Amount,
    pub unit: CurrencyUnit,
    pub ys: Vec<cdk01::PublicKey>,
    pub timestamp: u64,
    pub memo: Option<String>,
    pub metadata: HashMap<String, String>,
    pub quote_id: Option<String>,
}

impl std::convert::From<Transaction> for TransactionEntry {
    fn from(tx: Transaction) -> Self {
        let tx_id = TransactionId::new(tx.ys.clone());
        TransactionEntry {
            tx_id: tx_id.to_string(),
            mint_url: tx.mint_url,
            direction: tx.direction,
            amount: tx.amount,
            fee: tx.fee,
            unit: tx.unit,
            ys: tx.ys,
            timestamp: tx.timestamp,
            memo: tx.memo,
            metadata: tx.metadata,
            quote_id: tx.quote_id,
        }
    }
}
impl std::convert::From<TransactionEntry> for Transaction {
    fn from(entry: TransactionEntry) -> Self {
        Transaction {
            mint_url: entry.mint_url,
            direction: entry.direction,
            amount: entry.amount,
            fee: entry.fee,
            unit: entry.unit,
            ys: entry.ys,
            timestamp: entry.timestamp,
            memo: entry.memo,
            metadata: entry.metadata,
            quote_id: entry.quote_id,
        }
    }
}

///////////////////////////////////////////// TransactionDB
pub struct TransactionDB {
    db: Arc<Database>,
    transaction_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
}

impl TransactionDB {
    const TRANSACTION_BASE_DB_NAME: &'static str = "transactions";

    pub fn new(db: Arc<Database>, wallet_id: &str) -> Result<Self> {
        // Leak once to get static string, because of dynamically generated table names
        let transaction_name: &'static str =
            Box::leak(format!("{wallet_id}_{}", Self::TRANSACTION_BASE_DB_NAME).into_boxed_str());
        let transaction_table = TableDefinition::new(transaction_name);
        Ok(Self {
            db,
            transaction_table,
        })
    }

    fn store_tx_sync(
        db: Arc<Database>,
        tx_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        tx: Transaction,
    ) -> Result<TransactionId> {
        let id = tx.id();
        let entry: TransactionEntry = tx.into();
        let write_txn = db.begin_write()?;

        {
            let mut table = write_txn.open_table(tx_table)?;

            let mut serialized = Vec::new();
            ciborium::into_writer(&entry, &mut serialized)?;
            table.insert(id.as_bytes().as_slice(), serialized)?;
        }

        write_txn.commit()?;
        Ok(id)
    }

    fn load_tx_sync(
        db: Arc<Database>,
        tx_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        tx_id: TransactionId,
    ) -> Result<Option<TransactionEntry>> {
        let read_txn = db.begin_read()?;

        match read_txn.open_table(tx_table) {
            Ok(table) => {
                let entry = table.get(tx_id.as_bytes().as_slice())?;
                match entry {
                    Some(e) => {
                        let tx: TransactionEntry = ciborium::from_reader(e.value().as_slice())?;
                        Ok(Some(tx))
                    }
                    None => Ok(None),
                }
            }
            Err(TableError::TableDoesNotExist(_)) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn delete_tx_sync(
        db: Arc<Database>,
        tx_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        tx_id: TransactionId,
    ) -> Result<()> {
        let write_txn = db.begin_write()?;

        {
            let mut table = write_txn.open_table(tx_table)?;
            table.remove(tx_id.as_bytes().as_slice())?;
        }

        write_txn.commit()?;
        Ok(())
    }

    fn list_tx_ids_sync(
        db: Arc<Database>,
        tx_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
    ) -> Result<Vec<TransactionId>> {
        let read_txn = db.begin_read()?;

        match read_txn.open_table(tx_table) {
            Ok(table) => {
                let mut res = Vec::new();
                for (_, v) in table.range::<&[u8]>(..)?.flatten() {
                    let tx: TransactionEntry = ciborium::from_reader(v.value().as_slice())?;
                    res.push(TransactionId::from_str(&tx.tx_id)?);
                }
                Ok(res)
            }
            Err(TableError::TableDoesNotExist(_)) => Ok(vec![]),
            Err(e) => Err(e.into()),
        }
    }

    fn update_meta_sync(
        db: Arc<Database>,
        tx_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        tx_id: TransactionId,
        k: String,
        v: String,
    ) -> Result<Option<String>> {
        let write_txn = db.begin_write()?;
        let old_v = {
            let mut table = write_txn.open_table(tx_table)?;
            let old_value = table.get(tx_id.as_bytes().as_slice())?.map(|v| v.value());

            if let Some(old_value) = old_value {
                let mut tx: TransactionEntry = ciborium::from_reader(old_value.as_slice())?;
                let old = tx.metadata.insert(k, v);

                let mut serialized = Vec::new();
                ciborium::into_writer(&tx, &mut serialized)?;
                table.insert(tx_id.as_bytes().as_slice(), serialized)?;
                old
            } else {
                None
            }
        };

        write_txn.commit()?;
        Ok(old_v)
    }
}

#[async_trait]
impl TransactionRepository for TransactionDB {
    async fn store_tx(&self, tx: Transaction) -> Result<TransactionId> {
        let db_clone = self.db.clone();
        let table = self.transaction_table;
        spawn_blocking(move || Self::store_tx_sync(db_clone, table, tx)).await?
    }

    async fn load_tx(&self, tx_id: TransactionId) -> Result<Transaction> {
        let db_clone = self.db.clone();
        let table = self.transaction_table;
        let res = spawn_blocking(move || Self::load_tx_sync(db_clone, table, tx_id)).await??;
        let entry = res.ok_or(Error::TransactionNotFound(tx_id))?;
        Ok(entry.into())
    }

    async fn delete_tx(&self, tx_id: TransactionId) -> Result<()> {
        let db_clone = self.db.clone();
        let table = self.transaction_table;
        spawn_blocking(move || Self::delete_tx_sync(db_clone, table, tx_id)).await??;
        Ok(())
    }

    async fn list_tx_ids(&self) -> Result<Vec<TransactionId>> {
        let db_clone = self.db.clone();
        let table = self.transaction_table;
        spawn_blocking(move || Self::list_tx_ids_sync(db_clone, table)).await?
    }

    async fn update_metadata(
        &self,
        tx_id: TransactionId,
        k: String,
        v: String,
    ) -> Result<Option<String>> {
        let db_clone = self.db.clone();
        let table = self.transaction_table;
        spawn_blocking(move || Self::update_meta_sync(db_clone, table, tx_id, k, v)).await?
    }
}

///////////////////////////////////////////// WalletEntry
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
struct WalletEntry {
    wallet_id: String,
    name: String,
    network: bitcoin::Network,
    mint: cashu::MintUrl,
    mnemonic: bip39::Mnemonic,
    debit: CurrencyUnit,
    credit: Option<CurrencyUnit>,
}
impl std::convert::From<WalletConfig> for WalletEntry {
    fn from(wallet: WalletConfig) -> Self {
        Self {
            wallet_id: wallet.wallet_id,
            name: wallet.name,
            network: wallet.network,
            mint: wallet.mint,
            mnemonic: wallet.mnemonic,
            debit: wallet.debit,
            credit: wallet.credit,
        }
    }
}
impl std::convert::From<WalletEntry> for WalletConfig {
    fn from(wallet: WalletEntry) -> Self {
        Self {
            wallet_id: wallet.wallet_id,
            name: wallet.name,
            network: wallet.network,
            mint: wallet.mint,
            mnemonic: wallet.mnemonic,
            debit: wallet.debit,
            credit: wallet.credit,
        }
    }
}

///////////////////////////////////////////// PurseDB
const WALLET_TABLE: TableDefinition<&[u8], Vec<u8>> = TableDefinition::new("wallets");
pub struct PurseDB {
    db: Arc<Database>,
}

impl PurseDB {
    pub fn new(db: Arc<Database>) -> Result<Self> {
        Ok(Self { db })
    }

    fn store_sync(db: Arc<Database>, wallet: WalletConfig) -> Result<()> {
        let id = wallet.wallet_id.clone();
        let entry: WalletEntry = wallet.into();
        let write_txn = db.begin_write()?;

        {
            let mut table = write_txn.open_table(WALLET_TABLE)?;

            let mut serialized = Vec::new();
            ciborium::into_writer(&entry, &mut serialized)?;
            table.insert(id.as_bytes(), serialized)?;
        }

        write_txn.commit()?;
        Ok(())
    }

    fn load_sync(db: Arc<Database>, wallet_id: &str) -> Result<Option<WalletConfig>> {
        let read_txn = db.begin_read()?;

        match read_txn.open_table(WALLET_TABLE) {
            Ok(table) => {
                let entry = table.get(wallet_id.as_bytes())?;
                match entry {
                    Some(e) => {
                        let wallet: WalletEntry = ciborium::from_reader(e.value().as_slice())?;
                        Ok(Some(wallet.into()))
                    }
                    None => Ok(None),
                }
            }
            Err(TableError::TableDoesNotExist(_)) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    #[allow(unused)]
    fn delete_sync(db: Arc<Database>, wallet_id: &str) -> Result<()> {
        let write_txn = db.begin_write()?;

        {
            let mut table = write_txn.open_table(WALLET_TABLE)?;
            table.remove(wallet_id.as_bytes())?;
        }

        write_txn.commit()?;
        Ok(())
    }

    fn list_ids_sync(db: Arc<Database>) -> Result<Vec<String>> {
        let read_txn = db.begin_read()?;

        match read_txn.open_table(WALLET_TABLE) {
            Ok(table) => {
                let mut res = Vec::new();
                for (_, v) in table.range::<&[u8]>(..)?.flatten() {
                    let wallet: WalletEntry = ciborium::from_reader(v.value().as_slice())?;
                    res.push(wallet.wallet_id);
                }
                Ok(res)
            }
            Err(TableError::TableDoesNotExist(_)) => Ok(vec![]),
            Err(e) => Err(e.into()),
        }
    }
}

#[async_trait]
impl PurseRepository for PurseDB {
    async fn store(&self, wallet: WalletConfig) -> Result<()> {
        let db_clone = self.db.clone();
        spawn_blocking(move || Self::store_sync(db_clone, wallet)).await?
    }

    async fn load(&self, wallet_id: &str) -> Result<WalletConfig> {
        let db_clone = self.db.clone();
        let id = wallet_id.to_owned();
        let res = spawn_blocking(move || Self::load_sync(db_clone, &id)).await??;
        res.ok_or(Error::WalletIdNotFound(wallet_id.to_owned()))
    }

    async fn delete(&self, wallet_id: &str) -> Result<()> {
        let db_clone = self.db.clone();
        let id = wallet_id.to_owned();
        spawn_blocking(move || Self::delete_sync(db_clone, &id)).await??;
        Ok(())
    }

    async fn list_ids(&self) -> Result<Vec<String>> {
        let db_clone = self.db.clone();
        spawn_blocking(move || Self::list_ids_sync(db_clone)).await?
    }
}

///////////////////////////////////////////// MeltEntry
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Default)]
struct MeltEntry {
    quote_id: String,
    premints: Vec<(
        cdk00::BlindedMessage,
        cashu::secret::Secret,
        cdk01::SecretKey,
        Amount,
    )>,
    kid: Option<cashu::Id>,
}

fn convert_melt_entry_from(qid: String, premints: Option<cdk00::PreMintSecrets>) -> MeltEntry {
    let mut entry = MeltEntry {
        quote_id: qid,
        ..Default::default()
    };
    let Some(premints) = premints else {
        return entry;
    };
    entry.premints = Vec::with_capacity(premints.len());
    let cdk00::PreMintSecrets { secrets, keyset_id } = premints;
    entry.kid = Some(keyset_id);
    for premint in secrets {
        entry.premints.push((
            premint.blinded_message,
            premint.secret,
            premint.r,
            premint.amount,
        ));
    }
    entry
}

fn convert_melt_entry_to(entry: MeltEntry) -> (String, Option<cdk00::PreMintSecrets>) {
    let MeltEntry {
        quote_id,
        premints,
        kid,
    } = entry;
    if kid.is_none() {
        return (quote_id, None);
    }
    let keyset_id = kid.unwrap();
    let mut secrets: Vec<cdk00::PreMint> = Vec::with_capacity(premints.len());
    for premint in premints {
        let pre = cdk00::PreMint {
            blinded_message: premint.0,
            secret: premint.1,
            r: premint.2,
            amount: premint.3,
        };
        secrets.push(pre);
    }
    (quote_id, Some(cdk00::PreMintSecrets { secrets, keyset_id }))
}

///////////////////////////////////////////// MintMeltDB
pub struct MintMeltDB {
    db: Arc<Database>,
    melt_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
}

impl MintMeltDB {
    const MELT_BASE_DB_NAME: &'static str = "melts";

    pub fn new(db: Arc<Database>, unit: &CurrencyUnit) -> Result<Self> {
        // Leak once to get static string, because of dynamically generated table names
        let melt_name: &'static str =
            Box::leak(format!("{unit}_{}", Self::MELT_BASE_DB_NAME).into_boxed_str());
        let melt_table = TableDefinition::new(melt_name);
        Ok(MintMeltDB { db, melt_table })
    }

    fn store_melt_sync(
        db: Arc<Database>,
        melt_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        melt: MeltEntry,
    ) -> Result<String> {
        let id = melt.quote_id.clone();
        let write_txn = db.begin_write()?;

        {
            let mut table = write_txn.open_table(melt_table)?;

            let mut serialized = Vec::new();
            ciborium::into_writer(&melt, &mut serialized)?;
            table.insert(id.as_bytes(), serialized)?;
        }

        write_txn.commit()?;
        Ok(id)
    }

    fn load_melt_sync(
        db: Arc<Database>,
        melt_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        qid: String,
    ) -> Result<Option<MeltEntry>> {
        let read_txn = db.begin_read()?;

        match read_txn.open_table(melt_table) {
            Ok(table) => {
                let entry = table.get(qid.as_bytes())?;
                match entry {
                    Some(e) => {
                        let entry: MeltEntry = ciborium::from_reader(e.value().as_slice())?;
                        Ok(Some(entry))
                    }
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
    ) -> Result<Vec<String>> {
        let read_txn = db.begin_read()?;

        match read_txn.open_table(melt_table) {
            Ok(table) => {
                let mut res = Vec::new();
                for (_, v) in table.range::<&[u8]>(..)?.flatten() {
                    let entry: MeltEntry = ciborium::from_reader(v.value().as_slice())?;
                    res.push(entry.quote_id);
                }
                Ok(res)
            }
            Err(TableError::TableDoesNotExist(_)) => Ok(vec![]),
            Err(e) => Err(e.into()),
        }
    }
}

#[async_trait]
impl MintMeltRepository for MintMeltDB {
    async fn store_melt(
        &self,
        qid: String,
        premints: Option<cdk00::PreMintSecrets>,
    ) -> Result<String> {
        let db_clone = self.db.clone();
        let table = self.melt_table;
        let entry = convert_melt_entry_from(qid, premints);
        spawn_blocking(move || Self::store_melt_sync(db_clone, table, entry)).await?
    }

    async fn load_melt(&self, qid: String) -> Result<cdk00::PreMintSecrets> {
        let db_clone = self.db.clone();
        let table = self.melt_table;
        let id_clone = qid.clone();
        let res = spawn_blocking(move || Self::load_melt_sync(db_clone, table, id_clone)).await??;
        let entry = res.ok_or(Error::MeltNotFound(qid.clone()))?;
        let (qid, premints) = convert_melt_entry_to(entry);
        premints.ok_or(Error::MeltNotFound(qid))
    }

    async fn list_melts(&self) -> Result<Vec<String>> {
        let db_clone = self.db.clone();
        let table = self.melt_table;
        spawn_blocking(move || Self::list_melts_sync(db_clone, table)).await?
    }

    async fn delete_melt(&self, qid: String) -> Result<()> {
        let db_clone = self.db.clone();
        let table = self.melt_table;
        spawn_blocking(move || Self::delete_melt_sync(db_clone, table, qid)).await??;
        Ok(())
    }
}

///////////////////////////////////////////// SettingEntry
const SETTINGS_TABLE: TableDefinition<&[u8], Vec<u8>> = TableDefinition::new("settings");

///////////////////////////////////////////// SettingsDB
pub struct SettingsDB {
    db: Arc<Database>,
}

impl SettingsDB {
    const SETTINGS_MAIN_ID: &'static str = "main";

    pub fn new(db: Arc<Database>) -> Result<Self> {
        Ok(Self { db })
    }

    fn store_sync(&self, settings: Settings) -> Result<()> {
        let write_txn = self.db.begin_write()?;

        {
            let mut table = write_txn.open_table(SETTINGS_TABLE)?;

            let mut serialized = Vec::new();
            ciborium::into_writer(&settings, &mut serialized)?;
            table.insert(Self::SETTINGS_MAIN_ID.as_bytes(), serialized)?;
        }

        write_txn.commit()?;
        Ok(())
    }

    fn load_sync(&self) -> Result<Settings> {
        let read_txn = self.db.begin_read()?;

        match read_txn.open_table(SETTINGS_TABLE) {
            Ok(table) => {
                let entry = table.get(Self::SETTINGS_MAIN_ID.as_bytes())?;
                match entry {
                    Some(e) => {
                        let settings: Settings = ciborium::from_reader(e.value().as_slice())?;
                        Ok(settings)
                    }
                    None => Ok(Settings::default()),
                }
            }
            Err(TableError::TableDoesNotExist(_)) => Ok(Settings::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn store(self: Arc<Self>, settings: Settings) -> Result<()> {
        spawn_blocking(move || self.store_sync(settings)).await?
    }

    pub async fn load(self: Arc<Self>) -> Result<Settings> {
        spawn_blocking(move || self.load_sync()).await?
    }
}
