use crate::job::JobState;
use crate::mint::MintConnector;
use crate::{
    config::{Config, SameMintSafeMode, Settings},
    purse::Wallet,
    types::{PaymentSummary, RedemptionSummary},
    wallet::{CreditPocket, WalletBalance},
};
use bcr_wallet_lib::wallet::Token;
use bitcoin::{
    hashes::{Hash, HashEngine, sha256},
    hex::DisplayHex,
};
use cashu::{CurrencyUnit, KeySetInfo, MintInfo, MintUrl};
use cdk::wallet::{MintConnector as MintCon, types::TransactionId};
use chrono::Utc;
use error::{Error, Result};
use std::{
    collections::{HashMap, HashSet},
    str::FromStr,
    sync::Arc,
};
use tokio::sync::RwLock;
use uuid::Uuid;

mod clowder_models;
pub mod config;
mod db;
pub mod error;
mod job;
mod mint;
pub mod persistence;
pub mod pocket;
mod purse;
mod restore;
mod types;
pub mod utils;
pub mod wallet;

pub type TStamp = chrono::DateTime<chrono::Utc>;

mod prod {
    pub type ProductionPocketRepository = crate::persistence::redb::PocketDB;
    pub type ProductionMintMeltRepository = crate::persistence::redb::MintMeltDB;
    pub type ProductionPurseRepository = crate::persistence::redb::PurseDB;
    pub type ProductionTransactionRepository = crate::persistence::redb::TransactionDB;
    pub type ProductionSettingsRepository = crate::persistence::redb::SettingsDB;
    pub type ProductionJobsRepository = crate::persistence::redb::JobsDB;
}

type ProductionConnector = crate::mint::HttpClientExt;
type ProductionDebitPocket = crate::pocket::debit::Pocket;
type ProductionCreditPocket = crate::pocket::credit::Pocket;
type ProductionWallet = crate::wallet::Wallet<ProductionDebitPocket>;
type ProductionPurse = crate::purse::Purse<ProductionWallet>;

mod sync {
    pub trait SendSync: Send + Sync {}
    impl<T> SendSync for T where T: Send + Sync {}
}

pub enum LocalDB {
    Delete,
    Keep,
}

pub struct AppState {
    purse: Arc<ProductionPurse>,
    settings: Arc<prod::ProductionSettingsRepository>,
    jobs: Arc<prod::ProductionJobsRepository>,
    db: Arc<redb::Database>,
}

impl AppState {
    pub const DB_VERSION: u32 = 1;

    pub async fn initialize(db_path: &str) -> Result<Self> {
        tracing::debug!("Initializing API");

        // Open Database file - only allowed to do once!
        let db = Arc::new(redb::Database::create(db_path)?);

        let pursedb = db::build_pursedb(AppState::DB_VERSION, db.clone()).await?;
        let settingsdb = Arc::new(db::build_settingsdb(AppState::DB_VERSION, db.clone()).await?);
        let jobsdb = Arc::new(db::build_jobsdb(AppState::DB_VERSION, db.clone()).await?);
        let settings = settingsdb.clone().load().await?;
        let config = Config::new(settings)?;
        let nostr_cl = nostr_sdk::Client::new(config.nostr_signer);
        for relay in &config.relays {
            nostr_cl.add_relay(relay).await?;
        }
        nostr_cl.connect().await;
        let http_cl = reqwest::Client::new();
        let purse = ProductionPurse::new(pursedb, http_cl, nostr_cl, config.nprofile).await?;
        let mut appstate = AppState::new(Arc::new(purse), settingsdb, jobsdb, db);
        appstate.load_wallets().await?;
        Ok(appstate)
    }

    fn new(
        purse: Arc<ProductionPurse>,
        settings: Arc<prod::ProductionSettingsRepository>,
        jobs: Arc<prod::ProductionJobsRepository>,
        db: Arc<redb::Database>,
    ) -> Self {
        tracing::debug!("Creating new AppState");
        Self {
            purse,
            settings,
            jobs,
            db,
        }
    }

    pub async fn load_settings(&self) -> Result<Settings> {
        self.settings.clone().load().await
    }

