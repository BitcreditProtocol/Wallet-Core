use crate::{
    MintMeltRepository,
    error::{Error, Result},
};
use async_trait::async_trait;
use bcr_common::cashu::{self, Amount, CurrencyUnit, nut00 as cdk00, nut01 as cdk01};
use bcr_wallet_core::types::MintSummary;
use bitcoin::address::NetworkUnchecked;
use redb::{Database, ReadableDatabase, TableDefinition, TableError};
use std::sync::Arc;
use tokio::task::spawn_blocking;
use uuid::Uuid;

///////////////////////////////////////////// MeltEntry
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Default)]
struct MeltEntry {
    quote_id: String,
    premints: Vec<(
        cdk00::BlindedMessage,
        cashu::secret::Secret,
        cdk01::SecretKey,
        Amount,
    )>,
    kid: Option<cashu::Id>,
}

fn convert_melt_entry_from(qid: String, premints: Option<cdk00::PreMintSecrets>) -> MeltEntry {
    let mut entry = MeltEntry {
        quote_id: qid,
        ..Default::default()
    };
    let Some(premints) = premints else {
        return entry;
    };
    entry.premints = Vec::with_capacity(premints.len());
    let cdk00::PreMintSecrets { secrets, keyset_id } = premints;
    entry.kid = Some(keyset_id);
    for premint in secrets {
        entry.premints.push((
            premint.blinded_message,
            premint.secret,
            premint.r,
            premint.amount,
        ));
    }
    entry
}

fn convert_melt_entry_to(entry: MeltEntry) -> (String, Option<cdk00::PreMintSecrets>) {
    let MeltEntry {
        quote_id,
        premints,
        kid,
    } = entry;
    if kid.is_none() {
        return (quote_id, None);
    }
    let keyset_id = kid.unwrap();
    let mut secrets: Vec<cdk00::PreMint> = Vec::with_capacity(premints.len());
    for premint in premints {
        let pre = cdk00::PreMint {
            blinded_message: premint.0,
            secret: premint.1,
            r: premint.2,
            amount: premint.3,
        };
        secrets.push(pre);
    }
    (quote_id, Some(cdk00::PreMintSecrets { secrets, keyset_id }))
}

///////////////////////////////////////////// MintEntry
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
struct MintEntry {
    quote_id: Uuid,
    amount: bitcoin::Amount,
    address: bitcoin::Address<NetworkUnchecked>,
    expiry: u64,
    premints: Vec<(
        cdk00::BlindedMessage,
        cashu::secret::Secret,
        cdk01::SecretKey,
        Amount,
    )>,
    kid: cashu::Id,
    content: String,
    commitment: bitcoin::secp256k1::schnorr::Signature,
}

fn convert_mint_entry_from(
    quote_id: Uuid,
    amount: bitcoin::Amount,
    address: bitcoin::Address<NetworkUnchecked>,
    expiry: u64,
    premints: cdk00::PreMintSecrets,
    content: String,
    commitment: bitcoin::secp256k1::schnorr::Signature,
) -> MintEntry {
    let cdk00::PreMintSecrets { secrets, keyset_id } = premints;
    let mut entry = MintEntry {
        quote_id,
        amount,
        address,
        expiry,
        premints: Vec::with_capacity(secrets.len()),
        kid: keyset_id,
        content,
        commitment,
    };
    for premint in secrets {
        entry.premints.push((
            premint.blinded_message,
            premint.secret,
            premint.r,
            premint.amount,
        ));
    }
    entry
}

fn convert_mint_entry_to(entry: MintEntry) -> crate::MintRecord {
    let summary = MintSummary {
        quote_id: entry.quote_id,
        amount: entry.amount,
        address: entry.address,
        expiry: entry.expiry,
    };
    let keyset_id = entry.kid;
    let mut secrets: Vec<cdk00::PreMint> = Vec::with_capacity(entry.premints.len());
    for premint in entry.premints {
        let pre = cdk00::PreMint {
            blinded_message: premint.0,
            secret: premint.1,
            r: premint.2,
            amount: premint.3,
        };
        secrets.push(pre);
    }
    crate::MintRecord {
        summary,
        premint: cdk00::PreMintSecrets { secrets, keyset_id },
        content: entry.content,
        commitment: entry.commitment,
    }
}

