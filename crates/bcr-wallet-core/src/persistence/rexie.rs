// ----- standard library imports
use std::{collections::HashMap, rc::Rc, str::FromStr};
// ----- extra library imports
use anyhow::Error as AnyError;
use async_trait::async_trait;
use cashu::{
    CurrencyUnit, MintUrl, nut00 as cdk00, nut01 as cdk01, nut02 as cdk02, nut07 as cdk07,
    nut12 as cdk12, secret::Secret,
};
use cdk::wallet::types::{Transaction, TransactionDirection, TransactionId};
use rexie::{Rexie, TransactionMode};
use serde_wasm_bindgen::{from_value, to_value};
use wasm_bindgen::JsValue;
// ----- local imports
use crate::{
    error::{Error, Result},
    pocket::PocketRepository,
    purse::PurseRepository,
    types::WalletConfig,
    wallet,
};

// ----- end imports

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
    db: Rc<Rexie>,

    proof_store: String,
    counter_store: String,
}

impl PocketDB {
    const PROOF_BASE_DB_NAME: &'static str = "proofs";
    const PROOF_DB_KEY: &'static str = "y";
    const COUNTER_BASE_DB_NAME: &'static str = "counters";
    const COUNTER_DB_KEY: &'static str = "kid";

    fn proof_store_name(unit: &CurrencyUnit) -> String {
        format!("{unit}_{}", Self::PROOF_BASE_DB_NAME)
    }
    fn counter_store_name(unit: &CurrencyUnit) -> String {
        format!("{unit}_{}", Self::COUNTER_BASE_DB_NAME)
    }

    pub fn object_stores(unit: &CurrencyUnit) -> Vec<rexie::ObjectStore> {
        let proof_store_name = Self::proof_store_name(unit);
        let counter_store_name = Self::counter_store_name(unit);
        vec![
            rexie::ObjectStore::new(&proof_store_name)
                .auto_increment(false)
                .key_path(Self::PROOF_DB_KEY),
            rexie::ObjectStore::new(&counter_store_name)
                .auto_increment(false)
                .key_path(Self::COUNTER_DB_KEY),
        ]
    }

    pub fn new(db: Rc<Rexie>, unit: CurrencyUnit) -> Result<Self> {
        let proof_store = Self::proof_store_name(&unit);
        let counter_store = Self::counter_store_name(&unit);
        if !db.store_names().contains(&proof_store) {
            return Err(Error::BadPocketDB);
        }
        if !db.store_names().contains(&counter_store) {
            return Err(Error::BadPocketDB);
        }

        let db = PocketDB {
            db,
            proof_store,
            counter_store,
        };
        Ok(db)
    }

    async fn store_entry(&self, proof: ProofEntry) -> Result<cdk01::PublicKey> {
        let entry = to_value(&proof)?;
        let tx = self
            .db
            .transaction(&[self.proof_store.clone()], TransactionMode::ReadWrite)?;
        let proofs = tx.store(&self.proof_store)?;
        proofs.add(&entry, None).await?;
        tx.done().await?;
        Ok(proof.y)
    }

    async fn load_entry(&self, y: cdk01::PublicKey) -> Result<Option<ProofEntry>> {
        let tx = self
            .db
            .transaction(&[self.proof_store.clone()], TransactionMode::ReadOnly)?;
        let proofs = tx.store(&self.proof_store)?;
        let js_entry = proofs.get(y.to_string().into()).await?;
        tx.done().await?;
        let entry = js_entry.map(from_value::<ProofEntry>).transpose()?;
        Ok(entry)
    }

    async fn delete_entry(&self, y: cdk01::PublicKey) -> Result<()> {
        let tx = self
            .db
            .transaction(&[self.proof_store.clone()], TransactionMode::ReadWrite)?;
        let proofs = tx.store(&self.proof_store)?;
        proofs.delete(y.to_string().into()).await?;
        tx.done().await?;
        Ok(())
    }

