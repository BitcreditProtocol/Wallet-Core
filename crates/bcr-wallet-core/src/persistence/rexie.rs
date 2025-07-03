// ----- standard library imports
use std::rc::Rc;
// ----- extra library imports
use anyhow::Error as AnyError;
use async_trait::async_trait;
use cashu::{
    CurrencyUnit, PublicKey, nut00 as cdk00, nut02 as cdk02, nut07 as cdk07, nut12 as cdk12,
    secret::Secret,
};
use rexie::{Rexie, TransactionMode};
use serde_wasm_bindgen::{from_value, to_value};
use wasm_bindgen::JsValue;
// ----- local imports
use crate::{
    error::{Error, Result},
    pocket::PocketRepository,
};

// ----- end imports

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct ProofEntry {
    y: PublicKey,
    amount: cashu::Amount,
    keyset_id: cdk02::Id,
    secret: Secret,
    c: PublicKey,
    witness: Option<cdk00::Witness>,
    dleq: Option<cdk12::ProofDleq>,
    state: cdk07::State,
}

impl std::convert::From<cdk00::Proof> for ProofEntry {
    fn from(proof: cdk00::Proof) -> Self {
        let y = cashu::dhke::hash_to_curve(proof.secret.as_bytes())
            .expect("Hash to curve should not fail");
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
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct CounterEntry {
    kid: cdk02::Id,
    counter: u32,
}

pub struct ProofDB {
    db: Rc<Rexie>,

    proof_store: String,
    counter_store: String,
}

impl ProofDB {
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
            return Err(Error::BadProofDB);
        }
        if !db.store_names().contains(&counter_store) {
            return Err(Error::BadProofDB);
        }

        let db = ProofDB {
            db,
            proof_store,
            counter_store,
        };
        Ok(db)
    }

    async fn store_proof(&self, proof: ProofEntry) -> Result<PublicKey> {
        let entry = to_value(&proof)?;
        let tx = self
            .db
            .transaction(&[self.proof_store.clone()], TransactionMode::ReadWrite)?;
        let proofs = tx.store(&self.proof_store)?;
        proofs.add(&entry, None).await?;
        tx.done().await?;
        Ok(proof.y)
    }

    #[allow(dead_code)]
    async fn delete_proof(&self, y: PublicKey) -> Result<()> {
        let key = JsValue::from_str(&y.to_string());
        let tx = self
            .db
            .transaction(&[self.proof_store.clone()], TransactionMode::ReadWrite)?;
        let proofs = tx.store(&self.proof_store)?;
        proofs.delete(key).await?;
        tx.done().await?;
        Ok(())
    }

    #[allow(dead_code)]
    async fn update_proof_state(
        &self,
        index: PublicKey,
        new_state: cdk07::State,
    ) -> Result<ProofEntry> {
        let key = JsValue::from_str(&index.to_string());
        let tx = self
            .db
            .transaction(&[self.proof_store.clone()], TransactionMode::ReadWrite)?;
        let proofs = tx.store(&self.proof_store)?;
        let mut proof = proofs
            .get(key.clone())
            .await?
            .map(from_value::<ProofEntry>)
            .ok_or(Error::ProofNotFound(index))??;
        proof.state = new_state;
        let entry = to_value(&proof)?;
        let new_entry = proofs.put(&entry, Some(&key)).await?;
        tx.done().await?;
        let new_proof: ProofEntry = from_value(new_entry)?;
        Ok(new_proof)
    }

    async fn list_proofs(&self, state: Option<cdk07::State>) -> Result<Vec<ProofEntry>> {
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
impl PocketRepository for ProofDB {
    async fn store_new(&self, proof: cdk00::Proof) -> Result<()> {
        let entry = ProofEntry::from(proof);
        self.store_proof(entry).await?;
        Ok(())
    }

    async fn list_unspent(&self) -> Result<Vec<cdk00::Proof>> {
        self.list_proofs(Some(cdk07::State::Unspent))
            .await
            .map(|proofs| proofs.into_iter().map(cdk00::Proof::from).collect())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proof_store_name() {
        let name = ProofDB::proof_store_name(&CurrencyUnit::Sat);
        assert_eq!("sat_proofs", name);
        let name = ProofDB::proof_store_name(&CurrencyUnit::Custom(String::from("test")));
        assert_eq!("test_proofs", name);
        let name = ProofDB::proof_store_name(&CurrencyUnit::Custom(String::from("TEST")));
        assert_eq!("test_proofs", name);
    }
    #[test]

    fn counter_store_name() {
        let name = ProofDB::counter_store_name(&CurrencyUnit::Sat);
        assert_eq!("sat_counters", name);
        let name = ProofDB::counter_store_name(&CurrencyUnit::Custom(String::from("test")));
        assert_eq!("test_counters", name);
        let name = ProofDB::counter_store_name(&CurrencyUnit::Custom(String::from("TEST")));
        assert_eq!("test_counters", name);
    }
}
