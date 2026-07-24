use std::collections::HashMap;

use bcr_common::cashu::{self, CurrencyUnit};
use bcr_wallet_core::types::WalletConfig;
use nostr::types::RelayUrl;
use redb::ReadableTable;

use crate::error::{Error, Result};
use crate::redb::purse::{self, WALLET_TABLE};

///////////////////////////////////////////////////////////////// MIGRATION 0002
const MIGRATION_0002_ADD_VERSIONED_ENVELOPE: &str = "0002_add_versioned_envelope";

pub(super) fn migration_name_for_purse() -> String {
    format!("{}_purse", MIGRATION_0002_ADD_VERSIONED_ENVELOPE)
}

pub(super) fn migration_0002_add_purse_envelope(txn: &redb::WriteTransaction) -> Result<()> {
    migrate_purse_table_to_envelope(txn)?;

    Ok(())
}

fn migrate_purse_table_to_envelope(txn: &redb::WriteTransaction) -> Result<()> {
    let table_def = WALLET_TABLE;

    let mut table = match txn.open_table(table_def) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => {
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    };

    let mut migrated_wallets: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    for item in table.iter()? {
        let (k, v) = item?;
        let wallet: WalletEntry = ciborium::from_reader(v.value().as_slice())?;
        let stored_wallet_v1 = purse::to_stored_wallet_v1(wallet.into())?;

        let migrated_wallet_bytes = borsh::to_vec(&stored_wallet_v1)
            .map_err(|e| Error::BorshSerialization(e.to_string()))?;
        migrated_wallets.push((k.value().to_vec(), migrated_wallet_bytes));
    }

    for (key, new_wallet) in migrated_wallets.iter() {
        table.insert(key.as_slice(), new_wallet)?;
    }

    Ok(())
}

///////////////////////////////////////////// WalletEntry
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
struct WalletEntry {
    wallet_id: String,
    name: String,
    network: bitcoin::Network,
    mint: url::Url,
    mint_keyset_infos: HashMap<cashu::Id, cashu::KeySetInfo>,
    clowder_id: bitcoin::secp256k1::PublicKey,
    pub_key: bitcoin::secp256k1::PublicKey,
    debit: CurrencyUnit,
    betas: Vec<url::Url>,
    nostr_relays: Vec<RelayUrl>,
}

