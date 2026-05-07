use crate::config::{AppStateConfig, CreateWalletConfig};
use crate::external::mint::{ClowderMintConnector, HttpClientExt};
use crate::wallet::types::{WalletBalance, WalletDetailedBalanceEntry, WalletProtestResult};
use crate::{config::NostrConfig, wallet::api::WalletApi};
use bcr_common::cdk_common::wallet::Transaction;
use bcr_common::{
    cashu::{self, CurrencyUnit, MintUrl},
    cdk_common::wallet::TransactionId,
    wallet::Token,
};
use bcr_wallet_core::types::{
    self, MintSummary, PaymentResultCallback, PaymentSummary, Seed, WalletConfig,
};
use bcr_wallet_core::util::{build_wallet_id, keypair_from_mnemonic, seed_from_mnemonic};
use bcr_wallet_persistence::redb::{Database, build_pursedb, build_wallet_dbs, create_db};
use error::{Error, Result};
use nostr::nips::nip19::Nip19Profile;
use nostr::types::RelayUrl;
use std::{
    collections::{HashMap, HashSet},
    str::FromStr,
    sync::Arc,
};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub mod config;
pub mod error;
mod external;
mod pocket;
mod purse;
mod wallet;

pub struct AppState {
    purse: Arc<purse::Purse<wallet::Wallet>>,
    db: Arc<Database>,
    cfg: AppStateConfig,
    http_cl: Arc<reqwest::Client>,
}

impl AppState {
    pub const DB_VERSION: u32 = 1;
    pub const MINT_MELT_THRESHOLD_SAT: u64 = 1000;

    pub async fn initialize(cfg: AppStateConfig) -> Result<Self> {
        tracing::debug!("Initializing API");

        // Open Database file - only allowed to do once!
        let db = Arc::new(create_db(&cfg.db_path)?);
        let pursedb = build_pursedb(AppState::DB_VERSION, db.clone()).await?;

        let http_cl = Arc::new(reqwest::Client::new());
        let purse = purse::Purse::new(pursedb).await?;
        let mut appstate = Self {
            purse: Arc::new(purse),
            db,
            cfg,
            http_cl,
        };
        appstate.load_wallets().await?;
        Ok(appstate)
    }

    async fn load_wallets(&mut self) -> Result<()> {
        tracing::debug!("AppState::load_wallets()");

        let purse = self.get_purse();
        let db = self.get_db();
        let w_ids = purse.list_wallets().await?;
        for wid in w_ids.iter() {
            tracing::debug!("Loading wallet with id: {wid}");
            let mut w_cfg = purse.load_wallet_config(wid).await?;
            let Some(mnemonic) = self.cfg.mnemonics.get(wid) else {
                return Err(Error::MnemonicNotFound(wid.to_owned()));
            };
            let seed = seed_from_mnemonic(mnemonic);

            let client = HttpClientExt::new(w_cfg.mint.clone());
            let (nostr_cl, nprofile) = setup_nostr_client(mnemonic, &w_cfg.nostr_relays).await?;

            // Attempt to fetch clowder id/betas/keyset infos and fall back to saved ones
            match client.get_clowder_id().await {
                Ok(cid) => {
                    w_cfg.clowder_id = cid;
                }
                Err(e) => {
                    tracing::warn!(
                        "Could not fetch clowder_id while loading wallets - falling back to config {}: {e}",
                        &w_cfg.clowder_id.to_string()
                    );
                }
            };
            match client.get_mint_keysets().await {
                Ok(ks) => {
                    w_cfg.mint_keyset_infos = ks;
                }
                Err(e) => {
                    tracing::warn!(
                        "Could not fetch mint keysets while loading wallets - falling back to config {:?}: {e}",
                        &w_cfg.mint_keyset_infos
                    );
                }
            };
            match client.get_clowder_betas().await {
                Ok(betas) => {
                    w_cfg.betas = betas;
                }
                Err(e) => {
                    tracing::warn!(
                        "Could not fetch betas while loading wallets - falling back to config {:?}: {e}",
                        &w_cfg
                            .betas
                            .iter()
                            .map(|b| b.to_string())
                            .collect::<Vec<String>>()
                    );
                }
            };

            let wallet = build_wallet(
                w_cfg,
                client,
                Self::DB_VERSION,
                self.cfg.swap_expiry,
                db.clone(),
                seed,
                nostr_cl,
                nprofile,
            )
            .await?;
            purse.add_wallet(wallet).await?;
        }
        Ok(())
    }

