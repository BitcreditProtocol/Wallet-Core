// ----- standard library imports
use std::rc::Rc;
// ----- extra library imports
use rexie::{ObjectStore, Rexie, TransactionMode};
// ----- local modules
use crate::db::DatabaseError;
// ----- end imports

pub struct Manager {
    db: Rc<Rexie>,
}

fn proof_store(id: &str) -> ObjectStore {
    ObjectStore::new(id).key_path("id")
}

impl Manager {
    pub async fn new(db_name: &str) -> Option<Manager> {
        let mut rexie = Rexie::builder(db_name).version(1);
        for i in 0..99 {
            rexie = rexie.add_object_store(proof_store(&format!("wallet_{}", i)));
        }
        let rexie = rexie
            .add_object_store(ObjectStore::new(super::KEYSET_COUNTER))
            .add_object_store(ObjectStore::new(super::WALLET_METADATA).key_path("id"))
            .build()
            .await;
        if let Ok(rexie) = rexie {
            return Some(Manager { db: Rc::new(rexie) });
        }
        None
    }
    pub fn get_db(&self) -> Rc<Rexie> {
        self.db.clone()
    }
    pub async fn clear(&self) -> Result<(), DatabaseError> {
        let tx = self.db.transaction(
            std::slice::from_ref(&super::constants::WALLET_METADATA),
            TransactionMode::ReadWrite,
        )?;

        for i in 0..99 {
            let store = tx.store(&format!("wallet_{}", i))?;
            store.clear().await?;
        }

        let store = tx.store(super::constants::WALLET_METADATA)?;
        store.clear().await?;

        let store = tx.store(super::KEYSET_COUNTER)?;
        store.clear().await?;

        tx.done().await?;
        Ok(())
    }
}
