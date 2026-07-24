use crate::{
    PurseRepository,
    error::{Error, Result},
};
use async_trait::async_trait;
use bcr_common::cashu::{self, CurrencyUnit};
use bcr_common::wire::borsh::{
    deserialize_from_str, deserialize_vec_of_strs, serialize_as_str, serialize_vec_of_strs,
};
use bcr_wallet_core::{
    borsh::{deserialize_mint_keyset_infos, serialize_mint_keyset_infos},
    types::WalletConfig,
};
use bitcoin::secp256k1;
use borsh::{BorshDeserialize, BorshSerialize};
use nostr::RelayUrl;
use redb::{Database, ReadableDatabase, TableDefinition, TableError};
use std::{collections::HashMap, sync::Arc};
use tokio::task::spawn_blocking;

/// StoredWallet is a versioned, borsh-serialized wallet entry
#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub(super) enum StoredWallet {
    V1(StoredWalletPayloadV1),
}

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub(super) struct StoredWalletPayloadV1 {
    wallet_id: String,
    name: String,
    #[borsh(
        serialize_with = "serialize_as_str",
        deserialize_with = "deserialize_from_str"
    )]
    network: bitcoin::Network,
    #[borsh(
        serialize_with = "serialize_as_str",
        deserialize_with = "deserialize_from_str"
    )]
    mint: url::Url,
    #[borsh(
        serialize_with = "serialize_mint_keyset_infos",
        deserialize_with = "deserialize_mint_keyset_infos"
    )]
    mint_keyset_infos: HashMap<cashu::Id, cashu::KeySetInfo>,
    #[borsh(
        serialize_with = "serialize_as_str",
        deserialize_with = "deserialize_from_str"
    )]
    clowder_id: secp256k1::PublicKey,
    #[borsh(
        serialize_with = "serialize_as_str",
        deserialize_with = "deserialize_from_str"
    )]
    pub_key: secp256k1::PublicKey,
    #[borsh(
        serialize_with = "serialize_as_str",
        deserialize_with = "deserialize_from_str"
    )]
    debit: CurrencyUnit,
    #[borsh(
        serialize_with = "serialize_vec_of_strs",
        deserialize_with = "deserialize_vec_of_strs"
    )]
    betas: Vec<url::Url>,
    #[borsh(
        serialize_with = "serialize_vec_of_strs",
        deserialize_with = "deserialize_vec_of_strs"
    )]
    nostr_relays: Vec<RelayUrl>,
}

impl std::convert::From<WalletConfig> for StoredWalletPayloadV1 {
    fn from(wallet: WalletConfig) -> Self {
        Self {
            wallet_id: wallet.wallet_id,
            name: wallet.name,
            network: wallet.network,
            mint: wallet.mint,
            mint_keyset_infos: wallet.mint_keyset_infos,
            clowder_id: wallet.clowder_id,
            pub_key: wallet.pub_key,
            debit: wallet.debit,
            betas: wallet.betas,
            nostr_relays: wallet.nostr_relays,
        }
    }
}

impl std::convert::From<StoredWalletPayloadV1> for WalletConfig {
    fn from(wallet: StoredWalletPayloadV1) -> Self {
        Self {
            wallet_id: wallet.wallet_id,
            name: wallet.name,
            network: wallet.network,
            mint: wallet.mint,
            mint_keyset_infos: wallet.mint_keyset_infos,
            clowder_id: wallet.clowder_id,
            pub_key: wallet.pub_key,
            debit: wallet.debit,
            betas: wallet.betas,
            nostr_relays: wallet.nostr_relays,
        }
    }
}

pub(super) fn to_stored_wallet_v1(wallet: WalletConfig) -> Result<StoredWallet> {
    let payload = StoredWalletPayloadV1::from(wallet);
    Ok(StoredWallet::V1(payload))
}

pub(super) fn from_stored_wallet_v1(wallet: StoredWallet) -> Result<WalletConfig> {
    let StoredWallet::V1(payload) = wallet;
    Ok(payload.into())
}

///////////////////////////////////////////// PurseDB
pub const WALLET_TABLE: TableDefinition<&[u8], Vec<u8>> = TableDefinition::new("wallets");
pub struct PurseDB {
    db: Arc<Database>,
}

impl PurseDB {
    pub fn new(db: Arc<Database>) -> Result<Self> {
        Ok(Self { db })
    }

    fn store_sync(db: Arc<Database>, wallet: WalletConfig) -> Result<()> {
        let id = wallet.wallet_id.clone();
        let entry = to_stored_wallet_v1(wallet)?;
        let write_txn = db.begin_write()?;

        {
            let mut table = write_txn.open_table(WALLET_TABLE)?;

            let serialized =
                borsh::to_vec(&entry).map_err(|e| Error::BorshSerialization(e.to_string()))?;
            table.insert(id.as_bytes(), serialized)?;
        }

        write_txn.commit()?;
        Ok(())
    }