    async fn get_wallet(&self, id: &str) -> Result<Arc<RwLock<wallet::Wallet>>> {
        let purse = self.get_purse();
        purse
            .get_wallet(id)
            .await
            .ok_or(Error::WalletNotFound(id.to_owned()))
    }

    fn get_purse(&self) -> Arc<purse::Purse<wallet::Wallet>> {
        self.purse.clone()
    }

    fn get_db(&self) -> Arc<Database> {
        self.db.clone()
    }

    //////////////////////////////////////////////////// Purse-Level API methods
    pub async fn purse_wallets_ids(&self) -> Result<Vec<String>> {
        tracing::debug!("get_wallet_ids");
        let purse = self.get_purse();
        Ok(purse.ids().await)
    }

    pub async fn purse_add_wallet(&self, cfg: CreateWalletConfig) -> Result<String> {
        tracing::debug!(
            "Adding a new wallet for mint {}, {}",
            cfg.name,
            cfg.default_mint_url
        );
        let purse = self.get_purse();

        self.validate_add_wallet(&cfg).await?;
        let wallet = create_new_wallet(
            cfg,
            AppState::DB_VERSION,
            self.cfg.swap_expiry,
            self.get_db(),
        )
        .await?;

        let id = purse.add_wallet(wallet).await?;

        Ok(id)
    }

    pub async fn purse_restore_wallet(&self, cfg: CreateWalletConfig) -> Result<String> {
        tracing::debug!(
            "Restoring a new wallet for mint {}, {}",
            cfg.name,
            cfg.default_mint_url
        );
        let purse = self.get_purse();

        self.validate_add_wallet(&cfg).await?;
        let wallet = create_new_wallet(
            cfg,
            AppState::DB_VERSION,
            self.cfg.swap_expiry,
            self.get_db(),
        )
        .await?;
        wallet.restore_local_proofs().await?;

        let id = purse.add_wallet(wallet).await?;
        tracing::debug!("Wallet restored successfully");
        Ok(id)
    }

    async fn validate_add_wallet(&self, cfg: &CreateWalletConfig) -> Result<()> {
        let existing_wallet_names = self.purse.names().await;
        if existing_wallet_names.contains(&cfg.name) {
            return Err(Error::WalletUniqueName(cfg.name.clone()));
        }

        let seed = seed_from_mnemonic(&cfg.mnemonic);
        let wallet_id = build_wallet_id(&seed, cfg.network);
        let existing_wallet_ids = self.purse.ids().await;
        if existing_wallet_ids.contains(&wallet_id) {
            return Err(Error::WalletUniqueId(wallet_id));
        }

        Ok(())
    }

    pub async fn purse_delete_wallet(&self, id: String) -> Result<()> {
        tracing::debug!("delete wallet {id}");
        let purse = self.get_purse();
        purse.delete_wallet(&id).await?;
        Ok(())
    }

    pub async fn purse_migrate_rabid(&self) -> Result<HashMap<String, MintUrl>> {
        tracing::debug!("purse_migrate_rabid");

        let purse = self.get_purse();
        let migrated = purse.migrate_rabid_wallets().await?;

        Ok(migrated)
    }

    ////////////////////////////////////////////////////  Wallet-Level API methods
    pub async fn wallet_name(&self, id: String) -> Result<String> {
        tracing::debug!("name for wallet {id}");

        let wallet = self.get_wallet(&id).await?;
        Ok(wallet.read().await.name())
    }