    pub async fn load_wallets(&mut self) -> Result<()> {
        tracing::debug!("AppState::load_wallets()");

        let settings = self.load_settings().await?;
        let purse = self.get_purse();
        let db = self.get_db();
        let w_ids = purse.list_wallets().await?;
        for wid in w_ids {
            tracing::debug!("Loading wallet with id: {wid}");
            let w_cfg = purse.load_wallet_config(&wid).await?;
            if w_cfg.network != settings.network {
                tracing::info!(
                    "Skipping wallet {wid} with network {:?}, expected {:?}",
                    w_cfg.network,
                    settings.network,
                );
                continue;
            }
            let wallet = build_wallet(
                w_cfg.name,
                w_cfg.network,
                w_cfg.mint,
                w_cfg.mnemonic,
                LocalDB::Keep,
                Self::DB_VERSION,
                settings.same_mint_safe_mode,
                db.clone(),
            )
            .await?;
            purse.add_wallet(wallet).await?;
        }
        Ok(())
    }

    async fn get_wallet(&self, idx: usize) -> Result<Arc<RwLock<ProductionWallet>>> {
        let purse = self.get_purse();
        purse
            .get_wallet(idx)
            .await
            .ok_or(Error::WalletNotFound(idx))
    }

    fn get_purse(&self) -> Arc<ProductionPurse> {
        self.purse.clone()
    }

    fn get_settingsdb(&self) -> Arc<prod::ProductionSettingsRepository> {
        self.settings.clone()
    }

    fn get_jobsdb(&self) -> Arc<prod::ProductionJobsRepository> {
        self.jobs.clone()
    }

    fn get_db(&self) -> Arc<redb::Database> {
        self.db.clone()
    }
    // methods

    pub async fn add_wallet(
        &self,
        name: String,
        mint_url: MintUrl,
        mnemonic: String,
    ) -> Result<usize> {
        tracing::debug!("Adding a new wallet for mint {name}, {mint_url}, {mnemonic}");

        let settings = self.get_settingsdb().load().await?;
        let mnemonic = bip39::Mnemonic::from_str(&mnemonic)?;
        let wallet = build_wallet(
            name,
            settings.network,
            mint_url,
            mnemonic,
            LocalDB::Keep,
            AppState::DB_VERSION,
            settings.same_mint_safe_mode,
            self.get_db(),
        )
        .await?;

        let purse = self.get_purse();
        let idx = purse.add_wallet(wallet).await?;

        Ok(idx)
    }

    pub async fn restore_wallet(
        &self,
        name: String,
        mint_url: MintUrl,
        mnemonic: String,
    ) -> Result<usize> {
        tracing::debug!("Restoring a new wallet for mint {name}, {mint_url}");

        let settings = self.get_settingsdb().load().await?;
        let mnemonic = bip39::Mnemonic::from_str(&mnemonic)?;
        let wallet = build_wallet(
            name,
            settings.network,
            mint_url,
            mnemonic,
            LocalDB::Delete,
            AppState::DB_VERSION,
            settings.same_mint_safe_mode,
            self.get_db(),
        )
        .await?;
        wallet.restore_local_proofs().await?;

        let purse = self.get_purse();
        let idx = purse.add_wallet(wallet).await?;
        Ok(idx)
    }

    pub async fn get_wallet_name(&self, idx: usize) -> Result<String> {
        tracing::debug!("name for wallet {idx}");

        let wallet = self.get_wallet(idx).await?;
        Ok(wallet.read().await.name())
    }

    pub async fn get_wallet_mint_url(&self, idx: usize) -> Result<String> {
        tracing::debug!("mint_url for wallet {idx}");
        let wallet = self.get_wallet(idx).await?;
        Ok(wallet.read().await.mint_url()?.to_string())
    }

    pub async fn get_wallet_currency_unit(&self, idx: usize) -> Result<WalletCurrencyUnit> {
        tracing::debug!("wallet_currency_unit({idx})");
        let wallet = self.get_wallet(idx).await?;
        Ok(WalletCurrencyUnit {
            credit: wallet.read().await.credit_unit().to_string(),
            debit: wallet.read().await.debit_unit().to_string(),
        })
    }

    pub async fn get_wallet_balance(&self, idx: usize) -> Result<WalletBalance> {
        tracing::debug!("wallet_balance({idx})");

        let wallet = self.get_wallet(idx).await?;
        wallet.read().await.balance().await
    }

