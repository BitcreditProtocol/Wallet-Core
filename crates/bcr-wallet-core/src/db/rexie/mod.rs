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
use tracing::info;
use wasm_bindgen::JsValue;
// ----- local modules
use crate::db::WalletDatabase;
use crate::db::types::DatabaseError;
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
    to_value(value).map_err(|e| DatabaseError::SerializationError("Cannot convert into JS".into()))
}

pub fn from_js<T: DeserializeOwned>(js: JsValue) -> Result<T, DatabaseError> {
    from_value(js).map_err(|e| DatabaseError::SerializationError("Cannot convert from JS".into()))
}

impl WalletDatabase for RexieWalletDatabase {
    async fn get_proofs(&self) -> Result<Vec<Proof>, DatabaseError> {
        let tx = self
            .db
            .transaction(&[&self.store_name], TransactionMode::ReadOnly)?;

        let store = tx.store(&self.store_name)?;
        let all = store.get_all(None, None).await?;

        info!(all=?all,"Rexie get all");

        let proofs = all
            .into_iter()
            .map(from_js)
            .collect::<Result<Vec<Proof>, DatabaseError>>()?;

        Ok(proofs)
    }

    // TODO inefficient
    async fn set_proofs(&self, proofs: Vec<Proof>) -> Result<(), DatabaseError> {
        let tx = self
            .db
            .transaction(&[self.store_name.clone()], TransactionMode::ReadWrite)?;
        let store = tx.store(&self.store_name.clone())?;
        store.clear().await?;

        for p in &proofs {
            self.add_proof(p.clone()).await?
        }
        Ok(())
    }

    async fn add_proof(&self, proof: Proof) -> Result<(), DatabaseError> {
        info!(name=?self.store_name,"Rexie add");
        let tx = self
            .db
            .transaction(&[self.store_name.clone()], TransactionMode::ReadWrite)?;
        let store = tx.store(&self.store_name.clone())?;
        info!("Rexie add got store");

        let key = to_js(&proof.c.to_string())?;
        let value = to_js(&proof)?;

        info!(key=?key,value=?value,"tryint to store");

        match store.add(&value, None).await {
            Ok(_) => {}
            Err(e) => {
                info!(err=?e,"Error");
            }
        }
        info!("rexie added kv");

        tx.done().await?;
        info!("rexie add done");

        Ok(())
    }
}
