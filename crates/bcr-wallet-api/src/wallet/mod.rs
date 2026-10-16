pub mod api;
#[cfg(test)]
pub mod test_utils;
pub mod types;
pub mod util;

use crate::{
    ClowderMintConnector,
    error::{Error, Result},
    external::mint::ClientFactory,
    pocket::{RestoreSummary, debit::DebitPocketApi},
    types::PaymentSummary,
    wallet::{
        api::WalletApi,
        types::{
            PayReference, SwapConfig, WalletBalance, WalletDetailedBalanceEntry, WalletPaymentType,
        },
        util::tx_can_be_refreshed,
    },
};
use bcr_common::{
    cashu::{self, Amount, CurrencyUnit, Proof, ProofsMethods},
    cdk_common::wallet::TransactionDirection,
    core::{NodeId, htlc, swap::wallet::prepare_swap},
    ecash::{self, KeySetInfo},
    wallet::{BitcrTokenV5, Token},
    wire::clowder::{ConnectedMintResponse, ConnectedMintsResponse},
};
use bcr_wallet_core::{
    event::{
        ContactPaymentPayload, ContactPaymentRequestPayload, EventEnvelope,
        PaymentRequestActionKind, PaymentRequestActionPayload,
    },
    types::{
        ClowderBeta, ForeignMintProof, ForeignMintProofReason, ListTransactionsResult,
        PaymentRequest, PaymentRequestActionOrigin, PaymentRequestDirection, PaymentRequestState,
        PaymentRequestTransition, PaymentRequestTransitionOutcome, PaymentType, Transaction,
        TransactionCursor, TransactionFees, TransactionFilters, TransactionLinkReason,
        TransactionSort, TransactionStatus, extract_fees_per_month,
    },
    util::{from_mint_url, to_mint_url},
};
use bcr_wallet_persistence::{
    ContactStoreApi, MigrationJournalEntry, MigrationJournalHeader, MigrationJournalRepository,
    MigrationJournalState, NostrRepository, PaymentRequestStoreApi, TransactionRepository,
};
use bcr_wallet_transport::{ConsumerApi, NostrEventChannel, NostrWalletEvent, TransportApi};
use bitcoin::{
    base58,
    hashes::{Hash, sha256::Hash as Sha256},
    secp256k1,
};
use nostr::{
    event::EventId,
    nips::nip19::{Nip19Profile, ToBech32},
    types::RelayUrl,
};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::{Arc, Weak},
    time::Duration,
};
use tokio::sync::{Mutex, Notify, RwLock, broadcast};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub type NostrReceiver = broadcast::Receiver<NostrWalletEvent>;

/// The task handling the nostr events of one wallet instance
pub struct NostrListener {
    stop: CancellationToken,
    task: tokio::task::JoinHandle<NostrReceiver>,
    channel: NostrEventChannel,
}

impl NostrListener {
    /// Stops the listener between two events and returns its subscription, which keeps every
    /// event it has not taken yet
    pub async fn stop(self) -> NostrReceiver {
        self.stop.cancel();
        match self.task.await {
            Ok(nostr_receiver) => nostr_receiver,
            Err(e) => {
                tracing::error!("Nostr event listener failed: {e}");
                self.channel.subscribe()
            }
        }
    }
}

pub struct Wallet {
    this: Weak<RwLock<Wallet>>,
    network: bitcoin::Network,
    client: Arc<dyn ClowderMintConnector>,
    mint_keyset_infos: HashMap<ecash::Id, KeySetInfo>,
    beta_clients: BTreeMap<url::Url, Arc<dyn ClowderMintConnector>>,
    tx_repo: Arc<dyn TransactionRepository>,
    contact_repo: Arc<dyn ContactStoreApi>,
    payment_request_repo: Arc<dyn PaymentRequestStoreApi>,
    debit: Box<dyn DebitPocketApi>,
    journal: Arc<dyn MigrationJournalRepository>,
    name: String,
    id: String,
    pub_key: secp256k1::PublicKey,
    current_payment: Mutex<Option<PayReference>>,
    payment_request_paid: Arc<Notify>,
    clowder_id: secp256k1::PublicKey,
    client_factory: Arc<ClientFactory>,
    swap_expiry: time::Duration,
    nostr_transport: Arc<dyn TransportApi>,
    nostr_event_channel: NostrEventChannel,
    nostr_repo: Arc<dyn NostrRepository>,
    nostr_consumer_running: Arc<Mutex<bool>>,
    nostr_shutdown: CancellationToken,
    nostr_consumer: Arc<dyn ConsumerApi>,
    nostr_listener: std::sync::Mutex<Option<NostrListener>>,
}

pub(crate) fn is_authentic_sender(
    claimed: &NodeId,
    sender: &nostr::key::PublicKey,
    network: bitcoin::Network,
) -> bool {
    claimed.network() == network && claimed.equals_npub(sender)
}

impl Wallet {
    pub async fn new(
        network: bitcoin::Network,
        client: Arc<dyn ClowderMintConnector>,
        mint_keyset_infos: HashMap<ecash::Id, KeySetInfo>,
        tx_repo: Arc<dyn TransactionRepository>,
        contact_repo: Arc<dyn ContactStoreApi>,
        payment_request_repo: Arc<dyn PaymentRequestStoreApi>,
        debit: Box<dyn DebitPocketApi>,
        journal: Arc<dyn MigrationJournalRepository>,
        name: String,
        id: String,
        pub_key: secp256k1::PublicKey,
        clowder_id: secp256k1::PublicKey,
        beta_clients: BTreeMap<url::Url, Arc<dyn ClowderMintConnector>>,
        client_factory: Arc<ClientFactory>,
        swap_expiry: time::Duration,
        nostr_transport: Arc<dyn TransportApi>,
        nostr_event_channel: NostrEventChannel,
        nostr_repo: Arc<dyn NostrRepository>,
        nostr_consumer: Arc<dyn ConsumerApi>,
    ) -> Arc<RwLock<Self>> {
        let cancel = CancellationToken::new();
        Arc::new_cyclic(|this| {
            RwLock::new(Self {
                this: this.clone(),
                network,
                client,
                mint_keyset_infos,
                tx_repo,
                contact_repo,
                payment_request_repo,
                debit,
                journal,
                name,
                id,
                pub_key,
                current_payment: Mutex::new(None),
                payment_request_paid: Arc::new(Notify::new()),
                beta_clients,
                clowder_id,
                client_factory,
                swap_expiry,
                nostr_transport,
                nostr_event_channel,
                nostr_repo,
                nostr_consumer_running: Arc::new(Mutex::new(false)),
                nostr_shutdown: cancel.clone(),
                nostr_consumer,
                nostr_listener: std::sync::Mutex::new(None),
            })
        })
    }

