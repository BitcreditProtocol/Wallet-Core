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
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

pub fn create_db(path: impl AsRef<Path>) -> Result<Database> {
    let db = Database::create(&path)?;
    Ok(db)
}

pub async fn build_pursedbs(
    _db_version: u32,
    db: Arc<Database>,
) -> Result<(
    purse::PurseDB,
    HashMap<bitcoin::Network, Arc<contact::ContactDB>>,
)> {
    // Execute Migrations for purse
    migration::migrate_purse(db.clone()).await?;

    let mut contact_dbs = HashMap::new();
    contact_dbs.insert(
        bitcoin::Network::Testnet,
        Arc::new(contact::ContactDB::new(
            db.clone(),
            bitcoin::Network::Testnet,
        )?),
    );
    contact_dbs.insert(
        bitcoin::Network::Testnet4,
        Arc::new(contact::ContactDB::new(
            db.clone(),
            bitcoin::Network::Testnet4,
        )?),
    );
    contact_dbs.insert(
        bitcoin::Network::Regtest,
        Arc::new(contact::ContactDB::new(
            db.clone(),
            bitcoin::Network::Regtest,
        )?),
    );
    contact_dbs.insert(
        bitcoin::Network::Signet,
        Arc::new(contact::ContactDB::new(
            db.clone(),
            bitcoin::Network::Signet,
        )?),
    );
    contact_dbs.insert(
        bitcoin::Network::Bitcoin,
        Arc::new(contact::ContactDB::new(
            db.clone(),
            bitcoin::Network::Bitcoin,
        )?),
    );

    let pursedb = purse::PurseDB::new(db)?;
    Ok((pursedb, contact_dbs))
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
    let pending_payment_requests_db =
        payment_request::PaymentRequestDB::new(db.clone(), wallet_id)?;
    Ok((
        txdb,
        debitdb,
        mintmeltdb,
        nostrdb,
        pending_payment_requests_db,
    ))
}