    pub async fn wallet_mint_url(&self, id: String) -> Result<String> {
        tracing::debug!("mint_url for wallet {id}");
        let wallet = self.get_wallet(&id).await?;
        Ok(wallet.read().await.mint_url()?.to_string())
    }

    pub async fn wallet_currency_unit(&self, id: String) -> Result<WalletCurrencyUnit> {
        tracing::debug!("wallet_currency_unit({id})");
        let wallet = self.get_wallet(&id).await?;
        Ok(WalletCurrencyUnit {
            unit: wallet.read().await.debit_unit().to_string(),
        })
    }

    pub async fn wallet_balance(&self, id: String) -> Result<WalletBalance> {
        tracing::debug!("wallet_balance({id})");

        let wallet = self.get_wallet(&id).await?;
        wallet.read().await.balance().await
    }

    pub async fn wallet_receive_token(&self, id: String, token: String) -> Result<TransactionId> {
        let tstamp = chrono::Utc::now().timestamp() as u64;
        tracing::debug!("wallet_receive({id}, {token}, {tstamp})");

        let token = Token::from_str(&token).map_err(|e| Error::InvalidToken(e.to_string()))?;
        let wallet = self.get_wallet(&id).await?;
        let tx_id = wallet.read().await.receive_token(token, tstamp).await?;
        Ok(tx_id)
    }

    pub async fn wallet_mint_is_rabid(&self, id: String) -> Result<bool> {
        tracing::debug!("wallet_is_rabid({id})");
        let wallet = self.get_wallet(&id).await?;
        let is_rabid = wallet.read().await.is_wallet_mint_rabid().await?;
        Ok(is_rabid)
    }

    pub async fn wallet_mint_is_offline(&self, id: String) -> Result<bool> {
        tracing::debug!("wallet_is_offline({id})");
        let wallet = self.get_wallet(&id).await?;
        let is_offline = wallet.read().await.is_wallet_mint_offline().await?;
        Ok(is_offline)
    }

    pub async fn wallet_prepare_pay_by_token(
        &self,
        id: String,
        amount: u64,
        description: Option<String>,
    ) -> Result<PaymentSummary> {
        tracing::debug!("wallet_prepare_pay_by_token({id}, {amount}, {description:?})");
        let amount = cashu::Amount::from(amount);
        let wallet = self.get_wallet(&id).await?;
        let unit = wallet.read().await.debit_unit();

        let summary = wallet
            .read()
            .await
            .prepare_pay_by_token(amount, unit, description)
            .await?;

        Ok(summary)
    }

    pub async fn wallet_pay_by_token(&self, id: String, rid: String) -> Result<CreatedToken> {
        let tstamp = chrono::Utc::now().timestamp() as u64;
        tracing::debug!("wallet_pay_by_token({rid}, {tstamp})");
        let p_id = Uuid::from_str(&rid)?;

        let wallet = self.get_wallet(&id).await?;
        let (tx_id, token) = wallet.read().await.pay(p_id, &self.http_cl, tstamp).await?;

        Ok(CreatedToken {
            tx_id,
            token: token.expect("pay by token returns a token"),
        })
    }

    pub async fn wallet_prepare_melt(
        &self,
        id: String,
        amount: u64,
        address: String,
        description: Option<String>,
    ) -> Result<PaymentSummary> {
        tracing::debug!("wallet_prepare_melt({id}, {amount}, {address}, {description:?})");

        if amount < Self::MINT_MELT_THRESHOLD_SAT {
            return Err(Error::InsufficientOnChainMeltAmount(amount));
        }
        let parsed_amount = bitcoin::Amount::from_sat(amount);
        let parsed_address = bitcoin::Address::from_str(&address)
            .map_err(|_| Error::InvalidBitcoinAddress(address.clone()))?;

        let wallet = self.get_wallet(&id).await?;
        if !parsed_address.is_valid_for_network(wallet.read().await.network()) {
            return Err(Error::InvalidBitcoinAddress(address.clone()));
        }
        let summary = wallet
            .read()
            .await
            .prepare_melt(parsed_amount, parsed_address, description)
            .await?;

        Ok(summary)
    }

