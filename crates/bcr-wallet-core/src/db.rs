use cashu::CurrencyUnit;

use super::*;

pub async fn build_pursedb(
    _db_version: u32,
    db: Arc<redb::Database>,
) -> Result<prod::ProductionPurseRepository> {
    prod::ProductionPurseRepository::new(db)
}

pub async fn build_wallet_dbs(
    _db_version: u32,
    wallet_id: &str,
    debit: &CurrencyUnit,
    credit: Option<&CurrencyUnit>,
    _local: LocalDB,
    db: Arc<redb::Database>,
) -> Result<(
    prod::ProductionTransactionRepository,
    (
        (
            prod::ProductionPocketRepository,
            prod::ProductionMintMeltRepository,
        ),
        Option<prod::ProductionPocketRepository>,
    ),
)> {
    let txdb = prod::ProductionTransactionRepository::new(db.clone(), wallet_id)?;
    let debitdb = prod::ProductionPocketRepository::new(db.clone(), debit)?;
    let mintmeltdb = prod::ProductionMintMeltRepository::new(db.clone(), debit)?;
    let creditdb = if let Some(cr) = credit {
        Some(prod::ProductionPocketRepository::new(db, cr)?)
    } else {
        None
    };
    Ok((txdb, ((debitdb, mintmeltdb), creditdb)))
}

pub async fn build_settingsdb(
    _db_version: u32,
    db: Arc<redb::Database>,
) -> Result<prod::ProductionSettingsRepository> {
    prod::ProductionSettingsRepository::new(db)
}

pub async fn build_jobsdb(
    _db_version: u32,
    db: Arc<redb::Database>,
) -> Result<prod::ProductionJobsRepository> {
    prod::ProductionJobsRepository::new(db)
}
