use std::collections::HashSet;
// ----- standard library im
use std::rc::Rc;
// ----- extra library imports
use cashu::MintUrl;
use rexie::Rexie;
use rexie::TransactionMode;
// ----- local modules
use super::utils;
use crate::db::rexie::utils::from_js;
use crate::db::types::DatabaseError;
use crate::db::{Metadata, WalletMetadata};
// ----- end imports

pub struct RexieMetadata {
    db: Rc<Rexie>,
}

impl RexieMetadata {
    pub fn new(db: Rc<Rexie>) -> Self {
        Self { db }
    }

    /// Finds the first unoccupied natural number to use as id
    async fn get_empty_id(&self) -> Result<usize, DatabaseError> {
        let tx = self.db.transaction(
            std::slice::from_ref(&super::constants::WALLET_METADATA),
            TransactionMode::ReadOnly,
        )?;

        let store = tx.store(super::constants::WALLET_METADATA)?;
        let keys = store.get_all_keys(None, None).await?;
        tx.done().await?;

        let keys = keys
            .into_iter()
            .map(from_js)
            .collect::<Result<Vec<usize>, DatabaseError>>()?;
        let keys = HashSet::<usize>::from_iter(keys);

        for i in 0..100 {
            if !keys.contains(&i) {
                return Ok(i);
            }
        }

        Err(DatabaseError::WalletDatabaseFull)
    }
}

impl Metadata for RexieMetadata {
    async fn get_wallets(&self) -> Result<Vec<WalletMetadata>, DatabaseError> {
        let tx = self.db.transaction(
            std::slice::from_ref(&super::constants::WALLET_METADATA),
            TransactionMode::ReadOnly,
        )?;

        let store = tx.store(super::constants::WALLET_METADATA)?;

        let mut wallets = store
            .get_all(None, None)
            .await?
            .into_iter()
            .map(utils::from_js)
            .collect::<Result<Vec<WalletMetadata>, DatabaseError>>()?;

        tx.done().await?;

        wallets.sort_by_key(|w| w.id);

        Ok(wallets)
    }

    async fn get_wallet(&self, id: usize) -> Result<WalletMetadata, DatabaseError> {
        let tx = self.db.transaction(
            std::slice::from_ref(&super::constants::WALLET_METADATA),
            TransactionMode::ReadOnly,
        )?;

        let store = tx.store(super::constants::WALLET_METADATA)?;

        let key = utils::to_js(&id)?;
        let res = if let Some(wallet) = store.get(key).await? {
            Ok(from_js(wallet)?)
        } else {
            Err(DatabaseError::WalletNotFound(id))
        };
        tx.done().await?;

        res
    }

    async fn add_wallet(
        &self,
        name: String,
        mint_url: MintUrl,
        mnemonic: Vec<String>,
        unit: String,
        is_credit: bool,
    ) -> Result<WalletMetadata, DatabaseError> {
        let id = self.get_empty_id().await?;
        let wallet = WalletMetadata {
            id: id,
            name,
            is_active: true,
            mint_url,
            mnemonic,
            unit,
            is_credit,
        };

        let tx = self.db.transaction(
            std::slice::from_ref(&super::constants::WALLET_METADATA),
            TransactionMode::ReadWrite,
        )?;

        let store = tx.store(super::constants::WALLET_METADATA)?;

        let wallet_js = utils::to_js(&wallet)?;

        store.put(&wallet_js, None).await?;

        tx.done().await?;

        Ok(wallet)
    }
    async fn remove_wallet(&self, id: usize) -> Result<(), DatabaseError> {
        let tx = self.db.transaction(
            std::slice::from_ref(&super::constants::WALLET_METADATA),
            TransactionMode::ReadWrite,
        )?;

        let store = tx.store(super::constants::WALLET_METADATA)?;

        let key = utils::to_js(&id)?;
        store.delete(key).await?;

        tx.done().await?;

        Ok(())
    }
}
