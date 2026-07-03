pub mod api;
#[cfg(test)]
pub mod test_utils;
pub mod types;
pub mod util;

use crate::{
    ClowderMintConnector,
    error::{Error, Result},
    pocket::debit::DebitPocketApi,
    types::{PAYMENT_TYPE_METADATA_KEY, TRANSACTION_STATUS_METADATA_KEY},
    wallet::{
        api::WalletApi,
        types::{PayReference, SwapConfig, WalletBalance, WalletDetailedBalanceEntry},
        util::tx_can_be_refreshed,
    },
};
use bcr_common::{
    cashu::{self, Amount, CurrencyUnit, KeySetInfo, Proof, ProofsMethods},
    cdk_common::wallet::{Transaction, TransactionDirection, TransactionId},
    core::NodeId,
    wallet::Token,
    wire::clowder::{ConnectedMintResponse, ConnectedMintsResponse},
};
use bcr_wallet_core::{
    contact::Contact,
    event::{ContactPaymentPayload, EventEnvelope},
    types::{
        CONTACT_NODE_ID_METADATA_KEY, ListTransactionsResult, PAYMENT_REQUEST_ID_METADATA_KEY,
        PaymentRequest, PaymentType, TransactionCursor, TransactionFilters, TransactionSort,
        TransactionStatus, extract_fees_per_month,
    },
    util::{from_mint_url, to_mint_url},
};
use bcr_wallet_persistence::{
    ContactStoreApi, NostrRepository, PaymentRequestStoreApi, TransactionRepository,
};
use bcr_wallet_transport::{ConsumerApi, NostrEventChannel, TransportApi};
use bitcoin::{
    base58,
    hashes::{Hash, sha256::Hash as Sha256},
    secp256k1,
};
use chrono::Utc;
use std::{collections::HashMap, str::FromStr, sync::Arc, time::Duration};
use tokio::sync::{Mutex, RwLock, broadcast};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub struct Wallet {
    network: bitcoin::Network,
    client: Arc<dyn ClowderMintConnector>,
    mint_keyset_infos: HashMap<cashu::Id, KeySetInfo>,
    beta_clients: HashMap<url::Url, Arc<dyn ClowderMintConnector>>,
    tx_repo: Box<dyn TransactionRepository>,
    contact_repo: Box<dyn ContactStoreApi>,
    payment_request_repo: Box<dyn PaymentRequestStoreApi>,
    debit: Box<dyn DebitPocketApi>,
    name: String,
    id: String,
    pub_key: secp256k1::PublicKey,
    current_payment: Mutex<Option<PayReference>>,
    current_payment_request: Mutex<Option<cashu::PaymentRequest>>,
    clowder_id: secp256k1::PublicKey,
    client_factory: Box<dyn Fn(url::Url) -> Arc<dyn ClowderMintConnector> + Send + Sync>,
    swap_expiry: chrono::TimeDelta,
    nostr_transport: Arc<dyn TransportApi>,
    nostr_event_channel: NostrEventChannel,
    nostr_repo: Arc<dyn NostrRepository>,
    nostr_consumer_running: Arc<Mutex<bool>>,
    nostr_shutdown: CancellationToken,
}

impl Wallet {
    pub async fn new(
        network: bitcoin::Network,
        client: Arc<dyn ClowderMintConnector>,
        mint_keyset_infos: HashMap<cashu::Id, KeySetInfo>,
        tx_repo: Box<dyn TransactionRepository>,
        contact_repo: Box<dyn ContactStoreApi>,
        payment_request_repo: Box<dyn PaymentRequestStoreApi>,
        debit: Box<dyn DebitPocketApi>,
        name: String,
        id: String,
        pub_key: secp256k1::PublicKey,
        clowder_id: secp256k1::PublicKey,
        beta_clients: HashMap<url::Url, Arc<dyn ClowderMintConnector>>,
        client_factory: Box<dyn Fn(url::Url) -> Arc<dyn ClowderMintConnector> + Send + Sync>,
        swap_expiry: chrono::TimeDelta,
        nostr_transport: Arc<dyn TransportApi>,
        nostr_event_channel: NostrEventChannel,
        nostr_repo: Arc<dyn NostrRepository>,
        nostr_consumer: Box<dyn ConsumerApi>,
    ) -> Arc<RwLock<Self>> {
        let cancel = CancellationToken::new();
        let wallet = Arc::new(RwLock::new(Self {
            network,
            client,
            mint_keyset_infos,
            tx_repo,
            contact_repo,
            payment_request_repo,
            debit,
            name,
            id,
            pub_key,
            current_payment: Mutex::new(None),
            current_payment_request: Mutex::new(None),
            beta_clients,
            clowder_id,
            client_factory,
            swap_expiry,
            nostr_transport,
            nostr_event_channel,
            nostr_repo,
            nostr_consumer_running: Arc::new(Mutex::new(false)),
            nostr_shutdown: cancel.clone(),
        }));
        wallet
            .read()
            .await
            .nostr_connect(nostr_consumer, cancel)
            .await;
        Self::start_nostr_event_listener(wallet.clone()).await;
        wallet
    }

    async fn nostr_connect(&self, nostr_consumer: Box<dyn ConsumerApi>, cancel: CancellationToken) {
        let wallet_id = self.id.clone();
        let nostr_consumer_running = self.nostr_consumer_running.clone();
        tokio::spawn(async move {
            *nostr_consumer_running.lock().await = false;
            // attempt to start nostr consumer
            loop {
                let mut handle = tokio::select! {
                    _ = cancel.cancelled() => {
                        tracing::info!("Nostr consumer cancelled for {}", wallet_id);
                        break;
                    }

                    result = nostr_consumer.start() => match result {
                        Ok(handle) => handle,
                        Err(e) => {
                            tracing::warn!("Could not start Nostr consumer: {e}");

                            tokio::select! {
                                _ = cancel.cancelled() => break,
                                _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                            }

                            continue;
                        }
                    },
                };

                *nostr_consumer_running.lock().await = true;
                tracing::info!("Nostr transport connected for {}", wallet_id);

                // wait for nostr consumer to fail and restart, or cancel the whole process
                tokio::select! {
                    _ = cancel.cancelled() => {
                        tracing::info!("Nostr consumer cancelled for {}", wallet_id);
                        handle.abort_all();
                        *nostr_consumer_running.lock().await = false;
                        break;
                    }

                    _ = async {
                        while let Some(result) = handle.join_next().await {
                            match result {
                                Ok(()) => {
                                    tracing::info!("Nostr consumer task shutdown with success");
                                }
                                Err(e) if e.is_cancelled() => {
                                    tracing::info!("Nostr consumer task was cancelled");
                                }
                                Err(e) => {
                                    tracing::warn!("Nostr consumer task shutdown with error: {e}");
                                }
                            }
                        }
                    } => {
                        *nostr_consumer_running.lock().await = false;
                        tracing::warn!("Nostr consumer stopped, reconnecting in 5 seconds...");
                    }
                }

                // reconnect or cancel
                tokio::select! {
                    _ = cancel.cancelled() => {
                        tracing::info!("Nostr reconnect cancelled for {}", wallet_id);
                        break;
                    }

                    _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                }
            }

            *nostr_consumer_running.lock().await = false;
        });
    }