    async fn update_entry_state(
        &self,
        y: cdk01::PublicKey,
        old_state_set: &[cdk07::State],
        new_state: cdk07::State,
    ) -> Result<ProofEntry> {
        let key = JsValue::from_str(&y.to_string());
        let tx = self
            .db
            .transaction(&[self.proof_store.clone()], TransactionMode::ReadWrite)?;
        let proofs = tx.store(&self.proof_store)?;
        let mut proof = proofs
            .get(key.clone())
            .await?
            .map(from_value::<ProofEntry>)
            .ok_or(Error::ProofNotFound(y))??;
        if !old_state_set.contains(&proof.state) {
            return Err(Error::InvalidProofState(y));
        }
        proof.state = new_state;
        let entry = to_value(&proof)?;
        proofs.put(&entry, None).await?;
        tx.done().await?;
        Ok(proof)
    }

    async fn list_entry_keys(&self) -> Result<Vec<cdk01::PublicKey>> {
        let tx = self
            .db
            .transaction(&[self.proof_store.clone()], TransactionMode::ReadOnly)?;
        let proof_repo = tx.store(&self.proof_store)?;
        let ys = proof_repo
            .get_all_keys(None, None)
            .await?
            .into_iter()
            .map(from_value::<cdk01::PublicKey>)
            .map(|r| r.map_err(Error::from))
            .collect::<Result<Vec<_>>>()?;
        tx.done().await?;
        Ok(ys)
    }

    async fn list_entries(&self, state: Option<cdk07::State>) -> Result<Vec<ProofEntry>> {
        let tx = self
            .db
            .transaction(&[self.proof_store.clone()], TransactionMode::ReadOnly)?;
        let proof_repo = tx.store(&self.proof_store)?;
        let proofs = proof_repo
            .get_all(None, None)
            .await?
            .into_iter()
            .map(from_value::<ProofEntry>)
            .map(|r| r.map_err(Error::from))
            .collect::<Result<Vec<_>>>()?;
        tx.done().await?;
        if let Some(state) = state {
            let filtered = proofs
                .into_iter()
                .filter(|proof| proof.state == state)
                .collect();
            return Ok(filtered);
        }
        Ok(proofs)
    }

    async fn counter(&self, kid: cdk02::Id) -> Result<CounterEntry> {
        let tx = self
            .db
            .transaction(&[self.counter_store.clone()], TransactionMode::ReadWrite)?;
        let counters_repo = tx.store(&self.counter_store)?;
        let response = counters_repo.get(kid.to_string().into()).await?;
        let entry = if let Some(entry) = response {
            from_value::<CounterEntry>(entry)?
        } else {
            let new_entry = CounterEntry { kid, counter: 0 };
            let entry = to_value(&new_entry)?;
            counters_repo.add(&entry, None).await?;
            new_entry
        };
        tx.done().await?;
        Ok(entry)
    }
    async fn update_counter(&self, old: CounterEntry, new: CounterEntry) -> Result<()> {
        if old.kid != new.kid {
            return Err(Error::Any(AnyError::msg(
                "rexie::increment_counter input kid mismatch",
            )));
        }
        let tx = self
            .db
            .transaction(&[self.counter_store.clone()], TransactionMode::ReadWrite)?;
        let counters_repo = tx.store(&self.counter_store)?;
        let response: Option<CounterEntry> = counters_repo
            .get(old.kid.to_string().into())
            .await?
            .map(from_value)
            .transpose()?;
        let Some(entry) = response else {
            return Err(Error::Any(AnyError::msg(
                "rexie::increment_counter entry for {kid} not found",
            )));
        };
        if entry.counter != old.counter {
            return Err(Error::Any(AnyError::msg(
                "rexie::increment_counter old counter mismatch",
            )));
        }
        let new_entry = to_value(&new)?;
        counters_repo.put(&new_entry, None).await?;
        tx.done().await?;
        Ok(())
    }
}

#[async_trait(?Send)]
impl PocketRepository for PocketDB {
    async fn store_new(&self, proof: cdk00::Proof) -> Result<cdk01::PublicKey> {
        let entry = ProofEntry::from(proof);
        let y = entry.y;
        self.store_entry(entry).await?;
        Ok(y)
    }