    pub async fn wallet_receive_token(&self, idx: usize, token: String) -> Result<TransactionId> {
        let tstamp = chrono::Utc::now().timestamp() as u64;
        tracing::debug!("wallet_receive({idx}, {token}, {tstamp})");

        let token = bcr_wallet_lib::wallet::Token::from_str(&token)?;
        let wallet = self.get_wallet(idx).await?;
        let tx_id = wallet.read().await.receive_token(token, tstamp).await?;
        Ok(tx_id)
    }

    pub async fn wallet_redeem_credit(&self, idx: usize) -> Result<cashu::Amount> {
        tracing::debug!("wallet_redeem_credit({idx})");

        let wallet = self.get_wallet(idx).await?;
        let amount_redeemed = wallet.read().await.redeem_credit().await?;
        Ok(amount_redeemed)
    }

    pub async fn wallet_list_redemptions(
        &self,
        idx: usize,
        payment_window: std::time::Duration,
    ) -> Result<Vec<RedemptionSummary>> {
        tracing::debug!(
            "wallet_list_redemptions({idx}, {})",
            payment_window.as_secs()
        );

        let wallet = self.get_wallet(idx).await?;
        let redemptions = wallet.read().await.list_redemptions(payment_window).await?;
        Ok(redemptions)
    }

    pub async fn wallet_clean_local_db(&self, idx: usize) -> Result<u32> {
        tracing::debug!("wallet_clean_local_db({idx})");

        let wallet = self.get_wallet(idx).await?;
        let deleted = wallet.read().await.clean_local_db().await?;
        Ok(deleted)
    }

    pub async fn purse_migrate_rabid(&self) -> Result<()> {
        let tstamp = chrono::Utc::now().timestamp() as u64;
        tracing::debug!("purse_migrate_rabid");

        let purse = self.get_purse();
        purse.migrate_rabid_wallets(tstamp).await?;

        Ok(())
    }

    pub async fn wallet_load_tx(&self, idx: usize, tx_id: &str) -> Result<Transaction> {
        tracing::debug!("wallet_load_tx({idx}, {tx_id})");

        let tx_id = TransactionId::from_str(tx_id)?;
        let wallet = self.get_wallet(idx).await?;
        let tx = wallet.read().await.load_tx(tx_id).await?;
        Ok(Transaction::from(tx))
    }

    pub async fn wallet_prepare_pay_by_token(
        &self,
        idx: usize,
        amount: u64,
        unit: String,
        description: Option<String>,
    ) -> Result<PaymentSummary> {
        tracing::debug!("wallet_prepare_pay_by_token({idx}, {amount}, {unit}, {description:?})");
        let amount = cashu::Amount::from(amount);
        let unit = cashu::CurrencyUnit::from_str(&unit)
            .map_err(|_| Error::InvalidCurrencyUnit(unit.clone()))?;
        let purse = self.get_purse();
        let summary = purse
            .prepare_pay_by_token(idx, amount, unit, description)
            .await?;
        Ok(summary)
    }

    pub async fn wallet_pay_by_token(&self, rid: String) -> Result<CreatedToken> {
        let tstamp = chrono::Utc::now().timestamp() as u64;
        tracing::debug!("wallet_pay_by_token({rid}, {tstamp})");
        let rid = Uuid::from_str(&rid)?;
        let purse = self.get_purse();
        let (tx_id, token) = purse.pay_by_token(rid, tstamp).await?;
        Ok(CreatedToken { tx_id, token })
    }

    pub async fn wallet_prepare_payment(
        &self,
        idx: usize,
        input: String,
    ) -> Result<PaymentSummary> {
        let now = chrono::Utc::now().timestamp() as u64;
        tracing::debug!("wallet_prepare_payment({idx}, {input})");

        let purse = self.get_purse();
        let summary = purse.prepare_pay(idx, input, now).await?;
        Ok(summary)
    }

    pub async fn wallet_pay(&self, rid: String) -> Result<TransactionId> {
        let tstamp = chrono::Utc::now().timestamp() as u64;
        tracing::debug!("wallet_pay({rid}, {tstamp})");

        let purse = self.get_purse();
        let rid = Uuid::from_str(&rid)?;
        let tx_id = purse.pay(rid, tstamp).await?;
        Ok(tx_id)
    }