    pub async fn wallet_melt(&self, id: String, rid: String) -> Result<TransactionId> {
        let tstamp = chrono::Utc::now().timestamp() as u64;
        tracing::debug!("wallet_melt({rid}, {tstamp})");

        let wallet = self.get_wallet(&id).await?;
        let p_id = Uuid::from_str(&rid)?;

        let (tx_id, _) = wallet.read().await.pay(p_id, &self.http_cl, tstamp).await?;

        Ok(tx_id)
    }

    pub async fn wallet_mint(&self, id: String, amount: u64) -> Result<MintSummary> {
        tracing::debug!("wallet_mint({id}, {amount})");

        if amount < Self::MINT_MELT_THRESHOLD_SAT {
            return Err(Error::InsufficientOnChainMintAmount(amount));
        }

        let parsed_amount = bitcoin::Amount::from_sat(amount);
        let wallet = self.get_wallet(&id).await?;
        let summary = wallet.read().await.mint(parsed_amount).await?;

        Ok(summary)
    }

    pub async fn wallet_check_pending_mints(&self, id: String) -> Result<Vec<TransactionId>> {
        tracing::debug!("wallet_check_pending_mints({id})");
        let wallet = self.get_wallet(&id).await?;
        let tx_ids = wallet.read().await.check_pending_mints().await?;

        Ok(tx_ids)
    }

    pub async fn wallet_check_pending_commitments(&self, id: String) -> Result<()> {
        tracing::debug!("wallet_check_pending_commitments({id})");
        let wallet = self.get_wallet(&id).await?;
        wallet.read().await.check_pending_commitments().await?;
        Ok(())
    }

    pub async fn wallet_protest_mint(
        &self,
        id: String,
        quote_id: String,
    ) -> Result<(
        bcr_common::wire::common::ProtestStatus,
        Option<cashu::Amount>,
    )> {
        tracing::debug!("wallet_protest_mint({id}, {quote_id})");
        let qid = Uuid::from_str(&quote_id)?;
        let wallet = self.get_wallet(&id).await?;
        let WalletProtestResult { status, result } = wallet.read().await.protest_mint(qid).await?;
        Ok((status, result.map(|(amount, _)| amount)))
    }

    pub async fn wallet_protest_swap(
        &self,
        id: String,
        commitment_sig: String,
    ) -> Result<(
        bcr_common::wire::common::ProtestStatus,
        Option<cashu::Amount>,
    )> {
        tracing::debug!("wallet_protest_swap({id}, {commitment_sig})");
        let sig = bitcoin::secp256k1::schnorr::Signature::from_str(&commitment_sig)
            .map_err(|e| Error::SchnorrSignature(e.to_string()))?;
        let wallet = self.get_wallet(&id).await?;
        let WalletProtestResult { status, result } = wallet.read().await.protest_swap(sig).await?;
        Ok((status, result.map(|(amount, _)| amount)))
    }

    pub async fn wallet_protest_melt(
        &self,
        id: String,
        quote_id: String,
    ) -> Result<(
        bcr_common::wire::common::ProtestStatus,
        Option<cashu::Amount>,
    )> {
        tracing::debug!("wallet_protest_melt({id}, {quote_id})");
        let qid = Uuid::from_str(&quote_id)?;
        let wallet = self.get_wallet(&id).await?;
        let WalletProtestResult { status, result } = wallet.read().await.protest_melt(qid).await?;
        Ok((status, result.map(|(amount, _)| amount)))
    }

    pub async fn wallet_check_pending_melt_commitments(&self, id: String) -> Result<()> {
        tracing::debug!("wallet_check_pending_melt_commitments({id})");
        let wallet = self.get_wallet(&id).await?;
        wallet.read().await.check_pending_melt_commitments().await?;
        Ok(())
    }

