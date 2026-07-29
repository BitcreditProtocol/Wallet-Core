use crate::config::{AppStateConfig, CreateWalletConfig};
use crate::external::mint::{ClowderMintConnector, HttpClientExt};
use crate::wallet::api::WalletApi;
use crate::wallet::types::{
    WalletBalance, WalletDetailedBalanceEntry, WalletInfo, WalletProtestResult,
};
use bcr_common::core::NodeId;
use bcr_common::{
    cashu::{self, CurrencyUnit},
    wallet::Token,
};
use bcr_wallet_core::contact::Contact;
use bcr_wallet_core::name::Name;
use bcr_wallet_core::types::{
    self, BtcTxStatus, ListTransactionsResult, MeltEstimation, MintSummary, PaymentRequest,
    PaymentRequestDirection, PaymentRequestState, PaymentResultCallback, PaymentSummary,
    PendingPaymentSubscriptionCallback, Seed, Transaction, TransactionCursor, TransactionFilters,
    TransactionSort, WalletConfig,
};
use bcr_wallet_core::util::{
    build_wallet_id, keypair_from_mnemonic, keypair_from_seed, seed_from_mnemonic,
};
use bcr_wallet_persistence::redb::{Database, build_pursedb, build_wallet_dbs, create_db};
use bcr_wallet_transport::NostrEventChannel;
use bcr_wallet_transport::nostr;
use error::{Error, Result};
use std::sync::atomic::Ordering;
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
    btc_cl: Arc<external::bitcoin::BitcoinClient>,
}

impl AppState {
    pub const DB_VERSION: u32 = 1;
    pub const MINT_THRESHOLD_SAT: u64 = 2000;
    pub const MELT_THRESHOLD_SAT: u64 = 546;