impl std::convert::From<WalletConfig> for WalletEntry {
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

impl std::convert::From<WalletEntry> for WalletConfig {
    fn from(wallet: WalletEntry) -> Self {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::redb::purse::{StoredWallet, from_stored_wallet_v1};
    use redb::{Builder, Database, ReadableDatabase, backends::InMemoryBackend};
    use std::sync::Arc;

    fn test_database() -> Arc<Database> {
        let backend = InMemoryBackend::new();
        Arc::new(
            Builder::new()
                .create_with_backend(backend)
                .expect("create in-memory database"),
        )
    }

    fn test_wallet() -> WalletConfig {
        let clowder_keypair =
            bitcoin::secp256k1::Keypair::new_global(&mut bitcoin::secp256k1::rand::thread_rng());
        let wallet_keypair =
            bitcoin::secp256k1::Keypair::new_global(&mut bitcoin::secp256k1::rand::thread_rng());
        WalletConfig {
            wallet_id: "wallet-123".to_string(),
            name: "Test wallet".to_string(),
            network: bitcoin::Network::Regtest,
            mint: url::Url::parse("https://mint.example.com").expect("valid mint URL"),
            mint_keyset_infos: HashMap::new(),
            clowder_id: clowder_keypair.public_key(),
            pub_key: wallet_keypair.public_key(),
            debit: CurrencyUnit::Sat,
            betas: vec![url::Url::parse("https://beta.example.com").expect("valid beta URL")],
            nostr_relays: vec![
                RelayUrl::parse("wss://relay.example.com").expect("valid relay URL"),
            ],
        }
    }

    fn insert_legacy_wallet(db: &Arc<Database>, wallet: WalletConfig) {
        let legacy_wallet = WalletEntry::from(wallet.clone());

        let mut encoded = Vec::new();
        ciborium::into_writer(&legacy_wallet, &mut encoded).expect("serialize legacy wallet");

        let write_txn = db.begin_write().expect("begin write transaction");
        {
            let mut table = write_txn
                .open_table(WALLET_TABLE)
                .expect("open wallet table");

            table
                .insert(wallet.wallet_id.as_bytes(), encoded)
                .expect("insert legacy wallet");
        }

        write_txn.commit().expect("commit legacy wallet");
    }

    fn load_migrated_wallet(db: &Arc<Database>, wallet_id: &str) -> WalletConfig {
        let read_txn = db.begin_read().expect("begin read transaction");
        let table = read_txn
            .open_table(WALLET_TABLE)
            .expect("open migrated wallet table");
        let stored = table
            .get(wallet_id.as_bytes())
            .expect("read migrated wallet")
            .expect("migrated wallet exists");
        let envelope: StoredWallet = borsh::from_slice(stored.value().as_slice())
            .expect("deserialize stored wallet envelope");
        from_stored_wallet_v1(envelope).expect("read wallet from V1 envelope")
    }

    #[test]
    fn migration_name_is_correct() {
        assert_eq!(
            migration_name_for_purse(),
            "0002_add_versioned_envelope_purse"
        );
    }

    #[test]
    fn migration_succeeds_when_wallet_table_does_not_exist() {
        let db = test_database();
        let write_txn = db.begin_write().expect("begin write transaction");
        migration_0002_add_purse_envelope(&write_txn)
            .expect("migration succeeds when wallet table is missing");
        write_txn.commit().expect("commit migration");
    }

    #[test]
    fn migration_converts_legacy_wallet_to_v1_envelope() {
        let db = test_database();
        let original = test_wallet();
        insert_legacy_wallet(&db, original.clone());
        let write_txn = db.begin_write().expect("begin write transaction");
        migration_0002_add_purse_envelope(&write_txn).expect("migrate legacy wallet");
        write_txn.commit().expect("commit migration");
        let migrated = load_migrated_wallet(&db, &original.wallet_id);
        assert_eq!(migrated.wallet_id, original.wallet_id);
        assert_eq!(migrated.name, original.name);
        assert_eq!(migrated.network, original.network);
        assert_eq!(migrated.mint, original.mint);
        assert_eq!(migrated.mint_keyset_infos, original.mint_keyset_infos);
        assert_eq!(migrated.clowder_id, original.clowder_id);
        assert_eq!(migrated.pub_key, original.pub_key);
        assert_eq!(migrated.debit, original.debit);
        assert_eq!(migrated.betas, original.betas);
        assert_eq!(migrated.nostr_relays, original.nostr_relays);
    }

    #[test]
    fn migration_preserves_database_key() {
        let db = test_database();
        let original = test_wallet();
        let expected_key = original.wallet_id.clone();
        insert_legacy_wallet(&db, original);
        let write_txn = db.begin_write().expect("begin write transaction");
        migration_0002_add_purse_envelope(&write_txn).expect("migrate legacy wallet");
        write_txn.commit().expect("commit migration");
        let read_txn = db.begin_read().expect("begin read transaction");
        let table = read_txn
            .open_table(WALLET_TABLE)
            .expect("open wallet table");
        assert!(
            table
                .get(expected_key.as_bytes())
                .expect("read wallet entry")
                .is_some()
        );
    }

    #[test]
    fn migration_converts_multiple_wallets() {
        let db = test_database();
        let first = test_wallet();
        let second = WalletConfig {
            wallet_id: "wallet-456".to_string(),
            name: "Second wallet".to_string(),
            ..test_wallet()
        };
        insert_legacy_wallet(&db, first.clone());
        insert_legacy_wallet(&db, second.clone());
        let write_txn = db.begin_write().expect("begin write transaction");
        migration_0002_add_purse_envelope(&write_txn).expect("migrate all legacy wallets");
        write_txn.commit().expect("commit migration");
        let migrated_first = load_migrated_wallet(&db, &first.wallet_id);
        let migrated_second = load_migrated_wallet(&db, &second.wallet_id);
        assert_eq!(migrated_first.wallet_id, first.wallet_id);
        assert_eq!(migrated_first.name, first.name);
        assert_eq!(migrated_second.wallet_id, second.wallet_id);
        assert_eq!(migrated_second.name, second.name);
    }

    #[test]
    fn migrated_value_is_not_legacy_cbor() {
        let db = test_database();
        let original = test_wallet();
        insert_legacy_wallet(&db, original.clone());
        let write_txn = db.begin_write().expect("begin write transaction");
        migration_0002_add_purse_envelope(&write_txn).expect("migrate legacy wallet");
        write_txn.commit().expect("commit migration");
        let read_txn = db.begin_read().expect("begin read transaction");
        let table = read_txn
            .open_table(WALLET_TABLE)
            .expect("open wallet table");
        let stored = table
            .get(original.wallet_id.as_bytes())
            .expect("read wallet")
            .expect("wallet exists");
        let legacy_result = ciborium::from_reader::<WalletEntry, _>(stored.value().as_slice());
        assert!(
            legacy_result.is_err(),
            "migrated value should no longer deserialize as legacy CBOR"
        );
    }
}