    async fn store_pendingspent(&self, proof: cdk00::Proof) -> Result<cdk01::PublicKey> {
        let mut entry = ProofEntry::from(proof);
        let y = entry.y;
        entry.state = cdk07::State::PendingSpent;
        self.store_entry(entry).await?;
        Ok(y)
    }

    async fn load_proof(&self, y: cdk01::PublicKey) -> Result<(cdk00::Proof, cdk07::State)> {
        let proof_state = self.load_entry(y).await?.map(|entry| {
            let state = entry.state;
            (cdk00::Proof::from(entry), state)
        });
        let proof_state = proof_state.ok_or(Error::ProofNotFound(y))?;
        Ok(proof_state)
    }

    async fn delete_proof(&self, y: cdk01::PublicKey) -> Result<()> {
        self.delete_entry(y).await
    }

    async fn list_all(&self) -> Result<Vec<cdk01::PublicKey>> {
        let ys = self.list_entry_keys().await?;
        Ok(ys)
    }

    async fn list_unspent(&self) -> Result<HashMap<cdk01::PublicKey, cdk00::Proof>> {
        self.list_entries(Some(cdk07::State::Unspent))
            .await
            .map(|proofs| {
                proofs
                    .into_iter()
                    .map(|entry| (entry.y, cdk00::Proof::from(entry)))
                    .collect()
            })
    }
    async fn list_pending(&self) -> Result<HashMap<cdk01::PublicKey, cdk00::Proof>> {
        let pendings = self
            .list_entries(Some(cdk07::State::Pending))
            .await
            .map(|proofs| {
                proofs
                    .into_iter()
                    .map(|entry| (entry.y, cdk00::Proof::from(entry)))
            })?;
        let mut pendingspents: HashMap<cdk01::PublicKey, cdk00::Proof> = self
            .list_entries(Some(cdk07::State::PendingSpent))
            .await
            .map(|proofs| {
                proofs
                    .into_iter()
                    .map(|entry| (entry.y, cdk00::Proof::from(entry)))
                    .collect()
            })?;
        pendingspents.extend(pendings);
        Ok(pendingspents)
    }

    async fn list_reserved(&self) -> Result<HashMap<cdk01::PublicKey, cdk00::Proof>> {
        self.list_entries(Some(cdk07::State::Reserved))
            .await
            .map(|proofs| {
                proofs
                    .into_iter()
                    .map(|entry| (entry.y, cdk00::Proof::from(entry)))
                    .collect()
            })
    }

    async fn mark_as_pendingspent(&self, y: cdk01::PublicKey) -> Result<cdk00::Proof> {
        let entry = self
            .update_entry_state(y, &[cdk07::State::Unspent], cdk07::State::PendingSpent)
            .await?;
        Ok(cdk00::Proof::from(entry))
    }

    async fn counter(&self, kid: cdk02::Id) -> Result<u32> {
        let entry = self.counter(kid).await?;
        Ok(entry.counter)
    }

    async fn increment_counter(&self, kid: cdk02::Id, old: u32, increment: u32) -> Result<()> {
        let old = CounterEntry { kid, counter: old };
        let new = CounterEntry {
            kid,
            counter: old.counter + increment,
        };
        self.update_counter(old, new).await?;
        Ok(())
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
        }
    }
}

///////////////////////////////////////////// TransactionDB
#[allow(dead_code)]
pub struct TransactionDB {
    db: Rc<Rexie>,

    tx_store: String,
}

#[allow(dead_code)]
impl TransactionDB {
    const TRANSACTION_BASE_DB_NAME: &'static str = "transactions";
    const TRANSACTION_DB_KEY: &'static str = "tx_id"; // MUST match TransactionDB field
    const TRANSACTION_DB_INDEX: &'static str = "timestamp"; // MUST match TransactionDB field

    fn tx_store_name(wallet_id: &str) -> String {
        format!("{wallet_id}_{}", Self::TRANSACTION_BASE_DB_NAME)
    }