    pub async fn initialize(cfg: AppStateConfig) -> Result<Self> {
        tracing::debug!("Initializing API");

        // Open Database file - only allowed to do once!
        let db = Arc::new(create_db(&cfg.db_path)?);
        let pursedb = build_pursedb(AppState::DB_VERSION, db.clone()).await?;

        let http_cl = Arc::new(reqwest::Client::new());
        let purse = purse::Purse::new(pursedb).await?;
        let btc_cl = Arc::new(external::bitcoin::BitcoinClient::new(
            cfg.esplora_base_urls.clone(),
        ));
        let mut appstate = Self {
            purse: Arc::new(purse),
            db,
            cfg,
            http_cl,
            btc_cl,
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
            let keypair = keypair_from_seed(seed);

            let client = HttpClientExt::new(w_cfg.mint.clone());
            let nostr_cl =
                Arc::new(nostr::Client::new(&keypair, w_cfg.nostr_relays.clone()).await?);

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
                    w_cfg.mint_keyset_infos = ks.into_iter().map(|k| (k.id, k)).collect();
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
            )
            .await?;
            purse.add_wallet(wallet).await?;
        }
        Ok(())
    }

    async fn get_wallet(&self, wallet_id: &str) -> Result<Arc<RwLock<wallet::Wallet>>> {
        let purse = self.get_purse();
        purse
            .get_wallet(wallet_id)
            .await
            .ok_or(Error::WalletNotFound(wallet_id.to_owned()))
    }

    fn get_purse(&self) -> Arc<purse::Purse<wallet::Wallet>> {
        self.purse.clone()
    }

    fn get_db(&self) -> Arc<Database> {
        self.db.clone()
    }

    //////////////////////////////////////////////////// Purse-Level API methods
    pub async fn purse_wallets_nostr_connected(&self) -> HashMap<String, bool> {
        tracing::debug!("purse_wallets_nostr_connected");
        let purse = self.get_purse();
        purse.wallets_nostr_connected().await
    }

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
        wallet.read().await.restore_local_proofs().await?;

        let id = purse.add_wallet(wallet).await?;
        tracing::debug!("Wallet restored successfully");
        Ok(id)
    }

    async fn validate_add_wallet(&self, cfg: &CreateWalletConfig) -> Result<()> {
        if self
            .purse
            .names_by_network(cfg.network)
            .await
            .contains(&cfg.name)
        {
            return Err(Error::WalletUniqueName(cfg.name.clone(), cfg.network));
        }

        let seed = seed_from_mnemonic(&cfg.mnemonic);
        let wallet_id = build_wallet_id(&seed, cfg.network);
        let existing_wallet_ids = self.purse.ids().await;
        if existing_wallet_ids.contains(&wallet_id) {
            return Err(Error::WalletUniqueId(wallet_id));
        }

        Ok(())
    }

    pub async fn purse_delete_wallet(&self, wallet_id: String) -> Result<()> {
        tracing::debug!("delete wallet {wallet_id}");
        let purse = self.get_purse();
        purse.delete_wallet(&wallet_id).await?;
        Ok(())
    }

    pub async fn purse_migrate_rabid(&self) -> Result<HashMap<String, url::Url>> {
        tracing::debug!("purse_migrate_rabid");

        let purse = self.get_purse();
        let migrated = purse.migrate_rabid_wallets().await?;

        Ok(migrated)
    }

    ////////////////////////////////////////////////////  Wallet-Level API methods
    pub async fn wallet_info(&self, wallet_id: String) -> Result<WalletInfo> {
        tracing::debug!("info for wallet {wallet_id}");

        let wallet = self.get_wallet(&wallet_id).await?;
        Ok(wallet.read().await.info())
    }

    pub async fn wallet_node_id(&self, wallet_id: String) -> Result<NodeId> {
        tracing::debug!("node_id for wallet {wallet_id}");

        let wallet = self.get_wallet(&wallet_id).await?;
        Ok(wallet.read().await.node_id())
    }

    pub async fn wallet_name(&self, wallet_id: String) -> Result<String> {
        tracing::debug!("name for wallet {wallet_id}");

        let wallet = self.get_wallet(&wallet_id).await?;
        Ok(wallet.read().await.name())
    }

    pub async fn wallet_mint_url(&self, wallet_id: String) -> Result<String> {
        tracing::debug!("mint_url for wallet {wallet_id}");
        let wallet = self.get_wallet(&wallet_id).await?;
        Ok(wallet.read().await.mint_url().to_string())
    }

    pub async fn wallet_currency_unit(&self, wallet_id: String) -> Result<WalletCurrencyUnit> {
        tracing::debug!("wallet_currency_unit({wallet_id})");
        let wallet = self.get_wallet(&wallet_id).await?;
        Ok(WalletCurrencyUnit {
            unit: wallet.read().await.debit_unit().to_string(),
        })
    }

    pub async fn wallet_balance(&self, wallet_id: String) -> Result<WalletBalance> {
        tracing::debug!("wallet_balance({wallet_id})");

        let wallet = self.get_wallet(&wallet_id).await?;
        wallet.read().await.balance().await
    }

    pub async fn wallet_receive_token(&self, wallet_id: String, token: String) -> Result<Uuid> {
        let tstamp = chrono::Utc::now().timestamp() as u64;
        tracing::debug!("wallet_receive({wallet_id}, {token}, {tstamp})");

        let token = Token::from_str(&token).map_err(|e| Error::InvalidToken(e.to_string()))?;
        let wallet = self.get_wallet(&wallet_id).await?;
        let tx_id = wallet.read().await.receive_token(token, tstamp).await?;
        Ok(tx_id)
    }

    pub async fn wallet_mint_is_rabid(&self, wallet_id: String) -> Result<bool> {
        tracing::debug!("wallet_is_rabid({wallet_id})");
        let wallet = self.get_wallet(&wallet_id).await?;
        let is_rabid = wallet.read().await.is_wallet_mint_rabid().await?;
        Ok(is_rabid)
    }

    pub async fn wallet_mint_is_offline(&self, wallet_id: String) -> Result<bool> {
        tracing::debug!("wallet_is_offline({wallet_id})");
        let wallet = self.get_wallet(&wallet_id).await?;
        let is_offline = wallet.read().await.is_wallet_mint_offline().await?;
        Ok(is_offline)
    }

    pub async fn wallet_prepare_pay_by_token(
        &self,
        wallet_id: String,
        amount: u64,
        description: Option<String>,
    ) -> Result<PaymentSummary> {
        tracing::debug!("wallet_prepare_pay_by_token({wallet_id}, {amount}, {description:?})");
        let amount = cashu::Amount::from(amount);
        let wallet = self.get_wallet(&wallet_id).await?;
        let unit = wallet.read().await.debit_unit();

        let summary = wallet
            .read()
            .await
            .prepare_pay_by_token(amount, unit, description)
            .await?;

        Ok(summary)
    }

    pub async fn wallet_pay_by_token(
        &self,
        wallet_id: String,
        rid: String,
    ) -> Result<CreatedToken> {
        let tstamp = chrono::Utc::now().timestamp() as u64;
        tracing::debug!("wallet_pay_by_token({wallet_id}, {rid}, {tstamp})");
        let p_id = Uuid::from_str(&rid)?;

        let wallet = self.get_wallet(&wallet_id).await?;
        let (tx_id, token) = wallet.read().await.pay(p_id, &self.http_cl, tstamp).await?;

        Ok(CreatedToken {
            tx_id,
            token: token.expect("pay by token returns a token"),
        })
    }

    pub async fn wallet_prepare_pay_to_contact(
        &self,
        wallet_id: String,
        node_id: String,
        amount: u64,
        description: Option<String>,
    ) -> Result<PaymentSummary> {
        tracing::debug!(
            "wallet_prepare_pay_to_contact({wallet_id}, {node_id}, {amount}, {description:?})"
        );
        let amount = cashu::Amount::from(amount);
        let wallet = self.get_wallet(&wallet_id).await?;
        let unit = wallet.read().await.debit_unit();
        let node_id = NodeId::from_str(&node_id)?;
        let wallet_network = wallet.read().await.network();
        if node_id.network() != wallet_network {
            return Err(Error::InvalidNetwork(wallet_network, node_id.network()));
        }

        let summary = wallet
            .read()
            .await
            .prepare_pay_to_contact(node_id, amount, unit, description)
            .await?;

        Ok(summary)
    }

    pub async fn wallet_pay_to_contact(&self, wallet_id: String, rid: String) -> Result<Uuid> {
        let tstamp = chrono::Utc::now().timestamp() as u64;
        tracing::debug!("wallet_pay_to_contact({wallet_id}, {rid}, {tstamp})");
        let p_id = Uuid::from_str(&rid)?;

        let wallet = self.get_wallet(&wallet_id).await?;
        let (tx_id, _) = wallet.read().await.pay(p_id, &self.http_cl, tstamp).await?;

        Ok(tx_id)
    }

    pub async fn wallet_request_payment_from_contact(
        &self,
        wallet_id: String,
        node_id: String,
        amount: u64,
        description: Option<String>,
        deadline: Option<u64>,
    ) -> Result<Uuid> {
        tracing::debug!(
            "wallet_request_payment_from_contact({wallet_id}, {node_id}, {amount}, {description:?}, {deadline:?})"
        );
        let node_id = NodeId::from_str(&node_id)?;
        let amount = cashu::Amount::from(amount);
        let wallet = self.get_wallet(&wallet_id).await?;
        let wallet_network = wallet.read().await.network();
        if node_id.network() != wallet_network {
            return Err(Error::InvalidNetwork(wallet_network, node_id.network()));
        }
        let unit = wallet.read().await.debit_unit();
        let payment_req_id = wallet
            .read()
            .await
            .request_payment_from_contact(node_id, amount, unit, description, deadline)
            .await?;
        Ok(payment_req_id)
    }

    pub async fn wallet_subscribe_to_payment_requests(
        &self,
        wallet_id: String,
        cancel_token: CancellationToken,
        item_callback: PendingPaymentSubscriptionCallback,
    ) -> Result<()> {
        tracing::debug!("wallet_subscribe_to_payment_requests({wallet_id})");
        let wallet = self.get_wallet(&wallet_id).await?;
        wallet
            .read()
            .await
            .subscribe_to_payment_requests(cancel_token, item_callback)
            .await?;
        Ok(())
    }

    pub async fn wallet_list_payment_requests(
        &self,
        wallet_id: String,
        direction: PaymentRequestDirection,
        states: Vec<PaymentRequestState>,
    ) -> Result<Vec<PaymentRequest>> {
        tracing::debug!("wallet_list_payment_requests({wallet_id})");
        let wallet = self.get_wallet(&wallet_id).await?;
        let res = wallet
            .read()
            .await
            .list_payment_requests(direction, states)
            .await?;
        Ok(res)
    }

    pub async fn wallet_get_payment_request(
        &self,
        wallet_id: String,
        payment_req_id: String,
    ) -> Result<PaymentRequest> {
        tracing::debug!("wallet_get_payment_request({wallet_id}, {payment_req_id})");
        let payment_req_id = Uuid::from_str(&payment_req_id)?;
        let wallet = self.get_wallet(&wallet_id).await?;
        match wallet
            .read()
            .await
            .get_payment_request(payment_req_id)
            .await?
        {
            Some(p) => Ok(p),
            None => Err(Error::PaymentRequestNotFound(payment_req_id)),
        }
    }

    pub async fn wallet_prepare_pay_payment_request(
        &self,
        wallet_id: String,
        payment_req_id: String,
    ) -> Result<PaymentSummary> {
        tracing::debug!("wallet_prepare_pay_payment_request({wallet_id}, {payment_req_id})");
        let payment_req_id = Uuid::from_str(&payment_req_id)?;
        let wallet = self.get_wallet(&wallet_id).await?;
        let tx_id = wallet
            .read()
            .await
            .prepare_pay_payment_request(payment_req_id)
            .await?;
        Ok(tx_id)
    }

    pub async fn wallet_pay_payment_request(&self, wallet_id: String, rid: String) -> Result<Uuid> {
        tracing::debug!("wallet_pay_payment_request({wallet_id}, {rid})");
        let tstamp = chrono::Utc::now().timestamp() as u64;
        let p_id = Uuid::from_str(&rid)?;
        let wallet = self.get_wallet(&wallet_id).await?;
        let (tx_id, _) = wallet.read().await.pay(p_id, &self.http_cl, tstamp).await?;
        Ok(tx_id)
    }

    pub async fn wallet_reject_payment_request(
        &self,
        wallet_id: String,
        payment_req_id: String,
    ) -> Result<()> {
        tracing::debug!("wallet_reject_payment_request({wallet_id}, {payment_req_id})");
        let payment_req_id = Uuid::from_str(&payment_req_id)?;
        let wallet = self.get_wallet(&wallet_id).await?;
        wallet
            .read()
            .await
            .reject_payment_request(payment_req_id)
            .await?;
        Ok(())
    }

    pub async fn wallet_cancel_payment_request(
        &self,
        wallet_id: String,
        payment_req_id: String,
    ) -> Result<()> {
        tracing::debug!("wallet_cancel_payment_request({wallet_id}, {payment_req_id})");
        let payment_req_id = Uuid::from_str(&payment_req_id)?;
        let wallet = self.get_wallet(&wallet_id).await?;
        wallet
            .read()
            .await
            .cancel_payment_request(payment_req_id)
            .await?;
        Ok(())
    }

    pub async fn wallet_estimate_melt(
        &self,
        wallet_id: String,
        amount: u64,
    ) -> Result<MeltEstimation> {
        if amount < Self::MELT_THRESHOLD_SAT {
            return Err(Error::InsufficientOnChainMeltAmount(amount));
        }
        let parsed_amount = bitcoin::Amount::from_sat(amount);
        let wallet = self.get_wallet(&wallet_id).await?;
        let res = wallet.read().await.estimate_melt(parsed_amount).await?;
        Ok(res)
    }

    pub async fn wallet_prepare_melt(
        &self,
        wallet_id: String,
        amount: u64,
        network_fee: u64,
        melt_fee: u64,
        address: String,
        description: Option<String>,
    ) -> Result<PaymentSummary> {
        tracing::debug!(
            "wallet_prepare_melt({wallet_id}, {amount}, {network_fee}, {melt_fee} {address}, {description:?})"
        );

        if amount < Self::MELT_THRESHOLD_SAT {
            return Err(Error::InsufficientOnChainMeltAmount(amount));
        }
        let parsed_amount = bitcoin::Amount::from_sat(amount);
        let parsed_network_fee = bitcoin::Amount::from_sat(network_fee);
        let parsed_melt_fee = bitcoin::Amount::from_sat(melt_fee);
        let parsed_address = bitcoin::Address::from_str(&address)
            .map_err(|_| Error::InvalidBitcoinAddress(address.clone()))?;

        let wallet = self.get_wallet(&wallet_id).await?;
        if !parsed_address.is_valid_for_network(wallet.read().await.network()) {
            return Err(Error::InvalidBitcoinAddress(address.clone()));
        }
        let summary = wallet
            .read()
            .await
            .prepare_melt(
                parsed_amount,
                parsed_network_fee,
                parsed_melt_fee,
                parsed_address,
                description,
            )
            .await?;

        Ok(summary)
    }

    pub async fn wallet_melt(&self, wallet_id: String, rid: String) -> Result<Uuid> {
        let tstamp = chrono::Utc::now().timestamp() as u64;
        tracing::debug!("wallet_melt({wallet_id}, {rid}, {tstamp})");

        let wallet = self.get_wallet(&wallet_id).await?;
        let p_id = Uuid::from_str(&rid)?;

        let (tx_id, _) = wallet.read().await.pay(p_id, &self.http_cl, tstamp).await?;

        Ok(tx_id)
    }

    pub async fn wallet_mint(&self, wallet_id: String, amount: u64) -> Result<MintSummary> {
        tracing::debug!("wallet_mint({wallet_id}, {amount})");

        if amount < Self::MINT_THRESHOLD_SAT {
            return Err(Error::InsufficientOnChainMintAmount(amount));
        }

        let parsed_amount = bitcoin::Amount::from_sat(amount);
        let wallet = self.get_wallet(&wallet_id).await?;
        let summary = wallet.read().await.mint(parsed_amount).await?;

        Ok(summary)
    }

    pub async fn wallet_check_pending_mints(&self, wallet_id: String) -> Result<Vec<Uuid>> {
        tracing::debug!("wallet_check_pending_mints({wallet_id})");
        let wallet = self.get_wallet(&wallet_id).await?;
        let tx_ids = wallet.read().await.check_pending_mints().await?;

        Ok(tx_ids)
    }

    pub async fn wallet_check_pending_commitments(&self, wallet_id: String) -> Result<()> {
        tracing::debug!("wallet_check_pending_commitments({wallet_id})");
        let wallet = self.get_wallet(&wallet_id).await?;
        wallet.read().await.check_pending_commitments().await?;
        Ok(())
    }

    pub async fn wallet_protest_mint(
        &self,
        wallet_id: String,
        quote_id: String,
    ) -> Result<(
        bcr_common::wire::common::ProtestStatus,
        Option<cashu::Amount>,
    )> {
        tracing::debug!("wallet_protest_mint({wallet_id}, {quote_id})");
        let qid = Uuid::from_str(&quote_id)?;
        let wallet = self.get_wallet(&wallet_id).await?;
        let WalletProtestResult { status, result } = wallet.read().await.protest_mint(qid).await?;
        Ok((status, result.map(|(amount, _)| amount)))
    }

    pub async fn wallet_protest_swap(
        &self,
        wallet_id: String,
        commitment_sig: String,
    ) -> Result<(
        bcr_common::wire::common::ProtestStatus,
        Option<cashu::Amount>,
    )> {
        tracing::debug!("wallet_protest_swap({wallet_id}, {commitment_sig})");
        let sig = bitcoin::secp256k1::schnorr::Signature::from_str(&commitment_sig)
            .map_err(|e| Error::SchnorrSignature(e.to_string()))?;
        let wallet = self.get_wallet(&wallet_id).await?;
        let WalletProtestResult { status, result } = wallet.read().await.protest_swap(sig).await?;
        Ok((status, result.map(|(amount, _)| amount)))
    }

    pub async fn wallet_protest_melt(
        &self,
        wallet_id: String,
        quote_id: String,
    ) -> Result<(
        bcr_common::wire::common::ProtestStatus,
        Option<cashu::Amount>,
    )> {
        tracing::debug!("wallet_protest_melt({wallet_id}, {quote_id})");
        let qid = Uuid::from_str(&quote_id)?;
        let wallet = self.get_wallet(&wallet_id).await?;
        let WalletProtestResult { status, result } = wallet.read().await.protest_melt(qid).await?;
        Ok((status, result.map(|(amount, _)| amount)))
    }

    pub async fn wallet_check_pending_melt_commitments(&self, wallet_id: String) -> Result<()> {
        tracing::debug!("wallet_check_pending_melt_commitments({wallet_id})");
        let wallet = self.get_wallet(&wallet_id).await?;
        wallet.read().await.check_pending_melt_commitments().await?;
        Ok(())
    }

    pub async fn wallet_prepare_cdk18_payment(
        &self,
        wallet_id: String,
        input: String,
    ) -> Result<PaymentSummary> {
        tracing::debug!("wallet_prepare_cdk18_payment({wallet_id}, {input})");

        let wallet = self.get_wallet(&wallet_id).await?;
        let summary = wallet.read().await.prepare_pay_cdk18(input).await?;

        Ok(summary)
    }

    pub async fn wallet_pay(&self, wallet_id: String, rid: String) -> Result<Uuid> {
        let tstamp = chrono::Utc::now().timestamp() as u64;
        tracing::debug!("wallet_pay({wallet_id}, {rid}, {tstamp})");

        let wallet = self.get_wallet(&wallet_id).await?;
        let p_id = Uuid::from_str(&rid)?;

        let (tx_id, _) = wallet.read().await.pay(p_id, &self.http_cl, tstamp).await?;
        Ok(tx_id)
    }

    pub async fn wallet_prepare_payment_request(
        &self,
        wallet_id: String,
        amount: u64,
        description: Option<String>,
    ) -> Result<Cdk18PaymentRequest> {
        tracing::debug!("wallet_prepare_pay_request({wallet_id}, {amount}, {description:?})");

        let amount = cashu::Amount::from(amount);

        let wallet = self.get_wallet(&wallet_id).await?;
        let unit = wallet.read().await.debit_unit();
        let request = wallet
            .read()
            .await
            .prepare_cdk18_payment_request(amount, unit, description)
            .await?;
        Ok(Cdk18PaymentRequest {
            p_id: request.payment_id.clone().unwrap_or_default(),
            request: request.to_string(),
        })
    }

    pub async fn wallet_check_received_payment(
        &self,
        wallet_id: String,
        max_wait_sec: u64,
        p_id: String,
        cancel_token: CancellationToken,
        result_callback: PaymentResultCallback,
    ) -> Result<()> {
        tracing::debug!("wallet_check_received_payment({wallet_id}, {p_id})");

        let p_id = Uuid::from_str(&p_id)?;
        let wallet = self.get_wallet(&wallet_id).await?;

        let max_wait = core::time::Duration::from_secs(max_wait_sec);
        wallet
            .read()
            .await
            .check_received_payment(max_wait, p_id, cancel_token, result_callback)
            .await?;
        Ok(())
    }

    pub async fn wallet_list_tx_ids(&self, wallet_id: String) -> Result<Vec<Uuid>> {
        tracing::debug!("wallet_list_tx_ids({wallet_id})");

        let wallet = self.get_wallet(&wallet_id).await?;
        let tx_ids = wallet.read().await.list_tx_ids().await?;
        Ok(tx_ids)
    }

    pub async fn wallet_list_txs(
        &self,
        wallet_id: String,
        filter: TransactionFilters,
        sort: TransactionSort,
        limit: usize,
        cursor: Option<TransactionCursor>,
    ) -> Result<ListTransactionsResult> {
        tracing::debug!("wallet_list_txs({wallet_id}, {filter:?}, {sort:?}, {limit}, {cursor:?})");

        let wallet = self.get_wallet(&wallet_id).await?;
        let res = wallet
            .read()
            .await
            .list_txs(filter, sort, limit, cursor)
            .await?;
        Ok(res)
    }

    pub async fn wallet_load_tx(&self, wallet_id: String, tx_id: &str) -> Result<Transaction> {
        tracing::debug!("wallet_load_tx({wallet_id}, {tx_id})");

        let tx_id = Uuid::from_str(tx_id)?;
        let wallet = self.get_wallet(&wallet_id).await?;
        let tx = wallet.read().await.load_tx(tx_id).await?;
        Ok(tx)
    }

    pub async fn wallet_edit_tx_memo(
        &self,
        wallet_id: String,
        tx_id: String,
        new_memo: Option<String>,
    ) -> Result<()> {
        tracing::debug!("wallet_edit_tx_memo({wallet_id}, {tx_id}, {new_memo:?})");

        let tx_id = Uuid::from_str(&tx_id)?;
        let wallet = self.get_wallet(&wallet_id).await?;
        wallet.read().await.edit_tx_memo(tx_id, new_memo).await?;
        Ok(())
    }

    pub async fn wallet_reclaim_tx(&self, wallet_id: String, tx_id: &str) -> Result<cashu::Amount> {
        tracing::debug!("wallet_reclaim_tx({wallet_id}, {tx_id})");
        let tx_id = Uuid::from_str(tx_id)?;
        let wallet = self.get_wallet(&wallet_id).await?;
        let amount = wallet.read().await.reclaim_tx(tx_id).await?;
        Ok(amount)
    }

    pub async fn wallet_add_contact(
        &self,
        wallet_id: String,
        node_id: String,
        name: String,
    ) -> Result<()> {
        let node_id = NodeId::from_str(&node_id)?;
        let name = Name::from_str(&name)?;
        let wallet = self.get_wallet(&wallet_id).await?;
        let wallet_network = wallet.read().await.network();
        if node_id.network() != wallet_network {
            return Err(Error::InvalidNetwork(wallet_network, node_id.network()));
        }
        wallet.read().await.add_contact(node_id, name).await?;
        Ok(())
    }

    pub async fn wallet_edit_contact(
        &self,
        wallet_id: String,
        node_id: String,
        name: String,
    ) -> Result<()> {
        let node_id = NodeId::from_str(&node_id)?;
        let name = Name::from_str(&name)?;
        let wallet = self.get_wallet(&wallet_id).await?;
        let wallet_network = wallet.read().await.network();
        if node_id.network() != wallet_network {
            return Err(Error::InvalidNetwork(wallet_network, node_id.network()));
        }
        wallet.read().await.edit_contact(node_id, name).await?;
        Ok(())
    }

    pub async fn wallet_delete_contact(&self, wallet_id: String, node_id: String) -> Result<()> {
        let node_id = NodeId::from_str(&node_id)?;
        let wallet = self.get_wallet(&wallet_id).await?;
        let wallet_network = wallet.read().await.network();
        if node_id.network() != wallet_network {
            return Err(Error::InvalidNetwork(wallet_network, node_id.network()));
        }
        wallet.read().await.delete_contact(node_id).await?;
        Ok(())
    }

    pub async fn wallet_get_contact(&self, wallet_id: String, node_id: String) -> Result<Contact> {
        let node_id = NodeId::from_str(&node_id)?;
        let wallet = self.get_wallet(&wallet_id).await?;
        let wallet_network = wallet.read().await.network();
        if node_id.network() != wallet_network {
            return Err(Error::InvalidNetwork(wallet_network, node_id.network()));
        }
        match wallet.read().await.get_contact(node_id.clone()).await? {
            Some(c) => Ok(c),
            None => Err(Error::ContactNotFound(node_id)),
        }
    }

    pub async fn wallet_list_contacts(
        &self,
        wallet_id: String,
        search_term: Option<String>,
    ) -> Result<Vec<Contact>> {
        let wallet = self.get_wallet(&wallet_id).await?;
        let contacts = wallet.read().await.list_contacts(search_term).await?;
        Ok(contacts)
    }

    // Recover pending stale proofs
    pub async fn wallet_recover_pending_stale_proofs(
        &self,
        wallet_id: String,
    ) -> Result<cashu::Amount> {
        tracing::debug!("wallet_recover_pending_stale_proofs({wallet_id})");
        let wallet = self.get_wallet(&wallet_id).await?;
        let wlt = wallet.read().await;
        let recovered = wlt.recover_pending_stale_proofs().await?;

        Ok(recovered)
    }

    // Clean up Spent proofs
    pub async fn wallet_clean_up_spent_proofs(&self, wallet_id: String) -> Result<usize> {
        tracing::debug!("wallet_clean_up_spent_proofs({wallet_id})");
        let wallet = self.get_wallet(&wallet_id).await?;
        let wlt = wallet.read().await;
        let cleaned_up = wlt.clean_up_spent_proofs().await?;
        Ok(cleaned_up)
    }

    // Retry Nostr Messages
    pub async fn wallet_retry_nostr_messages(&self, wallet_id: String) -> Result<usize> {
        tracing::debug!("wallet_retry_nostr_messages({wallet_id})");
        let wallet = self.get_wallet(&wallet_id).await?;
        let wlt = wallet.read().await;
        let retried = wlt.retry_nostr_messages().await?;
        Ok(retried)
    }

    // Refreshes the state of all pending transactions of the given wallet
    pub async fn wallet_refresh_txs(&self, wallet_id: String) -> Result<usize> {
        tracing::debug!("wallet_refresh_txs({wallet_id})");
        let wallet = self.get_wallet(&wallet_id).await?;
        let updated = wallet.read().await.refresh_txs().await?;
        Ok(updated)
    }

    // Refreshes the state of the transaction with the given id
    pub async fn wallet_refresh_tx(&self, wallet_id: String, tx_id: &str) -> Result<bool> {
        tracing::debug!("wallet_refresh_tx({wallet_id}, {tx_id})");

        let tx_id = Uuid::from_str(tx_id)?;
        let wallet = self.get_wallet(&wallet_id).await?;
        let updated = wallet.read().await.refresh_tx(tx_id).await?;
        Ok(updated)
    }

    pub fn set_dev_mode(&self, dev_mode: bool) {
        self.cfg.dev_mode.store(dev_mode, Ordering::Relaxed);
    }

    //////////////////////////////////////////////////// Wallet Dev Mode Calls
    pub async fn wallet_dev_mode_detailed_balance(
        &self,
        wallet_id: String,
    ) -> Result<Vec<WalletDetailedBalanceEntry>> {
        tracing::debug!("dev_mode_detailed_wallet_balance({wallet_id})");
        if !self.cfg.dev_mode.load(Ordering::Relaxed) {
            return Err(Error::NoDevMode);
        }

        let wallet = self.get_wallet(&wallet_id).await?;
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

    pub async fn check_btc_tx_status(
        &self,
        tx_id: String,
        bitcoin_network: String,
    ) -> Result<BtcTxStatus> {
        tracing::debug!("check_btc_tx_status({tx_id}, {bitcoin_network})");
        let parsed_network = bitcoin::Network::from_str(&bitcoin_network)
            .map_err(|_| Error::InvalidBitcoinNetwork(bitcoin_network))?;
        let parsed_tx_id =
            bitcoin::Txid::from_str(&tx_id).map_err(|_| Error::InvalidBitcoinTxId(tx_id))?;
        let res = self
            .btc_cl
            .check_status_for_transaction(parsed_tx_id, parsed_network)
            .await?;
        Ok(res)
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
            match self
                .wallet_clean_up_spent_proofs(wallet_id.to_owned())
                .await
            {
                Ok(num) => {
                    tracing::info!("Cleaned up {num} spent proofs for wallet {wallet_id}");
                }
                Err(e) => {
                    job_failed = true;
                    tracing::error!(
                        "Error running wallet_clean_up_spent_proofs job for wallet {wallet_id}: {e}"
                    );
                }
            }
            match self.wallet_retry_nostr_messages(wallet_id.to_owned()).await {
                Ok(retried) => {
                    tracing::info!("Retried {retried} nostr messages for wallet {wallet_id}");
                }
                Err(e) => {
                    job_failed = true;
                    tracing::error!(
                        "Error running wallet_refresh_txs job for wallet {wallet_id}: {e}"
                    );
                }
            };
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
pub struct Cdk18PaymentRequest {
    pub request: String,
    pub p_id: String,
}

#[derive(Default, Clone, Debug)]
pub struct WalletCurrencyUnit {
    pub unit: String,
}

#[derive(Clone, Debug)]
pub struct CreatedToken {
    pub tx_id: Uuid,
    pub token: Token,
}

async fn create_new_wallet(
    cfg: CreateWalletConfig,
    db_version: u32,
    swap_expiry: chrono::TimeDelta,
    db: Arc<Database>,
) -> Result<Arc<RwLock<wallet::Wallet>>> {
    let seed = seed_from_mnemonic(&cfg.mnemonic);
    let keypair = keypair_from_mnemonic(&cfg.mnemonic);
    let client = HttpClientExt::new(cfg.default_mint_url.clone());

    let wallet_id = build_wallet_id(&seed, cfg.network);
    let clowder_id = client.get_clowder_id().await?;
    let keyset_infos: HashMap<cashu::Id, cashu::KeySetInfo> = client
        .get_mint_keysets()
        .await?
        .into_iter()
        .map(|k| (k.id, k))
        .collect();
    let betas = client.get_clowder_betas().await?;
    // Attempt to find debit unit in the given keysets
    let currencies = keyset_infos
        .values()
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

    let nostr_cl = Arc::new(nostr::Client::new(&keypair, cfg.nostr_relays.clone()).await?);

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
    build_wallet(w_cfg, client, db_version, swap_expiry, db, seed, nostr_cl).await
}

async fn build_wallet(
    w_cfg: WalletConfig,
    client: HttpClientExt,
    db_version: u32,
    swap_expiry: chrono::TimeDelta,
    db: Arc<Database>,
    seed: Seed,
    nostr_cl: Arc<nostr::Client>,
) -> Result<Arc<RwLock<wallet::Wallet>>> {
    // building wallet dbs
    let (tx_repo, debitdb, mintmeltdb, nostrdb, contactdb, pending_incoming_payment_request_db) =
        build_wallet_dbs(db_version, &w_cfg.wallet_id, &w_cfg.debit, db, seed).await?;

    let nostr_repo = Arc::new(nostrdb);
    let nostr_event_channel = NostrEventChannel::new();
    let nostr_consumer = nostr::Consumer::new(
        nostr_cl.clone(),
        nostr_repo.clone(),
        nostr_event_channel.clone(),
    );
    let nostr_transport = nostr::Transport::new(nostr_cl, nostr_repo.clone());

    // building the debit pocket
    let mut beta_clients = HashMap::<url::Url, Arc<dyn ClowderMintConnector>>::new();
    for beta in w_cfg.betas.clone() {
        let beta_client = HttpClientExt::new(beta.clone());
        beta_clients.insert(beta, Arc::new(beta_client));
    }

    let beta_provider = Arc::new(pocket::RandomBetaProvider::new(
        beta_clients.values().cloned().collect(),
        w_cfg.clowder_id,
    )?);

    let debit_pocket = Box::new(pocket::debit::Pocket::new(
        w_cfg.debit.clone(),
        Arc::new(debitdb),
        Arc::new(mintmeltdb),
        seed,
        beta_provider,
    ));
    // Wrap the client with SentinelClient to send events to sentinel nodes
    let client = {
        let cl = external::mint::SentinelClient::new(client);
        Arc::new(cl) as Arc<dyn ClowderMintConnector>
    };

    let new_wallet = wallet::Wallet::new(
        w_cfg.network,
        client,
        w_cfg.mint_keyset_infos,
        Box::new(tx_repo),
        Box::new(contactdb),
        Box::new(pending_incoming_payment_request_db),
        debit_pocket,
        w_cfg.name,
        w_cfg.wallet_id,
        w_cfg.pub_key,
        w_cfg.clowder_id,
        beta_clients,
        Box::new(|url| Arc::new(external::mint::HttpClientExt::new(url))),
        swap_expiry,
        Arc::new(nostr_transport),
        nostr_event_channel,
        nostr_repo,
        Box::new(nostr_consumer),
    )
    .await;

    Ok(new_wallet)
}