///////////////////////////////////////////// MintMeltDB
pub struct MintMeltDB {
    db: Arc<Database>,
    melt_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
    mint_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
}

impl MintMeltDB {
    const MELT_BASE_DB_NAME: &'static str = "melts";
    const MINT_BASE_DB_NAME: &'static str = "mints";

    pub fn new(db: Arc<Database>, wallet_id: &str, unit: &CurrencyUnit) -> Result<Self> {
        // Leak once to get static string, because of dynamically generated table names
        let melt_name: &'static str =
            Box::leak(format!("{wallet_id}_{unit}_{}", Self::MELT_BASE_DB_NAME).into_boxed_str());
        let mint_name: &'static str =
            Box::leak(format!("{wallet_id}_{unit}_{}", Self::MINT_BASE_DB_NAME).into_boxed_str());
        let melt_table = TableDefinition::new(melt_name);
        let mint_table = TableDefinition::new(mint_name);
        Ok(MintMeltDB {
            db,
            melt_table,
            mint_table,
        })
    }

    // melt
    fn store_melt_sync(
        db: Arc<Database>,
        melt_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        melt: MeltEntry,
    ) -> Result<String> {
        let id = melt.quote_id.clone();
        let write_txn = db.begin_write()?;

        {
            let mut table = write_txn.open_table(melt_table)?;

            let mut serialized = Vec::new();
            ciborium::into_writer(&melt, &mut serialized)?;
            table.insert(id.as_bytes(), serialized)?;
        }

        write_txn.commit()?;
        Ok(id)
    }

