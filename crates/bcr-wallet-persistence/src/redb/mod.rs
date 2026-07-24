pub mod contact;
pub mod migration;
pub mod mintmelt;
pub mod nostr;
pub mod payment_request;
pub mod pocket;
pub mod purse;
pub mod transaction;

use crate::error::Result;
pub use ::redb::Database;
use bcr_common::cashu::CurrencyUnit;
use bcr_wallet_core::types::Seed;
use bcr_wallet_core::util::keypair_from_seed;
use std::path::Path;
use std::sync::Arc;

pub fn create_db(path: impl AsRef<Path>) -> Result<Database> {
    let db = Database::create(&path)?;
    Ok(db)
}

pub async fn build_pursedb(_db_version: u32, db: Arc<Database>) -> Result<purse::PurseDB> {
    // Execute Migrations for purse
    migration::migrate_purse(db.clone()).await?;

    purse::PurseDB::new(db)
}

pub async fn build_wallet_dbs(
    _db_version: u32,
    wallet_id: &str,
    debit: &CurrencyUnit,
    db: Arc<Database>,
    seed: Seed,
) -> Result<(
    transaction::TransactionDB,
    pocket::PocketDB,
    mintmelt::MintMeltDB,
    nostr::NostrDB,
    contact::ContactDB,
    payment_request::PaymentRequestDB,
)> {
    let keys = keypair_from_seed(seed);

    // Execute Migrations for wallet
    let wallet_namespace =
        migration::collect_wallet_namespace(wallet_id.to_owned(), debit.to_owned(), keys);
    migration::migrate_wallet(db.clone(), wallet_namespace).await?;

    let txdb = transaction::TransactionDB::new(db.clone(), wallet_id)?;
    let debitdb = pocket::PocketDB::new(db.clone(), wallet_id, debit, keys)?;
    let mintmeltdb = mintmelt::MintMeltDB::new(db.clone(), wallet_id, debit)?;
    let nostrdb = nostr::NostrDB::new(db.clone(), wallet_id)?;
    let contactdb = contact::ContactDB::new(db.clone(), wallet_id)?;
    let pending_payment_requests_db =
        payment_request::PaymentRequestDB::new(db.clone(), wallet_id)?;
    Ok((
        txdb,
        debitdb,
        mintmeltdb,
        nostrdb,
        contactdb,
        pending_payment_requests_db,
    ))
}