    pub fn nostr_consumer(&self) -> Arc<dyn ConsumerApi> {
        self.nostr_consumer.clone()
    }

    async fn nostr_connect(&self, nostr_consumer: Arc<dyn ConsumerApi>) {
        let wallet_id = self.id.clone();
        let nostr_consumer_running = self.nostr_consumer_running.clone();
        let cancel = self.nostr_shutdown.clone();
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

    fn listen_nostr(&self, mut nostr_receiver: NostrReceiver) {
        let wallet = self
            .this
            .upgrade()
            .expect("a wallet is only reached through the Arc it was created in");
        let wallet_network = self.network;
        let stop = CancellationToken::new();
        let stopped = stop.clone();
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = stopped.cancelled() => return nostr_receiver,
                    evt = nostr_receiver.recv() => {
                        let received_evt = match evt {
                            Ok(e) => e,
                            Err(broadcast::error::RecvError::Lagged(_)) => {
                                tracing::warn!("listen_nostr channel lagged behind");
                                continue;
                            },
                            Err(broadcast::error::RecvError::Closed) => {
                                tracing::warn!("listen_nostr channel closed");
                                return nostr_receiver;
                            },
                        };
                        match received_evt {
                            bcr_wallet_transport::NostrWalletEvent::ContactPaymentRequest { sender, payload, event_id } => {
                                if !is_authentic_sender(&payload.sender, &sender, wallet_network) {
                                    tracing::warn!("Rejected incoming Contact payment request: claimed sender {} is not on this network or not the authenticated Nostr sender {sender}", payload.sender);
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
                            bcr_wallet_transport::NostrWalletEvent::PaymentRequestAction { payload, event_id, sender } => {
                                let payment_request_id = payload.payment_request_id;
                                let wallet_guard = wallet.read().await;
                                if let Err(e) = wallet_guard.apply_remote_payment_request_action(payload, event_id, sender).await {
                                    tracing::error!("Could not apply payment request action for {payment_request_id}: {e}");
                                }
                            },
                            bcr_wallet_transport::NostrWalletEvent::Cdk18Payment { payload, event_id, .. } => {
                                let amount = payload.proofs.total_amount().unwrap_or(Amount::ZERO);
                                let wallet_guard = wallet.read().await;
                                match <Self as api::WalletApi>::receive_proofs(
                                    &*wallet_guard,
                                    payload.proofs,
                                    payload.unit,
                                    from_mint_url(&payload.mint),
                                    time::OffsetDateTime::now_utc().unix_timestamp() as u64,
                                    payload.memo,
                                    PaymentType::PaymentRequest,
                                    TransactionStatus::Settled,
                                    None,
                                    None,
                                    Some(event_id),
                                ).await {
                                    Ok(tx_id) => {
                                        tracing::info!("Received legacy Cdk18 payment for {amount} with Transaction ID: {tx_id}")
                                    },
                                    Err(e) => {
                                        tracing::error!("Error processing legacy Cdk18 payment for {amount}: {e}")
                                    }
                                };
                            },
                            bcr_wallet_transport::NostrWalletEvent::ContactPayment { payload, event_id, sender } => {
                                if !is_authentic_sender(&payload.sender, &sender, wallet_network) {
                                    tracing::warn!("Rejected incoming Contact payment: claimed sender {} is not on this network or not the authenticated Nostr sender {sender}", payload.sender);
                                    continue;
                                }

                                let amount = payload.proofs.total_amount().unwrap_or(Amount::ZERO);
                                let unit = payload.unit.clone();
                                let wallet_guard = wallet.read().await;
                                match <Self as api::WalletApi>::receive_proofs(
                                    &*wallet_guard,
                                    payload.proofs,
                                    payload.unit,
                                    from_mint_url(&payload.mint),
                                    time::OffsetDateTime::now_utc().unix_timestamp() as u64,
                                    payload.memo,
                                    if payload.payment_request_id.is_some() {
                                        PaymentType::PaymentRequest
                                    } else {
                                        PaymentType::Contact
                                    },
                                    TransactionStatus::Settled,
                                    payload.payment_request_id,
                                    Some(payload.sender.clone()),
                                    Some(event_id)
                                ).await {
                                    Ok(tx_id) => {
                                        if let Some(p_req_id) = payload.payment_request_id {
                                            wallet_guard.settle_outgoing_payment_request(p_req_id, tx_id, amount, &unit, &payload.sender, event_id).await;
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
        *self.nostr_listener.lock().unwrap() = Some(NostrListener {
            stop,
            task,
            channel: self.nostr_event_channel.clone(),
        });
    }

    async fn settle_outgoing_payment_request(
        &self,
        p_req_id: Uuid,
        tx_id: Uuid,
        amount: Amount,
        unit: &CurrencyUnit,
        sender: &NodeId,
        event_id: EventId,
    ) {
        let req = match self
            .payment_request_repo
            .get_payment_request(p_req_id)
            .await
        {
            Ok(Some(req)) => req,
            Ok(None) => {
                tracing::debug!("Payment for unknown payment request {p_req_id}, tx {tx_id}");
                return;
            }
            Err(e) => {
                tracing::error!("Could not read payment request {p_req_id}: {e}");
                return;
            }
        };
        if req.direction != PaymentRequestDirection::Outgoing {
            tracing::warn!("Payment {tx_id} names payment request {p_req_id} we did not issue");
            return;
        }
        if let PaymentRequestState::Paid { tx_id: paid_by } = req.state {
            tracing::info!("Payment request {p_req_id} already paid by {paid_by}, tx {tx_id}");
            return;
        }
        if let Some(expected) = &req.node_id
            && expected != sender
        {
            tracing::warn!(
                "Payment {tx_id} for request {p_req_id} is from {sender}, not {expected}"
            );
            return;
        }
        if &req.unit != unit || amount < req.amount {
            tracing::warn!(
                "Payment {tx_id} of {amount} {unit} does not cover request {p_req_id} for {} {}",
                req.amount,
                req.unit
            );
            return;
        }
        if let Err(e) = <Self as api::WalletApi>::mark_payment_request_as_paid(
            self,
            p_req_id,
            tx_id,
            sender.clone(),
            PaymentRequestActionOrigin::Remote {
                event_id: event_id.to_string(),
            },
        )
        .await
        {
            tracing::error!("Could not set Payment Request {p_req_id} to paid: {e}");
        }
    }

    pub fn name(&self) -> String {
        self.name.clone()
    }

    pub fn network(&self) -> bitcoin::Network {
        self.network
    }

    async fn swap_exchanged(
        &self,
        debit: &dyn DebitPocketApi,
        substitute: Arc<dyn ClowderMintConnector>,
        keysets_info: &HashMap<ecash::Id, KeySetInfo>,
        proofs: Vec<Proof>,
        swap_config: SwapConfig,
    ) -> Result<Amount> {
        let ys = proofs
            .iter()
            .map(|p| p.y())
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let infos: HashMap<cashu::Id, KeySetInfo> = keysets_info
            .iter()
            .map(|(id, info)| ((*id).into(), info.clone()))
            .collect();
        let planned = prepare_swap(&proofs, &infos)?
            .into_values()
            .fold(Amount::ZERO, |acc, a| acc + a);
        let e = match debit
            .receive_proofs(substitute.clone(), keysets_info, proofs, swap_config)
            .await
        {
            Ok((amount, _)) => return Ok(amount),
            Err(e) => e,
        };
        tracing::error!("Could not swap exchanged proofs at substitute: {e}");
        let expected = ys.len();
        let landed = async {
            let states = substitute
                .post_check_state(cashu::CheckStateRequest { ys })
                .await?;
            if states.len() != expected || states.iter().any(|s| s.state != cashu::State::Spent) {
                return Ok(false);
            }
            debit.restore_local_proofs(keysets_info, substitute).await?;
            Ok::<_, Error>(true)
        }
        .await;
        match landed {
            Ok(true) => Ok(planned),
            Ok(false) => Err(e),
            Err(restore_err) => {
                tracing::error!("Could not recover a lost swap: {restore_err}");
                Err(e)
            }
        }
    }

    async fn exchange_journaled(
        &self,
        substitute: &dyn ClowderMintConnector,
        header: &MigrationJournalHeader,
        y: cashu::PublicKey,
        entry: MigrationJournalEntry,
    ) -> Result<Vec<Proof>> {
        let secrets = [entry.proof.secret.clone()];
        let beta_proofs = util::post_offline_exchange(
            substitute,
            header.substitute_clowder_id,
            &header.alpha_id,
            &header.evidence_digest,
            vec![entry.proof],
            &entry.exchange_key,
        )
        .await?;
        let unlocked = util::unlock_offline_exchanged(
            beta_proofs.clone(),
            &secrets,
            &entry.exchange_key,
            &header.substitute_clowder_id,
            0,
        )?;
        self.journal
            .update(vec![y], MigrationJournalState::Exchanged(beta_proofs))
            .await?;
        Ok(unlocked)
    }

    async fn journaled_tx(
        &self,
        tx: &Transaction,
    ) -> Result<Option<Vec<(cashu::PublicKey, MigrationJournalEntry)>>> {
        let mut entries = self.journal.load().await?;
        let mut journaled: Vec<_> = tx
            .ys
            .iter()
            .filter_map(|y| entries.remove(y).map(|e| (*y, e)))
            .collect();
        if journaled.is_empty() {
            return Ok(None);
        }
        if journaled.len() != tx.ys.len() {
            return Err(Error::PartlyJournaledTransaction(tx.id));
        }
        journaled.sort_by_key(|(y, _)| *y);
        Ok(Some(journaled))
    }

    async fn reclaim_journaled(
        &self,
        entries: Vec<(cashu::PublicKey, MigrationJournalEntry)>,
        keysets_info: &HashMap<ecash::Id, KeySetInfo>,
    ) -> Result<Amount> {
        if let Some(amount) = reclaimed_amount(&entries) {
            return Ok(amount);
        }
        let mut unlocked = Vec::new();
        let mut ys = Vec::new();
        for (y, entry) in entries {
            match entry.state {
                MigrationJournalState::Swapped => continue,
                MigrationJournalState::Exchanged(beta_proofs) => {
                    unlocked.extend(util::unlock_offline_exchanged(
                        beta_proofs,
                        &[entry.proof.secret],
                        &entry.exchange_key,
                        &entry.header.substitute_clowder_id,
                        0,
                    )?);
                }
                MigrationJournalState::Pending
                | MigrationJournalState::Held
                | MigrationJournalState::Reclaimed(_) => {
                    let header = entry.header.clone();
                    unlocked.extend(
                        self.exchange_journaled(self.client.as_ref(), &header, y, entry)
                            .await?,
                    )
                }
            }
            ys.push(y);
        }
        let amount = if unlocked.is_empty() {
            Amount::ZERO
        } else {
            self.swap_exchanged(
                self.debit.as_ref(),
                self.client.clone(),
                keysets_info,
                unlocked,
                self.swap_config(),
            )
            .await?
        };
        self.journal
            .update(ys, MigrationJournalState::Reclaimed(amount))
            .await?;
        Ok(amount)
    }

    async fn finish_reclaim(
        &self,
        entries: &[(cashu::PublicKey, MigrationJournalEntry)],
    ) -> Result<()> {
        let ys = entries.iter().map(|(y, _)| *y).collect();
        self.journal
            .update(ys, MigrationJournalState::Swapped)
            .await?;
        self.journal.prune_swapped().await?;
        Ok(())
    }

    fn swap_config(&self) -> SwapConfig {
        SwapConfig {
            expiry: self.swap_expiry,
            alpha_pk: self.clowder_id,
        }
    }

    pub async fn list_tx_ids(&self) -> Result<Vec<Uuid>> {
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
                res.sort_by(|a, b| a.tstamp.cmp(&b.tstamp).then_with(|| a.id.cmp(&b.id)));
            }

            TransactionSort::TimeDesc => {
                res.sort_by(|a, b| b.tstamp.cmp(&a.tstamp).then_with(|| b.id.cmp(&a.id)));
            }

            TransactionSort::AmountAsc => {
                res.sort_by(|a, b| a.amount.cmp(&b.amount).then_with(|| a.id.cmp(&b.id)));
            }

            TransactionSort::AmountDesc => {
                res.sort_by(|a, b| b.amount.cmp(&a.amount).then_with(|| b.id.cmp(&a.id)));
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
        Option<(ConnectedMintsResponse, HashMap<ecash::Id, KeySetInfo>)>,
        HashMap<ecash::Id, KeySetInfo>,
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
                    .ok_or(Error::BetaNotFound(beta_mint.to_string()))?
            };

            // In the offline case we can only ask the substitute, in the online case we can ask the mint
            // The Beta mint (after Alpha in the path) should have it in any case
            // This can be revised based on some criteria ?
            let alpha_keysets = substitute_client.get_alpha_keysets(alpha_id).await?;

            // The endpoint only returns active keysets
            let intermint_alpha_infos: HashMap<ecash::Id, KeySetInfo> = alpha_keysets
                .iter()
                .map(|keyset| {
                    (
                        keyset.id.into(),
                        ecash::KeySetInfo {
                            id: keyset.id.into(),
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

    async fn get_wallet_mint_keyset_infos(&self) -> Result<HashMap<ecash::Id, KeySetInfo>> {
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

    pub async fn restore_local_proofs(&self) -> Result<RestoreSummary> {
        let keysets_info = self.get_wallet_mint_keyset_infos().await?;
        self.debit
            .restore_local_proofs(&keysets_info, self.client.clone())
            .await
    }

    pub async fn load_tx(&self, tx_id: Uuid) -> Result<Transaction> {
        let tx = self.tx_repo.load_tx(tx_id).await?;
        Ok(tx)
    }

    pub async fn edit_tx_memo(&self, tx_id: Uuid, new_memo: Option<String>) -> Result<()> {
        let _ = self.tx_repo.update_memo(tx_id, new_memo).await?;
        Ok(())
    }

    // Fetches the transaction with the given ID from the database and, if it's in a pending state
    // it attempts to get the current state from the mint and, if it's spent, changes it to spent
    // Returns whether the transaction has been updated
    pub async fn refresh_tx(&self, tx_id: Uuid) -> Result<bool> {
        let mut updated = false;
        let tx = self.tx_repo.load_tx(tx_id).await?;
        if !util::tx_can_be_refreshed(&tx) || self.journaled_tx(&tx).await?.is_some() {
            return Ok(updated);
        }
        let request = cashu::CheckStateRequest { ys: tx.ys.clone() };
        let response = self.client.post_check_state(request).await?;
        let is_any_spent = response
            .iter()
            .any(|s| matches!(s.state, cashu::State::Spent));
        if is_any_spent {
            self.tx_repo
                .update_status(tx_id, TransactionStatus::Settled)
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

            let tx_id = tx.id;

            match self.refresh_tx(tx.id).await {
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

    pub async fn reclaim_foreign_mint_proofs(&self) -> Result<Amount> {
        // if our mint is offline, we can't reclaim
        if self.is_wallet_mint_offline().await? {
            tracing::warn!(
                "Attempting to reclaim foreign mint proofs, but wallet mint is offline - trying again on the next run."
            );
            return Ok(Amount::ZERO);
        }
        // we treat all foreign mint proofs the same - it's possible we have foreign mint proofs from our own mint in case we migrated
        let mut all_mints = self.client.get_clowder_betas().await?;
        all_mints.push(ClowderBeta {
            clowder_id: self.clowder_id,
            url: self.client.mint_url().to_owned(),
        });
        // get foreign mint proofs and collect by clowder id
        let foreign_mint_proofs = self.debit.fetch_foreign_mint_proofs().await?;
        // an exchange refund's alpha may be several hops away, so it is not necessarily one of
        // our own clowder's betas: add it from the proof's own reason instead
        let mut known_clowder_ids: HashSet<secp256k1::PublicKey> =
            all_mints.iter().map(|m| m.clowder_id).collect();
        for fmp in foreign_mint_proofs.iter() {
            if let ForeignMintProofReason::ExchangeRefund { alpha_url } = &fmp.reason
                && known_clowder_ids.insert(fmp.clowder_id)
            {
                all_mints.push(ClowderBeta {
                    clowder_id: fmp.clowder_id,
                    url: alpha_url.clone(),
                });
            }
        }
        let fmps_by_clowder_id: HashMap<secp256k1::PublicKey, Vec<ForeignMintProof>> =
            foreign_mint_proofs
                .into_iter()
                .fold(HashMap::new(), |mut map, item| {
                    map.entry(item.clowder_id).or_default().push(item);
                    map
                });

        let mut reclaimed = Amount::ZERO;

        // create tokens and attempt to reclaim
        // if it's from our own mint, create a token and swap it to be safe
        // if it's from another mint, create a token and do an intermint swap
        for mint in all_mints.iter() {
            if let Some(fmps) = fmps_by_clowder_id.get(&mint.clowder_id)
                && !fmps.is_empty()
            {
                let mint_client = if &mint.url == self.client.mint_url() {
                    self.client.clone()
                } else {
                    (self.client_factory)(mint.url.clone())
                };
                let ys: Vec<cashu::PublicKey> = fmps
                    .iter()
                    .map(|fmp| fmp.proof.y().expect("proof has valid y"))
                    .collect();
                let states = match mint_client
                    .post_check_state(cashu::CheckStateRequest { ys: ys.clone() })
                    .await
                {
                    Ok(states) => states,
                    Err(e) => {
                        tracing::error!(
                            "Could not check state of foreign mint proofs from mint {}: {e}",
                            mint.url
                        );
                        continue;
                    }
                };
                let states: HashMap<cashu::PublicKey, cashu::State> =
                    states.into_iter().map(|s| (s.y, s.state)).collect();
                let spent_ys: Vec<cashu::PublicKey> = ys
                    .iter()
                    .filter(|y| states.get(y) == Some(&cashu::State::Spent))
                    .copied()
                    .collect();
                if !spent_ys.is_empty() {
                    tracing::info!(
                        "Dropping {} spent foreign mint proofs from mint {}",
                        spent_ys.len(),
                        mint.url
                    );
                    self.debit
                        .delete_foreign_mint_proofs(mint.clowder_id, spent_ys)
                        .await;
                }

                let (unspent_ys, proofs): (Vec<cashu::PublicKey>, Vec<Proof>) = ys
                    .into_iter()
                    .zip(fmps)
                    .filter(|(y, _)| states.get(y) == Some(&cashu::State::Unspent))
                    .map(|(y, fmp)| (y, fmp.proof.clone()))
                    .unzip();
                if proofs.is_empty() {
                    continue;
                }
                let amount = proofs.total_amount()?;
                match self.reclaim_foreign_proofs(mint, proofs.clone()).await {
                    Ok(tx_id) => {
                        tracing::info!(
                            "Reclaimed {amount} from foreign mint proofs from {}, tx_id: {tx_id}",
                            mint.url
                        );
                        reclaimed += amount;
                        // if everything went well, delete the foreign mint proofs
                        self.debit
                            .delete_foreign_mint_proofs(mint.clowder_id, unspent_ys)
                            .await;
                    }
                    Err(e) => {
                        tracing::error!(
                            "Could not reclaim foreign mint proofs from mint {}: {e}",
                            mint.url
                        );
                        reclaimed += self
                            .reclaim_foreign_proofs_one_by_one(mint, unspent_ys, proofs)
                            .await;
                    }
                }
            }
        }
        Ok(reclaimed)
    }

    async fn reclaim_foreign_proofs(&self, mint: &ClowderBeta, proofs: Vec<Proof>) -> Result<Uuid> {
        let token = Token::BitcrV5(
            BitcrTokenV5::new(
                NodeId::new(mint.clowder_id, self.network()),
                self.debit_unit(),
                proofs.into_iter().map(|p| p.into()).collect(),
            )
            .with_mint_url(to_mint_url(&mint.url).to_string())
            .with_memo(format!("Reclaimed Foreign Mint Funds from {}", mint.url)),
        );
        self.receive_token(
            token,
            time::OffsetDateTime::now_utc().unix_timestamp() as u64,
        )
        .await
    }

    async fn reclaim_foreign_proofs_one_by_one(
        &self,
        mint: &ClowderBeta,
        ys: Vec<cashu::PublicKey>,
        proofs: Vec<Proof>,
    ) -> Amount {
        if proofs.len() < 2 {
            return Amount::ZERO;
        }
        let mut reclaimed = Amount::ZERO;
        let mut rejected = Vec::new();
        for (y, proof) in ys.into_iter().zip(proofs) {
            let amount = proof.amount;
            match self.reclaim_foreign_proofs(mint, vec![proof]).await {
                Ok(_) => {
                    reclaimed += amount;
                    self.debit
                        .delete_foreign_mint_proofs(mint.clowder_id, vec![y])
                        .await;
                }
                Err(e) if e.is_mint_rejection() => rejected.push(y),
                Err(e) => tracing::error!(
                    "Could not reclaim foreign mint proof {y} from mint {}: {e}",
                    mint.url
                ),
            }
        }
        if reclaimed > Amount::ZERO && !rejected.is_empty() {
            tracing::warn!(
                "Dropping {} foreign mint proofs mint {} rejected",
                rejected.len(),
                mint.url
            );
            self.debit
                .delete_foreign_mint_proofs(mint.clowder_id, rejected)
                .await;
        }
        reclaimed
    }

    /// Check that the transaction can be reclaimed, then
    /// * Create a new transaction with state `Canceled`
    /// * Set the initial transaction to `Settled`
    /// * Link the two transactions with reason `Reclaim`
    pub async fn reclaim_tx(&self, tx_id: Uuid) -> Result<Amount> {
        let infos = self.get_wallet_mint_keyset_infos().await?;
        self.refresh_tx(tx_id).await?;
        let tx = self.load_tx(tx_id).await?;
        let journaled = self.journaled_tx(&tx).await?;
        if tx.status == TransactionStatus::Settled
            && let Some(entries) = &journaled
            && let Some(amount) = reclaimed_amount(entries)
        {
            self.finish_reclaim(entries).await?;
            return Ok(amount);
        }

        // Only Outgoing and Pending transactions can be reclaimed
        if !util::tx_can_be_refreshed(&tx) {
            return Err(Error::TransactionCantBeReclaimed(tx_id));
        }
        if tx.unit != self.debit.unit() {
            return Err(Error::InvalidCurrencyUnit(tx.unit.to_string()));
        }

        // Reclaim proofs
        tracing::debug!("Reclaim Debit Transaction {tx_id}");
        let amount = match journaled.clone() {
            Some(entries) => {
                let migrated = entries
                    .iter()
                    .all(|(_, e)| &e.header.substitute_url == self.client.mint_url());
                if !migrated || tx.payment_type == PaymentType::OnChain {
                    return Err(Error::TransactionCantBeReclaimed(tx_id));
                }
                self.reclaim_journaled(entries, &infos).await?
            }
            None => {
                self.debit
                    .reclaim_proofs(&tx.ys, &infos, self.client.clone(), self.swap_config())
                    .await?
            }
        };

        // If amount is zero - this means the transaction was already claimed - we set the transaction to Settled
        if amount == Amount::ZERO {
            self.tx_repo
                .update_status(tx_id, TransactionStatus::Settled)
                .await?;
        } else {
            let fee = if tx.amount > amount {
                tx.amount - amount
            } else {
                Amount::ZERO
            };

            // Create new, cancelled transaction
            let reclaim_tx = Transaction {
                id: Uuid::new_v4(),
                mint_url: tx.mint_url.clone(),
                direction: TransactionDirection::Incoming, // incoming, since we're getting back the funds
                fees: TransactionFees {
                    swap: fee,
                    ..Default::default()
                },
                amount: tx.amount,
                memo: tx.memo.clone(),
                tstamp: time::OffsetDateTime::now_utc().unix_timestamp() as u64,
                unit: tx.unit,
                payment_type: tx.payment_type,
                status: TransactionStatus::Canceled, // canceled, since it's the reclaim tx
                ys: tx.ys.clone(),
                quote_id: tx.quote_id,
                contact_node_id: tx.contact_node_id.clone(),
                payment_request_id: tx.payment_request_id,
                nostr_event_id: tx.nostr_event_id,
                btc_tx_id: tx.btc_tx_id,
                linked_txs: vec![],
            };
            let reclaim_txid = self.tx_repo.store_tx(reclaim_tx).await?;

            // Set the initial transaction to Settled
            self.tx_repo
                .update_status(tx_id, TransactionStatus::Settled)
                .await?;

            // Link the two transactions with reason Reclaim
            self.tx_repo
                .link_txs(tx_id, reclaim_txid, TransactionLinkReason::Reclaim)
                .await?;
        }
        if let Some(entries) = &journaled {
            self.finish_reclaim(entries).await?;
        }

        Ok(amount)
    }

    async fn _receive_proofs(
        &self,
        local_alpha_keysets_info: &HashMap<ecash::Id, KeySetInfo>,
        proofs: Vec<cashu::Proof>,
        unit: CurrencyUnit,
        mint: url::Url,
        intermint_infos: Option<(ConnectedMintsResponse, HashMap<ecash::Id, KeySetInfo>)>,
        tstamp: u64,
        memo: Option<String>,
        payment_type: PaymentType,
        status: TransactionStatus,
        payment_request_id: Option<Uuid>,
        contact_node_id: Option<NodeId>,
        nostr_event_id: Option<EventId>,
    ) -> Result<Uuid> {
        if unit != self.debit.unit() {
            return Err(Error::InvalidCurrencyUnit(unit.to_string()));
        }
        let initial_amount = proofs.total_amount()?;
        let mut proofs = proofs;

        let mut is_intermint = false;
        let mut exchange_hash_lock = None;
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
                        .ok_or(Error::BetaNotFound(
                            substitute_beta_mint.clone().to_string(),
                        ))?
                };
                tracing::debug!("Using substitute {}", substitute_beta_mint.to_string());

                // check if alpha is offline
                let alpha_offline = substitute_client.get_alpha_offline(alpha_id).await?;
                if !alpha_offline.offline {
                    tracing::debug!("Online exchange from {}", mint.to_string());
                    (proofs, exchange_hash_lock) = self
                        .online_exchange(proofs, mint, alpha_client, clowder_path.mints, tstamp)
                        .await?;
                } else {
                    tracing::debug!("Offline exchange from {}", mint.to_string());
                    let substitute_clowder_id = substitute_client.get_clowder_id().await?;
                    let evidence_digest = alpha_offline
                        .evidence_digest
                        .ok_or_else(|| Error::Swap("alpha not offline at substitute".into()))?;
                    let substitute_proofs = self
                        .offline_exchange(
                            substitute_client.as_ref(),
                            proofs,
                            substitute_clowder_id,
                            alpha_id,
                            evidence_digest,
                        )
                        .await?;

                    // Alpha proofs -> Substitute Beta proofs is done, so we only need the path from
                    // Substitute Beta to the Wallet Mint
                    tracing::debug!("Got substitute proofs - online exchange to own mint next");
                    let path = clowder_path.mints[1..].to_vec();
                    (proofs, exchange_hash_lock) = self
                        .online_exchange(
                            substitute_proofs,
                            substitute_beta_mint,
                            substitute_client.clone(),
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
        if let Some(hash_lock) = exchange_hash_lock
            && let Err(e) = self.debit.delete_exchange_record(hash_lock).await
        {
            tracing::warn!("Could not delete exchange record after storing its proofs: {e}");
        }
        let fee = if initial_amount > stored_amount {
            initial_amount - stored_amount
        } else {
            Amount::ZERO
        };
        let tx = Transaction {
            id: Uuid::new_v4(),
            mint_url: to_mint_url(self.client.mint_url()),
            direction: TransactionDirection::Incoming,
            fees: TransactionFees {
                swap: fee,
                ..Default::default()
            },
            amount: initial_amount,
            memo,
            tstamp,
            unit,
            payment_type,
            status,
            ys,
            quote_id: None,
            contact_node_id,
            payment_request_id,
            nostr_event_id,
            btc_tx_id: None,
            linked_txs: vec![],
        };
        let txid = self.tx_repo.store_tx(tx).await?;
        Ok(txid)
    }

    async fn offline_exchange(
        &self,
        substitute_client: &dyn ClowderMintConnector,
        proofs: Vec<Proof>,
        substitute_clowder_id: secp256k1::PublicKey,
        alpha_id: secp256k1::PublicKey,
        evidence_digest: [u8; 32],
    ) -> Result<Vec<Proof>> {
        // P2PK secret
        let wallet_pk = self.debit.exchange_key(&proofs)?;

        let secrets: Vec<_> = proofs.iter().map(|p| p.secret.clone()).collect();
        let beta_proofs = util::post_offline_exchange(
            substitute_client,
            substitute_clowder_id,
            &alpha_id,
            &evidence_digest,
            proofs,
            &wallet_pk,
        )
        .await?;
        util::unlock_offline_exchanged(
            beta_proofs,
            &secrets,
            &wallet_pk,
            &substitute_clowder_id,
            crate::config::LOCK_REDUCTION_SECONDS_PER_HOP,
        )
    }

    /// Exchanges alpha proofs for proofs of the wallet's mint along `path`.
    /// Returns them with the hash lock of the exchange record, to delete once they are stored.
    pub async fn online_exchange(
        &self,
        alpha_proofs: Vec<cashu::Proof>,
        alpha_url: url::Url,
        alpha_client: Arc<dyn ClowderMintConnector>,
        path: Vec<ConnectedMintResponse>,
        tstamp: u64,
    ) -> Result<(Vec<Proof>, Option<Sha256>)> {
        tracing::debug!(alpha_url=?alpha_url, "intermint exchange from ");
        // Already proofs on our mint
        if &alpha_url == self.client.mint_url() {
            tracing::debug!("not intermint exchanging proofs, since they're already on our mint");
            return Ok((alpha_proofs, None));
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
            .map(|b| (self.client_factory)(b.url.clone()))
            .collect();
        let alpha_beta = Arc::new(crate::pocket::RandomBetaProvider::new(
            alpha_beta_clients,
            path[0].node_id,
        )?);
        let lock = util::prepare_htlc_lock(
            tstamp,
            alpha_client.as_ref(),
            &alpha_proofs,
            hash_lock,
            key_locks,
            *wallet_pk.public_key(),
        )
        .await?;
        let record = bcr_wallet_persistence::ExchangeRecord {
            hash_lock,
            refund_secret: *wallet_pk,
            alpha_inputs: alpha_proofs,
            premints: lock.premints,
            alpha_url,
            alpha_clowder_id: path[0].node_id,
            locktime: lock.lock_time,
            locked_proofs: None,
        };
        let locked_alpha_proofs = self
            .debit
            .lock_exchange(
                record,
                lock.keysets,
                alpha_client,
                SwapConfig {
                    alpha_pk: path[0].node_id,
                    ..self.swap_config()
                },
                alpha_beta,
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
            htlc::sign_htlc(p, &preimage, &wallet_pk)?;
        }
        tracing::debug!("Returning same mint proofs");
        Ok((beta_proofs, Some(hash_lock)))
    }

    pub async fn refund_expired_exchanges(&self, tstamp: u64) -> Result<usize> {
        self.debit
            .refund_expired_exchanges(tstamp, self.client_factory.as_ref())
            .await
    }

    pub async fn receive_token(&self, token: Token, tstamp: u64) -> Result<Uuid> {
        let token_teaser = token.to_string().chars().take(20).collect::<String>();
        let token_mint_url = match token {
            Token::BitcrV4(ref bitcr_token_v4) => from_mint_url(&bitcr_token_v4.mint_url),
            Token::BitcrV5(ref bitcr_token_v5) => {
                if bitcr_token_v5.mint_id.network() != self.network() {
                    return Err(Error::InvalidNetwork(
                        self.network(),
                        bitcr_token_v5.mint_id.network(),
                    ));
                }

                let clowder_node_id = bitcr_token_v5.mint_id.clone();
                if clowder_node_id == self.clowder_node_id() {
                    self.mint_url()
                } else {
                    match bitcr_token_v5
                        .mint_url
                        .as_deref()
                        .and_then(|mint_url| url::Url::parse(mint_url).ok())
                    {
                        Some(mint_url) => mint_url,
                        None => {
                            let betas: HashMap<NodeId, url::Url> = self
                                .client
                                .get_clowder_betas()
                                .await?
                                .iter()
                                .map(|cb| {
                                    (
                                        NodeId::new(cb.clowder_id, self.network()),
                                        cb.url.to_owned(),
                                    )
                                })
                                .collect();
                            betas
                                .get(&clowder_node_id)
                                .ok_or(Error::BetaNotFound(clowder_node_id.to_string()))?
                                .to_owned()
                        }
                    }
                }
            }
        };
        let (intermint_infos, keysets_info) = self
            .get_clowder_path_and_keysets_info(token_mint_url.clone())
            .await?;

        let is_same_mint = &token_mint_url == self.client.mint_url();

        let proofs = if is_same_mint {
            let keysets: Vec<bcr_common::ecash::KeySetInfo> =
                keysets_info.values().map(|ks| ks.to_owned()).collect();
            token.proofs(&keysets)?
        } else if let Some((_, ref intermint_alpha_infos)) = intermint_infos {
            let keysets: Vec<bcr_common::ecash::KeySetInfo> = intermint_alpha_infos
                .values()
                .map(|ks| ks.to_owned())
                .collect();
            token.proofs(&keysets)?
        } else {
            // different mint, but no clowder-path set
            return Err(Error::InterMintButNoClowderPath);
        };

        if proofs.is_empty() {
            return Err(Error::EmptyToken(token_teaser));
        }

        let tx_id = if token.unit().is_some() && token.unit() == Some(self.debit.unit()) {
            tracing::debug!("import debit token");

            self._receive_proofs(
                &keysets_info,
                proofs.into_iter().map(|p| p.into()).collect(),
                self.debit.unit(),
                token_mint_url,
                intermint_infos,
                tstamp,
                token.memo().clone(),
                PaymentType::Token,
                TransactionStatus::Settled,
                None,
                None,
                None,
            )
            .await?
        } else {
            return Err(Error::InvalidToken(token_teaser));
        };
        Ok(tx_id)
    }

    async fn pay_to_node_id(
        &self,
        node_id: &NodeId,
        relays: Vec<RelayUrl>,
        proofs: Vec<cashu::Proof>,
        payment_request_id: Option<Uuid>,
        mut partial_tx: Transaction,
    ) -> Result<Uuid> {
        let payload = ContactPaymentPayload {
            payment_request_id,
            sender: self.node_id(),
            proofs,
            memo: partial_tx.memo.clone(),
            unit: partial_tx.unit.clone(),
            mint: to_mint_url(self.client.mint_url()),
            created_at: time::OffsetDateTime::now_utc().unix_timestamp() as u64,
        };
        let event: EventEnvelope =
            bcr_wallet_core::event::Event::new_contact_payment(payload).try_into()?;
        let payload = base58::encode(&borsh::to_vec(&event)?);
        let event_id = self
            .send_payment_via_nostr(node_id, relays, payload)
            .await?;
        partial_tx.nostr_event_id = Some(event_id);
        let txid = self.tx_repo.store_tx(partial_tx).await?;
        Ok(txid)
    }

    async fn send_payment_via_nostr(
        &self,
        node_id: &NodeId,
        relays: Vec<RelayUrl>,
        payload: String,
    ) -> Result<EventId> {
        let target = Nip19Profile::new(node_id.npub(), relays)
            .to_bech32()
            .map_err(|_| Error::Unsupported(node_id.to_string()))?;
        match self
            .nostr_transport
            .send_private_msg(target.clone(), payload.clone())
            .await
        {
            Ok(event_id) => Ok(event_id),
            Err(bcr_wallet_transport::error::Error::NostrSendPrivateMsg(event_id)) => {
                tracing::error!("Failed to send private message to {node_id}, queuing for retry");
                self.nostr_transport
                    .queue_retry_message(Some(target), payload)
                    .await?;
                Ok(event_id)
            }
            Err(e) => Err(e.into()),
        }
    }

    async fn send_payment_request(
        &self,
        node_id: NodeId,
        relays: Vec<RelayUrl>,
        amount: Amount,
        unit: CurrencyUnit,
        description: Option<String>,
        deadline: Option<u64>,
    ) -> Result<Uuid> {
        let payload = ContactPaymentRequestPayload::new(
            self.node_id(),
            amount,
            unit.clone(),
            description.clone(),
            deadline,
            to_mint_url(self.client.mint_url()),
        );
        let created_at = payload.created_at;
        let payment_req_id = payload.id;
        let event: EventEnvelope =
            bcr_wallet_core::event::Event::new_contact_payment_request(payload).try_into()?;
        let payload = base58::encode(&borsh::to_vec(&event)?);
        let event_id = self
            .send_payment_via_nostr(&node_id, relays, payload)
            .await?;
        tracing::info!("Sent payment request {payment_req_id} with nostr event_id {event_id}");
        let outgoing_payment_request = PaymentRequest {
            id: payment_req_id,
            node_id: Some(node_id),
            amount,
            unit,
            description,
            deadline,
            created_at,
            state: PaymentRequestState::Pending,
            direction: PaymentRequestDirection::Outgoing,
            history: Vec::new(),
            tombstone: false,
        };

        self.payment_request_repo
            .add_payment_request(outgoing_payment_request)
            .await?;
        Ok(payment_req_id)
    }

    pub(crate) async fn handle_payment_request_event(
        &self,
        event: bcr_wallet_transport::NostrWalletEvent,
    ) -> Option<Uuid> {
        match event {
            bcr_wallet_transport::NostrWalletEvent::ContactPaymentRequest {
                event_id,
                payload,
                sender,
            } => {
                if !is_authentic_sender(&payload.sender, &sender, self.network()) {
                    tracing::warn!(
                        "Rejected incoming Contact payment request: claimed sender {} is not on this network or not the authenticated Nostr sender {sender}",
                        payload.sender
                    );
                    return None;
                }
                tracing::info!(
                    "Received contact payment request {} from {sender}, event_id: {event_id}",
                    payload.id
                );
                let pending_incoming_payment_request: PaymentRequest = payload.into();
                let payment_request_id = pending_incoming_payment_request.id;
                match <Self as api::WalletApi>::add_payment_request(
                    self,
                    pending_incoming_payment_request,
                )
                .await
                {
                    Ok(_) => Some(payment_request_id),
                    Err(Error::Database(
                        bcr_wallet_persistence::error::Error::PaymentRequestAlreadyExists(_),
                    )) => {
                        // already had it - either sent again, or already processed - sending it either way and the caller can choose to ignore it
                        Some(payment_request_id)
                    }
                    Err(e) => {
                        tracing::error!("Could not store payment request: {e}");
                        None
                    }
                }
            }
            bcr_wallet_transport::NostrWalletEvent::PaymentRequestAction {
                event_id,
                payload,
                sender,
            } => {
                let payment_request_id = payload.payment_request_id;
                match self
                    .apply_remote_payment_request_action(payload, event_id, sender)
                    .await
                {
                    Ok(true) => Some(payment_request_id),
                    Ok(false) => None,
                    Err(e) => {
                        tracing::error!(
                            "Could not apply payment request action for {payment_request_id}: {e}"
                        );
                        None
                    }
                }
            }
            _ => None,
        }
    }

    pub(crate) async fn resolve_payment_request(
        &self,
        payment_req_id: Uuid,
        action: PaymentRequestActionKind,
    ) -> Result<()> {
        let (target_state, direction) = action.actor_transition();
        let Some(req) = self
            .payment_request_repo
            .get_payment_request(payment_req_id)
            .await?
        else {
            return Err(Error::PaymentRequestNotFound(payment_req_id));
        };
        if req.direction != direction {
            return Err(Error::PaymentRequestInWrongState(payment_req_id));
        }
        let payload =
            PaymentRequestActionPayload::new(payment_req_id, action, self.node_id(), None);
        let outcome = self
            .payment_request_repo
            .apply_payment_request_transition(
                payment_req_id,
                PaymentRequestTransition {
                    target_state,
                    actor: Some(payload.actor.clone()),
                    at: payload.acted_at,
                    origin: PaymentRequestActionOrigin::Local,
                    reason: None,
                },
            )
            .await?;
        if outcome != PaymentRequestTransitionOutcome::Applied {
            return Err(Error::PaymentRequestInWrongState(payment_req_id));
        }
        if let Some(node_id) = &req.node_id {
            self.send_payment_request_action(node_id, payload).await;
        }
        Ok(())
    }

    async fn send_payment_request_action(
        &self,
        node_id: &NodeId,
        payload: PaymentRequestActionPayload,
    ) {
        let payment_req_id = payload.payment_request_id;
        let relays = self.resolve_relays_for_node(node_id).await;
        let (target, message) = match Self::encode_payment_request_action(node_id, relays, payload)
        {
            Ok(encoded) => encoded,
            Err(e) => {
                tracing::error!(
                    "Could not encode payment request action for {payment_req_id}: {e}"
                );
                return;
            }
        };
        match self
            .nostr_transport
            .send_private_msg(target.clone(), message.clone())
            .await
        {
            Ok(event_id) => {
                tracing::info!(
                    "Sent payment request action for {payment_req_id} with nostr event_id {event_id}"
                );
            }
            Err(e) => {
                tracing::error!("Failed to send payment request action, queuing for retry: {e}");
                if let Err(e) = self
                    .nostr_transport
                    .queue_retry_message(Some(target), message)
                    .await
                {
                    tracing::error!("Failed to queue payment request action retry: {e}");
                }
            }
        }
    }

    fn encode_payment_request_action(
        node_id: &NodeId,
        relays: Vec<RelayUrl>,
        payload: PaymentRequestActionPayload,
    ) -> Result<(String, String)> {
        let target = Nip19Profile::new(node_id.npub(), relays)
            .to_bech32()
            .map_err(|_| Error::Unsupported(node_id.to_string()))?;
        let event: EventEnvelope =
            bcr_wallet_core::event::Event::new_payment_request_action(payload).try_into()?;
        Ok((target, base58::encode(&borsh::to_vec(&event)?)))
    }

    pub(crate) async fn apply_remote_payment_request_action(
        &self,
        payload: PaymentRequestActionPayload,
        event_id: EventId,
        sender: nostr::key::PublicKey,
    ) -> Result<bool> {
        let payment_req_id = payload.payment_request_id;
        if !is_authentic_sender(&payload.actor, &sender, self.network()) {
            tracing::warn!(
                "Rejected payment request action for {payment_req_id}: claimed actor {} is not on this network or not the authenticated Nostr sender {sender}",
                payload.actor
            );
            return Ok(false);
        }
        let (target_state, actor_direction) = payload.action.actor_transition();
        let outcome = self
            .payment_request_repo
            .apply_remote_payment_request_transition(
                payment_req_id,
                payload.actor.clone(),
                actor_direction.opposite(),
                PaymentRequestTransition {
                    target_state,
                    actor: Some(payload.actor),
                    at: payload.acted_at,
                    origin: PaymentRequestActionOrigin::Remote {
                        event_id: event_id.to_string(),
                    },
                    reason: payload.reason,
                },
            )
            .await?;
        tracing::info!("Applied remote payment request action for {payment_req_id}: {outcome:?}");
        Ok(outcome.is_some())
    }

    async fn resolve_relays_for_node(&self, node_id: &NodeId) -> Vec<RelayUrl> {
        let mut known_relays = self.nostr_transport.relays().to_owned();
        if let Ok(contacts) = self
            .contact_repo
            .get_contacts_by_node_id(node_id.clone())
            .await
        {
            for contact in contacts {
                for relay in contact.nostr_relays {
                    if !known_relays.contains(&relay) {
                        known_relays.push(relay);
                    }
                }
            }
        }

        self.fetch_nostr_relays(node_id.npub(), known_relays).await
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
    async fn refresh_contact_relays(&self, contact_id: &Uuid) {
        let Ok(Some(existing_contact)) = self.contact_repo.get_contact(contact_id.to_owned()).await
        else {
            return;
        };

        let Some(node_id) = existing_contact.node_id else {
            return;
        };

        let existing_relays = existing_contact.nostr_relays;
        let relays = self
            .fetch_nostr_relays(node_id.npub(), existing_relays.clone())
            .await;
        if relays != existing_relays
            && let Err(e) = self
                .contact_repo
                .edit_contact_relays(contact_id.to_owned(), relays)
                .await
        {
            tracing::warn!("Could not update relays for contact {node_id}: {e}");
        }
    }

    async fn prepare_pay_node_id(
        &self,
        node_id: NodeId,
        amount: Amount,
        payment_request_id: Option<Uuid>,
        memo: Option<String>,
    ) -> Result<PaymentSummary> {
        let infos = self.get_wallet_mint_keyset_infos().await?;

        let s_summary = self.debit.prepare_send(amount, &infos).await?;
        let mut summary = PaymentSummary::from(s_summary);
        summary.ptype = if payment_request_id.is_some() {
            PaymentType::PaymentRequest
        } else {
            PaymentType::Contact
        };
        let pref = PayReference {
            request_id: summary.request_id,
            unit: summary.unit.clone(),
            fees: summary.fees,
            ptype: WalletPaymentType::PaymentRequest {
                node_id,
                payment_request_id,
            },
            memo,
        };
        *self.current_payment.lock().await = Some(pref);
        Ok(summary)
    }
}

fn reclaimed_amount(entries: &[(cashu::PublicKey, MigrationJournalEntry)]) -> Option<Amount> {
    entries.iter().find_map(|(_, e)| match e.state {
        MigrationJournalState::Reclaimed(amount) => Some(amount),
        _ => None,
    })
}

#[cfg(test)]
pub(crate) mod tests;
