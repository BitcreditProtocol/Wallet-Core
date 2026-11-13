use crate::{
    error::{Error, Result},
    redb::purse::{StoredWallet, WALLET_TABLE, from_stored_wallet, to_stored_wallet},
};
use redb::ReadableTable;

///////////////////////////////////////////////////////////////// MIGRATION 0006
const MIGRATION_0006_WALLET_BETA_CLOWDER_ID: &str = "0006_wallet_beta_clowder_id";

pub(super) fn migration_name_for_purse() -> String {
    format!("{}_purse", MIGRATION_0006_WALLET_BETA_CLOWDER_ID)
}

pub(super) fn migration_0006_wallet_beta_clowder_id(txn: &redb::WriteTransaction) -> Result<()> {
    let mut table = match txn.open_table(WALLET_TABLE) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => {
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    };

    let mut upgraded: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    for item in table.iter()? {
        let (k, v) = item?;
        let stored: StoredWallet = borsh::from_slice(v.value().as_slice())
            .map_err(|e| Error::BorshSerialization(e.to_string()))?;
        if matches!(stored, StoredWallet::V1(_)) {
            let bytes = borsh::to_vec(&to_stored_wallet(from_stored_wallet(stored)))
                .map_err(|e| Error::BorshSerialization(e.to_string()))?;
            upgraded.push((k.value().to_vec(), bytes));
        }
    }

    for (key, bytes) in upgraded {
        table.insert(key.as_slice(), bytes)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::redb::purse::to_stored_wallet_v1;
    use bcr_common::cashu::CurrencyUnit;
    use bcr_wallet_core::types::{WalletBeta, WalletConfig};
    use bitcoin::secp256k1;
    use redb::{Builder, Database, ReadableDatabase, backends::InMemoryBackend};
    use std::{collections::HashMap, sync::Arc};

    fn test_database() -> Arc<Database> {
        Arc::new(
            Builder::new()
                .create_with_backend(InMemoryBackend::new())
                .expect("create in-memory database"),
        )
    }

    fn random_key() -> secp256k1::PublicKey {
        secp256k1::Keypair::new_global(&mut secp256k1::rand::thread_rng()).public_key()
    }

    fn test_wallet(id: &str, betas: Vec<WalletBeta>) -> WalletConfig {
        WalletConfig {
            wallet_id: id.to_string(),
            name: format!("wallet {id}"),
            network: bitcoin::Network::Regtest,
            mint: url::Url::parse("https://mint.example.com").expect("valid mint URL"),
            mint_keyset_infos: HashMap::new(),
            clowder_id: random_key(),
            pub_key: random_key(),
            debit: CurrencyUnit::Sat,
            betas,
            nostr_relays: vec![],
        }
    }

    fn beta(n: u8, clowder_id: Option<secp256k1::PublicKey>) -> WalletBeta {
        WalletBeta {
            url: url::Url::parse(&format!("https://beta{n}.example.com")).expect("valid beta URL"),
            clowder_id,
        }
    }

    fn insert_stored(db: &Database, id: &str, stored: &StoredWallet) {
        let write_txn = db.begin_write().expect("begin write transaction");
        {
            let mut table = write_txn.open_table(WALLET_TABLE).expect("open table");
            table
                .insert(
                    id.as_bytes(),
                    borsh::to_vec(stored).expect("serialize stored wallet"),
                )
                .expect("insert wallet");
        }
        write_txn.commit().expect("commit insert");
    }

    fn migrate(db: &Database) {
        let write_txn = db.begin_write().expect("begin write transaction");
        migration_0006_wallet_beta_clowder_id(&write_txn).expect("migration succeeds");
        write_txn.commit().expect("commit migration");
    }

    fn load_stored(db: &Database, id: &str) -> StoredWallet {
        let read_txn = db.begin_read().expect("begin read transaction");
        let table = read_txn.open_table(WALLET_TABLE).expect("open table");
        let bytes = table
            .get(id.as_bytes())
            .expect("read wallet")
            .expect("wallet exists")
            .value();
        borsh::from_slice(bytes.as_slice()).expect("deserialize stored wallet")
    }

    #[test]
    fn migration_succeeds_when_wallet_table_does_not_exist() {
        let db = test_database();
        migrate(&db);
    }

    #[test]
    fn migration_0006_keeps_every_v1_beta_url_with_unknown_clowder_id() {
        let db = test_database();
        let original = test_wallet("w1", vec![beta(1, None), beta(2, None), beta(3, None)]);
        insert_stored(
            &db,
            "w1",
            &to_stored_wallet_v1(original.clone()).expect("v1 envelope"),
        );

        migrate(&db);

        let migrated = load_stored(&db, "w1");
        assert!(matches!(migrated, StoredWallet::V2(_)));
        let migrated = from_stored_wallet(migrated);
        assert_eq!(migrated.betas, original.betas);
        assert!(migrated.betas.iter().all(|b| b.clowder_id.is_none()));
        assert_eq!(migrated.wallet_id, original.wallet_id);
        assert_eq!(migrated.name, original.name);
        assert_eq!(migrated.mint, original.mint);
        assert_eq!(migrated.clowder_id, original.clowder_id);
        assert_eq!(migrated.pub_key, original.pub_key);
    }

    #[test]
    fn migration_0006_upgrades_every_wallet_and_keeps_v2_entries() {
        let db = test_database();
        let empty = test_wallet("empty", vec![]);
        let legacy = test_wallet("legacy", vec![beta(1, None)]);
        let current = test_wallet("current", vec![beta(2, Some(random_key()))]);
        insert_stored(&db, "empty", &to_stored_wallet_v1(empty).expect("v1"));
        insert_stored(&db, "legacy", &to_stored_wallet_v1(legacy).expect("v1"));
        insert_stored(&db, "current", &to_stored_wallet(current.clone()));

        migrate(&db);

        for id in ["empty", "legacy", "current"] {
            assert!(matches!(load_stored(&db, id), StoredWallet::V2(_)));
        }
        assert!(
            from_stored_wallet(load_stored(&db, "empty"))
                .betas
                .is_empty()
        );
        assert_eq!(
            from_stored_wallet(load_stored(&db, "current")).betas,
            current.betas
        );
    }

    #[test]
    fn v2_round_trips_known_clowder_id() {
        let db = test_database();
        let wallet = test_wallet("w1", vec![beta(1, Some(random_key())), beta(2, None)]);
        insert_stored(&db, "w1", &to_stored_wallet(wallet.clone()));

        assert_eq!(
            from_stored_wallet(load_stored(&db, "w1")).betas,
            wallet.betas
        );
    }
}
