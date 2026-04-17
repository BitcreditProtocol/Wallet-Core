use crate::config::AppStateConfig;
use crate::external::mint::{ClowderMintConnector, HttpClientExt};
use crate::wallet::types::{WalletBalance, WalletProtestResult};
use crate::{config::NostrConfig, wallet::api::WalletApi};
use bcr_common::cdk_common::wallet::Transaction;
use bcr_common::{
    cashu::{self, CurrencyUnit, MintUrl, nut18 as cdk18},
    cdk::wallet::{MintConnector as MintCon, types::TransactionId},
    wallet::Token,
};
use bcr_wallet_core::types::{
    self, MintSummary, PaymentResultCallback, PaymentSummary, Seed, WalletConfig,
};
use bcr_wallet_core::util::{build_wallet_id, keypair_from_mnemonic, seed_from_mnemonic};
use bcr_wallet_persistence::redb::{Database, build_pursedb, build_wallet_dbs, create_db};
use error::{Error, Result};
use nostr::nips::nip19::{Nip19Profile, ToBech32};
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
    nostr_cl: Arc<nostr_sdk::Client>,
    http_cl: Arc<reqwest::Client>,
    myself: Nip19Profile,
}

impl AppState {
    pub const DB_VERSION: u32 = 1;
    pub const MINT_MELT_THRESHOLD_SAT: u64 = 1000;

    pub async fn initialize(cfg: AppStateConfig) -> Result<Self> {
        tracing::debug!("Initializing API");

        // Open Database file - only allowed to do once!
        let db = Arc::new(create_db(&cfg.db_path)?);

        let pursedb = build_pursedb(AppState::DB_VERSION, db.clone()).await?;

        let nostr_cfg = NostrConfig::new(cfg.mnemonic.clone(), cfg.nostr_relays.clone())?;
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

        let http_cl = Arc::new(reqwest::Client::new());
        let purse = purse::Purse::new(pursedb).await?;
        let mut appstate = Self {
            purse: Arc::new(purse),
            db,
            cfg,
            http_cl,
            nostr_cl,
            myself: nostr_cfg.nprofile,
        };
        appstate.load_wallets().await?;
        Ok(appstate)
    }

