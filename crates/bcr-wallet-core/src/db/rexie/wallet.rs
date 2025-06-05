// ----- standard library im
use std::rc::Rc;
// ----- extra library imports
use cashu::{Id, Proof};
use rexie::Rexie;
use rexie::TransactionMode;
// ----- local modules
use super::utils;
use crate::db::types::{DatabaseError, ProofStatus, WalletProof};
use crate::db::{KeysetDatabase, WalletDatabase};
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

impl WalletDatabase for RexieWalletDatabase {
    async fn get_active_proofs(&self) -> Result<Vec<Proof>, DatabaseError> {
        let tx = self
            .db
            .transaction(&[&self.store_name], TransactionMode::ReadOnly)?;

        let store = tx.store(&self.store_name)?;
        let all = store.get_all(None, None).await?;

        let proofs = all
            .into_iter()
            .map(utils::from_js)
            .collect::<Result<Vec<WalletProof>, DatabaseError>>()?;

        let unspent = proofs
            .into_iter()
            .filter(|p| p.status == ProofStatus::Unspent)
            .map(|p| p.proof)
            .collect::<Vec<Proof>>();

        Ok(unspent)
    }

    async fn deactivate_proof(&self, proof: Proof) -> Result<(), DatabaseError> {
        let tx = self.db.transaction(
            std::slice::from_ref(&self.store_name),
            TransactionMode::ReadWrite,
        )?;
        let store = tx.store(&self.store_name.clone())?;

        let key = proof
            .y()
            .map_err(|e| DatabaseError::CdkError(e.to_string()))?;
        let key = utils::to_js(&key)?;
        if let Ok(Some(wp)) = store.get(key).await {
            let mut wp: WalletProof = utils::from_js(wp)?;
            wp.status = ProofStatus::Spent;

            let wp = utils::to_js(&wp)?;
            store.put(&wp, None).await?;
        }
        tx.done().await?;
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

        let value = utils::to_js(&wallet_proof)?;

        store.add(&value, None).await?;

        tx.done().await?;

        Ok(())
    }
}

impl KeysetDatabase for RexieWalletDatabase {
    async fn get_count(&self, id: Id) -> Result<u32, DatabaseError> {
        let tx = self.db.transaction(
            std::slice::from_ref(&super::constants::KEYSET_COUNTER),
            TransactionMode::ReadOnly,
        )?;

        let store = tx.store(super::constants::KEYSET_COUNTER)?;

        let key = utils::to_js(&id)?;
        if let Ok(Some(count)) = store.get(key).await {
            let count: u32 = utils::from_js(count)?;
            return Ok(count);
        }
        Err(DatabaseError::KeysetNotFound)
    }

    async fn increase_count(&self, keyset_id: Id, addition: u32) -> Result<u32, DatabaseError> {
        let tx = self.db.transaction(
            std::slice::from_ref(&super::constants::KEYSET_COUNTER),
            TransactionMode::ReadWrite,
        )?;
        let store = tx.store(super::constants::KEYSET_COUNTER)?;

        let key = keyset_id;
        let key = utils::to_js(&key)?;
        if let Ok(Some(wp)) = store.get(key.clone()).await {
            let mut count: u32 = utils::from_js(wp)?;
            count += addition;

            let _ = store.put(&utils::to_js(&count)?, Some(&key)).await?;
            tx.done().await?;
            Ok(count)
        } else {
            let _ = store.put(&utils::to_js(&addition)?, Some(&key)).await?;
            tx.done().await?;
            Ok(addition)
        }
    }
}