    pub async fn wallet_prepare_payment_request(
        &self,
        idx: usize,
        amount: u64,
        unit: String,
        description: Option<String>,
    ) -> Result<PaymentRequest> {
        tracing::debug!("wallet_prepare_pay_request({idx}, {amount}, {unit}, {description:?})");

        let amount = cashu::Amount::from(amount);
        let unit = if unit.trim().is_empty() {
            None
        } else {
            cashu::CurrencyUnit::from_str(&unit).ok()
        };
        let purse = self.get_purse();
        let request = purse
            .prepare_payment_request(amount, unit, description)
            .await?;
        Ok(PaymentRequest {
            p_id: request.payment_id.clone().unwrap_or_default(),
            request: request.to_string(),
        })
    }

    pub async fn wallet_check_received_payment(
        &self,
        max_wait_sec: u64,
        p_id: String,
    ) -> Result<Option<TransactionId>> {
        tracing::debug!("wallet_check_received_payment({p_id})");

        let p_id = Uuid::from_str(&p_id)?;
        let purse = self.get_purse();
        let max_wait = core::time::Duration::from_secs(max_wait_sec);
        let tx_id = purse.check_received_payment(max_wait, p_id).await?;
        Ok(tx_id)
    }

    pub async fn wallet_check_pending_melts(&self, idx: usize) -> Result<cashu::Amount> {
        tracing::debug!("wallet_check_pending_melts({idx})");

        let wallet = self.get_wallet(idx).await?;
        wallet.read().await.check_pending_melts().await
    }

    pub async fn wallet_list_tx_ids(&self, idx: usize) -> Result<Vec<TransactionId>> {
        tracing::debug!("wallet_list_tx_ids({idx})");

        let wallet = self.get_wallet(idx).await?;
        let tx_ids = wallet.read().await.list_tx_ids().await?;
        Ok(tx_ids)
    }

    pub async fn get_wallets_ids(&self) -> Result<Vec<usize>> {
        tracing::debug!("get_wallet_ids");
        let purse = self.get_purse();
        Ok(purse.ids().await.iter().map(|id| *id as usize).collect())
    }

    pub async fn get_wallets_names(&self) -> Result<Vec<String>> {
        tracing::debug!("get_wallet_names");
        let purse = self.get_purse();
        purse.names().await
    }

    pub fn generate_random_mnemonic(&self, mnemonic_len: u32) -> String {
        let mnemonic_len = if mnemonic_len == 0 { 12 } else { mnemonic_len };
        tracing::info!("Generate random {}-word mnemonic", mnemonic_len);

        const VALID_MNEMONIC_LENGTHS: [u32; 5] = [12, 15, 18, 21, 24];
        assert!(
            VALID_MNEMONIC_LENGTHS.contains(&mnemonic_len),
            "word count must be one of: {VALID_MNEMONIC_LENGTHS:?}"
        );
        let returned =
            bip39::Mnemonic::generate_in(bip39::Language::English, mnemonic_len as usize);
        match returned {
            Ok(mnemonic) => mnemonic.to_string(),
            Err(e) => {
                tracing::error!("generate_random_mnemonic({mnemonic_len}): {e}");
                String::default()
            }
        }
    }

    /// Checks when the jobs were run the last time and if it's greater than 1 day
    /// then it runs the jobs.
    /// This should be called in an interval and on app initialization
    pub async fn run_jobs(&self) -> Result<()> {
        tracing::info!("Run Jobs triggered");
        let last_run_ts = self.get_jobsdb().load().await?.last_run;
        let now = Utc::now();

        let diff = now.signed_duration_since(last_run_ts);

        if diff.num_days() < 1 {
            tracing::info!("Run Jobs called, but not yet 1 day since last job run.");
            return Ok(());
        }

        if self.execute_jobs().await {
            tracing::info!("Run Jobs executed successfully");
            self.get_jobsdb().store(JobState { last_run: now }).await?;
        } else {
            tracing::info!(
                "Run Jobs executed with some errors - will run again at the next interval."
            );
        }

        Ok(())
    }