    pub async fn wallet_prepare_payment(
        &self,
        id: String,
        input: String,
    ) -> Result<PaymentSummary> {
        tracing::debug!("wallet_prepare_payment({id}, {input})");

        let wallet = self.get_wallet(&id).await?;
        let summary = wallet.read().await.prepare_pay(input).await?;

        Ok(summary)
    }

    pub async fn wallet_pay(&self, id: String, rid: String) -> Result<TransactionId> {
        let tstamp = chrono::Utc::now().timestamp() as u64;
        tracing::debug!("wallet_pay({rid}, {tstamp})");

        let wallet = self.get_wallet(&id).await?;
        let p_id = Uuid::from_str(&rid)?;

        let (tx_id, _) = wallet.read().await.pay(p_id, &self.http_cl, tstamp).await?;
        Ok(tx_id)
    }

    pub async fn wallet_prepare_payment_request(
        &self,
        id: String,
        amount: u64,
        description: Option<String>,
    ) -> Result<PaymentRequest> {
        tracing::debug!("wallet_prepare_pay_request({id}, {amount}, {description:?})");

        let amount = cashu::Amount::from(amount);

        let wallet = self.get_wallet(&id).await?;
        let unit = wallet.read().await.debit_unit();
        let request = wallet
            .read()
            .await
            .prepare_payment_request(amount, unit, description)
            .await?;
        Ok(PaymentRequest {
            p_id: request.payment_id.clone().unwrap_or_default(),
            request: request.to_string(),
        })
    }

    pub async fn wallet_check_received_payment(
        &self,
        id: String,
        max_wait_sec: u64,
        p_id: String,
        cancel_token: CancellationToken,
        result_callback: PaymentResultCallback,
    ) -> Result<()> {
        tracing::debug!("wallet_check_received_payment({p_id})");

        let p_id = Uuid::from_str(&p_id)?;
        let wallet = self.get_wallet(&id).await?;

        let max_wait = core::time::Duration::from_secs(max_wait_sec);
        wallet
            .read()
            .await
            .check_received_payment(max_wait, p_id, cancel_token, result_callback)
            .await?;
        Ok(())
    }

    pub async fn wallet_list_tx_ids(&self, id: String) -> Result<Vec<TransactionId>> {
        tracing::debug!("wallet_list_tx_ids({id})");

        let wallet = self.get_wallet(&id).await?;
        let tx_ids = wallet.read().await.list_tx_ids().await?;
        Ok(tx_ids)
    }

    pub async fn wallet_list_txs(&self, id: String) -> Result<Vec<Transaction>> {
        tracing::debug!("wallet_list_txs({id})");

        let wallet = self.get_wallet(&id).await?;
        let mut txs = wallet.read().await.list_txs().await?;
        txs.sort_by_key(|b| std::cmp::Reverse(b.timestamp)); // sort by timestamp desc
        Ok(txs)
    }

    pub async fn wallet_load_tx(&self, id: String, tx_id: &str) -> Result<Transaction> {
        tracing::debug!("wallet_load_tx({id}, {tx_id})");

        let tx_id = TransactionId::from_str(tx_id)?;
        let wallet = self.get_wallet(&id).await?;
        let tx = wallet.read().await.load_tx(tx_id).await?;
        Ok(tx)
    }

    pub async fn wallet_reclaim_tx(&self, id: String, tx_id: &str) -> Result<cashu::Amount> {
        tracing::debug!("wallet_reclaim_tx({id}, {tx_id})");
        let tx_id = TransactionId::from_str(tx_id)?;
        let wallet = self.get_wallet(&id).await?;
        let amount = wallet.read().await.reclaim_tx(tx_id).await?;
        Ok(amount)
    }

