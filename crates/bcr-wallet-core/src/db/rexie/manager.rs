// ----- standard library imports
use std::rc::Rc;
// ----- extra library imports
use rexie::{ObjectStore, Rexie};
// ----- local modules

// ----- end imports

pub struct Manager {
    db: Rc<Rexie>,
}

fn proof_store(id: &str) -> ObjectStore {
    ObjectStore::new(id).key_path("id")
}

impl Manager {
    pub async fn new() -> Option<Manager> {
        let mut rexie = Rexie::builder("wallets_db_4").version(1);
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
}