    /// Actually runs the jobs - gets called via `run_jobs` for creating a
    /// regular job interval, but calling this directly forces a job run right now
    /// Returns a boolean indicating if all jobs ran to success
    pub async fn execute_jobs(&self) -> bool {
        let mut job_failed = false;
        if let Err(e) = self.purse_migrate_rabid().await {
            job_failed = true;
            tracing::error!("Error running purse_migrate_rabid job: {e}");
        }

        let wallet_ids = self.get_purse().ids().await;
        for wallet_id in wallet_ids.iter() {
            match self.wallet_redeem_credit(*wallet_id as usize).await {
                Ok(amount) => {
                    tracing::info!(
                        "Redeemed credit for wallet {wallet_id}. Amount redeemed: {amount}"
                    );
                }
                Err(e) => {
                    job_failed = true;
                    tracing::error!(
                        "Error running wallet_redeem_credit job for wallet {wallet_id}: {e}"
                    );
                }
            };
        }
        // successful = true
        !job_failed
    }
}

// FFI types

#[derive(Default, Clone, Debug)]
pub struct PaymentRequest {
    pub request: String,
    pub p_id: String,
}

#[derive(Default, Clone, Debug)]
pub struct WalletCurrencyUnit {
    pub credit: String,
    pub debit: String,
}

#[derive(Debug, Clone, Copy, Default)]
pub enum TransactionDirection {
    #[default]
    Incoming,
    Outgoing,
}
impl std::convert::From<cdk::wallet::types::TransactionDirection> for TransactionDirection {
    fn from(dir: cdk::wallet::types::TransactionDirection) -> Self {
        match dir {
            cdk::wallet::types::TransactionDirection::Incoming => TransactionDirection::Incoming,
            cdk::wallet::types::TransactionDirection::Outgoing => TransactionDirection::Outgoing,
        }
    }
}
#[derive(Debug, Clone, Copy, Default)]
pub enum PaymentType {
    #[default]
    NotApplicable,
    Token,
    Cdk18,
    Lightning,
}

