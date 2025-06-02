// ----- standard library im
use std::rc::Rc;
// ----- extra library imports
// use anyhow::Result;
use cashu::Proof;
use rexie::Rexie;
use rexie::TransactionMode;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_wasm_bindgen::{from_value, to_value};
use wasm_bindgen::JsValue;
// ----- local modules
use crate::db::WalletDatabase;
use crate::db::types::{DatabaseError, ProofStatus, WalletProof};
// ----- end imports

pub struct RexieWalletDatabase {
    db: Rc<Rexie>,
    store_name: String,
}

impl RexieWalletDatabase {
    pub fn new(store_name: String, db: Rc<Rexie>) -> Self {
        RexieWalletDatabase { db, store_name }
    }
}

impl From<rexie::Error> for DatabaseError {
    fn from(err: rexie::Error) -> Self {
        DatabaseError::DatabaseError(err.to_string())
    }
}

pub fn to_js<T: Serialize>(value: &T) -> Result<JsValue, DatabaseError> {
    to_value(value)
        .map_err(|e| DatabaseError::SerializationError(format!("Cannot convert into JS: {:?}", e)))
}

pub fn from_js<T: DeserializeOwned>(js: JsValue) -> Result<T, DatabaseError> {
    from_value(js)
        .map_err(|e| DatabaseError::SerializationError(format!("Cannot convert from JS: {:?}", e)))
}

impl WalletDatabase for RexieWalletDatabase {
    async fn get_active_proofs(&self) -> Result<Vec<Proof>, DatabaseError> {
        let tx = self
            .db
            .transaction(&[&self.store_name], TransactionMode::ReadOnly)?;

        let store = tx.store(&self.store_name)?;
        let all = store.get_all(None, None).await?;

        let proofs = all
            .into_iter()
            .map(from_js)
            .collect::<Result<Vec<WalletProof>, DatabaseError>>()?;

        let unspent = proofs
            .into_iter()
            .filter(|p| p.status == ProofStatus::Unspent)
            .map(|p| p.proof)
            .collect::<Vec<Proof>>();

        Ok(unspent)
    }

    async fn inactivate_proof(&self, proof: Proof) -> Result<(), DatabaseError> {
        let tx = self.db.transaction(
            std::slice::from_ref(&self.store_name),
            TransactionMode::ReadWrite,
        )?;
        let store = tx.store(&self.store_name.clone())?;

        let key = proof.y().unwrap();
        let key = to_js(&key)?;
        if let Ok(Some(wp)) = store.get(key).await {
            let mut wp: WalletProof = from_js(wp)?;
            wp.status = ProofStatus::Spent;

            let wp = to_js(&wp)?;
            store.put(&wp, None).await?;
        }
        Ok(())
    }

    async fn add_proof(&self, proof: Proof) -> Result<(), DatabaseError> {
        let tx = self.db.transaction(
            std::slice::from_ref(&self.store_name),
            TransactionMode::ReadWrite,
        )?;
        let store = tx.store(&self.store_name.clone())?;

        let wallet_proof = WalletProof {
            proof: proof.clone(),
            status: ProofStatus::Unspent,
            id: proof.y().unwrap(),
        };

        let value = to_js(&wallet_proof)?;

        store.add(&value, None).await?;

        tx.done().await?;

        Ok(())
    }
}
