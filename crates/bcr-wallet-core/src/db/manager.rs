// ----- standard library imports
use std::rc::Rc;
use std::str::FromStr;
// ----- extra library imports
use cashu::MintUrl;
use rexie::{ObjectStore, Rexie};
// ----- local modules
use super::rexie::RexieWalletDatabase;
use crate::wallet::new_credit;
use crate::wallet::{CreditWallet, Wallet};

// ----- end imports

pub struct Manager {
    db: Rc<Rexie>,
}

fn proof_store(id: &str) -> ObjectStore {
    ObjectStore::new(&id).key_path("id")
}

impl Manager {
    pub async fn new() -> Option<Manager> {
        // TODO use macro up to 99
        let rexie = Rexie::builder("wallets_db")
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
    pub fn get_wallet(&self, name: String) -> Wallet<CreditWallet, RexieWalletDatabase> {
        let rexie_wallet = RexieWalletDatabase::new(name, self.get_db());
        let mint_url = MintUrl::from_str("http://127.0.0.1:4343").unwrap();

        new_credit()
            .set_mint_url(mint_url)
            .set_database(rexie_wallet)
            .set_seed([0; 32])
            .build()
    }
}