    // Recover pending stale proofs
    pub async fn wallet_recover_pending_stale_proofs(&self, id: String) -> Result<cashu::Amount> {
        tracing::debug!("wallet_recover_pending_stale_proofs({id})");
        let wallet = self.get_wallet(&id).await?;
        let wlt = wallet.read().await;
        // collect ys for pending transactions, so we don't recover proofs from open transactions

        let pending_txs_ys: Vec<cashu::PublicKey> = wlt
            .list_txs()
            .await?
            .into_iter()
            .filter(wallet::util::tx_can_be_refreshed)
            .flat_map(|tx| tx.ys)
            .collect();

        let recovered = wlt.recover_pending_stale_proofs(&pending_txs_ys).await?;

        Ok(recovered)
    }

    // Refreshes the state of all pending transactions of the given wallet
    pub async fn wallet_refresh_txs(&self, id: String) -> Result<usize> {
        tracing::debug!("wallet_refresh_txs({id})");
        let wallet = self.get_wallet(&id).await?;
        let txs = wallet.read().await.list_txs().await?;
        let mut updated = 0;

        for tx in txs.iter() {
            if !wallet::util::tx_can_be_refreshed(tx) {
                continue;
            }

            let tx_id = tx.id();

            match wallet.read().await.refresh_tx(tx_id).await {
                Ok(tx_updated) => {
                    if tx_updated {
                        updated += 1;
                    }
                }
                Err(e) => {
                    tracing::error!("Error refreshing tx {}: {e}", tx_id);
                }
            };
        }

        Ok(updated)
    }

    // Refreshes the state of the transaction with the given id
    pub async fn wallet_refresh_tx(&self, id: String, tx_id: &str) -> Result<bool> {
        tracing::debug!("wallet_refresh_tx({id}, {tx_id})");

        let tx_id = TransactionId::from_str(tx_id)?;
        let wallet = self.get_wallet(&id).await?;
        let updated = wallet.read().await.refresh_tx(tx_id).await?;
        Ok(updated)
    }

    //////////////////////////////////////////////////// Wallet Dev Mode Calls
    pub async fn wallet_dev_mode_detailed_balance(
        &self,
        id: String,
    ) -> Result<Vec<WalletDetailedBalanceEntry>> {
        tracing::debug!("dev_mode_detailed_wallet_balance({id})");
        if !self.cfg.dev_mode {
            return Err(Error::NoDevMode);
        }

        let wallet = self.get_wallet(&id).await?;
        wallet.read().await.dev_mode_detailed_balance().await
    }

    //////////////////////////////////////////////////// General App-Level calls
    /// Runs the regular jobs for each interval
    /// This should be called in an interval and on app initialization
    pub async fn run_jobs(&self) -> Result<()> {
        tracing::info!("Run Jobs triggered");
        if self.execute_regular_jobs().await {
            tracing::info!("Run Regular Jobs executed successfully");
        } else {
            tracing::info!(
                "Run Regular Jobs executed with some errors - will run again at the next interval."
            );
        }

        Ok(())
    }

    pub async fn execute_regular_jobs(&self) -> bool {
        let mut job_failed = false;

        let wallet_ids = self.get_purse().ids().await;
        for wallet_id in wallet_ids.iter() {
            match self.wallet_refresh_txs(wallet_id.to_owned()).await {
                Ok(updated) => {
                    tracing::info!("Updated {updated} transactions for wallet {wallet_id}");
                }
                Err(e) => {
                    job_failed = true;
                    tracing::error!(
                        "Error running wallet_refresh_txs job for wallet {wallet_id}: {e}"
                    );
                }
            };
            match self.wallet_check_pending_mints(wallet_id.to_owned()).await {
                Ok(result) => {
                    tracing::info!(
                        "Received {} transactions from pending mints for wallet {wallet_id}, Tx Ids: {:?}",
                        result.len(),
                        result
                            .iter()
                            .map(|txid| txid.to_string())
                            .collect::<Vec<String>>()
                    );
                }
                Err(e) => {
                    job_failed = true;
                    tracing::error!(
                        "Error running wallet_check_pending_mints job for wallet {wallet_id}: {e}"
                    );
                }
            }
            match self
                .wallet_check_pending_commitments(wallet_id.to_owned())
                .await
            {
                Ok(()) => {
                    tracing::info!("Checked pending commitments for wallet {wallet_id}");
                }
                Err(e) => {
                    job_failed = true;
                    tracing::error!(
                        "Error running wallet_check_pending_commitments job for wallet {wallet_id}: {e}"
                    );
                }
            }
            match self
                .wallet_recover_pending_stale_proofs(wallet_id.to_owned())
                .await
            {
                Ok(recovered) => {
                    tracing::info!(
                        "Recovered pending stale proofs for wallet {wallet_id}, recovered: {recovered}"
                    );
                }
                Err(e) => {
                    job_failed = true;
                    tracing::error!(
                        "Error running wallet_recover_pending_stale_proofs job for wallet {wallet_id}: {e}"
                    );
                }
            }
            match self
                .wallet_check_pending_melt_commitments(wallet_id.to_owned())
                .await
            {
                Ok(()) => {
                    tracing::info!("Checked pending melt commitments for wallet {wallet_id}");
                }
                Err(e) => {
                    job_failed = true;
                    tracing::error!(
                        "Error running wallet_check_pending_melt_commitments job for wallet {wallet_id}: {e}"
                    );
                }
            }
        }

        // successful = true
        !job_failed
    }
}