    fn load_sync(db: Arc<Database>, wallet_id: &str) -> Result<Option<WalletConfig>> {
        let read_txn = db.begin_read()?;

        match read_txn.open_table(WALLET_TABLE) {
            Ok(table) => {
                let entry = table.get(wallet_id.as_bytes())?;
                match entry {
                    Some(e) => {
                        let deserialized: StoredWallet = borsh::from_slice(e.value().as_slice())
                            .map_err(|e| Error::BorshSerialization(e.to_string()))?;
                        let wallet = from_stored_wallet_v1(deserialized)?;
                        Ok(Some(wallet))
                    }
                    None => Ok(None),
                }
            }
            Err(TableError::TableDoesNotExist(_)) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn delete_sync(db: Arc<Database>, wallet_id: &str) -> Result<()> {
        let write_txn = db.begin_write()?;

        {
            let mut table = write_txn.open_table(WALLET_TABLE)?;
            table.remove(wallet_id.as_bytes())?;
        }

        write_txn.commit()?;
        Ok(())
    }

    fn list_ids_sync(db: Arc<Database>) -> Result<Vec<String>> {
        let read_txn = db.begin_read()?;

        match read_txn.open_table(WALLET_TABLE) {
            Ok(table) => {
                let mut res = Vec::new();
                for (_, v) in table.range::<&[u8]>(..)?.flatten() {
                    let deserialized: StoredWallet = borsh::from_slice(v.value().as_slice())
                        .map_err(|e| Error::BorshSerialization(e.to_string()))?;
                    let wallet = from_stored_wallet_v1(deserialized)?;
                    res.push(wallet.wallet_id);
                }
                Ok(res)
            }
            Err(TableError::TableDoesNotExist(_)) => Ok(vec![]),
            Err(e) => Err(e.into()),
        }
    }
}

#[async_trait]
impl PurseRepository for PurseDB {
    async fn store(&self, wallet: WalletConfig) -> Result<()> {
        let db_clone = self.db.clone();
        spawn_blocking(move || Self::store_sync(db_clone, wallet)).await?
    }

    async fn load(&self, wallet_id: &str) -> Result<WalletConfig> {
        let db_clone = self.db.clone();
        let id = wallet_id.to_owned();
        let res = spawn_blocking(move || Self::load_sync(db_clone, &id)).await??;
        res.ok_or(Error::WalletIdNotFound(wallet_id.to_owned()))
    }

    async fn delete(&self, wallet_id: &str) -> Result<()> {
        let db_clone = self.db.clone();
        let id = wallet_id.to_owned();
        spawn_blocking(move || Self::delete_sync(db_clone, &id)).await??;
        Ok(())
    }

    async fn list_ids(&self) -> Result<Vec<String>> {
        let db_clone = self.db.clone();
        spawn_blocking(move || Self::list_ids_sync(db_clone)).await?
    }
}

#[cfg(test)]
mod tests {
    use crate::error::Error;
    use std::str::FromStr;

    use crate::test_utils::tests::test_pub_key;

    use super::*;
    use redb::{Builder, backends::InMemoryBackend};

    fn get_db() -> PurseDB {
        let in_mem = InMemoryBackend::new();
        PurseDB {
            db: Arc::new(
                Builder::new()
                    .create_with_backend(in_mem)
                    .expect("can create in-memory redb"),
            ),
        }
    }

    fn test_wallet(id: &str, name: &str) -> WalletConfig {
        let test_clowder_id = secp256k1::PublicKey::from_str(
            "02295fb5f4eeb2f21e01eaf3a2d9a3be10f39db870d28f02146130317973a40ac0",
        )
        .expect("valid key");
        WalletConfig {
            wallet_id: id.to_owned(),
            name: name.to_owned(),

            network: bitcoin::Network::Bitcoin,
            mint: url::Url::from_str("https://example.com").expect("valid mint url"),
            mint_keyset_infos: HashMap::new(),
            clowder_id: test_clowder_id,
            pub_key: test_pub_key(),
            debit: CurrencyUnit::Sat,
            betas: vec![],
            nostr_relays: vec![],
        }
    }

    #[tokio::test]
    async fn test_list_ids_empty() {
        let db = get_db();
        let ids = db.list_ids().await.expect("list_ids works");
        assert!(ids.is_empty());
    }

    #[tokio::test]
    async fn test_load_missing_returns_error() {
        let db = get_db();
        let err = db.load("does-not-exist").await.unwrap_err();

        match err {
            Error::WalletIdNotFound(id) => assert_eq!(id, "does-not-exist"),
            other => panic!("expected WalletIdNotFound, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_store_load() {
        let db = get_db();
        let w = test_wallet("w1", "My Wallet");

        db.store(w.clone()).await.expect("store works");
        let loaded = db.load("w1").await.expect("load works");

        assert_eq!(loaded.wallet_id, w.wallet_id);
        assert_eq!(loaded.name, w.name);
    }

    #[tokio::test]
    async fn test_list_ids_after_inserts() {
        let db = get_db();

        db.store(test_wallet("w1", "Wallet 1"))
            .await
            .expect("store w1");
        db.store(test_wallet("w2", "Wallet 2"))
            .await
            .expect("store w2");

        let mut ids = db.list_ids().await.expect("list_ids works");
        ids.sort();

        assert_eq!(ids, vec!["w1".to_string(), "w2".to_string()]);
    }

    #[tokio::test]
    async fn test_delete_removes_wallet() {
        let db = get_db();

        db.store(test_wallet("w1", "Wallet 1"))
            .await
            .expect("store works");

        db.delete("w1").await.expect("delete works");

        let err = db.load("w1").await.unwrap_err();
        match err {
            Error::WalletIdNotFound(id) => assert_eq!(id, "w1"),
            other => panic!("expected WalletIdNotFound, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_delete_missing_is_ok() {
        let db = get_db();
        db.delete("missing").await.expect("delete missing is ok");
    }
}