    async fn load_wallets(&mut self) -> Result<()> {
        tracing::debug!("AppState::load_wallets()");

        let purse = self.get_purse();
        let db = self.get_db();
        let w_ids = purse.list_wallets().await?;
        for wid in w_ids {
            tracing::debug!("Loading wallet with id: {wid}");
            let mut w_cfg = purse.load_wallet_config(&wid).await?;

            if w_cfg.network != self.cfg.network {
                tracing::error!(
                    "Network mismatch: wallet {wid} with network {:?}, expected {:?}",
                    w_cfg.network,
                    self.cfg.network,
                );
                return Err(Error::InvalidNetwork(w_cfg.network, self.cfg.network));
            }

            let seed = seed_from_mnemonic(&self.cfg.mnemonic);
            let keypair = keypair_from_mnemonic(&self.cfg.mnemonic);
            if w_cfg.pub_key != keypair.public_key() {
                tracing::error!(
                    "Key mismatch: wallet {wid} has a different pubkey than the one given via the config mnemonic"
                );
                return Err(Error::InvalidMnemonic);
            }

            if w_cfg.mint != self.cfg.default_mint_url {
                tracing::warn!(
                    "Mint URL mismatch: wallet {wid} with mint url {}, expected: {}",
                    w_cfg.mint,
                    self.cfg.default_mint_url
                );
            }

            let client = HttpClientExt::new(w_cfg.mint.clone());

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
                    w_cfg.mint_keyset_infos = ks.keysets;
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
            )
            .await?;
            purse.add_wallet(wallet).await?;
        }
        Ok(())
    }

    async fn get_wallet(&self, idx: usize) -> Result<Arc<RwLock<wallet::Wallet>>> {
        let purse = self.get_purse();
        purse
            .get_wallet(idx)
            .await
            .ok_or(Error::WalletNotFound(idx))
    }

    fn get_purse(&self) -> Arc<purse::Purse<wallet::Wallet>> {
        self.purse.clone()
    }

    fn get_db(&self) -> Arc<Database> {
        self.db.clone()
    }

    //////////////////////////////////////////////////// Purse-Level API methods
    pub async fn purse_wallets_ids(&self) -> Result<Vec<usize>> {
        tracing::debug!("get_wallet_ids");
        let purse = self.get_purse();
        Ok(purse.ids().await.iter().map(|id| *id as usize).collect())
    }

    pub async fn purse_add_wallet(&self, name: String) -> Result<usize> {
        let mint_url = self.cfg.default_mint_url.clone();
        tracing::debug!("Adding a new wallet for mint {name}, {mint_url}");
        let purse = self.get_purse();
        if !purse.can_add_wallet().await {
            return Err(Error::WalletAlreadyExists);
        }

        let wallet = create_new_wallet(
            name,
            self.cfg.network,
            mint_url,
            self.cfg.mnemonic.clone(),
            AppState::DB_VERSION,
            self.cfg.swap_expiry,
            self.get_db(),
        )
        .await?;

        let idx = purse.add_wallet(wallet).await?;

        Ok(idx)
    }

    pub async fn purse_restore_wallet(&self, name: String) -> Result<usize> {
        let mint_url = self.cfg.default_mint_url.clone();
        tracing::debug!("Restoring a new wallet for mint {name}, {mint_url}");
        let purse = self.get_purse();
        if !purse.can_add_wallet().await {
            return Err(Error::WalletAlreadyExists);
        }

        let wallet = create_new_wallet(
            name,
            self.cfg.network,
            mint_url,
            self.cfg.mnemonic.clone(),
            AppState::DB_VERSION,
            self.cfg.swap_expiry,
            self.get_db(),
        )
        .await?;
        wallet.restore_local_proofs().await?;

        let idx = purse.add_wallet(wallet).await?;
        tracing::debug!("Wallet restored successfully");
        Ok(idx)
    }

    pub async fn purse_delete_wallet(&self, idx: usize) -> Result<()> {
        tracing::debug!("delete wallet {idx}");
        let purse = self.get_purse();
        purse.delete_wallet(idx).await?;
        Ok(())
    }

    pub async fn purse_migrate_rabid(&self) -> Result<HashMap<String, MintUrl>> {
        tracing::debug!("purse_migrate_rabid");

        let purse = self.get_purse();
        let migrated = purse.migrate_rabid_wallets().await?;

        Ok(migrated)
    }

    ////////////////////////////////////////////////////  Wallet-Level API methods
    pub async fn wallet_name(&self, idx: usize) -> Result<String> {
        tracing::debug!("name for wallet {idx}");

        let wallet = self.get_wallet(idx).await?;
        Ok(wallet.read().await.name())
    }

    pub async fn wallet_mint_url(&self, idx: usize) -> Result<String> {
        tracing::debug!("mint_url for wallet {idx}");
        let wallet = self.get_wallet(idx).await?;
        Ok(wallet.read().await.mint_url()?.to_string())
    }

    pub async fn wallet_currency_unit(&self, idx: usize) -> Result<WalletCurrencyUnit> {
        tracing::debug!("wallet_currency_unit({idx})");
        let wallet = self.get_wallet(idx).await?;
        Ok(WalletCurrencyUnit {
            unit: wallet.read().await.debit_unit().to_string(),
        })
    }

    pub async fn wallet_balance(&self, idx: usize) -> Result<WalletBalance> {
        tracing::debug!("wallet_balance({idx})");

        let wallet = self.get_wallet(idx).await?;
        wallet.read().await.balance().await
    }

    pub async fn wallet_receive_token(&self, idx: usize, token: String) -> Result<TransactionId> {
        let tstamp = chrono::Utc::now().timestamp() as u64;
        tracing::debug!("wallet_receive({idx}, {token}, {tstamp})");

        let token = Token::from_str(&token).map_err(|e| Error::InvalidToken(e.to_string()))?;
        let wallet = self.get_wallet(idx).await?;
        let tx_id = wallet.read().await.receive_token(token, tstamp).await?;
        Ok(tx_id)
    }

    pub async fn wallet_mint_is_rabid(&self, idx: usize) -> Result<bool> {
        tracing::debug!("wallet_is_rabid({idx})");
        let wallet = self.get_wallet(idx).await?;
        let is_rabid = wallet.read().await.is_wallet_mint_rabid().await?;
        Ok(is_rabid)
    }

    pub async fn wallet_mint_is_offline(&self, idx: usize) -> Result<bool> {
        tracing::debug!("wallet_is_offline({idx})");
        let wallet = self.get_wallet(idx).await?;
        let is_offline = wallet.read().await.is_wallet_mint_offline().await?;
        Ok(is_offline)
    }

    pub async fn wallet_prepare_pay_by_token(
        &self,
        idx: usize,
        amount: u64,
        description: Option<String>,
    ) -> Result<PaymentSummary> {
        tracing::debug!("wallet_prepare_pay_by_token({idx}, {amount}, {description:?})");
        let amount = cashu::Amount::from(amount);
        let wallet = self.get_wallet(idx).await?;
        let unit = wallet.read().await.debit_unit();

        let summary = wallet
            .read()
            .await
            .prepare_pay_by_token(amount, unit, description)
            .await?;

        Ok(summary)
    }

    pub async fn wallet_pay_by_token(&self, idx: usize, rid: String) -> Result<CreatedToken> {
        let tstamp = chrono::Utc::now().timestamp() as u64;
        tracing::debug!("wallet_pay_by_token({rid}, {tstamp})");
        let p_id = Uuid::from_str(&rid)?;

        let wallet = self.get_wallet(idx).await?;
        let (tx_id, token) = wallet
            .read()
            .await
            .pay(p_id, &self.nostr_cl, &self.http_cl, tstamp)
            .await?;

        Ok(CreatedToken {
            tx_id,
            token: token.expect("pay by token returns a token"),
        })
    }

    pub async fn wallet_prepare_melt(
        &self,
        idx: usize,
        amount: u64,
        address: String,
        description: Option<String>,
    ) -> Result<PaymentSummary> {
        tracing::debug!("wallet_prepare_melt({idx}, {amount}, {address}, {description:?})");

        if amount < Self::MINT_MELT_THRESHOLD_SAT {
            return Err(Error::InsufficientOnChainMeltAmount(amount));
        }
        let parsed_amount = bitcoin::Amount::from_sat(amount);
        let parsed_address = bitcoin::Address::from_str(&address)
            .map_err(|_| Error::InvalidBitcoinAddress(address.clone()))?;

        if !parsed_address.is_valid_for_network(self.cfg.network) {
            return Err(Error::InvalidBitcoinAddress(address.clone()));
        }
        let wallet = self.get_wallet(idx).await?;
        let summary = wallet
            .read()
            .await
            .prepare_melt(parsed_amount, parsed_address, description)
            .await?;

        Ok(summary)
    }

    pub async fn wallet_melt(&self, idx: usize, rid: String) -> Result<TransactionId> {
        let tstamp = chrono::Utc::now().timestamp() as u64;
        tracing::debug!("wallet_melt({rid}, {tstamp})");

        let wallet = self.get_wallet(idx).await?;
        let p_id = Uuid::from_str(&rid)?;

        let (tx_id, _) = wallet
            .read()
            .await
            .pay(p_id, &self.nostr_cl, &self.http_cl, tstamp)
            .await?;

        Ok(tx_id)
    }

    pub async fn wallet_mint(&self, idx: usize, amount: u64) -> Result<MintSummary> {
        tracing::debug!("wallet_mint({idx}, {amount})");

        if amount < Self::MINT_MELT_THRESHOLD_SAT {
            return Err(Error::InsufficientOnChainMintAmount(amount));
        }

        let parsed_amount = bitcoin::Amount::from_sat(amount);
        let wallet = self.get_wallet(idx).await?;
        let summary = wallet.read().await.mint(parsed_amount).await?;

        Ok(summary)
    }

    pub async fn wallet_check_pending_mints(&self, idx: usize) -> Result<Vec<TransactionId>> {
        tracing::debug!("wallet_check_pending_mints({idx})");
        let wallet = self.get_wallet(idx).await?;
        let tx_ids = wallet.read().await.check_pending_mints().await?;

        Ok(tx_ids)
    }

    pub async fn wallet_check_pending_commitments(&self, idx: usize) -> Result<()> {
        tracing::debug!("wallet_check_pending_commitments({idx})");
        let wallet = self.get_wallet(idx).await?;
        wallet.read().await.check_pending_commitments().await?;
        Ok(())
    }

    pub async fn wallet_protest_mint(
        &self,
        idx: usize,
        quote_id: String,
    ) -> Result<(
        bcr_common::wire::common::ProtestStatus,
        Option<cashu::Amount>,
    )> {
        tracing::debug!("wallet_protest_mint({idx}, {quote_id})");
        let qid = Uuid::from_str(&quote_id)?;
        let wallet = self.get_wallet(idx).await?;
        let WalletProtestResult { status, result } = wallet.read().await.protest_mint(qid).await?;
        Ok((status, result.map(|(amount, _)| amount)))
    }

    pub async fn wallet_protest_swap(
        &self,
        idx: usize,
        commitment_sig: String,
    ) -> Result<(
        bcr_common::wire::common::ProtestStatus,
        Option<cashu::Amount>,
    )> {
        tracing::debug!("wallet_protest_swap({idx}, {commitment_sig})");
        let sig = bitcoin::secp256k1::schnorr::Signature::from_str(&commitment_sig)
            .map_err(|e| Error::SchnorrSignature(e.to_string()))?;
        let wallet = self.get_wallet(idx).await?;
        let WalletProtestResult { status, result } = wallet.read().await.protest_swap(sig).await?;
        Ok((status, result.map(|(amount, _)| amount)))
    }

    pub async fn wallet_prepare_payment(
        &self,
        idx: usize,
        input: String,
    ) -> Result<PaymentSummary> {
        tracing::debug!("wallet_prepare_payment({idx}, {input})");

        let wallet = self.get_wallet(idx).await?;
        let summary = wallet.read().await.prepare_pay(input).await?;

        Ok(summary)
    }

    pub async fn wallet_pay(&self, idx: usize, rid: String) -> Result<TransactionId> {
        let tstamp = chrono::Utc::now().timestamp() as u64;
        tracing::debug!("wallet_pay({rid}, {tstamp})");

        let wallet = self.get_wallet(idx).await?;
        let p_id = Uuid::from_str(&rid)?;

        let (tx_id, _) = wallet
            .read()
            .await
            .pay(p_id, &self.nostr_cl, &self.http_cl, tstamp)
            .await?;
        Ok(tx_id)
    }

    pub async fn wallet_prepare_payment_request(
        &self,
        idx: usize,
        amount: u64,
        description: Option<String>,
    ) -> Result<PaymentRequest> {
        tracing::debug!("wallet_prepare_pay_request({idx}, {amount}, {description:?})");

        let amount = cashu::Amount::from(amount);

        let nostr_transport = cdk18::Transport {
            _type: cdk18::TransportType::Nostr,
            target: self.myself.to_bech32()?,
            tags: Some(vec![vec![String::from("n"), String::from("17")]]),
        };

        let wallet = self.get_wallet(idx).await?;
        let unit = wallet.read().await.debit_unit();
        let request = wallet
            .read()
            .await
            .prepare_payment_request(amount, unit, description, nostr_transport)
            .await?;
        Ok(PaymentRequest {
            p_id: request.payment_id.clone().unwrap_or_default(),
            request: request.to_string(),
        })
    }

    pub async fn wallet_check_received_payment(
        &self,
        idx: usize,
        max_wait_sec: u64,
        p_id: String,
        cancel_token: CancellationToken,
        result_callback: PaymentResultCallback,
    ) -> Result<()> {
        tracing::debug!("wallet_check_received_payment({p_id})");

        let p_id = Uuid::from_str(&p_id)?;
        let wallet = self.get_wallet(idx).await?;

        let max_wait = core::time::Duration::from_secs(max_wait_sec);
        wallet
            .read()
            .await
            .check_received_payment(
                max_wait,
                p_id,
                &self.nostr_cl,
                cancel_token,
                result_callback,
            )
            .await?;
        Ok(())
    }

    pub async fn wallet_list_tx_ids(&self, idx: usize) -> Result<Vec<TransactionId>> {
        tracing::debug!("wallet_list_tx_ids({idx})");

        let wallet = self.get_wallet(idx).await?;
        let tx_ids = wallet.read().await.list_tx_ids().await?;
        Ok(tx_ids)
    }

    pub async fn wallet_list_txs(&self, idx: usize) -> Result<Vec<Transaction>> {
        tracing::debug!("wallet_list_txs({idx})");

        let wallet = self.get_wallet(idx).await?;
        let mut txs = wallet.read().await.list_txs().await?;
        txs.sort_by_key(|b| std::cmp::Reverse(b.timestamp)); // sort by timestamp desc
        Ok(txs)
    }

    pub async fn wallet_load_tx(&self, idx: usize, tx_id: &str) -> Result<Transaction> {
        tracing::debug!("wallet_load_tx({idx}, {tx_id})");

        let tx_id = TransactionId::from_str(tx_id)?;
        let wallet = self.get_wallet(idx).await?;
        let tx = wallet.read().await.load_tx(tx_id).await?;
        Ok(tx)
    }

    pub async fn wallet_reclaim_tx(&self, idx: usize, tx_id: &str) -> Result<cashu::Amount> {
        tracing::debug!("wallet_reclaim_tx({idx}, {tx_id})");
        let tx_id = TransactionId::from_str(tx_id)?;
        let wallet = self.get_wallet(idx).await?;
        let amount = wallet.read().await.reclaim_tx(tx_id).await?;
        Ok(amount)
    }

    // Refreshes the state of all pending transactions of the given wallet
    pub async fn wallet_refresh_txs(&self, idx: usize) -> Result<usize> {
        tracing::debug!("wallet_refresh_txs({idx})");
        let wallet = self.get_wallet(idx).await?;
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
    pub async fn wallet_refresh_tx(&self, idx: usize, tx_id: &str) -> Result<bool> {
        tracing::debug!("wallet_refresh_tx({idx}, {tx_id})");

        let tx_id = TransactionId::from_str(tx_id)?;
        let wallet = self.get_wallet(idx).await?;
        let updated = wallet.read().await.refresh_tx(tx_id).await?;
        Ok(updated)
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
            match self.wallet_refresh_txs(*wallet_id as usize).await {
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
            match self.wallet_check_pending_mints(*wallet_id as usize).await {
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
                .wallet_check_pending_commitments(*wallet_id as usize)
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
        }

        // successful = true
        !job_failed
    }
}

pub fn generate_random_mnemonic(mnemonic_len: u32) -> String {
    let mnemonic_len = if mnemonic_len == 0 { 12 } else { mnemonic_len };
    tracing::info!("Generate random {}-word mnemonic", mnemonic_len);

    const VALID_MNEMONIC_LENGTHS: [u32; 5] = [12, 15, 18, 21, 24];
    assert!(
        VALID_MNEMONIC_LENGTHS.contains(&mnemonic_len),
        "word count must be one of: {VALID_MNEMONIC_LENGTHS:?}"
    );
    let returned = bip39::Mnemonic::generate_in(bip39::Language::English, mnemonic_len as usize);
    match returned {
        Ok(mnemonic) => mnemonic.to_string(),
        Err(e) => {
            tracing::error!("generate_random_mnemonic({mnemonic_len}): {e}");
            String::default()
        }
    }
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
    name: String,
    network: bitcoin::Network,
    mint_url: cashu::MintUrl,
    mnemonic: bip39::Mnemonic,
    db_version: u32,
    swap_expiry: chrono::TimeDelta,
    db: Arc<Database>,
) -> Result<wallet::Wallet> {
    let seed = seed_from_mnemonic(&mnemonic);
    let keypair = keypair_from_mnemonic(&mnemonic);
    let client = HttpClientExt::new(mint_url.clone());

    let wallet_id = build_wallet_id(&seed);
    let clowder_id = client.get_clowder_id().await?;
    let keyset_infos = client.get_mint_keysets().await?.keysets;
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

    let w_cfg = WalletConfig {
        wallet_id,
        name,
        network,
        mint: mint_url,
        mint_keyset_infos: keyset_infos,
        clowder_id,
        debit: debit_unit.to_owned(),
        pub_key: keypair.public_key(),
        betas,
    };
    build_wallet(w_cfg, client, db_version, swap_expiry, db, seed).await
}

async fn build_wallet(
    w_cfg: WalletConfig,
    client: HttpClientExt,
    db_version: u32,
    swap_expiry: chrono::TimeDelta,
    db: Arc<Database>,
    seed: Seed,
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
    )
    .await?;
    Ok(new_wallet)
}
