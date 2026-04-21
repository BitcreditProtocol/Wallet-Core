pub mod mintmelt;
pub mod pocket;
pub mod purse;
pub mod transaction;

use crate::error::Result;
pub use ::redb::Database;
use bcr_common::cashu::CurrencyUnit;
use std::path::Path;
use std::sync::Arc;

pub fn create_db(path: impl AsRef<Path>) -> Result<Database> {
    let db = Database::create(&path)?;
    Ok(db)
}

pub async fn build_pursedb(_db_version: u32, db: Arc<Database>) -> Result<purse::PurseDB> {
    purse::PurseDB::new(db)
}

pub async fn build_wallet_dbs(
    _db_version: u32,
    wallet_id: &str,
    debit: &CurrencyUnit,
    db: Arc<Database>,
) -> Result<(
    transaction::TransactionDB,
    (pocket::PocketDB, mintmelt::MintMeltDB),
)> {
    let txdb = transaction::TransactionDB::new(db.clone(), wallet_id)?;
    let debitdb = pocket::PocketDB::new(db.clone(), wallet_id, debit)?;
    let mintmeltdb = mintmelt::MintMeltDB::new(db.clone(), wallet_id, debit)?;
    Ok((txdb, (debitdb, mintmeltdb)))
}