    pub fn object_stores(wallet_id: &str) -> Vec<rexie::ObjectStore> {
        let tx_store_name = Self::tx_store_name(wallet_id);
        let tx_tstamp_index =
            rexie::Index::new(Self::TRANSACTION_DB_INDEX, Self::TRANSACTION_DB_INDEX).unique(false);
        vec![
            rexie::ObjectStore::new(&tx_store_name)
                .auto_increment(false)
                .key_path(Self::TRANSACTION_DB_KEY)
                .add_index(tx_tstamp_index),
        ]
    }

    pub fn new(db: Rc<Rexie>, wallet_id: &str) -> Result<Self> {
        let tx_store = Self::tx_store_name(wallet_id);
        if !db.store_names().contains(&tx_store) {
            return Err(Error::BadTransactionDB);
        }
        let db = TransactionDB { db, tx_store };
        Ok(db)
    }

    async fn store(&self, tx_entry: TransactionEntry) -> Result<TransactionId> {
        let entry = to_value(&tx_entry)?;
        let tx = self
            .db
            .transaction(&[self.tx_store.clone()], TransactionMode::ReadWrite)?;
        let transactions = tx.store(&self.tx_store)?;
        transactions.add(&entry, None).await?;
        tx.done().await?;
        let tx_id =
            TransactionId::from_str(&tx_entry.tx_id).expect("double conversion should not fail");
        Ok(tx_id)
    }

    async fn load(&self, tx_id: TransactionId) -> Result<Option<TransactionEntry>> {
        let tx = self
            .db
            .transaction(&[self.tx_store.clone()], TransactionMode::ReadOnly)?;
        let transactions = tx.store(&self.tx_store)?;
        let js_entry = transactions.get(tx_id.to_string().into()).await?;
        tx.done().await?;
        let entry = js_entry.map(from_value::<TransactionEntry>).transpose()?;
        Ok(entry)
    }

    async fn delete(&self, tx_id: TransactionId) -> Result<()> {
        let tx = self
            .db
            .transaction(&[self.tx_store.clone()], TransactionMode::ReadWrite)?;
        let transactions = tx.store(&self.tx_store)?;
        transactions.delete(tx_id.to_string().into()).await?;
        tx.done().await?;
        Ok(())
    }

    async fn list_ids(&self) -> Result<Vec<TransactionId>> {
        let tx = self
            .db
            .transaction(&[self.tx_store.clone()], TransactionMode::ReadOnly)?;
        let transactions = tx.store(&self.tx_store)?;

        let js_convert = |jsv| from_value::<String>(jsv).map_err(Error::from);
        let tx_convert = |s: String| TransactionId::from_str(&s).map_err(Error::from);
        let tx_tstamp_index = transactions.index(Self::TRANSACTION_DB_INDEX)?;
        let tx_ids = tx_tstamp_index
            .get_all_keys(None, None)
            .await?
            .into_iter()
            .map(js_convert)
            .map(|r| r.and_then(tx_convert))
            .collect::<Result<Vec<_>>>()?;
        tx.done().await?;
        Ok(tx_ids)
    }
}

#[async_trait(?Send)]
impl wallet::TransactionRepository for TransactionDB {
    async fn store_tx(&self, tx: Transaction) -> Result<TransactionId> {
        let tx_entry = TransactionEntry::from(tx);
        self.store(tx_entry).await
    }

    async fn load_tx(&self, tx_id: TransactionId) -> Result<Transaction> {
        let entry = self
            .load(tx_id)
            .await?
            .ok_or(Error::TransactionNotFound(tx_id))?;
        Ok(Transaction::from(entry))
    }

    async fn delete_tx(&self, tx_id: TransactionId) -> Result<()> {
        self.delete(tx_id).await
    }

    async fn list_tx_ids(&self) -> Result<Vec<TransactionId>> {
        self.list_ids().await
    }
}