pub fn generate_random_mnemonic(mnemonic_len: u32, network: bitcoin::Network) -> (String, String) {
    let mnemonic_len = if mnemonic_len == 0 { 12 } else { mnemonic_len };
    tracing::info!("Generate random {}-word mnemonic", mnemonic_len);

    const VALID_MNEMONIC_LENGTHS: [u32; 5] = [12, 15, 18, 21, 24];
    assert!(
        VALID_MNEMONIC_LENGTHS.contains(&mnemonic_len),
        "word count must be one of: {VALID_MNEMONIC_LENGTHS:?}"
    );
    let returned = bip39::Mnemonic::generate_in(bip39::Language::English, mnemonic_len as usize);
    match returned {
        Ok(mnemonic) => {
            let seed = seed_from_mnemonic(&mnemonic);
            (mnemonic.to_string(), build_wallet_id(&seed, network))
        }
        Err(e) => {
            tracing::error!("generate_random_mnemonic({mnemonic_len}): {e}");
            (String::default(), String::default())
        }
    }
}

pub fn get_wallet_id(mnemonic: &bip39::Mnemonic, network: bitcoin::Network) -> String {
    let seed = seed_from_mnemonic(mnemonic);
    build_wallet_id(&seed, network)
}

pub fn is_valid_token(token: &str) -> Result<Token> {
    let token = Token::from_str(token).map_err(|e| Error::InvalidToken(e.to_string()))?;
    Ok(token)
}

// FFI types

#[derive(Default, Clone, Debug)]
pub struct PaymentRequest {
    pub request: String,
    pub p_id: String,
}

#[derive(Default, Clone, Debug)]
pub struct WalletCurrencyUnit {
    pub unit: String,
}

#[derive(Clone, Debug)]
pub struct CreatedToken {
    pub tx_id: TransactionId,
    pub token: Token,
}