    async fn start_nostr_event_listener(wallet: Arc<RwLock<Self>>) {
        let mut nostr_receiver = wallet.read().await.nostr_event_channel.subscribe();
        let wallet_network = wallet.read().await.network();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    evt = nostr_receiver.recv() => {
                        let received_evt = match evt {
                            Ok(e) => e,
                            Err(broadcast::error::RecvError::Lagged(_)) => {
                                tracing::warn!("start_nostr_event_listener channel lagged behind");
                                continue;
                            },
                            Err(broadcast::error::RecvError::Closed) => {
                                tracing::warn!("start_nostr_event_listener channel closed");
                                return;
                            },
                        };
                        match received_evt {
                            bcr_wallet_transport::NostrWalletEvent::ContactPaymentRequest { sender, payload, event_id } => {
                                if payload.sender.network() != wallet_network {
                                    tracing::warn!("Rejected incoming Contact payment request from a different network: {}", payload.sender);
                                    continue;
                                }
                                let pending_incoming_payment_request: PaymentRequest = payload.into();
                                let payment_request_id = pending_incoming_payment_request.id;
                                let wallet_guard = wallet.read().await;
                                match <Self as api::WalletApi>::add_payment_request(&*wallet_guard, pending_incoming_payment_request).await {
                                    Ok(_) => {
                                        tracing::info!("Received contact payment request {payment_request_id} from {sender}, event_id: {event_id}");
                                    },
                                    Err(e) => {
                                        tracing::error!("Could not store payment request {payment_request_id}: {e}");
                                    }
                                };
                            },
                            bcr_wallet_transport::NostrWalletEvent::Cdk18Payment { .. } => {
                                // ignore - cdk18 payments are handled by explicitly awaiting them
                            },
                            bcr_wallet_transport::NostrWalletEvent::ContactPayment { sender, payload, event_id } => {
                                if payload.sender.network() != wallet_network {
                                    tracing::warn!("Rejected incoming Contact payment from a different network: {}", payload.sender);
                                    continue;
                                }
                                let mut meta = HashMap::from([
                                    (String::from("sender"), sender.to_string()),
                                    (String::from("nostr_event_id"), event_id.to_string()),
                                    (
                                        String::from(PAYMENT_TYPE_METADATA_KEY),
                                        PaymentType::Contact.to_string(),
                                    ),
                                    (
                                        String::from(TRANSACTION_STATUS_METADATA_KEY),
                                        TransactionStatus::Settled.to_string(),
                                    ),
                                    (
                                        String::from(CONTACT_NODE_ID_METADATA_KEY),
                                        payload.sender.to_string()
                                    ),
                                ]);
                                if let Some(p_req_id) = payload.payment_request_id {
                                    meta.insert(
                                        PAYMENT_REQUEST_ID_METADATA_KEY.to_owned(),
                                        p_req_id.to_string(),
                                    );
                                }

                                let amount = payload.proofs.total_amount().unwrap_or(Amount::ZERO);
                                let wallet_guard = wallet.read().await;
                                match <Self as api::WalletApi>::receive_proofs(
                                    &*wallet_guard,
                                    payload.proofs,
                                    payload.unit,
                                    from_mint_url(&payload.mint),
                                    chrono::Utc::now().timestamp() as u64,
                                    payload.memo,
                                    meta,
                                ).await {
                                    Ok(tx_id) => {
                                        // if it's from a payment request - attempt to set it to paid
                                        if let Some(p_req_id) = payload.payment_request_id &&
                                            let Err(e) = <Self as api::WalletApi>::mark_payment_request_as_paid(&*wallet_guard, p_req_id, tx_id).await {
                                                tracing::error!("Could not set Payment Request {p_req_id} to paid: {e}");
                                        }
                                        tracing::info!("Received Contact Payment from {} for {} with Transaction ID: {}",
                                            payload.sender, amount, tx_id)
                                    },
                                    Err(e) => {
                                        tracing::error!("Error processing Contact Payment from {} for {}: {e}",
                                            payload.sender, amount)
                                    }
                                };
                            },
                        };
                    }
                }
            }
        });
    }

    pub fn name(&self) -> String {
        self.name.clone()
    }

    pub fn network(&self) -> bitcoin::Network {
        self.network
    }

    fn swap_config(&self) -> SwapConfig {
        SwapConfig {
            expiry: self.swap_expiry,
            alpha_pk: self.clowder_id,
        }
    }

    pub async fn list_tx_ids(&self) -> Result<Vec<TransactionId>> {
        let res = self.tx_repo.list_tx_ids().await?;
        Ok(res)
    }

    pub async fn list_txs(
        &self,
        filters: TransactionFilters,
        sort: TransactionSort,
        limit: usize,
        cursor: Option<TransactionCursor>,
    ) -> Result<ListTransactionsResult> {
        if let Some(ref cursor) = cursor
            && !cursor.matches_sort(sort)
        {
            return Err(Error::SortMismatch);
        }

        let mut res = self.tx_repo.list_txs().await?;
        // apply filters
        res.retain(|tx| filters.matches_tx(tx));
        // apply cursor
        if let Some(ref cursor) = cursor {
            res.retain(|tx| cursor.tx_is_after(tx));
        }
        // apply sorting
        match sort {
            TransactionSort::TimeAsc => {
                res.sort_by(|a, b| {
                    a.timestamp
                        .cmp(&b.timestamp)
                        .then_with(|| a.id().cmp(&b.id()))
                });
            }

            TransactionSort::TimeDesc => {
                res.sort_by(|a, b| {
                    b.timestamp
                        .cmp(&a.timestamp)
                        .then_with(|| b.id().cmp(&a.id()))
                });
            }

            TransactionSort::AmountAsc => {
                res.sort_by(|a, b| a.amount.cmp(&b.amount).then_with(|| a.id().cmp(&b.id())));
            }

            TransactionSort::AmountDesc => {
                res.sort_by(|a, b| b.amount.cmp(&a.amount).then_with(|| b.id().cmp(&a.id())));
            }
        }
        // apply limit
        let has_more = res.len() > limit;
        res.truncate(limit);
        let next_cursor = if has_more {
            res.last().map(|tx| TransactionCursor::from_tx(tx, sort))
        } else {
            None
        };
        let fees_by_month = extract_fees_per_month(&res);
        Ok(ListTransactionsResult {
            txs: res,
            next_cursor,
            fees_by_month,
        })
    }

    // Returns (Option<(clowder_path, intermint_alpha_keyset)>, local_alpha_keyset)
    async fn get_clowder_path_and_keysets_info(
        &self,
        mint_url: url::Url,
    ) -> Result<(
        Option<(ConnectedMintsResponse, HashMap<cashu::Id, KeySetInfo>)>,
        HashMap<cashu::Id, KeySetInfo>,
    )> {
        let local_keysets_info = self.get_wallet_mint_keyset_infos().await?;
        if &mint_url == self.client.mint_url() {
            Ok((None, local_keysets_info))
        } else {
            // Intermint Exchange
            let path = self.client.post_clowder_path(mint_url).await?;
            tracing::debug!(
                "Received intermint proofs path {:?}",
                path.mints
                    .iter()
                    .map(|m| (m.mint.to_string(), m.node_id.to_string()))
                    .collect::<Vec<_>>()
            );
            if path.mints.len() < 2 {
                return Err(Error::InvalidClowderPath);
            }

            let alpha_id = path.mints[0].node_id;
            // The path goes through the substitute Beta if the Alpha origin mint is offline
            let beta_mint = path.mints[1].mint.clone();
            tracing::debug!(
                "Intermint Exchange - Alpha: {alpha_id}, Substitute Beta: {}",
                beta_mint.to_string()
            );
            // In the direct exchange case this is the same as the Wallet's mint
            let substitute_client = if &beta_mint == self.client.mint_url() {
                &self.client
            } else {
                self.beta_clients
                    .get(&beta_mint)
                    .ok_or(Error::BetaNotFound(beta_mint))?
            };

            // In the offline case we can only ask the substitute, in the online case we can ask the mint
            // The Beta mint (after Alpha in the path) should have it in any case
            // This can be revised based on some criteria ?
            let alpha_keysets = substitute_client.get_alpha_keysets(alpha_id).await?;

            // The endpoint only returns active keysets
            let intermint_alpha_infos: HashMap<cashu::Id, KeySetInfo> = alpha_keysets
                .iter()
                .map(|keyset| {
                    (
                        keyset.id,
                        cashu::KeySetInfo {
                            id: keyset.id,
                            unit: keyset.unit.clone(),
                            active: true,
                            input_fee_ppk: keyset.input_fee_ppk,
                            final_expiry: keyset.final_expiry,
                        },
                    )
                })
                .collect();
            Ok((Some((path, intermint_alpha_infos)), local_keysets_info))
        }
    }

    async fn get_wallet_mint_keyset_infos(&self) -> Result<HashMap<cashu::Id, KeySetInfo>> {
        Ok(match self.client.get_mint_keysets().await {
            Ok(infos) => infos.into_iter().map(|k| (k.id, k)).collect(),
            Err(e) => {
                tracing::warn!(
                    "Couldn't fetch mint keysets for wallet mint - falling back to config: {:?}, {e}",
                    &self.mint_keyset_infos
                );
                self.mint_keyset_infos.clone()
            }
        })
    }

    pub fn debit_unit(&self) -> CurrencyUnit {
        self.debit.unit()
    }

    pub async fn balance(&self) -> Result<WalletBalance> {
        let keysets_info = self.get_wallet_mint_keyset_infos().await?;
        let balance = self.debit.balance(&keysets_info).await?;
        Ok(WalletBalance {
            debit: balance.debit,
            credit: balance.credit,
            total: balance.debit + balance.credit,
        })
    }

    async fn check_nut18_request(
        &self,
        req: &cashu::PaymentRequest,
    ) -> Result<(Amount, CurrencyUnit, cashu::Transport)> {
        if !req.mints.is_empty() && !req.mints.contains(&to_mint_url(self.client.mint_url())) {
            return Err(Error::InterMint);
        }
        if req.nut10.is_some() {
            return Err(Error::SpendingConditions);
        }
        let Some(amount) = req.amount else {
            return Err(Error::MissingAmount);
        };
        let unit = if let Some(unit) = &req.unit {
            if *unit != self.debit.unit() {
                return Err(Error::InvalidCurrencyUnit(unit.to_string()));
            }
            unit.clone()
        } else {
            self.debit.unit()
        };
        let (nostr_transports, http_transports): (Vec<_>, Vec<_>) = req
            .transports
            .iter()
            .partition(|t| matches!(t._type, cashu::TransportType::Nostr));
        if !http_transports.is_empty() {
            Ok((amount, unit, http_transports[0].clone()))
        } else if !nostr_transports.is_empty() {
            Ok((amount, unit, nostr_transports[0].clone()))
        } else {
            Err(Error::NoTransport)
        }
    }

    pub async fn restore_local_proofs(&self) -> Result<()> {
        let keysets_info = self.get_wallet_mint_keyset_infos().await?;
        self.debit
            .restore_local_proofs(&keysets_info, self.client.clone())
            .await?;
        Ok(())
    }

    pub async fn load_tx(&self, tx_id: TransactionId) -> Result<Transaction> {
        let tx = self.tx_repo.load_tx(tx_id).await?;
        Ok(tx)
    }

    pub async fn edit_tx_memo(&self, tx_id: TransactionId, new_memo: Option<String>) -> Result<()> {
        let _ = self.tx_repo.update_memo(tx_id, new_memo).await?;
        Ok(())
    }

    // Fetches the transaction with the given ID from the database and, if it's in a pending state
    // it attempts to get the current state from the mint and, if it's spent, changes it to spent
    // Returns whether the transaction has been updated
    pub async fn refresh_tx(&self, tx_id: TransactionId) -> Result<bool> {
        let mut updated = false;
        let tx = self.tx_repo.load_tx(tx_id).await?;
        if !util::tx_can_be_refreshed(&tx) {
            return Ok(updated);
        }
        let request = cashu::CheckStateRequest { ys: tx.ys.clone() };
        let response = self.client.post_check_state(request).await?;
        let is_any_spent = response
            .iter()
            .any(|s| matches!(s.state, cashu::State::Spent));
        if is_any_spent {
            self.tx_repo
                .update_metadata(
                    tx_id,
                    String::from(TRANSACTION_STATUS_METADATA_KEY),
                    TransactionStatus::Settled.to_string(),
                )
                .await?;
            updated = true;
        }
        Ok(updated)
    }

    pub async fn refresh_txs(&self) -> Result<usize> {
        let txs = self.tx_repo.list_txs().await?;
        let mut updated = 0;

        for tx in txs.iter() {
            if !tx_can_be_refreshed(tx) {
                continue;
            }

            let tx_id = tx.id();

            match self.refresh_tx(tx_id).await {
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

    pub async fn recover_pending_stale_proofs(&self) -> Result<Amount> {
        let infos = self.get_wallet_mint_keyset_infos().await?;
        // collect ys for pending transactions, so we don't recover proofs from open transactions
        let pending_txs_ys: Vec<cashu::PublicKey> = self
            .tx_repo
            .list_txs()
            .await?
            .into_iter()
            .filter(tx_can_be_refreshed)
            .flat_map(|tx| tx.ys)
            .collect();
        let recovered = self
            .debit
            .recover_pending_stale_proofs(
                &pending_txs_ys,
                &infos,
                self.client.clone(),
                self.swap_config(),
            )
            .await?;
        Ok(recovered)
    }

    pub async fn retry_nostr_messages(&self) -> Result<usize> {
        let retried = self.nostr_transport.retry_messages().await?;
        Ok(retried)
    }

    pub async fn clean_up_spent_proofs(&self) -> Result<usize> {
        let cleaned_up = self
            .debit
            .clean_up_spent_proofs(self.client.clone())
            .await?;
        Ok(cleaned_up)
    }

    pub async fn reclaim_tx(&self, tx_id: TransactionId) -> Result<Amount> {
        let infos = self.get_wallet_mint_keyset_infos().await?;
        self.refresh_tx(tx_id).await?;
        let tx = self.load_tx(tx_id).await?;

        // Only Outgoing and Pending transactions can be reclaimed
        if !util::tx_can_be_refreshed(&tx) {
            return Err(Error::TransactionCantBeReclaimed(tx_id));
        }
        if tx.unit != self.debit.unit() {
            return Err(Error::InvalidCurrencyUnit(tx.unit.to_string()));
        }

        // Reclaim proofs
        tracing::debug!("Reclaim Debit Transaction {tx_id}");
        let amount = self
            .debit
            .reclaim_proofs(&tx.ys, &infos, self.client.clone(), self.swap_config())
            .await?;

        // If amount is zero - this means the transaction was already claimed - we set the transaction to Settled
        if amount == Amount::ZERO {
            self.tx_repo
                .update_metadata(
                    tx_id,
                    String::from(TRANSACTION_STATUS_METADATA_KEY),
                    TransactionStatus::Settled.to_string(),
                )
                .await?;
        } else {
            let fee = if tx.amount > amount {
                tx.amount - amount
            } else {
                Amount::ZERO
            };

            // Set reclaimed transaction to canceled
            self.tx_repo
                .update_metadata(
                    tx_id,
                    String::from(TRANSACTION_STATUS_METADATA_KEY),
                    TransactionStatus::Canceled.to_string(),
                )
                .await?;
            if fee > Amount::ZERO {
                // Set fee in reclaimed transaction
                self.tx_repo.update_fee(tx_id, fee).await?;
            }
        }

        Ok(amount)
    }

    async fn _receive_proofs(
        &self,
        local_alpha_keysets_info: &HashMap<cashu::Id, KeySetInfo>,
        proofs: Vec<cashu::Proof>,
        unit: CurrencyUnit,
        mint: url::Url,
        intermint_infos: Option<(ConnectedMintsResponse, HashMap<cashu::Id, KeySetInfo>)>,
        tstamp: u64,
        memo: Option<String>,
        metadata: HashMap<String, String>,
    ) -> Result<TransactionId> {
        if unit != self.debit.unit() {
            return Err(Error::InvalidCurrencyUnit(unit.to_string()));
        }
        let initial_amount = proofs.total_amount()?;
        let mut proofs = proofs;

        // check if these are proofs we created ourselves - if they are and are still outgoing and pending, we reclaim them
        let tx_id_to_check_for_reclaim = TransactionId::new(proofs.ys()?);
        if let Ok(tx_for_ys) = self.tx_repo.load_tx(tx_id_to_check_for_reclaim).await {
            // if it's Outgoing and Pending, we attempt to reclaim it
            if util::tx_can_be_refreshed(&tx_for_ys) {
                tracing::debug!(
                    "Detected incoming proofs from an outgoing, pending transaction of ourselves - reclaiming"
                );
                self.reclaim_tx(tx_id_to_check_for_reclaim).await?;
                return Ok(tx_id_to_check_for_reclaim);
            }
        }

        let mut is_intermint = false;
        if &mint != self.client.mint_url() {
            is_intermint = true;
            if let Some((clowder_path, _)) = intermint_infos {
                let alpha_id = clowder_path.mints[0].node_id;
                let alpha_client = (self.client_factory)(mint.clone());
                let substitute_beta_mint = clowder_path.mints[1].mint.clone();

                // In the direct exchange case this is the same as the Wallet's mint
                let substitute_client = if &substitute_beta_mint == self.client.mint_url() {
                    &self.client
                } else {
                    self.beta_clients
                        .get(&substitute_beta_mint)
                        .ok_or(Error::BetaNotFound(substitute_beta_mint.clone()))?
                };
                tracing::debug!("Using substitute {}", substitute_beta_mint.to_string());

                // check if alpha is offline
                let is_alpha_offline = substitute_client.get_alpha_offline(alpha_id).await?;
                if !is_alpha_offline {
                    tracing::debug!("Online exchange from {}", mint.to_string());
                    proofs = self
                        .online_exchange(
                            proofs,
                            mint,
                            alpha_client.as_ref(),
                            clowder_path.mints,
                            tstamp,
                        )
                        .await?;
                } else {
                    tracing::debug!("Offline exchange from {}", mint.to_string());
                    let substitute_clowder_id = substitute_client.get_clowder_id().await?;
                    let substitute_proofs = self
                        .offline_exchange(substitute_client.as_ref(), proofs, substitute_clowder_id)
                        .await?;

                    // Alpha proofs -> Substitute Beta proofs is done, so we only need the path from
                    // Substitute Beta to the Wallet Mint
                    tracing::debug!("Got substitute proofs - online exchange to own mint next");
                    let path = clowder_path.mints[1..].to_vec();
                    proofs = self
                        .online_exchange(
                            substitute_proofs,
                            substitute_beta_mint,
                            substitute_client.as_ref(),
                            path,
                            tstamp,
                        )
                        .await?;
                }
            } else {
                // different mint, but no clowder-path set
                return Err(Error::InterMintButNoClowderPath);
            };
        }

        // refresh keyset infos if it was intermint, since it could have changed in-between (added new keyset)
        let alpha_keysets_info = if is_intermint {
            &self.get_wallet_mint_keyset_infos().await?
        } else {
            local_alpha_keysets_info
        };

        let (stored_amount, ys) = self
            .debit
            .receive_proofs(
                self.client.clone(),
                alpha_keysets_info,
                proofs,
                self.swap_config(),
            )
            .await?;
        let fee = if initial_amount > stored_amount {
            initial_amount - stored_amount
        } else {
            Amount::ZERO
        };
        let tx = Transaction {
            mint_url: to_mint_url(self.client.mint_url()),
            direction: TransactionDirection::Incoming,
            fee,
            amount: initial_amount,
            memo,
            metadata,
            timestamp: tstamp,
            unit,
            ys,
            quote_id: None,
            payment_request: None,
            payment_proof: None,
            payment_method: None,
            saga_id: None,
        };
        let txid = self.tx_repo.store_tx(tx).await?;
        Ok(txid)
    }

    async fn offline_exchange(
        &self,
        substitute_client: &dyn ClowderMintConnector,
        proofs: Vec<Proof>,
        substitute_clowder_id: secp256k1::PublicKey,
    ) -> Result<Vec<Proof>> {
        // Ephemeral P2PK secret
        let wallet_pk = cashu::SecretKey::generate();

        let (fingerprints, secrets) = util::proofs_to_fingerprints(proofs)?;

        let hash_locks: Vec<Sha256> = secrets
            .iter()
            .map(|secret| Sha256::hash(&secret.to_bytes()))
            .collect();
        let mut beta_proofs = substitute_client
            .post_offline_exchange(
                fingerprints.clone(),
                hash_locks.clone(),
                *wallet_pk.public_key(),
                substitute_clowder_id,
            )
            .await?;
        for (p, s) in beta_proofs.iter_mut().zip(secrets) {
            util::sign_htlc_proof(p, &s.to_string(), &wallet_pk)?;
        }
        Ok(beta_proofs)
    }

    pub async fn online_exchange(
        &self,
        alpha_proofs: Vec<cashu::Proof>,
        alpha_url: url::Url,
        alpha_client: &dyn ClowderMintConnector,
        path: Vec<ConnectedMintResponse>,
        tstamp: u64,
    ) -> Result<Vec<Proof>> {
        tracing::debug!(alpha_url=?alpha_url, "intermint exchange from ");
        // Already proofs on our mint
        if &alpha_url == self.client.mint_url() {
            tracing::debug!("not intermint exchanging proofs, since they're already on our mint");
            return Ok(alpha_proofs);
        }

        // Ephemeral P2PK secret
        let wallet_pk = cashu::SecretKey::generate();

        // Require all intermediate mints to sign
        // Exclude alpha origin from p2pk lock as it doesn't need to sign its own eCash
        tracing::debug!(
            "Intermint proofs path {:?}",
            path.iter()
                .map(|m| (m.mint.to_string(), m.node_id.to_string()))
                .collect::<Vec<_>>()
        );

        let key_locks: Vec<secp256k1::PublicKey> = path.iter().skip(1).map(|m| m.node_id).collect();
        tracing::debug!(
            "Key locks {}",
            key_locks
                .iter()
                .map(|k| k.to_string())
                .collect::<Vec<String>>()
                .join(",")
        );

        let preimage_key = cashu::SecretKey::generate();
        let preimage = preimage_key.to_secret_hex();
        let hash_lock = Sha256::hash(&preimage_key.to_secret_bytes());

        let alpha_betas = alpha_client.get_clowder_betas().await?;
        let alpha_beta_clients: Vec<_> = alpha_betas
            .iter()
            .map(|url| (self.client_factory)(url.clone()))
            .collect();
        let alpha_beta =
            crate::pocket::RandomBetaProvider::new(alpha_beta_clients, path[0].node_id)?;
        let locked_alpha_proofs = util::htlc_lock(
            tstamp,
            alpha_client,
            alpha_proofs,
            hash_lock,
            key_locks,
            *wallet_pk.public_key(),
            SwapConfig {
                alpha_pk: path[0].node_id,
                ..self.swap_config()
            },
            &alpha_beta,
        )
        .await?;

        let mut exchange_path: Vec<secp256k1::PublicKey> = path.iter().map(|m| m.node_id).collect();
        // Include wallet pubkey as last to be p2pk
        exchange_path.push(*wallet_pk.public_key());

        // Multiple attempts as beta might not immediately have the signatures recorded
        let mut beta_proofs = {
            let mut attempts = 0;
            loop {
                attempts += 1;
                match self
                    .client
                    .post_online_exchange(locked_alpha_proofs.clone(), exchange_path.clone())
                    .await
                {
                    Ok(proofs) => break Ok(proofs),
                    Err(err) if attempts < crate::config::MAX_INTERMINT_ATTEMPTS => {
                        tracing::warn!("Failed to exchange HTLC proofs: {}", err);
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    }
                    Err(err) => {
                        tracing::error!(
                            "Failed to exchange HTLC proofs after max attempts: {}",
                            err
                        );
                        break Err(Error::MaxExchangeAttempts);
                    }
                }
            }
        }?;

        for p in beta_proofs.iter_mut() {
            util::sign_htlc_proof(p, &preimage, &wallet_pk)?;
        }
        tracing::debug!("Returning same mint proofs");
        Ok(beta_proofs)
    }

    pub async fn receive_token(&self, token: Token, tstamp: u64) -> Result<TransactionId> {
        let token_teaser = token.to_string().chars().take(20).collect::<String>();
        let token_mint_url = from_mint_url(&token.mint_url());
        let (intermint_infos, keysets_info) = self
            .get_clowder_path_and_keysets_info(from_mint_url(&token.mint_url()))
            .await?;

        let proofs = if &token_mint_url == self.client.mint_url() {
            let keysets: Vec<KeySetInfo> = keysets_info.values().cloned().collect();
            token.proofs(&keysets)?
        } else if let Some((_, ref intermint_alpha_infos)) = intermint_infos {
            let keysets: Vec<KeySetInfo> = intermint_alpha_infos.values().cloned().collect();
            token.proofs(&keysets)?
        } else {
            // different mint, but no clowder-path set
            return Err(Error::InterMintButNoClowderPath);
        };

        if proofs.is_empty() {
            return Err(Error::EmptyToken(token_teaser));
        }

        let mut metadata = HashMap::default();
        metadata.insert(
            PAYMENT_TYPE_METADATA_KEY.to_owned(),
            PaymentType::Token.to_string(),
        );
        metadata.insert(
            TRANSACTION_STATUS_METADATA_KEY.to_owned(),
            TransactionStatus::Settled.to_string(),
        );

        let tx_id = if token.unit().is_some() && token.unit() == Some(self.debit.unit()) {
            tracing::debug!("import debit token");

            self._receive_proofs(
                &keysets_info,
                proofs,
                self.debit.unit(),
                token_mint_url,
                intermint_infos,
                tstamp,
                token.memo().clone(),
                metadata,
            )
            .await?
        } else {
            return Err(Error::InvalidToken(token_teaser));
        };
        Ok(tx_id)
    }

    async fn pay_to_contact(
        &self,
        proofs: Vec<cashu::Proof>,
        nostr_cl: &Arc<dyn TransportApi>,
        contact: Contact,
        payment_request_id: Option<Uuid>,
        mut partial_tx: Transaction,
    ) -> Result<TransactionId> {
        let payload = ContactPaymentPayload {
            payment_request_id,
            sender: self.node_id(),
            proofs,
            memo: partial_tx.memo.clone(),
            unit: partial_tx.unit.clone(),
            mint: to_mint_url(self.client.mint_url()),
            created_at: Utc::now().timestamp() as u64,
        };
        let event: EventEnvelope =
            bcr_wallet_core::event::Event::new_contact_payment(payload).try_into()?;
        let payload = base58::encode(&borsh::to_vec(&event)?);
        let target = nostr_cl.nip19_for_contact(&contact).await?;

        let event_id = match nostr_cl
            .send_private_msg(target.clone(), payload.clone())
            .await
        {
            Ok(event_id) => event_id,
            Err(e) => {
                tracing::error!("Failed to send contact payment, queuing for retry: {e}");
                match e {
                    bcr_wallet_transport::error::Error::NostrSendPrivateMsg(event_id) => {
                        self.nostr_transport
                            .queue_retry_message(Some(target), payload)
                            .await?;
                        event_id
                    }
                    e => return Err(e.into()),
                }
            }
        };
        partial_tx
            .metadata
            .insert(String::from("nostr::event_id"), event_id.to_string());
        let txid = self.tx_repo.store_tx(partial_tx).await?;
        Ok(txid)
    }

    async fn pay_nut18(
        &self,
        proofs: Vec<cashu::Proof>,
        nostr_cl: &Arc<dyn TransportApi>,
        http_cl: &reqwest::Client,
        transport: cashu::Transport,
        p_id: Option<String>,
        mut partial_tx: Transaction,
    ) -> Result<TransactionId> {
        let payload = cashu::PaymentRequestPayload {
            id: p_id,
            memo: partial_tx.memo.clone(),
            unit: partial_tx.unit.clone(),
            mint: to_mint_url(self.client.mint_url()),
            proofs,
        };
        match transport._type {
            cashu::TransportType::HttpPost => {
                let url = reqwest::Url::from_str(&transport.target)?;
                let response = http_cl.post(url).json(&payload).send().await?;
                response.error_for_status()?;
            }
            cashu::TransportType::Nostr => {
                let payload = serde_json::to_string(&payload)?;
                let event_id = match nostr_cl
                    .send_private_msg(transport.target.clone(), payload.clone())
                    .await
                {
                    Ok(event_id) => event_id,
                    Err(e) => {
                        tracing::error!("Failed to send nut18 payment, queuing for retry: {e}");
                        match e {
                            bcr_wallet_transport::error::Error::NostrSendPrivateMsg(event_id) => {
                                self.nostr_transport
                                    .queue_retry_message(Some(transport.target), payload)
                                    .await?;
                                event_id
                            }
                            e => return Err(e.into()),
                        }
                    }
                };
                partial_tx
                    .metadata
                    .insert(String::from("nostr::event_id"), event_id.to_string());
            }
        }
        let txid = self.tx_repo.store_tx(partial_tx).await?;
        Ok(txid)
    }

    pub async fn dev_mode_detailed_balance(&self) -> Result<Vec<WalletDetailedBalanceEntry>> {
        let keysets_info = self.get_wallet_mint_keyset_infos().await?;

        let detailed_balance = self.debit.dev_mode_detailed_balance(&keysets_info).await?;
        let mut res = Vec::with_capacity(detailed_balance.len());
        for (kid, (final_expiry, amount)) in detailed_balance.into_iter() {
            res.push(WalletDetailedBalanceEntry {
                kid,
                final_expiry,
                amount,
            })
        }

        // sort by final expiry descending, with no final expiry at the end
        res.sort_by_key(|e| {
            (
                e.final_expiry.is_none(),
                std::cmp::Reverse(e.final_expiry.unwrap_or(0)),
            )
        });

        Ok(res)
    }

    // refresh relays for a given contact and update if necessary
    async fn refresh_contact_relays(&self, node_id: &NodeId) {
        if node_id.network() != self.network() {
            tracing::warn!("Tried to ensure nostr contact for a different network {node_id}");
            return;
        }

        let Ok(Some(existing_contact)) = self.contact_repo.get_contact(node_id.to_owned()).await
        else {
            return;
        };

        let existing_relays = existing_contact.nostr_relays;
        let Ok(fetched_relays) = self
            .nostr_transport
            .fetch_relay_list(node_id.npub(), existing_relays.clone())
            .await
        else {
            return;
        };

        // only update if they're not empty
        if !fetched_relays.is_empty()
            && let Err(e) = self
                .contact_repo
                .edit_contact_relays(node_id.to_owned(), fetched_relays)
                .await
        {
            tracing::warn!("Could not update relays for contact {node_id}: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use ::nostr::{
        event::EventId,
        key::PublicKey,
        nips::nip19::{Nip19Profile, ToBech32},
        types::RelayUrl,
    };
    use bcr_common::{cashu::nut18 as cdk18, core_tests, wire::clowder as wire_clowder};
    use bcr_wallet_core::{
        event::ContactPaymentRequestPayload,
        name::Name,
        types::{
            MintSummary, PaymentRequestDirection, PaymentRequestState, PaymentResultCallback,
            TimeRange, get_payment_type, get_transaction_status,
        },
    };
    use bcr_wallet_persistence::{
        MockContactStoreApi, MockNostrRepository, MockPaymentRequestStoreApi,
        MockTransactionRepository,
        test_utils::tests::{test_pub_key, valid_payment_address_testnet},
    };
    use bcr_wallet_transport::NostrEventChannel;
    use secp256k1::SECP256K1;
    use tokio_util::sync::CancellationToken;
    use uuid::Uuid;

    use super::*;
    use crate::{
        external::mint::{HttpClientExt, MockClowderMintConnector},
        pocket::{
            PocketBalance,
            test_utils::tests::{
                MockDebitPocket, mock_commitment_result, setup_attestation_mock, test_kinfos,
            },
        },
        wallet::{
            api::WalletApi,
            test_utils::tests::{MockConsumer, MockTransport},
            types::WalletPaymentType,
        },
    };

    const NODE_ID_1: &str =
        "bitcrt03205b8dec12bc9e879f5b517aa32192a2550e88adcee3e54ec2c7294802568fef";

    fn node_id(value: &str) -> NodeId {
        value.parse().expect("valid node id")
    }

    fn name(value: &str) -> Name {
        Name::from_str(value).expect("valid name")
    }

    fn test_contact() -> Contact {
        Contact {
            node_id: node_id(NODE_ID_1),
            name: name("Minka"),
            nostr_relays: vec![],
        }
    }

    fn payment_request_with(
        id: Uuid,
        direction: PaymentRequestDirection,
        state: PaymentRequestState,
    ) -> PaymentRequest {
        PaymentRequest {
            id,
            node_id: node_id(NODE_ID_1),
            amount: Amount::from(42),
            unit: CurrencyUnit::Sat,
            description: Some("payment request memo".to_string()),
            deadline: Some(999),
            created_at: 123,
            state,
            direction,
        }
    }

    fn test_keyset_and_proofs(
        amounts: &[Amount],
    ) -> (
        cashu::KeySetInfo,
        bcr_common::cashu::MintKeySet,
        Vec<cashu::Proof>,
    ) {
        let (info, keyset) = core_tests::generate_random_ecash_keyset();
        let proofs = core_tests::generate_random_ecash_proofs(&keyset, amounts);
        (cashu::KeySetInfo::from(info), keyset, proofs)
    }

    struct MockWalletCtx {
        pub client: MockClowderMintConnector,
        pub tx_repo: MockTransactionRepository,
        pub debit: MockDebitPocket,
        pub nostr_repo: MockNostrRepository,
        pub contact_repo: MockContactStoreApi,
        pub payment_request_repo: MockPaymentRequestStoreApi,
        pub nostr_transport: MockTransport,
        pub nostr_event_channel: NostrEventChannel,
        pub nostr_consumer: MockConsumer,
    }

    fn wallet_ctx() -> MockWalletCtx {
        let client = MockClowderMintConnector::new();
        MockWalletCtx {
            client,
            tx_repo: MockTransactionRepository::new(),
            debit: MockDebitPocket::new(),
            nostr_repo: MockNostrRepository::new(),
            contact_repo: MockContactStoreApi::new(),
            payment_request_repo: MockPaymentRequestStoreApi::new(),
            nostr_transport: MockTransport::new(),
            nostr_event_channel: NostrEventChannel::new(),
            nostr_consumer: MockConsumer::new(),
        }
    }

    async fn wallet(mut ctx: MockWalletCtx) -> Arc<RwLock<Wallet>> {
        let arc_client: Arc<dyn ClowderMintConnector> = Arc::new(ctx.client);
        let mut beta_mock = crate::external::mint::MockClowderMintConnector::new();
        beta_mock.expect_get_alpha_status().returning(|_| {
            Ok(bcr_common::wire::clowder::AlphaStateResponse {
                state: bcr_common::wire::clowder::SimpleAlphaState::Online(0),
            })
        });
        beta_mock.expect_get_alpha_substitute().returning(|_| {
            Err(bcr_common::client::mint::Error::Internal(
                "no substitute".to_string(),
            ))
        });
        let beta_url = url::Url::parse("https://beta.test").unwrap();
        let mut beta_clients: HashMap<url::Url, Arc<dyn ClowderMintConnector>> = HashMap::new();
        beta_clients.insert(beta_url, Arc::new(beta_mock));
        ctx.nostr_consumer
            .expect_start()
            .returning(|| Ok(tokio::task::JoinSet::new()));

        Wallet::new(
            bitcoin::Network::Testnet,
            arc_client,
            HashMap::new(),
            Box::new(ctx.tx_repo),
            Box::new(ctx.contact_repo),
            Box::new(ctx.payment_request_repo),
            Box::new(ctx.debit),
            "wallet-1".to_owned(),
            "w-1".to_owned(),
            test_pub_key(),
            test_pub_key(),
            beta_clients,
            Box::new(|url| Arc::new(HttpClientExt::new(url))),
            chrono::TimeDelta::seconds(60),
            Arc::new(ctx.nostr_transport),
            ctx.nostr_event_channel,
            Arc::new(ctx.nostr_repo),
            Box::new(ctx.nostr_consumer),
        )
        .await
    }

    async fn wallet_with_betas(
        w: Arc<RwLock<Wallet>>,
        betas: Vec<(url::Url, Arc<dyn ClowderMintConnector>)>,
    ) -> Arc<RwLock<Wallet>> {
        let mut map = HashMap::new();
        for (url, cl) in betas {
            map.insert(url, cl);
        }
        w.write().await.beta_clients = map;
        w
    }

    fn reclaimable_tx(amount: Amount) -> Transaction {
        Transaction {
            mint_url: cashu::MintUrl::from_str("https://mint.example").unwrap(),
            direction: TransactionDirection::Outgoing,
            fee: Amount::ZERO,
            amount,
            memo: None,
            metadata: HashMap::from([(
                String::from(TRANSACTION_STATUS_METADATA_KEY),
                TransactionStatus::Pending.to_string(),
            )]),
            timestamp: 123,
            unit: CurrencyUnit::Sat,
            ys: vec![],
            quote_id: None,
            payment_request: None,
            payment_proof: None,
            payment_method: None,
            saga_id: None,
        }
    }

    fn tx_with(
        n: u64,
        timestamp: u64,
        amount: u64,
        direction: TransactionDirection,
        ptype: PaymentType,
        status: TransactionStatus,
    ) -> Transaction {
        Transaction {
            mint_url: cashu::MintUrl::from_str("https://mint.example").unwrap(),
            direction,
            fee: Amount::ZERO,
            amount: Amount::from(amount),
            memo: None,
            metadata: HashMap::from([
                (String::from(PAYMENT_TYPE_METADATA_KEY), ptype.to_string()),
                (
                    String::from(TRANSACTION_STATUS_METADATA_KEY),
                    status.to_string(),
                ),
            ]),
            timestamp,
            unit: CurrencyUnit::Sat,
            ys: vec![test_cashu_pubkey(n)],
            quote_id: None,
            payment_request: None,
            payment_proof: None,
            payment_method: None,
            saga_id: None,
        }
    }

    fn test_cashu_pubkey(n: u64) -> cashu::PublicKey {
        let mut sk_bytes = [1u8; 32];
        sk_bytes[24..].copy_from_slice(&(n + 1).to_be_bytes());
        let sk = secp256k1::SecretKey::from_slice(&sk_bytes).unwrap();
        let pk = secp256k1::PublicKey::from_secret_key(SECP256K1, &sk);
        cashu::PublicKey::from_slice(&pk.serialize()).unwrap()
    }

    fn sample_txs() -> Vec<Transaction> {
        vec![
            tx_with(
                1,
                100,
                50,
                TransactionDirection::Incoming,
                PaymentType::Token,
                TransactionStatus::Settled,
            ),
            tx_with(
                2,
                100,
                20,
                TransactionDirection::Outgoing,
                PaymentType::Token,
                TransactionStatus::Pending,
            ),
            tx_with(
                3,
                200,
                20,
                TransactionDirection::Incoming,
                PaymentType::Cdk18,
                TransactionStatus::Settled,
            ),
            tx_with(
                4,
                300,
                80,
                TransactionDirection::Outgoing,
                PaymentType::Cdk18,
                TransactionStatus::Canceled,
            ),
            tx_with(
                5,
                300,
                10,
                TransactionDirection::Incoming,
                PaymentType::Token,
                TransactionStatus::Pending,
            ),
            tx_with(
                6,
                400,
                80,
                TransactionDirection::Outgoing,
                PaymentType::Token,
                TransactionStatus::Settled,
            ),
        ]
    }

    fn sort_expected(mut txs: Vec<Transaction>, sort: TransactionSort) -> Vec<Transaction> {
        match sort {
            TransactionSort::TimeAsc => {
                txs.sort_by(|a, b| {
                    a.timestamp
                        .cmp(&b.timestamp)
                        .then_with(|| a.id().cmp(&b.id()))
                });
            }
            TransactionSort::TimeDesc => {
                txs.sort_by(|a, b| {
                    b.timestamp
                        .cmp(&a.timestamp)
                        .then_with(|| b.id().cmp(&a.id()))
                });
            }
            TransactionSort::AmountAsc => {
                txs.sort_by(|a, b| a.amount.cmp(&b.amount).then_with(|| a.id().cmp(&b.id())));
            }
            TransactionSort::AmountDesc => {
                txs.sort_by(|a, b| b.amount.cmp(&a.amount).then_with(|| b.id().cmp(&a.id())));
            }
        }

        txs
    }

    fn tx_ids(txs: &[Transaction]) -> Vec<TransactionId> {
        txs.iter().map(|tx| tx.id()).collect()
    }

    #[tokio::test]
    async fn test_config_builds_expected_config() {
        let mut ctx = wallet_ctx();

        ctx.nostr_transport
            .expect_relays()
            .return_const(vec![RelayUrl::from_str("wss://test.example.com").unwrap()]);

        ctx.debit
            .expect_unit()
            .times(1)
            .returning(|| CurrencyUnit::Sat);

        ctx.client
            .expect_mint_url()
            .times(1)
            .return_const(url::Url::from_str("https://mint.example").unwrap());

        let wlt = wallet(ctx).await;
        let cfg = wlt.read().await.config().expect("config works");

        assert_eq!(cfg.wallet_id, "w-1");
        assert_eq!(cfg.name, "wallet-1");
        assert_eq!(cfg.network, bitcoin::Network::Testnet);
        assert_eq!(cfg.debit, CurrencyUnit::Sat);
        assert_eq!(cfg.mint.to_string(), "https://mint.example/");
        assert_eq!(cfg.pub_key, test_pub_key());
        assert_eq!(cfg.clowder_id, test_pub_key());
        assert_eq!(cfg.betas.len(), 1);
    }

    #[tokio::test]
    async fn test_name() {
        let ctx = wallet_ctx();
        let wlt = wallet(ctx).await;

        let res = wlt.read().await.name();
        assert_eq!(res, "wallet-1".to_owned());
    }

    #[tokio::test]
    async fn test_id() {
        let ctx = wallet_ctx();
        let wlt = wallet(ctx).await;
        assert_eq!(wlt.read().await.id(), "w-1".to_string());
    }

    #[tokio::test]
    async fn test_mint_url() {
        let mut ctx = wallet_ctx();
        ctx.client
            .expect_mint_url()
            .times(1)
            .return_const(url::Url::from_str("https://mint.example").unwrap());

        let wlt = wallet(ctx).await;
        let url = wlt.read().await.mint_url();
        assert_eq!(url.to_string(), "https://mint.example/");
    }

    #[tokio::test]
    async fn test_betas_and_mint_urls() {
        let mut ctx = wallet_ctx();
        ctx.client
            .expect_mint_url()
            .times(1)
            .return_const(url::Url::from_str("https://mint.example").unwrap());
        let mut wlt = wallet(ctx).await;

        let b1 = url::Url::from_str("https://beta1.example").unwrap();
        let b2 = url::Url::from_str("https://beta2.example").unwrap();

        let beta1: Arc<dyn ClowderMintConnector> = Arc::new(MockClowderMintConnector::new());
        let beta2: Arc<dyn ClowderMintConnector> = Arc::new(MockClowderMintConnector::new());

        wlt = wallet_with_betas(wlt, vec![(b1.clone(), beta1), (b2.clone(), beta2)]).await;

        let betas = wlt.read().await.betas();
        assert_eq!(betas.len(), 2);
        assert!(betas.contains(&b1));
        assert!(betas.contains(&b2));

        let urls = wlt.read().await.mint_urls();
        assert!(urls.contains(&b1));
        assert!(urls.contains(&b2));
        assert!(urls.contains(&url::Url::from_str("https://mint.example").unwrap()));
        assert_eq!(urls.len(), 3);
    }

    #[tokio::test]
    async fn test_clowder_id() {
        let ctx = wallet_ctx();
        let wlt = wallet(ctx).await;
        assert_eq!(wlt.read().await.clowder_id(), test_pub_key());
    }

    #[tokio::test]
    async fn test_prepare_pay_unknown_payment_request() {
        let mut ctx = wallet_ctx();
        ctx.client
            .expect_get_mint_keysets()
            .times(1)
            .returning(|| Ok(vec![]));
        let wlt = wallet(ctx).await;

        let err = wlt
            .read()
            .await
            .prepare_pay_cdk18("not-a-request".to_string())
            .await
            .unwrap_err();

        match err {
            Error::UnknownPaymentRequest(s) => assert_eq!(s, "not-a-request"),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_prepare_payment_request_sets_current_request() {
        let mut ctx = wallet_ctx();

        ctx.nostr_transport.expect_cdk18_transport().returning(|| {
            Ok(cdk18::Transport {
                _type: cdk18::TransportType::Nostr,
                target: Nip19Profile::new(
                    PublicKey::from_byte_array([0u8; 32]),
                    vec![RelayUrl::from_str("wss://test.example.com").unwrap()],
                )
                .to_bech32()
                .unwrap(),
                tags: vec![vec![String::from("n"), String::from("17")]],
            })
        });

        ctx.client
            .expect_mint_url()
            .times(1)
            .return_const(url::Url::from_str("https://mint.example").unwrap());

        let wlt = wallet(ctx).await;
        let req = wlt
            .read()
            .await
            .prepare_cdk18_payment_request(
                cashu::Amount::from(123),
                CurrencyUnit::Sat,
                Some("hello".to_string()),
            )
            .await
            .unwrap();

        let stored = wlt
            .read()
            .await
            .current_payment_request
            .lock()
            .await
            .clone();
        assert!(stored.is_some());
        assert_eq!(stored.unwrap().payment_id, req.payment_id);
        assert_eq!(req.amount, Some(cashu::Amount::from(123)));
        assert_eq!(req.unit, Some(CurrencyUnit::Sat));
        assert_eq!(req.description, Some("hello".to_string()));
        assert_eq!(req.single_use, Some(true));
    }

    #[tokio::test]
    async fn test_check_received_payment_errors_if_no_current_request() {
        let ctx = wallet_ctx();
        let wlt = wallet(ctx).await;

        let callback: PaymentResultCallback = Arc::new(move |_| {});
        let cancel_token = CancellationToken::new();

        let pid = Uuid::new_v4();
        let err = wlt
            .read()
            .await
            .check_received_payment(
                std::time::Duration::from_millis(1),
                pid,
                cancel_token,
                callback,
            )
            .await
            .unwrap_err();

        match err {
            Error::NoPrepareRef(x) => assert_eq!(x, pid),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_debit_unit() {
        let mut ctx = wallet_ctx();
        ctx.debit
            .expect_unit()
            .times(1)
            .returning(|| CurrencyUnit::Sat);
        let wlt = wallet(ctx).await;

        let res = wlt.read().await.debit_unit();
        assert_eq!(res, CurrencyUnit::Sat);
    }

    #[tokio::test]
    async fn test_balance() {
        let mut ctx = wallet_ctx();
        ctx.client
            .expect_get_mint_keysets()
            .times(1)
            .returning(|| Ok(vec![]));
        ctx.debit
            .expect_balance()
            .times(1)
            .returning(|_| Ok(PocketBalance::default()));
        let wlt = wallet(ctx).await;

        let res = wlt.read().await.balance().await.expect("balance works");
        assert_eq!(res.debit, Amount::ZERO);
        assert_eq!(res.credit, Amount::ZERO);
        assert_eq!(res.total, Amount::ZERO);
    }

    #[tokio::test]
    async fn test_list_tx_ids() {
        let mut ctx = wallet_ctx();
        ctx.tx_repo
            .expect_list_tx_ids()
            .times(1)
            .returning(|| Ok(vec![]));
        let wlt = wallet(ctx).await;

        let res = wlt.read().await.list_tx_ids().await.unwrap();
        assert!(res.is_empty());
    }

    #[tokio::test]
    async fn test_list_txs() {
        let mut ctx = wallet_ctx();
        ctx.tx_repo
            .expect_list_txs()
            .times(1)
            .returning(|| Ok(vec![]));
        let wlt = wallet(ctx).await;

        let res = wlt
            .read()
            .await
            .list_txs(
                TransactionFilters::default(),
                TransactionSort::default(),
                5,
                None,
            )
            .await
            .unwrap();
        assert!(res.txs.is_empty());
        assert!(res.next_cursor.is_none());
    }

    #[tokio::test]
    async fn test_is_wallet_mint_offline_majority_true() {
        let ctx = wallet_ctx();
        let mut wlt = wallet(ctx).await;

        let b1 = url::Url::from_str("https://b1.example").unwrap();
        let b2 = url::Url::from_str("https://b2.example").unwrap();
        let b3 = url::Url::from_str("https://b3.example").unwrap();

        let mut m1 = MockClowderMintConnector::new();
        let mut m2 = MockClowderMintConnector::new();
        let mut m3 = MockClowderMintConnector::new();

        m1.expect_get_alpha_status().returning(|_pk| {
            Ok(wire_clowder::AlphaStateResponse {
                state: wire_clowder::SimpleAlphaState::Offline(0),
            })
        });
        m2.expect_get_alpha_status().returning(|_pk| {
            Ok(wire_clowder::AlphaStateResponse {
                state: wire_clowder::SimpleAlphaState::Offline(0),
            })
        });
        m3.expect_get_alpha_status().returning(|_pk| {
            Ok(wire_clowder::AlphaStateResponse {
                state: wire_clowder::SimpleAlphaState::Online(0),
            })
        });

        wlt = wallet_with_betas(
            wlt,
            vec![(b1, Arc::new(m1)), (b2, Arc::new(m2)), (b3, Arc::new(m3))],
        )
        .await;

        let res = wlt.read().await.is_wallet_mint_offline().await.unwrap();
        assert!(res);
    }

    #[tokio::test]
    async fn test_is_wallet_mint_rabid_majority_false() {
        let ctx = wallet_ctx();
        let mut wlt = wallet(ctx).await;

        let b1 = url::Url::from_str("https://b1.example").unwrap();
        let b2 = url::Url::from_str("https://b2.example").unwrap();

        let mut m1 = MockClowderMintConnector::new();
        let mut m2 = MockClowderMintConnector::new();

        m1.expect_get_alpha_status().returning(|_pk| {
            Ok(wire_clowder::AlphaStateResponse {
                state: wire_clowder::SimpleAlphaState::Rabid("rabid".to_string()),
            })
        });
        m2.expect_get_alpha_status().returning(|_pk| {
            Ok(wire_clowder::AlphaStateResponse {
                state: wire_clowder::SimpleAlphaState::Online(0),
            })
        });

        wlt = wallet_with_betas(wlt, vec![(b1, Arc::new(m1)), (b2, Arc::new(m2))]).await;

        let res = wlt.read().await.is_wallet_mint_rabid().await.unwrap();
        assert!(!res);
    }

    #[tokio::test]
    async fn test_mint_substitute_returns_some_on_majority_vote() {
        let ctx = wallet_ctx();
        let mut wlt = wallet(ctx).await;

        let b1 = url::Url::from_str("https://b1.example").unwrap();
        let b2 = url::Url::from_str("https://b2.example").unwrap();
        let b3 = url::Url::from_str("https://b3.example").unwrap();

        let substitute = url::Url::from_str("https://sub.example").unwrap();
        let other = url::Url::from_str("https://other.example").unwrap();

        let mut m1 = MockClowderMintConnector::new();
        let mut m2 = MockClowderMintConnector::new();
        let mut m3 = MockClowderMintConnector::new();

        m1.expect_get_alpha_substitute().returning({
            let substitute = substitute.clone();
            move |_pk| {
                Ok(wire_clowder::ConnectedMintResponse {
                    mint: substitute.clone(),
                    clowder: url::Url::from_str("https://clowder.example").unwrap(),
                    node_id: test_pub_key(),
                })
            }
        });
        m2.expect_get_alpha_substitute().returning({
            let substitute = substitute.clone();
            move |_pk| {
                Ok(wire_clowder::ConnectedMintResponse {
                    mint: substitute.clone(),
                    clowder: url::Url::from_str("https://clowder.example").unwrap(),
                    node_id: test_pub_key(),
                })
            }
        });
        m3.expect_get_alpha_substitute().returning({
            let other = other.clone();
            move |_pk| {
                Ok(wire_clowder::ConnectedMintResponse {
                    mint: other.clone(),
                    clowder: url::Url::from_str("https://clowder.example").unwrap(),
                    node_id: test_pub_key(),
                })
            }
        });

        wlt = wallet_with_betas(
            wlt,
            vec![(b1, Arc::new(m1)), (b2, Arc::new(m2)), (b3, Arc::new(m3))],
        )
        .await;

        let res = wlt.read().await.mint_substitute().await.unwrap();
        assert_eq!(res, Some(substitute));
    }

    #[tokio::test]
    async fn test_offline_pay_by_token_errors_if_no_substitute() {
        let mut ctx = wallet_ctx();
        ctx.debit
            .expect_unit()
            .times(1)
            .returning(|| CurrencyUnit::Sat);
        let wlt = wallet(ctx).await;
        wlt.write().await.beta_clients.clear();

        let err = wlt
            .read()
            .await
            .offline_pay_by_token(
                Uuid::new_v4(),
                CurrencyUnit::Sat,
                cashu::Amount::ZERO,
                None,
                123,
            )
            .await
            .unwrap_err();

        match err {
            Error::NoSubstitute => {}
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_pay_token_stores_tx_and_returns_token() {
        let mut ctx = wallet_ctx();

        let pid = Uuid::new_v4();

        ctx.client
            .expect_get_mint_keysets()
            .times(1)
            .returning(|| Ok(vec![]));
        ctx.client
            .expect_mint_url()
            .times(2) // token creation + tx mint_url
            .return_const(url::Url::from_str("https://mint.example").unwrap());

        ctx.debit
            .expect_unit()
            .times(2)
            .returning(|| CurrencyUnit::Sat);

        ctx.debit
            .expect_send_proofs()
            .times(1)
            .returning(|_rid, _infos, _client, _safe| Ok(HashMap::default()));

        ctx.tx_repo
            .expect_store_tx()
            .times(1)
            .returning(|_tx| Ok(TransactionId::new(vec![])));

        let wlt = wallet(ctx).await;
        *wlt.read().await.current_payment.lock().await = Some(PayReference {
            request_id: pid,
            unit: CurrencyUnit::Sat,
            fees: cashu::Amount::ZERO,
            ptype: WalletPaymentType::Token,
            memo: Some("memo".to_string()),
        });

        let http_cl = reqwest::Client::new();

        let (_txid, token) = wlt.read().await.pay(pid, &http_cl, 123).await.unwrap();

        assert!(token.is_some());
    }

    #[tokio::test]
    async fn test_pay_errors_if_no_current_payment_reference() {
        let ctx = wallet_ctx();
        let wlt = wallet(ctx).await;

        let pid = Uuid::new_v4();
        let http_cl = reqwest::Client::new();

        let err = wlt.read().await.pay(pid, &http_cl, 123).await.unwrap_err();

        match err {
            Error::NoPrepareRef(id) => assert_eq!(id, pid),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_pay_errors_if_payment_reference_id_mismatch() {
        let ctx = wallet_ctx();
        let wlt = wallet(ctx).await;

        let prepared_pid = Uuid::new_v4();
        let actual_pid = Uuid::new_v4();

        *wlt.read().await.current_payment.lock().await = Some(PayReference {
            request_id: prepared_pid,
            unit: CurrencyUnit::Sat,
            fees: cashu::Amount::ZERO,
            ptype: WalletPaymentType::Token,
            memo: None,
        });

        let http_cl = reqwest::Client::new();

        let err = wlt
            .read()
            .await
            .pay(actual_pid, &http_cl, 123)
            .await
            .unwrap_err();

        match err {
            Error::NoPrepareRef(id) => assert_eq!(id, actual_pid),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_pay_onchain() {
        let mut ctx = wallet_ctx();

        let pid = Uuid::new_v4();
        let tx_id = TransactionId::new(vec![]);

        ctx.client
            .expect_get_mint_keysets()
            .times(1)
            .returning(|| Ok(vec![]));

        ctx.client
            .expect_mint_url()
            .times(1)
            .return_const(url::Url::from_str("https://mint.example").unwrap());

        ctx.debit
            .expect_unit()
            .times(1)
            .returning(|| CurrencyUnit::Sat);

        ctx.debit
            .expect_pay_onchain_melt()
            .times(1)
            .returning(|_request_id, _client| Ok((bitcoin::Txid::all_zeros(), HashMap::default())));

        ctx.tx_repo.expect_store_tx().times(1).returning(move |tx| {
            assert_eq!(tx.direction, TransactionDirection::Outgoing);
            assert_eq!(tx.amount, Amount::ZERO);
            assert_eq!(tx.fee, Amount::ZERO);
            assert_eq!(tx.unit, CurrencyUnit::Sat);
            assert_eq!(tx.timestamp, 123);
            assert_eq!(tx.memo, Some("melt memo".to_string()));
            assert_eq!(
                tx.metadata.get(PAYMENT_TYPE_METADATA_KEY),
                Some(&PaymentType::OnChain.to_string())
            );
            assert_eq!(
                tx.metadata.get(TRANSACTION_STATUS_METADATA_KEY),
                Some(&TransactionStatus::Settled.to_string())
            );
            Ok(tx_id)
        });

        let wlt = wallet(ctx).await;

        *wlt.read().await.current_payment.lock().await = Some(PayReference {
            request_id: pid,
            unit: CurrencyUnit::Sat,
            fees: cashu::Amount::ZERO,
            ptype: WalletPaymentType::OnChain,
            memo: Some("melt memo".to_string()),
        });

        let http_cl = reqwest::Client::new();

        let (res_tx_id, token) = wlt.read().await.pay(pid, &http_cl, 123).await.unwrap();

        assert_eq!(res_tx_id, tx_id);
        assert!(token.is_none());
    }

    #[tokio::test]
    async fn test_pay_cdk18() {
        let mut ctx = wallet_ctx();

        let pid = Uuid::new_v4();
        let tx_id = TransactionId::new(vec![]);

        ctx.client
            .expect_get_mint_keysets()
            .times(1)
            .returning(|| Ok(vec![]));

        ctx.client
            .expect_mint_url()
            .times(2)
            .return_const(url::Url::from_str("https://mint.example").unwrap());

        ctx.debit
            .expect_unit()
            .times(1)
            .returning(|| CurrencyUnit::Sat);

        ctx.debit
            .expect_send_proofs()
            .times(1)
            .returning(|_rid, _infos, _client, _swap| Ok(HashMap::default()));

        ctx.nostr_transport
            .expect_send_private_msg()
            .times(1)
            .returning(|_target, _payload| Ok(EventId::all_zeros()));

        ctx.tx_repo.expect_store_tx().times(1).returning(move |tx| {
            assert_eq!(tx.direction, TransactionDirection::Outgoing);
            assert_eq!(tx.amount, Amount::ZERO);
            assert_eq!(tx.fee, Amount::ZERO);
            assert_eq!(tx.unit, CurrencyUnit::Sat);
            assert_eq!(tx.timestamp, 123);
            assert_eq!(tx.memo, Some("cdk18 memo".to_string()));
            assert_eq!(
                tx.metadata.get(PAYMENT_TYPE_METADATA_KEY),
                Some(&PaymentType::Cdk18.to_string())
            );
            assert_eq!(
                tx.metadata.get(TRANSACTION_STATUS_METADATA_KEY),
                Some(&TransactionStatus::Pending.to_string())
            );
            assert!(tx.metadata.contains_key("nostr::event_id"));
            Ok(tx_id)
        });

        let transport = cdk18::Transport {
            _type: cdk18::TransportType::Nostr,
            target: Nip19Profile::new(
                PublicKey::from_byte_array([0u8; 32]),
                vec![RelayUrl::from_str("wss://test.example.com").unwrap()],
            )
            .to_bech32()
            .unwrap(),
            tags: vec![],
        };

        let wlt = wallet(ctx).await;

        *wlt.read().await.current_payment.lock().await = Some(PayReference {
            request_id: pid,
            unit: CurrencyUnit::Sat,
            fees: cashu::Amount::ZERO,
            ptype: WalletPaymentType::Cdk18 {
                transport,
                id: Some("payment-id".to_string()),
            },
            memo: Some("cdk18 memo".to_string()),
        });

        let http_cl = reqwest::Client::new();

        let (res_tx_id, token) = wlt.read().await.pay(pid, &http_cl, 123).await.unwrap();

        assert_eq!(res_tx_id, tx_id);
        assert!(token.is_none());
    }

    #[tokio::test]
    async fn test_pay_contact() {
        let mut ctx = wallet_ctx();

        let pid = Uuid::new_v4();
        let tx_id = TransactionId::new(vec![]);
        let contact_node_id = node_id(NODE_ID_1);

        ctx.client
            .expect_get_mint_keysets()
            .times(1)
            .returning(|| Ok(vec![]));

        ctx.client
            .expect_mint_url()
            .times(2)
            .return_const(url::Url::from_str("https://mint.example").unwrap());

        ctx.debit
            .expect_unit()
            .times(1)
            .returning(|| CurrencyUnit::Sat);

        ctx.contact_repo
            .expect_get_contact()
            .times(2)
            .returning(|_| Ok(Some(test_contact())));

        ctx.nostr_transport
            .expect_fetch_relay_list()
            .times(1)
            .returning(|_, _| Ok(vec![]));

        ctx.nostr_transport
            .expect_nip19_for_contact()
            .times(1)
            .returning(|_| {
                Ok(Nip19Profile::new(
                    PublicKey::from_byte_array([0u8; 32]),
                    vec![RelayUrl::from_str("wss://test.example.com").unwrap()],
                )
                .to_bech32()
                .unwrap()
                .to_string())
            });

        ctx.nostr_transport
            .expect_send_private_msg()
            .times(1)
            .returning(|_target, _payload| Ok(EventId::all_zeros()));

        ctx.debit
            .expect_send_proofs()
            .times(1)
            .returning(|_rid, _infos, _client, _swap| Ok(HashMap::default()));

        ctx.tx_repo.expect_store_tx().times(1).returning(move |tx| {
            assert_eq!(tx.direction, TransactionDirection::Outgoing);
            assert_eq!(tx.amount, Amount::ZERO);
            assert_eq!(tx.fee, Amount::ZERO);
            assert_eq!(tx.unit, CurrencyUnit::Sat);
            assert_eq!(tx.timestamp, 123);
            assert_eq!(tx.memo, Some("contact memo".to_string()));
            assert_eq!(
                tx.metadata.get(PAYMENT_TYPE_METADATA_KEY),
                Some(&PaymentType::Contact.to_string())
            );
            assert_eq!(
                tx.metadata.get(TRANSACTION_STATUS_METADATA_KEY),
                Some(&TransactionStatus::Pending.to_string())
            );
            assert_eq!(
                tx.metadata.get(CONTACT_NODE_ID_METADATA_KEY),
                Some(&contact_node_id.to_string())
            );
            assert!(tx.metadata.contains_key("nostr::event_id"));
            Ok(tx_id)
        });

        let wlt = wallet(ctx).await;

        *wlt.read().await.current_payment.lock().await = Some(PayReference {
            request_id: pid,
            unit: CurrencyUnit::Sat,
            fees: cashu::Amount::ZERO,
            ptype: WalletPaymentType::Contact {
                node_id: node_id(NODE_ID_1),
                payment_request_id: None,
            },
            memo: Some("contact memo".to_string()),
        });

        let http_cl = reqwest::Client::new();

        let (res_tx_id, token) = wlt.read().await.pay(pid, &http_cl, 123).await.unwrap();

        assert_eq!(res_tx_id, tx_id);
        assert!(token.is_none());
    }

    #[tokio::test]
    async fn test_mint_uses_debit() {
        let mut ctx = wallet_ctx();

        ctx.client
            .expect_get_mint_keysets()
            .times(1)
            .returning(|| Ok(vec![]));

        ctx.debit
            .expect_mint_onchain()
            .times(1)
            .returning(|_amount, _keysets_info, _client| {
                Ok(MintSummary {
                    quote_id: Uuid::new_v4(),
                    amount: bitcoin::Amount::from_sat(1000),
                    address: valid_payment_address_testnet(),
                    expiry: 0,
                })
            });

        let wlt = wallet(ctx).await;
        let _ = wlt
            .read()
            .await
            .mint(bitcoin::Amount::from_sat(1000))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_recover_pending_stale_proofs() {
        let mut ctx = wallet_ctx();

        ctx.client
            .expect_get_mint_keysets()
            .times(1)
            .returning(|| Ok(vec![]));

        ctx.debit
            .expect_recover_pending_stale_proofs()
            .times(1)
            .returning(|_, _, _, _| Ok(Amount::from(10)));

        ctx.tx_repo.expect_list_txs().returning(|| Ok(vec![]));

        let wlt = wallet(ctx).await;
        let recovered = wlt
            .read()
            .await
            .recover_pending_stale_proofs()
            .await
            .unwrap();
        assert_eq!(recovered, Amount::from(10));
    }

    #[tokio::test]
    async fn test_reclaim_tx_errors_if_transaction_cant_be_reclaimed() {
        let mut ctx = wallet_ctx();
        let tx_id = TransactionId::new(vec![]);

        ctx.client
            .expect_get_mint_keysets()
            .times(1)
            .returning(|| Ok(vec![]));

        let tx = Transaction {
            metadata: HashMap::from([(
                String::from(TRANSACTION_STATUS_METADATA_KEY),
                TransactionStatus::Settled.to_string(),
            )]),
            ..reclaimable_tx(Amount::from(10))
        };

        ctx.tx_repo
            .expect_load_tx()
            .times(2)
            .returning(move |_| Ok(tx.clone()));

        let wlt = wallet(ctx).await;
        let err = wlt.read().await.reclaim_tx(tx_id).await.unwrap_err();

        match err {
            Error::TransactionCantBeReclaimed(id) => assert_eq!(id, tx_id),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_reclaim_tx_sets_settled_if_nothing_reclaimed() {
        let mut ctx = wallet_ctx();
        let tx_id = TransactionId::new(vec![]);
        let tx = reclaimable_tx(Amount::from(10));

        ctx.client
            .expect_get_mint_keysets()
            .times(1)
            .returning(|| Ok(vec![]));

        ctx.client
            .expect_post_check_state()
            .times(1)
            .returning(|_| Ok(vec![]));

        ctx.tx_repo
            .expect_load_tx()
            .times(2)
            .returning(move |_| Ok(tx.clone()));

        ctx.debit
            .expect_unit()
            .times(1)
            .returning(|| CurrencyUnit::Sat);

        ctx.debit
            .expect_reclaim_proofs()
            .times(1)
            .returning(|_, _, _, _| Ok(Amount::ZERO));

        ctx.tx_repo
            .expect_update_metadata()
            .times(1)
            .withf(|_, key, value| {
                key == TRANSACTION_STATUS_METADATA_KEY
                    && value == &TransactionStatus::Settled.to_string()
            })
            .returning(|_, _, _| Ok(None));

        let wlt = wallet(ctx).await;
        let amount = wlt.read().await.reclaim_tx(tx_id).await.unwrap();

        assert_eq!(amount, Amount::ZERO);
    }

    #[tokio::test]
    async fn test_reclaim_tx_sets_canceled_without_fee_if_fully_reclaimed() {
        let mut ctx = wallet_ctx();
        let tx_id = TransactionId::new(vec![]);
        let tx = reclaimable_tx(Amount::from(10));

        ctx.client
            .expect_get_mint_keysets()
            .times(1)
            .returning(|| Ok(vec![]));

        ctx.client
            .expect_post_check_state()
            .times(1)
            .returning(|_| Ok(vec![]));

        ctx.tx_repo
            .expect_load_tx()
            .times(2)
            .returning(move |_| Ok(tx.clone()));

        ctx.debit
            .expect_unit()
            .times(1)
            .returning(|| CurrencyUnit::Sat);

        ctx.debit
            .expect_reclaim_proofs()
            .times(1)
            .returning(|_, _, _, _| Ok(Amount::from(10)));

        ctx.tx_repo
            .expect_update_metadata()
            .times(1)
            .withf(|_, key, value| {
                key == TRANSACTION_STATUS_METADATA_KEY
                    && value == &TransactionStatus::Canceled.to_string()
            })
            .returning(|_, _, _| Ok(None));

        ctx.tx_repo.expect_update_fee().times(0);

        let wlt = wallet(ctx).await;
        let amount = wlt.read().await.reclaim_tx(tx_id).await.unwrap();

        assert_eq!(amount, Amount::from(10));
    }

    #[tokio::test]
    async fn test_reclaim_tx_sets_canceled_and_fee_if_partially_reclaimed() {
        let mut ctx = wallet_ctx();
        let tx_id = TransactionId::new(vec![]);
        let tx = reclaimable_tx(Amount::from(10));

        ctx.client
            .expect_get_mint_keysets()
            .times(1)
            .returning(|| Ok(vec![]));

        ctx.client
            .expect_post_check_state()
            .times(1)
            .returning(|_| Ok(vec![]));

        ctx.tx_repo
            .expect_load_tx()
            .times(2)
            .returning(move |_| Ok(tx.clone()));

        ctx.debit
            .expect_unit()
            .times(1)
            .returning(|| CurrencyUnit::Sat);

        ctx.debit
            .expect_reclaim_proofs()
            .times(1)
            .returning(|_, _, _, _| Ok(Amount::from(7)));

        ctx.tx_repo
            .expect_update_metadata()
            .times(1)
            .withf(|_, key, value| {
                key == TRANSACTION_STATUS_METADATA_KEY
                    && value == &TransactionStatus::Canceled.to_string()
            })
            .returning(|_, _, _| Ok(None));

        ctx.tx_repo
            .expect_update_fee()
            .times(1)
            .withf(|_, fee| *fee == Amount::from(3))
            .returning(|_, _| Ok(()));

        let wlt = wallet(ctx).await;
        let amount = wlt.read().await.reclaim_tx(tx_id).await.unwrap();

        assert_eq!(amount, Amount::from(7));
    }

    #[tokio::test]
    async fn test_edit_tx_memo() {
        let mut ctx = wallet_ctx();
        let tx_id = TransactionId::new(vec![]);
        ctx.tx_repo
            .expect_update_memo()
            .times(2)
            .returning(move |_, _| Ok(None));

        let wlt = wallet(ctx).await;
        wlt.read()
            .await
            .edit_tx_memo(tx_id, Some("new_memo".to_owned()))
            .await
            .unwrap();
        wlt.read().await.edit_tx_memo(tx_id, None).await.unwrap();
    }

    #[tokio::test]
    async fn test_list_txs_all_sorts() {
        for sort in [
            TransactionSort::TimeAsc,
            TransactionSort::TimeDesc,
            TransactionSort::AmountAsc,
            TransactionSort::AmountDesc,
        ] {
            let txs = sample_txs();
            let expected = sort_expected(txs.clone(), sort);

            let mut ctx = wallet_ctx();
            ctx.tx_repo
                .expect_list_txs()
                .times(1)
                .returning(move || Ok(txs.clone()));

            let wlt = wallet(ctx).await;
            let ListTransactionsResult {
                txs, next_cursor, ..
            } = wlt
                .read()
                .await
                .list_txs(TransactionFilters::default(), sort, 20, None)
                .await
                .unwrap();

            assert_eq!(tx_ids(&txs), tx_ids(&expected));
            assert!(next_cursor.is_none());
        }
    }

    #[tokio::test]
    async fn test_list_txs_paginates_without_overlap() {
        let txs = sample_txs();
        let expected = sort_expected(txs.clone(), TransactionSort::TimeDesc);

        let mut ctx = wallet_ctx();
        ctx.tx_repo
            .expect_list_txs()
            .times(3)
            .returning(move || Ok(txs.clone()));

        let wlt = wallet(ctx).await;

        let res1 = wlt
            .read()
            .await
            .list_txs(
                TransactionFilters::default(),
                TransactionSort::TimeDesc,
                2,
                None,
            )
            .await
            .unwrap();
        let page1 = res1.txs;
        let cursor1 = res1.next_cursor;

        assert_eq!(tx_ids(&page1), tx_ids(&expected[0..2]));
        assert_eq!(
            cursor1,
            Some(TransactionCursor::from_tx(
                page1.last().unwrap(),
                TransactionSort::TimeDesc
            ))
        );

        let res2 = wlt
            .read()
            .await
            .list_txs(
                TransactionFilters::default(),
                TransactionSort::TimeDesc,
                2,
                cursor1,
            )
            .await
            .unwrap();
        let page2 = res2.txs;
        let cursor2 = res2.next_cursor;

        assert_eq!(tx_ids(&page2), tx_ids(&expected[2..4]));
        assert_eq!(
            cursor2,
            Some(TransactionCursor::from_tx(
                page2.last().unwrap(),
                TransactionSort::TimeDesc
            ))
        );

        let res3 = wlt
            .read()
            .await
            .list_txs(
                TransactionFilters::default(),
                TransactionSort::TimeDesc,
                2,
                cursor2,
            )
            .await
            .unwrap();
        let page3 = res3.txs;
        let cursor3 = res3.next_cursor;

        assert_eq!(tx_ids(&page3), tx_ids(&expected[4..6]));
        assert!(cursor3.is_none());

        let all_ids = [tx_ids(&page1), tx_ids(&page2), tx_ids(&page3)].concat();
        let unique_ids: std::collections::BTreeSet<_> = all_ids.iter().collect();

        assert_eq!(all_ids.len(), expected.len());
        assert_eq!(unique_ids.len(), expected.len());
        assert_eq!(all_ids, tx_ids(&expected));
    }

    #[tokio::test]
    async fn test_list_txs_applies_filters_and_inclusive_time_range() {
        let txs = sample_txs();
        let filters = TransactionFilters {
            payment_types: vec![PaymentType::Token],
            statuses: vec![TransactionStatus::Pending],
            direction: Some(TransactionDirection::Incoming),
            time_range: Some(TimeRange {
                from: Some(100),
                to: Some(300),
            }),
        };

        let mut expected: Vec<_> = txs
            .clone()
            .into_iter()
            .filter(|tx| {
                get_payment_type(&tx.metadata) == PaymentType::Token
                    && get_transaction_status(&tx.metadata) == TransactionStatus::Pending
                    && tx.direction == TransactionDirection::Incoming
                    && tx.timestamp >= 100
                    && tx.timestamp <= 300
            })
            .collect();

        expected = sort_expected(expected, TransactionSort::TimeDesc);

        let mut ctx = wallet_ctx();
        ctx.tx_repo
            .expect_list_txs()
            .times(1)
            .returning(move || Ok(txs.clone()));

        let wlt = wallet(ctx).await;
        let ListTransactionsResult {
            txs, next_cursor, ..
        } = wlt
            .read()
            .await
            .list_txs(filters, TransactionSort::TimeDesc, 2, None)
            .await
            .unwrap();

        assert_eq!(tx_ids(&txs), tx_ids(&expected));
        assert!(next_cursor.is_none());
        assert!(txs.iter().any(|tx| tx.timestamp == 300));
    }

    #[tokio::test]
    async fn test_list_txs_next_cursor_uses_last_returned_tx() {
        let txs = sample_txs();
        let expected = sort_expected(txs.clone(), TransactionSort::TimeDesc);

        let mut ctx = wallet_ctx();
        ctx.tx_repo
            .expect_list_txs()
            .times(1)
            .returning(move || Ok(txs.clone()));

        let wlt = wallet(ctx).await;

        let ListTransactionsResult {
            txs, next_cursor, ..
        } = wlt
            .read()
            .await
            .list_txs(
                TransactionFilters::default(),
                TransactionSort::TimeDesc,
                5,
                None,
            )
            .await
            .unwrap();

        assert_eq!(tx_ids(&txs), tx_ids(&expected[0..5]));
        assert_eq!(
            next_cursor,
            Some(TransactionCursor::from_tx(
                txs.last().unwrap(),
                TransactionSort::TimeDesc,
            ))
        );

        assert_ne!(
            next_cursor,
            Some(TransactionCursor::from_tx(
                &expected[5],
                TransactionSort::TimeDesc,
            ))
        );
    }

    #[tokio::test]
    async fn test_list_txs_cursor_transaction_does_not_need_to_exist() {
        let txs = sample_txs();
        let expected = sort_expected(txs.clone(), TransactionSort::TimeDesc);

        let cursor_tx = expected[5].clone();
        let cursor = TransactionCursor::from_tx(&cursor_tx, TransactionSort::TimeDesc);

        let mut txs_without_cursor = txs.clone();
        txs_without_cursor.retain(|tx| tx.id() != cursor_tx.id());

        let expected_after_cursor: Vec<_> =
            sort_expected(txs_without_cursor.clone(), TransactionSort::TimeDesc)
                .into_iter()
                .filter(|tx| cursor.tx_is_after(tx))
                .take(6)
                .collect();

        let mut ctx = wallet_ctx();
        ctx.tx_repo
            .expect_list_txs()
            .times(1)
            .returning(move || Ok(txs_without_cursor.clone()));

        let wlt = wallet(ctx).await;
        let ListTransactionsResult { txs, .. } = wlt
            .read()
            .await
            .list_txs(
                TransactionFilters::default(),
                TransactionSort::TimeDesc,
                6,
                Some(cursor),
            )
            .await
            .unwrap();

        assert_eq!(tx_ids(&txs), tx_ids(&expected_after_cursor));
    }

    #[tokio::test]
    async fn test_contacts() {
        let mut ctx = wallet_ctx();

        ctx.nostr_transport
            .expect_relays()
            .return_const(vec![RelayUrl::from_str("wss://test.example.com").unwrap()]);

        ctx.nostr_transport
            .expect_fetch_relay_list()
            .returning(|_, _| Ok(vec![]));

        ctx.contact_repo
            .expect_add_contact()
            .times(1)
            .returning(|_| Ok(()));
        ctx.contact_repo
            .expect_edit_contact()
            .times(1)
            .returning(|_, _| Ok(()));
        ctx.contact_repo
            .expect_delete_contact()
            .times(1)
            .returning(|_| Ok(()));
        ctx.contact_repo
            .expect_get_contact()
            .times(1)
            .returning(|_| Ok(Some(test_contact())));
        ctx.contact_repo
            .expect_list_contacts()
            .times(1)
            .returning(|_| Ok(vec![test_contact()]));

        let wlt = wallet(ctx).await;
        wlt.read()
            .await
            .add_contact(node_id(NODE_ID_1), name("Minka"))
            .await
            .expect("create contact works");

        wlt.read()
            .await
            .edit_contact(node_id(NODE_ID_1), name("Nala"))
            .await
            .expect("edit contact works");

        wlt.read()
            .await
            .delete_contact(node_id(NODE_ID_1))
            .await
            .expect("delete contact works");
        let cts = wlt
            .read()
            .await
            .list_contacts(None)
            .await
            .expect("list contacts works");
        assert!(cts.len() == 1);
    }

    #[tokio::test]
    async fn test_req_payment_from_contact() {
        let mut ctx = wallet_ctx();

        ctx.nostr_transport
            .expect_send_private_msg()
            .times(1)
            .returning(|_, _| Ok(EventId::all_zeros()));
        ctx.nostr_transport
            .expect_fetch_relay_list()
            .times(1)
            .returning(|_, _| Ok(vec![]));
        ctx.nostr_transport
            .expect_nip19_for_contact()
            .times(1)
            .returning(|_| {
                Ok(Nip19Profile::new(
                    PublicKey::from_byte_array([0u8; 32]),
                    vec![RelayUrl::from_str("wss://test.example.com").unwrap()],
                )
                .to_bech32()
                .unwrap()
                .to_string())
            });
        ctx.contact_repo
            .expect_get_contact()
            .times(2)
            .returning(|_| Ok(Some(test_contact())));
        ctx.payment_request_repo
            .expect_add_payment_request()
            .times(1)
            .returning(|_| Ok(()));
        ctx.client
            .expect_mint_url()
            .times(1)
            .return_const(url::Url::from_str("https://mint.example").unwrap());

        let wlt = wallet(ctx).await;
        let _uuid = wlt
            .read()
            .await
            .request_payment_from_contact(
                node_id(NODE_ID_1),
                Amount::from(100),
                CurrencyUnit::Sat,
                None,
                None,
            )
            .await
            .expect("request payment from contact works");
    }

    #[tokio::test]
    async fn test_info_builds_expected_wallet_info() {
        let mut ctx = wallet_ctx();

        ctx.client
            .expect_mint_url()
            .times(1)
            .return_const(url::Url::from_str("https://mint.example").unwrap());

        ctx.nostr_transport
            .expect_relays()
            .times(1)
            .return_const(vec![RelayUrl::from_str("wss://test.example.com").unwrap()]);

        let wlt = wallet(ctx).await;
        let info = wlt.read().await.info();

        assert_eq!(info.name, "wallet-1");
        assert_eq!(info.network, bitcoin::Network::Testnet);
        assert_eq!(
            info.node_id,
            NodeId::new(test_pub_key(), bitcoin::Network::Testnet)
        );
        assert_eq!(info.default_mint_url.to_string(), "https://mint.example/");
        assert_eq!(
            info.nostr_relays,
            vec![RelayUrl::from_str("wss://test.example.com").unwrap()]
        );
    }

    #[tokio::test]
    async fn test_node_id() {
        let ctx = wallet_ctx();
        let wlt = wallet(ctx).await;

        assert_eq!(
            wlt.read().await.node_id(),
            NodeId::new(test_pub_key(), bitcoin::Network::Testnet)
        );
    }

    #[tokio::test]
    async fn test_nostr_relays() {
        let mut ctx = wallet_ctx();

        ctx.nostr_transport
            .expect_relays()
            .times(1)
            .return_const(vec![RelayUrl::from_str("wss://test.example.com").unwrap()]);

        let wlt = wallet(ctx).await;

        assert_eq!(
            wlt.read().await.nostr_relays(),
            vec![RelayUrl::from_str("wss://test.example.com").unwrap()]
        );
    }

    #[tokio::test]
    async fn test_is_nostr_connected() {
        let mut ctx = wallet_ctx();

        ctx.nostr_transport
            .expect_has_connected_relays()
            .times(1)
            .returning(|| true);

        let wlt = wallet(ctx).await;

        assert!(!wlt.read().await.is_nostr_connected().await);
    }

    #[tokio::test]
    async fn test_check_pending_commitments_delegates_to_debit() {
        let mut ctx = wallet_ctx();

        ctx.debit
            .expect_check_pending_commitments()
            .times(1)
            .returning(|_| Ok(()));

        let wlt = wallet(ctx).await;

        wlt.read().await.check_pending_commitments().await.unwrap();
    }

    #[tokio::test]
    async fn test_check_pending_melt_commitments_no_commitments() {
        let mut ctx = wallet_ctx();

        ctx.debit
            .expect_list_melt_commitments()
            .times(1)
            .returning(|| Ok(vec![]));

        let wlt = wallet(ctx).await;

        wlt.read()
            .await
            .check_pending_melt_commitments()
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_list_payment_requests() {
        let mut ctx = wallet_ctx();
        let req_id = Uuid::new_v4();
        let req = payment_request_with(
            req_id,
            PaymentRequestDirection::Incoming,
            PaymentRequestState::Pending,
        );

        ctx.payment_request_repo
            .expect_list_payment_requests()
            .times(1)
            .returning(move |direction, states| {
                assert_eq!(direction, PaymentRequestDirection::Incoming);
                assert_eq!(states, vec![PaymentRequestState::Pending]);
                Ok(vec![req.clone()])
            });

        let wlt = wallet(ctx).await;

        let res = wlt
            .read()
            .await
            .list_payment_requests(
                PaymentRequestDirection::Incoming,
                vec![PaymentRequestState::Pending],
            )
            .await
            .unwrap();

        assert_eq!(res.len(), 1);
        assert_eq!(res[0].id, req_id);
    }

    #[tokio::test]
    async fn test_get_payment_request() {
        let mut ctx = wallet_ctx();
        let req_id = Uuid::new_v4();
        let req = payment_request_with(
            req_id,
            PaymentRequestDirection::Incoming,
            PaymentRequestState::Pending,
        );

        ctx.payment_request_repo
            .expect_get_payment_request()
            .times(1)
            .returning(move |id| {
                assert_eq!(id, req_id);
                Ok(Some(req.clone()))
            });

        let wlt = wallet(ctx).await;

        let res = wlt.read().await.get_payment_request(req_id).await.unwrap();

        assert_eq!(res.unwrap().id, req_id);
    }

    #[tokio::test]
    async fn test_add_payment_request() {
        let mut ctx = wallet_ctx();
        let req_id = Uuid::new_v4();
        let req = payment_request_with(
            req_id,
            PaymentRequestDirection::Incoming,
            PaymentRequestState::Pending,
        );

        ctx.payment_request_repo
            .expect_add_payment_request()
            .times(1)
            .returning(move |stored| {
                assert_eq!(stored.id, req_id);
                assert_eq!(stored.direction, PaymentRequestDirection::Incoming);
                assert_eq!(stored.state, PaymentRequestState::Pending);
                Ok(())
            });

        let wlt = wallet(ctx).await;

        wlt.read().await.add_payment_request(req).await.unwrap();
    }

    #[tokio::test]
    async fn test_reject_payment_request_sets_rejected() {
        let mut ctx = wallet_ctx();
        let req_id = Uuid::new_v4();
        let req = payment_request_with(
            req_id,
            PaymentRequestDirection::Incoming,
            PaymentRequestState::Pending,
        );

        ctx.payment_request_repo
            .expect_get_payment_request()
            .times(1)
            .returning(move |id| {
                assert_eq!(id, req_id);
                Ok(Some(req.clone()))
            });

        ctx.payment_request_repo
            .expect_set_payment_request_state()
            .times(1)
            .returning(move |id, state| {
                assert_eq!(id, req_id);
                assert_eq!(state, PaymentRequestState::Rejected);
                Ok(())
            });

        let wlt = wallet(ctx).await;

        wlt.read()
            .await
            .reject_payment_request(req_id)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_reject_payment_request_errors_if_not_found() {
        let mut ctx = wallet_ctx();
        let req_id = Uuid::new_v4();

        ctx.payment_request_repo
            .expect_get_payment_request()
            .times(1)
            .returning(move |id| {
                assert_eq!(id, req_id);
                Ok(None)
            });

        let wlt = wallet(ctx).await;

        let err = wlt
            .read()
            .await
            .reject_payment_request(req_id)
            .await
            .unwrap_err();

        match err {
            Error::PaymentRequestNotFound(id) => assert_eq!(id, req_id),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_reject_payment_request_errors_if_outgoing() {
        let mut ctx = wallet_ctx();
        let req_id = Uuid::new_v4();
        let req = payment_request_with(
            req_id,
            PaymentRequestDirection::Outgoing,
            PaymentRequestState::Pending,
        );

        ctx.payment_request_repo
            .expect_get_payment_request()
            .times(1)
            .returning(move |_| Ok(Some(req.clone())));

        let wlt = wallet(ctx).await;

        let err = wlt
            .read()
            .await
            .reject_payment_request(req_id)
            .await
            .unwrap_err();

        match err {
            Error::PaymentRequestInWrongState(id) => assert_eq!(id, req_id),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_cancel_payment_request_sets_canceled() {
        let mut ctx = wallet_ctx();
        let req_id = Uuid::new_v4();
        let req = payment_request_with(
            req_id,
            PaymentRequestDirection::Outgoing,
            PaymentRequestState::Pending,
        );

        ctx.payment_request_repo
            .expect_get_payment_request()
            .times(1)
            .returning(move |id| {
                assert_eq!(id, req_id);
                Ok(Some(req.clone()))
            });

        ctx.payment_request_repo
            .expect_set_payment_request_state()
            .times(1)
            .returning(move |id, state| {
                assert_eq!(id, req_id);
                assert_eq!(state, PaymentRequestState::Canceled);
                Ok(())
            });

        let wlt = wallet(ctx).await;

        wlt.read()
            .await
            .cancel_payment_request(req_id)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_cancel_payment_request_errors_if_incoming() {
        let mut ctx = wallet_ctx();
        let req_id = Uuid::new_v4();
        let req = payment_request_with(
            req_id,
            PaymentRequestDirection::Incoming,
            PaymentRequestState::Pending,
        );

        ctx.payment_request_repo
            .expect_get_payment_request()
            .times(1)
            .returning(move |_| Ok(Some(req.clone())));

        let wlt = wallet(ctx).await;

        let err = wlt
            .read()
            .await
            .cancel_payment_request(req_id)
            .await
            .unwrap_err();

        match err {
            Error::PaymentRequestInWrongState(id) => assert_eq!(id, req_id),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_mark_payment_request_as_paid_sets_paid_state() {
        let mut ctx = wallet_ctx();
        let req_id = Uuid::new_v4();
        let tx_id = TransactionId::new(vec![]);
        let req = payment_request_with(
            req_id,
            PaymentRequestDirection::Incoming,
            PaymentRequestState::Rejected,
        );

        ctx.payment_request_repo
            .expect_get_payment_request()
            .times(1)
            .returning(move |id| {
                assert_eq!(id, req_id);
                Ok(Some(req.clone()))
            });

        ctx.payment_request_repo
            .expect_set_payment_request_state()
            .times(1)
            .returning(move |id, state| {
                assert_eq!(id, req_id);
                assert_eq!(state, PaymentRequestState::Paid { tx_id });
                Ok(())
            });

        let wlt = wallet(ctx).await;

        wlt.read()
            .await
            .mark_payment_request_as_paid(req_id, tx_id)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_mark_payment_request_as_paid_errors_if_not_found() {
        let mut ctx = wallet_ctx();
        let req_id = Uuid::new_v4();
        let tx_id = TransactionId::new(vec![]);

        ctx.payment_request_repo
            .expect_get_payment_request()
            .times(1)
            .returning(move |id| {
                assert_eq!(id, req_id);
                Ok(None)
            });

        let wlt = wallet(ctx).await;

        let err = wlt
            .read()
            .await
            .mark_payment_request_as_paid(req_id, tx_id)
            .await
            .unwrap_err();

        match err {
            Error::PaymentRequestNotFound(id) => assert_eq!(id, req_id),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_prepare_pay_payment_request_errors_if_not_found() {
        let mut ctx = wallet_ctx();
        let req_id = Uuid::new_v4();

        ctx.payment_request_repo
            .expect_get_payment_request()
            .times(1)
            .returning(move |id| {
                assert_eq!(id, req_id);
                Ok(None)
            });

        let wlt = wallet(ctx).await;

        let err = wlt
            .read()
            .await
            .prepare_pay_payment_request(req_id)
            .await
            .unwrap_err();

        match err {
            Error::PaymentRequestNotFound(id) => assert_eq!(id, req_id),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_prepare_pay_payment_request_errors_if_outgoing() {
        let mut ctx = wallet_ctx();
        let req_id = Uuid::new_v4();
        let req = payment_request_with(
            req_id,
            PaymentRequestDirection::Outgoing,
            PaymentRequestState::Pending,
        );

        ctx.payment_request_repo
            .expect_get_payment_request()
            .times(1)
            .returning(move |_| Ok(Some(req.clone())));

        let wlt = wallet(ctx).await;

        let err = wlt
            .read()
            .await
            .prepare_pay_payment_request(req_id)
            .await
            .unwrap_err();

        match err {
            Error::PaymentRequestInWrongState(id) => assert_eq!(id, req_id),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_prepare_pay_payment_request_errors_if_contact_missing() {
        let mut ctx = wallet_ctx();
        let req_id = Uuid::new_v4();
        let req = payment_request_with(
            req_id,
            PaymentRequestDirection::Incoming,
            PaymentRequestState::Pending,
        );
        let missing_node_id = req.node_id.clone();

        ctx.payment_request_repo
            .expect_get_payment_request()
            .times(1)
            .returning(move |_| Ok(Some(req.clone())));

        ctx.contact_repo
            .expect_get_contact()
            .times(1)
            .returning(|_| Ok(None));

        let wlt = wallet(ctx).await;

        let err = wlt
            .read()
            .await
            .prepare_pay_payment_request(req_id)
            .await
            .unwrap_err();

        match err {
            Error::ContactNotFound(id) => assert_eq!(id, missing_node_id),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_prepare_pay_to_contact_errors_if_contact_missing() {
        let mut ctx = wallet_ctx();

        ctx.debit
            .expect_unit()
            .times(1)
            .returning(|| CurrencyUnit::Sat);

        ctx.contact_repo
            .expect_get_contact()
            .times(1)
            .returning(|_| Ok(None));

        let wlt = wallet(ctx).await;

        let err = wlt
            .read()
            .await
            .prepare_pay_to_contact(
                node_id(NODE_ID_1),
                Amount::from(321),
                CurrencyUnit::Sat,
                None,
            )
            .await
            .unwrap_err();

        match err {
            Error::ContactNotFound(id) => assert_eq!(id, node_id(NODE_ID_1)),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_prepare_pay_payment_request_success_sets_contact_payment_reference() {
        let mut ctx = wallet_ctx();
        let req_id = Uuid::new_v4();

        let req = payment_request_with(
            req_id,
            PaymentRequestDirection::Incoming,
            PaymentRequestState::Pending,
        );

        ctx.payment_request_repo
            .expect_get_payment_request()
            .times(1)
            .returning(move |id| {
                assert_eq!(id, req_id);
                Ok(Some(req.clone()))
            });

        ctx.contact_repo
            .expect_get_contact()
            .times(1)
            .returning(|_| Ok(Some(test_contact())));

        ctx.client
            .expect_get_mint_keysets()
            .times(1)
            .returning(|| Ok(vec![]));

        ctx.debit
            .expect_prepare_send()
            .times(1)
            .returning(|amount, _infos| {
                assert_eq!(amount, Amount::from(42));
                Ok(Default::default())
            });

        let wlt = wallet(ctx).await;

        let summary = wlt
            .read()
            .await
            .prepare_pay_payment_request(req_id)
            .await
            .unwrap();

        assert_eq!(summary.ptype, PaymentType::Contact);

        let binding = wlt.read().await;
        let pref_lock = binding.current_payment.lock().await;
        let p = pref_lock.as_ref().unwrap();

        match &p.ptype {
            WalletPaymentType::Contact {
                node_id,
                payment_request_id,
            } => {
                assert_eq!(node_id, &self::node_id(NODE_ID_1));
                assert_eq!(payment_request_id, &Some(req_id));
            }
            _ => panic!("unexpected payment type"),
        }

        assert_eq!(p.memo, Some("payment request memo".to_string()));
    }

    #[tokio::test]
    async fn test_receive_proofs_stores_incoming_tx_for_wallet_mint() {
        let mut ctx = wallet_ctx();
        let tx_id = TransactionId::new(vec![]);
        let ys = vec![test_cashu_pubkey(10)];

        ctx.client
            .expect_get_mint_keysets()
            .times(1)
            .returning(|| Ok(vec![]));

        ctx.client
            .expect_mint_url()
            .times(3)
            .return_const(url::Url::from_str("https://mint.example").unwrap());

        ctx.debit
            .expect_unit()
            .times(1)
            .returning(|| CurrencyUnit::Sat);

        ctx.debit.expect_receive_proofs().times(1).returning(
            move |_client, _infos, proofs, _swap_config| {
                assert!(proofs.is_empty());
                Ok((Amount::ZERO, ys.clone()))
            },
        );

        ctx.tx_repo.expect_load_tx().times(1).returning(|_| {
            Err(bcr_wallet_persistence::error::Error::TransactionNotFound(
                TransactionId::new(vec![]),
            ))
        });

        ctx.tx_repo.expect_store_tx().times(1).returning(move |tx| {
            assert_eq!(tx.direction, TransactionDirection::Incoming);
            assert_eq!(tx.amount, Amount::ZERO);
            assert_eq!(tx.fee, Amount::ZERO);
            assert_eq!(tx.unit, CurrencyUnit::Sat);
            assert_eq!(tx.timestamp, 123);
            assert_eq!(tx.memo, Some("memo".to_string()));
            Ok(tx_id)
        });

        let wlt = wallet(ctx).await;

        let res = wlt
            .read()
            .await
            .receive_proofs(
                vec![],
                CurrencyUnit::Sat,
                url::Url::from_str("https://mint.example").unwrap(),
                123,
                Some("memo".to_string()),
                HashMap::from([(
                    String::from(PAYMENT_TYPE_METADATA_KEY),
                    PaymentType::Token.to_string(),
                )]),
            )
            .await
            .unwrap();

        assert_eq!(res, tx_id);
    }

    #[tokio::test]
    async fn test_delete_shuts_down_and_deletes_repositories() {
        let mut ctx = wallet_ctx();

        ctx.nostr_transport
            .expect_shutdown()
            .times(1)
            .returning(|| ());

        ctx.debit.expect_delete().times(1).returning(|| Ok(()));

        ctx.tx_repo
            .expect_delete_repo()
            .times(1)
            .returning(|| Ok(()));

        ctx.nostr_repo
            .expect_delete_repo()
            .times(1)
            .returning(|| Ok(()));

        ctx.contact_repo
            .expect_delete_repo()
            .times(1)
            .returning(|| Ok(()));

        ctx.payment_request_repo
            .expect_delete_repo()
            .times(1)
            .returning(|| Ok(()));

        let wlt = wallet(ctx).await;

        wlt.read().await.delete().await.unwrap();
    }

    #[tokio::test]
    async fn test_start_nostr_event_listener_contact_payment() {
        let mut ctx = wallet_ctx();
        let req_id = Uuid::new_v4();
        let tx_id = TransactionId::new(vec![]);
        let sender = node_id(NODE_ID_1);
        let sender_for_tx = sender.clone();
        let tx_id_for_store = tx_id;
        let tx_id_for_state = tx_id;
        let (processed_tx, processed_rx) = tokio::sync::oneshot::channel();
        let (marked_paid_tx, marked_paid_rx) = tokio::sync::oneshot::channel();

        let nostr_event_channel = ctx.nostr_event_channel.clone();

        ctx.debit
            .expect_unit()
            .times(1)
            .returning(|| CurrencyUnit::Sat);
        ctx.client
            .expect_get_mint_keysets()
            .times(1)
            .returning(|| Ok(vec![]));
        ctx.client
            .expect_mint_url()
            .return_const(url::Url::from_str("https://mint.example").unwrap());
        let ys = vec![test_cashu_pubkey(10)];
        ctx.debit.expect_receive_proofs().times(1).returning(
            move |_client, _infos, proofs, _swap_config| {
                assert!(proofs.is_empty());
                Ok((Amount::ZERO, ys.clone()))
            },
        );
        ctx.tx_repo.expect_load_tx().times(1).returning(|_| {
            Err(bcr_wallet_persistence::error::Error::TransactionNotFound(
                TransactionId::new(vec![]),
            ))
        });
        ctx.tx_repo
            .expect_store_tx()
            .times(1)
            .return_once(move |tx| {
                assert_eq!(tx.direction, TransactionDirection::Incoming);
                assert_eq!(tx.amount, Amount::ZERO);
                assert_eq!(tx.unit, CurrencyUnit::Sat);
                assert_eq!(tx.memo, Some("payment request memo".to_string()));
                assert_eq!(
                    tx.metadata.get(PAYMENT_TYPE_METADATA_KEY),
                    Some(&PaymentType::Contact.to_string())
                );
                assert_eq!(
                    tx.metadata.get(TRANSACTION_STATUS_METADATA_KEY),
                    Some(&TransactionStatus::Settled.to_string())
                );
                assert_eq!(
                    tx.metadata.get(PAYMENT_REQUEST_ID_METADATA_KEY),
                    Some(&req_id.to_string())
                );
                assert_eq!(
                    tx.metadata.get(CONTACT_NODE_ID_METADATA_KEY),
                    Some(&sender_for_tx.to_string())
                );

                processed_tx.send(()).expect("test receiver still alive");
                Ok(tx_id_for_store)
            });

        let req = payment_request_with(
            req_id,
            PaymentRequestDirection::Incoming,
            PaymentRequestState::Pending,
        );
        ctx.payment_request_repo
            .expect_get_payment_request()
            .times(1)
            .return_once(move |id| {
                assert_eq!(id, req_id);
                Ok(Some(req))
            });
        ctx.payment_request_repo
            .expect_set_payment_request_state()
            .times(1)
            .return_once(move |id, state| {
                assert_eq!(id, req_id);
                assert_eq!(
                    state,
                    PaymentRequestState::Paid {
                        tx_id: tx_id_for_state
                    }
                );
                marked_paid_tx.send(()).expect("test receiver still alive");
                Ok(())
            });

        let _wlt = wallet(ctx).await;

        let payload = ContactPaymentPayload {
            payment_request_id: Some(req_id),
            sender,
            proofs: vec![],
            unit: CurrencyUnit::Sat,
            memo: Some("payment request memo".to_string()),
            created_at: 123,
            mint: cashu::MintUrl::from_str("https://mint.example").unwrap(),
        };

        nostr_event_channel.publish(bcr_wallet_transport::NostrWalletEvent::ContactPayment {
            sender: node_id(NODE_ID_1).npub(),
            payload,
            event_id: EventId::all_zeros(),
        });

        tokio::time::timeout(Duration::from_secs(1), processed_rx)
            .await
            .expect("contact payment event should be processed")
            .expect("contact payment processing signal should be sent");
        tokio::time::timeout(Duration::from_secs(1), marked_paid_rx)
            .await
            .expect("payment request should be marked as paid")
            .expect("paid-state signal should be sent");
    }

    #[tokio::test]
    async fn test_start_nostr_event_listener_contact_payment_request() {
        let mut ctx = wallet_ctx();
        let req_id = Uuid::new_v4();
        let sender = node_id(NODE_ID_1);
        let sender_for_assert = sender.clone();
        let (stored_tx, stored_rx) = tokio::sync::oneshot::channel();

        let nostr_event_channel = ctx.nostr_event_channel.clone();

        ctx.payment_request_repo
            .expect_add_payment_request()
            .times(1)
            .return_once(move |stored| {
                assert_eq!(stored.id, req_id);
                assert_eq!(stored.node_id, sender_for_assert);
                assert_eq!(stored.amount, Amount::from(42));
                assert_eq!(stored.unit, CurrencyUnit::Sat);
                assert_eq!(stored.description, Some("payment request memo".to_string()));
                assert_eq!(stored.direction, PaymentRequestDirection::Incoming);
                assert_eq!(stored.state, PaymentRequestState::Pending);

                stored_tx.send(()).expect("test receiver still alive");
                Ok(())
            });

        let _wlt = wallet(ctx).await;

        let payload = ContactPaymentRequestPayload {
            id: req_id,
            sender,
            amount: Amount::from(42),
            unit: CurrencyUnit::Sat,
            memo: Some("payment request memo".to_string()),
            created_at: 123,
            mint: cashu::MintUrl::from_str("https://mint.example").unwrap(),
            deadline: None,
        };

        nostr_event_channel.publish(
            bcr_wallet_transport::NostrWalletEvent::ContactPaymentRequest {
                sender: node_id(NODE_ID_1).npub(),
                payload,
                event_id: EventId::all_zeros(),
            },
        );

        tokio::time::timeout(Duration::from_secs(1), stored_rx)
            .await
            .expect("contact payment request event should be processed")
            .expect("payment-request storage signal should be sent");
    }

    #[tokio::test]
    async fn test_online_exchange_returns_input_if_alpha_is_wallet_mint() {
        let mut ctx = wallet_ctx();

        ctx.client
            .expect_mint_url()
            .times(1)
            .return_const(url::Url::from_str("https://mint.example").unwrap());

        let wlt = wallet(ctx).await;

        let alpha_client = MockClowderMintConnector::new();
        let alpha_url = url::Url::from_str("https://mint.example").unwrap();

        let res = wlt
            .read()
            .await
            .online_exchange(vec![], alpha_url, &alpha_client, vec![], 123)
            .await
            .unwrap();

        assert!(res.is_empty());
    }

    #[tokio::test]
    async fn test_receive_token_errors_on_invalid_intermint_clowder_path() {
        let mut ctx = wallet_ctx();

        let token_mint = url::Url::from_str("https://other-mint.example").unwrap();

        ctx.client
            .expect_get_mint_keysets()
            .times(1)
            .returning(|| Ok(vec![]));

        ctx.client
            .expect_mint_url()
            .times(2)
            .return_const(url::Url::from_str("https://mint.example").unwrap());

        ctx.client
            .expect_post_clowder_path()
            .times(1)
            .withf({
                let token_mint = token_mint.clone();
                move |mint| *mint == token_mint
            })
            .returning(|mint| {
                Ok(wire_clowder::ConnectedMintsResponse {
                    mints: vec![wire_clowder::ConnectedMintResponse {
                        mint,
                        clowder: url::Url::from_str("https://clowder.example").unwrap(),
                        node_id: test_pub_key(),
                    }],
                })
            });

        let wlt = wallet(ctx).await;

        let token = Token::new_bitcr(
            to_mint_url(&token_mint),
            vec![],
            Some("intermint token".to_string()),
            CurrencyUnit::Sat,
        );

        let err = wlt
            .read()
            .await
            .receive_token(token, 123)
            .await
            .unwrap_err();

        match err {
            Error::InvalidClowderPath => {}
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_receive_token_same_mint_success() {
        let mut ctx = wallet_ctx();

        let tx_id = TransactionId::new(vec![]);
        let mint_url = url::Url::from_str("https://mint.example").unwrap();

        let (info, _keyset, proofs) = test_keyset_and_proofs(&[Amount::from(8), Amount::from(16)]);
        let k_infos = HashMap::from([(info.id, info.clone())]);

        let expected_ys: Vec<_> = proofs
            .iter()
            .map(|p| p.y().expect("valid proof y"))
            .collect();

        ctx.client
            .expect_get_mint_keysets()
            .times(1)
            .returning(move || Ok(k_infos.values().cloned().collect()));

        ctx.client
            .expect_mint_url()
            .times(4)
            .return_const(mint_url.clone());

        ctx.debit
            .expect_unit()
            .times(3)
            .returning(|| CurrencyUnit::Sat);

        ctx.debit.expect_receive_proofs().times(1).return_once(
            move |_client, keysets_info, received_proofs, _swap_config| {
                assert!(keysets_info.contains_key(&info.id));
                assert_eq!(received_proofs.len(), 2);
                assert_eq!(received_proofs.total_amount().unwrap(), Amount::from(24));
                Ok((Amount::from(24), expected_ys))
            },
        );

        ctx.tx_repo.expect_load_tx().times(1).returning(|_| {
            Err(bcr_wallet_persistence::error::Error::TransactionNotFound(
                TransactionId::new(vec![]),
            ))
        });

        ctx.tx_repo
            .expect_store_tx()
            .times(1)
            .return_once(move |tx| {
                assert_eq!(tx.direction, TransactionDirection::Incoming);
                assert_eq!(tx.amount, Amount::from(24));
                assert_eq!(tx.fee, Amount::ZERO);
                assert_eq!(tx.unit, CurrencyUnit::Sat);
                assert_eq!(tx.timestamp, 123);
                assert_eq!(tx.memo, Some("token memo".to_string()));
                assert_eq!(
                    tx.metadata.get(PAYMENT_TYPE_METADATA_KEY),
                    Some(&PaymentType::Token.to_string())
                );
                assert_eq!(
                    tx.metadata.get(TRANSACTION_STATUS_METADATA_KEY),
                    Some(&TransactionStatus::Settled.to_string())
                );
                Ok(tx_id)
            });

        let token = Token::new_bitcr(
            to_mint_url(&mint_url),
            proofs,
            Some("token memo".to_string()),
            CurrencyUnit::Sat,
        );

        let wlt = wallet(ctx).await;

        let res = wlt.read().await.receive_token(token, 123).await.unwrap();

        assert_eq!(res, tx_id);
    }

    #[tokio::test]
    async fn test_online_exchange_success_with_valid_proofs() {
        let mut ctx = wallet_ctx();

        let wallet_mint = url::Url::from_str("https://wallet-mint.example").unwrap();
        let alpha_mint = url::Url::from_str("https://alpha-mint.example").unwrap();
        let alpha_beta_url = url::Url::from_str("https://alpha-beta.example").unwrap();

        let (info, alpha_keyset) = core_tests::generate_random_ecash_keyset();
        let kid = info.id;
        let k_infos = test_kinfos(info);
        let alpha_proofs =
            core_tests::generate_random_ecash_proofs(&alpha_keyset, &[Amount::from(8)]);

        let mut alpha_client = MockClowderMintConnector::new();
        let mut alpha_beta = MockClowderMintConnector::new();
        setup_attestation_mock(&mut alpha_beta);

        ctx.client
            .expect_mint_url()
            .times(1)
            .return_const(wallet_mint.clone());

        ctx.client
            .expect_post_online_exchange()
            .times(1)
            .withf(|locked_proofs, exchange_path| {
                locked_proofs.len() == 1 && exchange_path.len() == 3
            })
            .returning(|locked_proofs, _exchange_path| Ok(locked_proofs));

        let url_clone = alpha_beta_url.clone();
        alpha_client
            .expect_get_clowder_betas()
            .times(1)
            .returning(move || Ok(vec![url_clone.clone()]));

        alpha_client
            .expect_get_mint_keysets()
            .times(1)
            .returning(move || Ok(k_infos.values().cloned().collect()));
        alpha_client
            .expect_post_swap_commitment()
            .times(1)
            .returning(|_, _, _, _, _| Ok(mock_commitment_result()));

        let alpha_keyset_for_lookup = alpha_keyset.clone();
        alpha_client
            .expect_get_mint_keyset()
            .times(1)
            .with(mockall::predicate::eq(kid))
            .returning(move |_| {
                Ok(bcr_wallet_core::util::to_keyset(
                    &alpha_keyset_for_lookup,
                    None,
                ))
            });

        let alpha_keyset_for_swap = alpha_keyset.clone();
        alpha_client
            .expect_post_swap_committed()
            .times(1)
            .returning(move |_inputs, outputs, _commitment| {
                let amounts = outputs.iter().map(|b| b.amount).collect::<Vec<_>>();
                Ok(core_tests::generate_ecash_signatures(
                    &alpha_keyset_for_swap,
                    &amounts,
                ))
            });

        let mut wlt = wallet(ctx).await;

        let arc_alpha_beta = Arc::new(alpha_beta);
        wlt = wallet_with_betas(wlt, vec![(alpha_beta_url, arc_alpha_beta.clone())]).await;
        wlt.write().await.client_factory = Box::new(move |url| {
            assert_eq!(
                url,
                url::Url::from_str("https://alpha-beta.example").unwrap()
            );
            arc_alpha_beta.clone()
        });

        let path = vec![
            wire_clowder::ConnectedMintResponse {
                mint: alpha_mint.clone(),
                clowder: url::Url::from_str("https://clowder-alpha.example").unwrap(),
                node_id: test_pub_key(),
            },
            wire_clowder::ConnectedMintResponse {
                mint: wallet_mint.clone(),
                clowder: url::Url::from_str("https://clowder-wallet.example").unwrap(),
                node_id: test_pub_key(),
            },
        ];
        let now = chrono::Utc::now().timestamp() as u64;
        let res = wlt
            .read()
            .await
            .online_exchange(alpha_proofs, alpha_mint, &alpha_client, path, now)
            .await
            .unwrap();

        assert_eq!(res.len(), 1);
        assert_eq!(res[0].amount, Amount::from(8));
        assert!(res[0].witness.is_some());
    }

    fn add_test_dleqs(proofs: &mut [cashu::Proof]) {
        for proof in proofs {
            proof.dleq = Some(cashu::nut12::ProofDleq {
                e: cashu::SecretKey::generate(),
                s: cashu::SecretKey::generate(),
                r: cashu::SecretKey::generate(),
            });
        }
    }
    #[tokio::test]
    async fn test_offline_exchange_success_with_valid_proofs() {
        let ctx = wallet_ctx();
        let wlt = wallet(ctx).await;

        let (_alpha_info, alpha_keyset) = core_tests::generate_random_ecash_keyset();
        let mut alpha_proofs =
            core_tests::generate_random_ecash_proofs(&alpha_keyset, &[Amount::from(8)]);
        add_test_dleqs(&mut alpha_proofs);

        let (_beta_info, beta_keyset) = core_tests::generate_random_ecash_keyset();
        let mut beta_proofs =
            core_tests::generate_random_ecash_proofs(&beta_keyset, &[Amount::from(8)]);
        add_test_dleqs(&mut beta_proofs);

        let mut substitute = MockClowderMintConnector::new();

        substitute
            .expect_post_offline_exchange()
            .times(1)
            .withf(
                |fingerprints, hash_locks, _wallet_pk, substitute_clowder_id| {
                    fingerprints.len() == 1
                        && hash_locks.len() == 1
                        && *substitute_clowder_id == test_pub_key()
                },
            )
            .return_once(
                move |_fingerprints, _hash_locks, _wallet_pk, _substitute_clowder_id| {
                    Ok(beta_proofs)
                },
            );

        let res = wlt
            .read()
            .await
            .offline_exchange(&substitute, alpha_proofs, test_pub_key())
            .await
            .unwrap();

        assert_eq!(res.len(), 1);
        assert_eq!(res[0].amount, Amount::from(8));

        assert!(res[0].witness.is_some());
    }
}