///////////////////////////////////////////// WalletEntry
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
struct WalletEntry {
    wallet_id: String,
    name: String,
    network: bitcoin::Network,
    mint: cashu::MintUrl,
    master: bitcoin::bip32::Xpriv,
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
            master: wallet.master,
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
            master: wallet.master,
            debit: wallet.debit,
            credit: wallet.credit,
        }
    }
}

///////////////////////////////////////////// PurseDB
#[allow(dead_code)]
pub struct PurseDB {
    db: Rc<Rexie>,

    wallet_store: String,
}

impl PurseDB {
    const WALLET_BASE_DB_NAME: &'static str = "wallets";
    const WALLET_DB_KEY: &'static str = "wallet_id"; // must match WalletEntry field

    fn wallet_store_name() -> String {
        String::from(Self::WALLET_BASE_DB_NAME)
    }

    pub fn object_stores() -> Vec<rexie::ObjectStore> {
        let wallet_store_name = Self::wallet_store_name();
        vec![
            rexie::ObjectStore::new(&wallet_store_name)
                .auto_increment(false)
                .key_path(Self::WALLET_DB_KEY),
        ]
    }

    pub fn new(db: Rc<Rexie>) -> Result<Self> {
        let wallet_store = Self::wallet_store_name();
        if !db.store_names().contains(&wallet_store) {
            return Err(Error::BadPurseDB);
        }

        let db = PurseDB { db, wallet_store };
        Ok(db)
    }

    async fn store(&self, wallet: WalletEntry) -> Result<String> {
        let entry = to_value(&wallet)?;
        let tx = self
            .db
            .transaction(&[self.wallet_store.clone()], TransactionMode::ReadWrite)?;
        let wallets = tx.store(&self.wallet_store)?;
        wallets.add(&entry, None).await?;
        tx.done().await?;
        Ok(wallet.wallet_id)
    }

    async fn load(&self, w_id: String) -> Result<Option<WalletEntry>> {
        let tx = self
            .db
            .transaction(&[self.wallet_store.clone()], TransactionMode::ReadOnly)?;
        let wallets = tx.store(&self.wallet_store)?;
        let js_entry = wallets.get(w_id.into()).await?;
        tx.done().await?;
        let entry = js_entry.map(from_value::<WalletEntry>).transpose()?;
        Ok(entry)
    }

    async fn delete(&self, w_id: String) -> Result<()> {
        let tx = self
            .db
            .transaction(&[self.wallet_store.clone()], TransactionMode::ReadWrite)?;
        let wallets = tx.store(&self.wallet_store)?;
        wallets.delete(w_id.into()).await?;
        tx.done().await?;
        Ok(())
    }

    async fn list_ids(&self) -> Result<Vec<String>> {
        let tx = self
            .db
            .transaction(&[self.wallet_store.clone()], TransactionMode::ReadOnly)?;
        let wallets = tx.store(&self.wallet_store)?;
        let w_ids = wallets
            .get_all_keys(None, None)
            .await?
            .into_iter()
            .map(from_value::<String>)
            .map(|r| r.map_err(Error::from))
            .collect::<Result<Vec<_>>>()?;
        tx.done().await?;
        Ok(w_ids)
    }
}

#[async_trait(?Send)]
impl PurseRepository for PurseDB {
    async fn store_wallet(&self, wallet: WalletConfig) -> Result<()> {
        let entry = WalletEntry::from(wallet);
        self.store(entry).await?;
        Ok(())
    }
    async fn load_wallet(&self, wallet_id: &str) -> Result<WalletConfig> {
        let wid = String::from(wallet_id);
        let entry = self
            .load(wid.clone())
            .await?
            .ok_or(Error::WalletIdNotFound(wid))?;
        let cfg = WalletConfig::from(entry);
        Ok(cfg)
    }
    async fn delete_wallet(&self, wallet_id: &str) -> Result<()> {
        let wid = String::from(wallet_id);
        self.delete(wid).await?;
        Ok(())
    }
    async fn list_wallets(&self) -> Result<Vec<String>> {
        self.list_ids().await
    }
}