async fn create_new_wallet(
    cfg: CreateWalletConfig,
    db_version: u32,
    swap_expiry: chrono::TimeDelta,
    db: Arc<Database>,
) -> Result<wallet::Wallet> {
    let seed = seed_from_mnemonic(&cfg.mnemonic);
    let keypair = keypair_from_mnemonic(&cfg.mnemonic);
    let client = HttpClientExt::new(cfg.default_mint_url.clone());

    let wallet_id = build_wallet_id(&seed, cfg.network);
    let clowder_id = client.get_clowder_id().await?;
    let keyset_infos = client.get_mint_keysets().await?;
    let betas = client.get_clowder_betas().await?;
    // Attempt to find debit unit in the given keysets
    let currencies = keyset_infos
        .iter()
        .map(|k| k.unit.clone())
        .collect::<HashSet<_>>();
    if currencies.len() > 1 {
        return Err(Error::Unsupported(
            "Mint supports more than 1 currency, not supported yet".into(),
        ));
    }
    let debit_unit = currencies.iter().find(|unit| *unit == &CurrencyUnit::Sat);

    let debit_unit = match debit_unit {
        Some(du) => du,
        None => {
            let currencies = currencies.iter().cloned().collect();
            return Err(Error::NoDebitCurrencyInMint(currencies));
        }
    };

    let (nostr_cl, nprofile) = setup_nostr_client(&cfg.mnemonic, &cfg.nostr_relays).await?;

    let w_cfg = WalletConfig {
        wallet_id,
        name: cfg.name,
        network: cfg.network,
        mint: cfg.default_mint_url,
        mint_keyset_infos: keyset_infos,
        clowder_id,
        debit: debit_unit.to_owned(),
        pub_key: keypair.public_key(),
        betas,
        nostr_relays: cfg.nostr_relays,
    };
    build_wallet(
        w_cfg,
        client,
        db_version,
        swap_expiry,
        db,
        seed,
        nostr_cl,
        nprofile,
    )
    .await
}

async fn setup_nostr_client(
    mnemonic: &bip39::Mnemonic,
    nostr_relays: &[RelayUrl],
) -> Result<(Arc<nostr_sdk::Client>, Nip19Profile)> {
    let nostr_cfg = NostrConfig::new(mnemonic.to_owned(), nostr_relays.to_owned())?;
    let nostr_filter = nostr_sdk::Filter::new()
        .kind(nostr_sdk::Kind::GiftWrap)
        .pubkey(nostr_cfg.nostr_signer.public_key());
    let nostr_cl = Arc::new(nostr_sdk::Client::new(nostr_cfg.nostr_signer));
    for nostr_relay in &nostr_cfg.relays {
        nostr_cl.add_relay(nostr_relay).await?;
    }
    nostr_cl.connect().await;

    // create long-running subscription
    nostr_cl.subscribe(nostr_filter, None).await?;

    Ok((nostr_cl, nostr_cfg.nprofile))
}

async fn build_wallet(
    w_cfg: WalletConfig,
    client: HttpClientExt,
    db_version: u32,
    swap_expiry: chrono::TimeDelta,
    db: Arc<Database>,
    seed: Seed,
    nostr_cl: Arc<nostr_sdk::Client>,
    nostr_profile: Nip19Profile,
) -> Result<wallet::Wallet> {
    // building wallet dbs
    let (tx_repo, (debitdb, mintmeltdb)) =
        build_wallet_dbs(db_version, &w_cfg.wallet_id, &w_cfg.debit, db).await?;

    // building the debit pocket
    let debit_pocket = Box::new(pocket::debit::Pocket::new(
        w_cfg.debit.clone(),
        Arc::new(debitdb),
        Arc::new(mintmeltdb),
        seed,
    ));

    let mut beta_clients = HashMap::<cashu::MintUrl, Arc<dyn ClowderMintConnector>>::new();
    for beta in w_cfg.betas.clone() {
        let beta_client = HttpClientExt::new(beta.clone());
        beta_clients.insert(beta, Arc::new(beta_client));
    }
    // Wrap the client with SentinelClient to send events to sentinel nodes
    let client = {
        let cl = external::mint::SentinelClient::new(client, w_cfg.betas);
        Arc::new(cl) as Arc<dyn ClowderMintConnector>
    };
    let new_wallet: wallet::Wallet = wallet::Wallet::new(
        w_cfg.network,
        client,
        w_cfg.mint_keyset_infos,
        Box::new(tx_repo),
        debit_pocket,
        w_cfg.name,
        w_cfg.wallet_id,
        w_cfg.pub_key,
        w_cfg.clowder_id,
        beta_clients,
        Box::new(|url| Arc::new(external::mint::HttpClientExt::new(url))),
        swap_expiry,
        w_cfg.nostr_relays,
        nostr_cl,
        nostr_profile,
    )
    .await?;
    Ok(new_wallet)
}