    fn load_melt_sync(
        db: Arc<Database>,
        melt_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        qid: String,
    ) -> Result<Option<MeltEntry>> {
        let read_txn = db.begin_read()?;

        match read_txn.open_table(melt_table) {
            Ok(table) => {
                let entry = table.get(qid.as_bytes())?;
                match entry {
                    Some(e) => {
                        let entry: MeltEntry = ciborium::from_reader(e.value().as_slice())?;
                        Ok(Some(entry))
                    }
                    None => Ok(None),
                }
            }
            Err(TableError::TableDoesNotExist(_)) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn delete_melt_sync(
        db: Arc<Database>,
        melt_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        qid: String,
    ) -> Result<()> {
        let write_txn = db.begin_write()?;

        {
            let mut table = write_txn.open_table(melt_table)?;
            table.remove(qid.as_bytes())?;
        }

        write_txn.commit()?;
        Ok(())
    }

    fn list_melts_sync(
        db: Arc<Database>,
        melt_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
    ) -> Result<Vec<String>> {
        let read_txn = db.begin_read()?;

        match read_txn.open_table(melt_table) {
            Ok(table) => {
                let mut res = Vec::new();
                for (_, v) in table.range::<&[u8]>(..)?.flatten() {
                    let entry: MeltEntry = ciborium::from_reader(v.value().as_slice())?;
                    res.push(entry.quote_id);
                }
                Ok(res)
            }
            Err(TableError::TableDoesNotExist(_)) => Ok(vec![]),
            Err(e) => Err(e.into()),
        }
    }

    // mint
    fn store_mint_sync(
        db: Arc<Database>,
        mint_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        mint: MintEntry,
    ) -> Result<Uuid> {
        let write_txn = db.begin_write()?;

        {
            let mut table = write_txn.open_table(mint_table)?;

            let mut serialized = Vec::new();
            ciborium::into_writer(&mint, &mut serialized)?;
            table.insert(mint.quote_id.as_bytes().as_slice(), serialized)?;
        }

        write_txn.commit()?;
        Ok(mint.quote_id)
    }

    fn load_mint_sync(
        db: Arc<Database>,
        mint_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        qid: Uuid,
    ) -> Result<Option<MintEntry>> {
        let read_txn = db.begin_read()?;

        match read_txn.open_table(mint_table) {
            Ok(table) => {
                let entry = table.get(qid.as_bytes().as_slice())?;
                match entry {
                    Some(e) => {
                        let entry: MintEntry = ciborium::from_reader(e.value().as_slice())?;
                        Ok(Some(entry))
                    }
                    None => Ok(None),
                }
            }
            Err(TableError::TableDoesNotExist(_)) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn delete_mint_sync(
        db: Arc<Database>,
        mint_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        qid: Uuid,
    ) -> Result<()> {
        let write_txn = db.begin_write()?;

        {
            let mut table = write_txn.open_table(mint_table)?;
            table.remove(qid.as_bytes().as_slice())?;
        }

        write_txn.commit()?;
        Ok(())
    }

    fn list_mints_sync(
        db: Arc<Database>,
        mint_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
    ) -> Result<Vec<Uuid>> {
        let read_txn = db.begin_read()?;

        match read_txn.open_table(mint_table) {
            Ok(table) => {
                let mut res = Vec::new();
                for (_, v) in table.range::<&[u8]>(..)?.flatten() {
                    let entry: MintEntry = ciborium::from_reader(v.value().as_slice())?;
                    res.push(entry.quote_id);
                }
                Ok(res)
            }
            Err(TableError::TableDoesNotExist(_)) => Ok(vec![]),
            Err(e) => Err(e.into()),
        }
    }
}

#[async_trait]
impl MintMeltRepository for MintMeltDB {
    // melt
    async fn store_melt(
        &self,
        qid: String,
        premints: Option<cdk00::PreMintSecrets>,
    ) -> Result<String> {
        let db_clone = self.db.clone();
        let table = self.melt_table;
        let entry = convert_melt_entry_from(qid, premints);
        spawn_blocking(move || Self::store_melt_sync(db_clone, table, entry)).await?
    }

    async fn load_melt(&self, qid: String) -> Result<cdk00::PreMintSecrets> {
        let db_clone = self.db.clone();
        let table = self.melt_table;
        let id_clone = qid.clone();
        let res = spawn_blocking(move || Self::load_melt_sync(db_clone, table, id_clone)).await??;
        let entry = res.ok_or(Error::MeltNotFound(qid.clone()))?;
        let (qid, premints) = convert_melt_entry_to(entry);
        premints.ok_or(Error::MeltNotFound(qid))
    }

    async fn list_melts(&self) -> Result<Vec<String>> {
        let db_clone = self.db.clone();
        let table = self.melt_table;
        spawn_blocking(move || Self::list_melts_sync(db_clone, table)).await?
    }

    async fn delete_melt(&self, qid: String) -> Result<()> {
        let db_clone = self.db.clone();
        let table = self.melt_table;
        spawn_blocking(move || Self::delete_melt_sync(db_clone, table, qid)).await??;
        Ok(())
    }
    // mint
    async fn store_mint(
        &self,
        quote_id: Uuid,
        amount: bitcoin::Amount,
        address: bitcoin::Address<NetworkUnchecked>,
        expiry: u64,
        premints: cdk00::PreMintSecrets,
        content: String,
        commitment: bitcoin::secp256k1::schnorr::Signature,
    ) -> Result<Uuid> {
        let db_clone = self.db.clone();
        let table = self.mint_table;
        let entry = convert_mint_entry_from(
            quote_id, amount, address, expiry, premints, content, commitment,
        );
        spawn_blocking(move || Self::store_mint_sync(db_clone, table, entry)).await?
    }

    async fn load_mint(&self, qid: Uuid) -> Result<crate::MintRecord> {
        let db_clone = self.db.clone();
        let table = self.mint_table;
        let res = spawn_blocking(move || Self::load_mint_sync(db_clone, table, qid)).await??;
        let entry = res.ok_or(Error::MintNotFound(qid.clone().to_string()))?;
        Ok(convert_mint_entry_to(entry))
    }

    async fn list_mints(&self) -> Result<Vec<Uuid>> {
        let db_clone = self.db.clone();
        let table = self.mint_table;
        spawn_blocking(move || Self::list_mints_sync(db_clone, table)).await?
    }

    async fn delete_mint(&self, qid: Uuid) -> Result<()> {
        let db_clone = self.db.clone();
        let table = self.mint_table;
        spawn_blocking(move || Self::delete_mint_sync(db_clone, table, qid)).await??;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::error::Error;
    use crate::test_utils::tests::{valid_payment_address_testnet, wallet_id};

    use super::*;
    use bcr_common::{
        cashu::{amount::SplitTarget, nut02 as cdk02},
        core_tests,
    };
    use chrono::Utc;
    use redb::{Builder, backends::InMemoryBackend};

    fn dummy_commitment() -> (String, bitcoin::secp256k1::schnorr::Signature) {
        let content = "dGVzdA==".to_string(); // base64 "test"
        let sig = bitcoin::secp256k1::schnorr::Signature::from_slice(&[0xab; 64])
            .expect("valid signature bytes");
        (content, sig)
    }

    fn get_db(wallet_id: &str, unit: CurrencyUnit) -> MintMeltDB {
        let in_mem = InMemoryBackend::new();
        let db = Arc::new(
            Builder::new()
                .create_with_backend(in_mem)
                .expect("can create in-memory redb"),
        );
        MintMeltDB::new(db, wallet_id, &unit).expect("can create MintMeltDB")
    }

    // melt

    #[tokio::test]
    async fn test_list_melts_empty() {
        let repo = get_db(&wallet_id(), CurrencyUnit::Sat);
        let melts = repo.list_melts().await.expect("list_melts works");
        assert!(melts.is_empty());
    }

    #[tokio::test]
    async fn test_load_melt_missing_returns_error() {
        let repo = get_db(&wallet_id(), CurrencyUnit::Sat);
        let qid = "missing-qid".to_string();
        let err = repo.load_melt(qid.clone()).await.unwrap_err();

        match err {
            Error::MeltNotFound(id) => assert_eq!(id, qid),
            other => panic!("expected Error::MeltNotFound, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_store_list_delete_melt() {
        let repo = get_db(&wallet_id(), CurrencyUnit::Sat);
        let qid = "qid-1".to_string();

        // create premint
        let amounts = [Amount::from(8u64)];
        let (_, mintkeyset) = core_tests::generate_random_ecash_keyset();
        let keyset = cdk02::KeySet::from(mintkeyset.clone());
        let premint =
            cdk00::PreMintSecrets::random(keyset.id, amounts[0], &SplitTarget::None).unwrap();

        let stored_id = repo
            .store_melt(qid.clone(), Some(premint.clone()))
            .await
            .expect("store_melt works");
        assert_eq!(stored_id, qid);

        let melts = repo.list_melts().await.expect("list_melts works");
        assert_eq!(melts, vec![qid.clone()]);

        let melt = repo.load_melt(qid.clone()).await.expect("load_melt works");
        assert_eq!(melt, premint);

        repo.delete_melt(qid.clone())
            .await
            .expect("delete_melt works");

        let melts = repo.list_melts().await.expect("list_melts works");
        assert!(melts.is_empty());

        // load should now error again
        let err = repo.load_melt(qid.clone()).await.unwrap_err();
        match err {
            Error::MeltNotFound(id) => assert_eq!(id, qid),
            other => panic!("expected Error::MeltNotFound, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_store_melt_with_none_premints_load_returns_not_found() {
        let repo = get_db(&wallet_id(), CurrencyUnit::Sat);
        let qid = "qid-none-premints".to_string();

        repo.store_melt(qid.clone(), None)
            .await
            .expect("store_melt works");

        let err = repo.load_melt(qid.clone()).await.unwrap_err();
        match err {
            Error::MeltNotFound(id) => assert_eq!(id, qid),
            other => panic!("expected Error::MeltNotFound, got: {other:?}"),
        }
    }

    // mint

    #[tokio::test]
    async fn test_list_mints_empty() {
        let repo = get_db(&wallet_id(), CurrencyUnit::Sat);
        let mints = repo.list_mints().await.expect("list_mints works");
        assert!(mints.is_empty());
    }

    #[tokio::test]
    async fn test_load_mint_missing_returns_error() {
        let repo = get_db(&wallet_id(), CurrencyUnit::Sat);
        let qid = Uuid::new_v4();
        let err = repo.load_mint(qid).await.unwrap_err();

        match err {
            Error::MintNotFound(id) => assert_eq!(id, qid.to_string()),
            other => panic!("expected Error::MintNotFound, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_store_load_mint() {
        let repo = get_db(&wallet_id(), CurrencyUnit::Sat);

        let qid = Uuid::new_v4();
        let amount = bitcoin::Amount::from_sat(12345);
        let address = valid_payment_address_testnet();
        let expiry = Utc::now().timestamp() as u64;
        let (content, commitment) = dummy_commitment();

        let (_, mintkeyset) = core_tests::generate_random_ecash_keyset();
        let keyset = cdk02::KeySet::from(mintkeyset.clone());
        let premint =
            cdk00::PreMintSecrets::random(keyset.id, Amount::from(12345u64), &SplitTarget::None)
                .unwrap();

        let stored = repo
            .store_mint(
                qid,
                amount,
                address.clone(),
                expiry,
                premint.clone(),
                content.clone(),
                commitment,
            )
            .await
            .expect("store_mint works");
        assert_eq!(stored, qid);

        let record = repo.load_mint(qid).await.expect("load_mint works");
        assert_eq!(record.summary.quote_id, qid);
        assert_eq!(record.summary.amount, amount);
        assert_eq!(record.summary.address, address);
        assert_eq!(record.summary.expiry, expiry);
        assert_eq!(record.premint, premint);
        assert_eq!(record.content, content);
        assert_eq!(record.commitment, commitment);
    }

    #[tokio::test]
    async fn test_list_mints_after_inserts() {
        let repo = get_db(&wallet_id(), CurrencyUnit::Sat);

        let q1 = Uuid::new_v4();
        let q2 = Uuid::new_v4();
        let (content, commitment) = dummy_commitment();

        let (_, mintkeyset) = core_tests::generate_random_ecash_keyset();
        let keyset = cdk02::KeySet::from(mintkeyset.clone());
        let premint1 =
            cdk00::PreMintSecrets::random(keyset.id, Amount::from(1u64), &SplitTarget::None)
                .unwrap();
        let premint2 =
            cdk00::PreMintSecrets::random(keyset.id, Amount::from(2u64), &SplitTarget::None)
                .unwrap();

        repo.store_mint(
            q1,
            bitcoin::Amount::from_sat(1),
            valid_payment_address_testnet(),
            111,
            premint1,
            content.clone(),
            commitment,
        )
        .await
        .expect("store_mint q1");
        repo.store_mint(
            q2,
            bitcoin::Amount::from_sat(2),
            valid_payment_address_testnet(),
            222,
            premint2,
            content.clone(),
            commitment,
        )
        .await
        .expect("store_mint q2");

        let mut ids = repo.list_mints().await.expect("list_mints works");
        ids.sort();

        let mut expected = vec![q1, q2];
        expected.sort();

        assert_eq!(ids, expected);
    }

    #[tokio::test]
    async fn test_delete_mint_removes_entry() {
        let repo = get_db(&wallet_id(), CurrencyUnit::Sat);

        let qid = Uuid::new_v4();
        let (content, commitment) = dummy_commitment();
        let (_, mintkeyset) = core_tests::generate_random_ecash_keyset();
        let keyset = cdk02::KeySet::from(mintkeyset.clone());
        let premint =
            cdk00::PreMintSecrets::random(keyset.id, Amount::from(42u64), &SplitTarget::None)
                .unwrap();

        repo.store_mint(
            qid,
            bitcoin::Amount::from_sat(42),
            valid_payment_address_testnet(),
            999,
            premint,
            content,
            commitment,
        )
        .await
        .expect("store_mint works");

        repo.delete_mint(qid).await.expect("delete_mint works");

        let err = repo.load_mint(qid).await.unwrap_err();
        match err {
            Error::MintNotFound(id) => assert_eq!(id, qid.to_string()),
            other => panic!("expected Error::MintNotFound, got: {other:?}"),
        }
    }
}
