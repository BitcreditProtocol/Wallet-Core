// ----- standard library imports
use std::rc::Rc;
use std::str::FromStr;
// ----- extra library imports
use cashu::MintUrl;
use rexie::{ObjectStore, Rexie};
// ----- local modules
use super::RexieWalletDatabase;
use crate::wallet::new_credit;
use crate::wallet::{CreditWallet, Wallet};

// ----- end imports

pub struct Manager {
    db: Rc<Rexie>,
}

fn proof_store(id: &str) -> ObjectStore {
    ObjectStore::new(id).key_path("id")
}

impl Manager {
    pub async fn new() -> Option<Manager> {
        // TODO use macro up to 99
        let rexie = Rexie::builder("wallets_db_2")
            .version(1)
            .add_object_store(proof_store("wallet_0"))
            .add_object_store(proof_store("wallet_1"))
            .add_object_store(proof_store("wallet_2"))
            .add_object_store(proof_store("wallet_3"))
            .add_object_store(proof_store("wallet_4"))
            .add_object_store(proof_store("wallet_5"))
            .add_object_store(proof_store("wallet_6"))
            .add_object_store(proof_store("wallet_7"))
            .add_object_store(proof_store("wallet_8"))
            .add_object_store(proof_store("wallet_9"))
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