impl std::convert::From<types::PaymentType> for PaymentType {
    fn from(ptype: types::PaymentType) -> Self {
        match ptype {
            types::PaymentType::NotApplicable => PaymentType::NotApplicable,
            types::PaymentType::Token => PaymentType::Token,
            types::PaymentType::Cdk18 => PaymentType::Cdk18,
            types::PaymentType::Lightning => PaymentType::Lightning,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub enum TransactionStatus {
    #[default]
    NotApplicable,
    Pending,
    CashedIn,
    Canceled,
}

impl std::convert::From<types::TransactionStatus> for TransactionStatus {
    fn from(status: types::TransactionStatus) -> Self {
        match status {
            types::TransactionStatus::NotApplicable => TransactionStatus::NotApplicable,
            types::TransactionStatus::Pending => TransactionStatus::Pending,
            types::TransactionStatus::CashedIn => TransactionStatus::CashedIn,
            types::TransactionStatus::Canceled => TransactionStatus::Canceled,
        }
    }
}

#[derive(Default, Clone, Debug)]
pub struct Transaction {
    pub amount: u64,
    pub fees: u64,
    pub unit: String,
    pub tstamp: u64,
    pub direction: TransactionDirection,
    pub memo: String,
    pub ptype: PaymentType,
    pub status: TransactionStatus,
}

impl std::convert::From<cdk::wallet::types::Transaction> for Transaction {
    fn from(tx: cdk::wallet::types::Transaction) -> Self {
        let status = TransactionStatus::from(types::get_transaction_status(&tx.metadata));
        let ptype = PaymentType::from(types::get_payment_type(&tx.metadata));
        Self {
            amount: u64::from(tx.amount),
            fees: u64::from(tx.fee),
            unit: tx.unit.to_string(),
            direction: TransactionDirection::from(tx.direction),
            tstamp: tx.timestamp,
            memo: tx.memo.unwrap_or_default(),
            ptype,
            status,
        }
    }
}

#[derive(Clone, Debug)]
pub struct CreatedToken {
    pub tx_id: TransactionId,
    pub token: Token,
}

// Wallet Initialization
// will be used in future refactoring see issue #92
#[allow(dead_code)]
fn build_mint_id(url: &MintUrl, info: &MintInfo) -> Vec<u8> {
    if let Some(pk) = info.pubkey {
        pk.to_bytes().to_vec()
    } else if let Some(name) = &info.name {
        name.to_string().as_bytes().to_vec()
    } else {
        url.to_string().as_bytes().to_vec()
    }
}

fn find_currency_units(
    keyset_infos: &[KeySetInfo],
) -> Result<(CurrencyUnit, Option<CurrencyUnit>)> {
    let currencies = keyset_infos
        .iter()
        .map(|k| k.unit.clone())
        .collect::<HashSet<_>>();
    if currencies.len() > 2 {
        return Err(Error::Unsupported(
            "Mint supports more than 2 currencies, not supported yet".into(),
        ));
    }
    let credit_unit = currencies
        .iter()
        .find(|unit| unit.to_string().starts_with("cr"));
    let debit_unit = currencies
        .iter()
        .find(|unit| !unit.to_string().starts_with("cr"));
    if debit_unit.is_none() {
        let currencies = currencies.iter().cloned().collect();
        return Err(Error::NoDebitCurrencyInMint(currencies));
    }
    let debit_unit = debit_unit.unwrap();
    Ok((debit_unit.clone(), credit_unit.cloned()))
}

fn build_wallet_id(seed: &[u8; 64]) -> String {
    let mut hasher = sha256::HashEngine::default();
    hasher.input(seed);
    sha256::Hash::from_engine(hasher)
        .as_byte_array()
        .as_hex()
        .to_string()
}

async fn build_wallet(
    name: String,
    network: bitcoin::Network,
    mint_url: cashu::MintUrl,
    mnemonic: bip39::Mnemonic,
    local: LocalDB,
    db_version: u32,
    same_mint_safe_mode: SameMintSafeMode,
    db: Arc<redb::Database>,
) -> Result<ProductionWallet> {
    let seed = mnemonic.to_seed("");
    // retrieving mint details
    let client = ProductionConnector::new(mint_url.clone());
    let keyset_infos = client.get_mint_keysets().await?.keysets;
    let (debit_unit, credit_unit) = find_currency_units(&keyset_infos)?;
    // building wallet dbs
    let wallet_id = build_wallet_id(&seed);
    let (tx_repo, ((debitdb, mintmeltdb), creditdb)) = crate::db::build_wallet_dbs(
        db_version,
        &wallet_id,
        &debit_unit,
        credit_unit.as_ref(),
        local,
        db,
    )
    .await?;
    // building the debit pocket
    let debit_pocket = ProductionDebitPocket::new(
        debit_unit.clone(),
        Arc::new(debitdb),
        Arc::new(mintmeltdb),
        seed,
    );
    // building the credit pocket
    let credit_pocket: Box<dyn CreditPocket> = if let Some(unit) = &credit_unit {
        let creditdb = creditdb.expect("Credit pocket DB should be present");
        let pocket = ProductionCreditPocket::new(unit.clone(), Arc::new(creditdb), seed);
        Box::new(pocket)
    } else {
        tracing::warn!("app::add_wallet: credit_pocket = DummyPocket");
        Box::new(crate::pocket::credit::DummyPocket {})
    };

    let mut beta_clients = HashMap::<cashu::MintUrl, Box<dyn MintConnector>>::new();

    let betas_urls = client.get_clowder_betas().await?;
    for beta in betas_urls.clone() {
        let beta_client = ProductionConnector::new(beta.clone());
        beta_clients.insert(beta, Box::new(beta_client));
    }
    // When same_mint_safe_mode is enabled, wrap the client with SentinelClient
    // to send events to sentinel nodes for monitoring
    let client = if matches!(same_mint_safe_mode, SameMintSafeMode::Disabled) {
        Box::new(client) as Box<dyn MintConnector>
    } else {
        let cl = crate::mint::SentinelClient::new(client, betas_urls);
        Box::new(cl) as Box<dyn MintConnector>
    };
    let new_wallet: ProductionWallet = ProductionWallet::new(
        network,
        client,
        Box::new(tx_repo),
        (debit_pocket, credit_pocket),
        name,
        wallet_id,
        mnemonic,
        beta_clients,
        Box::new(|url| Box::new(crate::mint::HttpClientExt::new(url))),
        same_mint_safe_mode,
    )
    .await?;
    Ok(new_wallet)
}
