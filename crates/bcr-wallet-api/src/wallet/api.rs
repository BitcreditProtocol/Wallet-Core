use crate::{
    ClowderMintConnector,
    error::{Error, Result},
    pocket::{
        RandomBetaProvider,
        debit::{MeltProtestResult, ProtestResult},
    },
    types::{MintSummary, PaymentSummary, WalletConfig},
    wallet::{
        api,
        types::{PayReference, SwapConfig, WalletInfo, WalletPaymentType, WalletProtestResult},
    },
};
use async_trait::async_trait;
use bcr_common::{
    cashu::{self, Amount, CurrencyUnit, KeySet, ProofsMethods, nut00 as cdk00, nut18 as cdk18},
    cdk_common::wallet::TransactionDirection,
    core::NodeId,
    wallet::{BitcrTokenV5, Token},
    wire::clowder::{self as wire_clowder},
};
use bcr_wallet_core::{
    SendSync,
    event::{ContactPaymentRequestPayload, EventEnvelope},
    types::{
        MeltEstimation, PaymentRequest, PaymentRequestDirection, PaymentRequestState,
        PaymentResultCallback, PaymentType, PendingPaymentSubscriptionCallback, Transaction,
        TransactionFees, TransactionStatus,
    },
    util::{from_mint_url, to_mint_url},
};
use bcr_wallet_transport::NostrWalletEvent;
use bitcoin::base58;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use nostr::{RelayUrl, event::EventId};
use std::{
    collections::{HashMap, HashSet},
    str::FromStr,
    sync::Arc,
};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait WalletApi: SendSync {
    fn config(&self) -> Result<WalletConfig>;
    fn info(&self) -> WalletInfo;
    fn name(&self) -> String;
    fn node_id(&self) -> NodeId;
    fn id(&self) -> String;
    fn mint_url(&self) -> url::Url;
    fn betas(&self) -> Vec<url::Url>;
    fn clowder_node_id(&self) -> NodeId;
    fn mint_urls(&self) -> Vec<url::Url>;
    async fn estimate_melt(&self, amount: bitcoin::Amount) -> Result<MeltEstimation>;
    async fn prepare_melt(
        &self,
        amount: bitcoin::Amount,
        network_fee: bitcoin::Amount,
        melt_fee: bitcoin::Amount,
        address: bitcoin::Address<bitcoin::address::NetworkUnchecked>,
        description: Option<String>,
    ) -> Result<PaymentSummary>;
    async fn prepare_pay_cdk18(&self, input: String) -> Result<PaymentSummary>;
    async fn prepare_cdk18_payment_request(
        &self,
        amount: Amount,
        unit: CurrencyUnit,
        description: Option<String>,
    ) -> Result<cdk18::PaymentRequest>;
    async fn check_received_payment(
        &self,
        max_wait: core::time::Duration,
        p_id: Uuid,
        cancel_token: CancellationToken,
        result_callback: PaymentResultCallback,
    ) -> Result<()>;
    async fn is_wallet_mint_rabid(&self) -> Result<bool>;
    async fn is_wallet_mint_offline(&self) -> Result<bool>;
    async fn mint_substitute(&self) -> Result<Option<url::Url>>;
    async fn pay(
        &self,
        p_id: Uuid,
        http_cl: &reqwest::Client,
        tstamp: u64,
    ) -> Result<(Uuid, Option<Token>)>;
    async fn mint(&self, amount: bitcoin::Amount) -> Result<MintSummary>;
    async fn check_pending_mints(&self) -> Result<Vec<Uuid>>;
    async fn check_pending_commitments(&self) -> Result<()>;
    async fn protest_mint(&self, quote_id: Uuid) -> Result<WalletProtestResult>;
    async fn protest_swap(
        &self,
        commitment_sig: bitcoin::secp256k1::schnorr::Signature,
    ) -> Result<WalletProtestResult>;
    async fn protest_melt(&self, quote_id: Uuid) -> Result<WalletProtestResult>;
    async fn check_pending_melt_commitments(&self) -> Result<()>;
    async fn migrate_pockets_substitute(
        &mut self,
        substitute: Arc<dyn ClowderMintConnector>,
    ) -> Result<url::Url>;
    async fn receive_proofs(
        &self,
        proofs: Vec<cdk00::Proof>,
        unit: CurrencyUnit,
        mint: url::Url,
        tstamp: u64,
        memo: Option<String>,
        payment_type: PaymentType,
        status: TransactionStatus,
        payment_request_id: Option<Uuid>,
        contact_node_id: Option<NodeId>,
        nostr_event_id: Option<EventId>,
    ) -> Result<Uuid>;
    async fn prepare_pay_by_token(
        &self,
        amount: Amount,
        unit: CurrencyUnit,
        description: Option<String>,
    ) -> Result<PaymentSummary>;
    async fn prepare_pay_to_contact(
        &self,
        contact_id: Uuid,
        amount: Amount,
        unit: CurrencyUnit,
        description: Option<String>,
    ) -> Result<PaymentSummary>;
    async fn offline_pay_by_token(
        &self,
        request_id: Uuid,
        unit: CurrencyUnit,
        fees: TransactionFees,
        memo: Option<String>,
        now: u64,
    ) -> Result<(Uuid, Option<Token>)>;
    async fn is_nostr_connected(&self) -> bool;
    async fn fetch_nostr_relays(
        &self,
        npub: nostr::PublicKey,
        relays: Vec<RelayUrl>,
    ) -> Result<Vec<RelayUrl>>;
    async fn delete(&self) -> Result<()>;
    async fn create_shareable_remote_payment_request(
        &self,
        amount: Amount,
        unit: CurrencyUnit,
        description: Option<String>,
    ) -> Result<String>;
    async fn prepare_pay_shared_payment_request(
        &self,
        payment_req: String,
    ) -> Result<PaymentSummary>;
    async fn request_payment_from_contact(
        &self,
        contact_id: Uuid,
        amount: Amount,
        unit: CurrencyUnit,
        description: Option<String>,
        deadline: Option<u64>,
    ) -> Result<Uuid>;
    async fn subscribe_to_payment_requests(
        &self,
        cancel_token: CancellationToken,
        item_callback: PendingPaymentSubscriptionCallback,
    ) -> Result<()>;
    async fn list_payment_requests(
        &self,
        direction: PaymentRequestDirection,
        states: Vec<PaymentRequestState>,
    ) -> Result<Vec<PaymentRequest>>;
    async fn get_payment_request(&self, payment_req_id: Uuid) -> Result<Option<PaymentRequest>>;
    async fn add_payment_request(
        &self,
        pending_incoming_payment_request: PaymentRequest,
    ) -> Result<()>;
    async fn prepare_pay_payment_request(&self, payment_req_id: Uuid) -> Result<PaymentSummary>;
    async fn reject_payment_request(&self, payment_req_id: Uuid) -> Result<()>;
    async fn cancel_payment_request(&self, payment_req_id: Uuid) -> Result<()>;
    async fn mark_payment_request_as_paid(&self, payment_req_id: Uuid, tx_id: Uuid) -> Result<()>;
}

#[async_trait]
impl WalletApi for super::Wallet {
    fn config(&self) -> Result<WalletConfig> {
        Ok(WalletConfig {
            wallet_id: self.id.clone(),
            name: self.name.clone(),
            network: self.network,
            debit: self.debit.unit(),
            mint: self.client.mint_url().to_owned(),
            mint_keyset_infos: self.mint_keyset_infos.clone(),
            clowder_id: self.clowder_id,
            pub_key: self.pub_key,
            betas: self.betas(),
            nostr_relays: self.nostr_transport.relays().to_owned(),
        })
    }

    fn info(&self) -> WalletInfo {
        WalletInfo {
            name: self.name.clone(),
            node_id: self.node_id(),
            network: self.network,
            default_mint_url: self.client.mint_url().to_owned(),
            nostr_relays: self.nostr_transport.relays().to_owned(),
        }
    }

    fn node_id(&self) -> NodeId {
        NodeId::new(self.pub_key, self.network)
    }

    fn name(&self) -> String {
        self.name.clone()
    }

    fn id(&self) -> String {
        self.id.clone()
    }

    fn mint_url(&self) -> url::Url {
        self.client.mint_url().to_owned()
    }

    async fn estimate_melt(&self, amount: bitcoin::Amount) -> Result<MeltEstimation> {
        let res = self.client.post_melt_estimate_onchain(amount).await?;
        Ok(res)
    }

    async fn prepare_melt(
        &self,
        amount: bitcoin::Amount,
        network_fee: bitcoin::Amount,
        melt_fee: bitcoin::Amount,
        address: bitcoin::Address<bitcoin::address::NetworkUnchecked>,
        description: Option<String>,
    ) -> Result<PaymentSummary> {
        let infos = self.get_wallet_mint_keyset_infos().await?;

        let m_summary = self
            .debit
            .prepare_onchain_melt(
                address.assume_checked().to_string(),
                amount.to_sat(),
                network_fee.to_sat(),
                melt_fee.to_sat(),
                &infos,
                self.client.clone(),
                self.swap_config(),
            )
            .await?;
        let summary = PaymentSummary::from(m_summary);
        let pref = PayReference {
            request_id: summary.request_id,
            unit: summary.unit.clone(),
            fees: summary.fees,
            ptype: WalletPaymentType::OnChain,
            memo: description,
        };
        *self.current_payment.lock().await = Some(pref);
        Ok(summary)
    }

    async fn prepare_pay_cdk18(&self, input: String) -> Result<PaymentSummary> {
        let infos = self.get_wallet_mint_keyset_infos().await?;

        if let Ok(request) = cashu::PaymentRequest::from_str(&input) {
            let (amount, unit, transport) = self.check_nut18_request(&request).await?;
            if unit != self.debit.unit() {
                return Err(Error::InvalidCurrencyUnit(unit.to_string()));
            }
            let s_summary = self.debit.prepare_send(amount, &infos).await?;
            let mut summary = PaymentSummary::from(s_summary);
            summary.ptype = PaymentType::Cdk18;
            let pref = PayReference {
                request_id: summary.request_id,
                unit: summary.unit.clone(),
                fees: summary.fees,
                ptype: WalletPaymentType::Cdk18 {
                    transport,
                    id: request.payment_id,
                },
                memo: request.description,
            };
            *self.current_payment.lock().await = Some(pref);
            Ok(summary)
        } else {
            Err(Error::UnknownPaymentRequest(input))
        }
    }

    async fn prepare_cdk18_payment_request(
        &self,
        amount: Amount,
        unit: CurrencyUnit,
        description: Option<String>,
    ) -> Result<cdk18::PaymentRequest> {
        let nostr_transport = self.nostr_transport.cdk18_transport().await?;
        let mints = self
            .mint_urls()
            .into_iter()
            .map(|url| to_mint_url(&url))
            .collect();
        let request = cdk18::PaymentRequest {
            payment_id: Some(Uuid::new_v4().to_string()),
            amount: Some(amount),
            mints,
            unit: Some(unit),
            single_use: Some(true),
            description,
            nut10: None,
            transports: vec![nostr_transport],
        };
        *self.current_payment_request.lock().await = Some(request.clone());
        Ok(request)
    }

    async fn check_received_payment(
        &self,
        max_wait: core::time::Duration,
        p_id: Uuid,
        cancel_token: CancellationToken,
        result_callback: PaymentResultCallback,
    ) -> Result<()> {
        let current_request = self.current_payment_request.lock().await.take();
        let Some(req) = current_request else {
            return Err(Error::NoPrepareRef(p_id));
        };

        if req.payment_id != Some(p_id.to_string()) {
            return Err(Error::NoPrepareRef(p_id));
        }
        let expected = req.amount.unwrap_or_default();

        let start = tokio::time::Instant::now();

        tracing::debug!("Subscribing to events from Nostr...");
        let deadline = start + max_wait;
        let mut nostr_receiver = self.nostr_event_channel.subscribe();

        loop {
            tokio::select! {
                _ = cancel_token.cancelled() => {
                    tracing::info!("check_received_payment cancelled: {p_id}");
                    result_callback(None);
                    return Ok(());
                },
                _ = tokio::time::sleep_until(deadline) => {
                    tracing::warn!("check_received_payment timed out: {p_id}");
                    result_callback(None);
                    return Ok(());
                },
                evt = nostr_receiver.recv() => {
                    let received_evt = match evt {
                        Ok(e) => e,
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            tracing::warn!("check_received_payment channel lagged behind: {p_id}");
                            continue;
                        },
                        Err(broadcast::error::RecvError::Closed) => {
                            tracing::warn!("check_received_payment channel closed: {p_id}");
                            result_callback(None);
                            return Ok(());
                        },
                    };

                    let NostrWalletEvent::Cdk18Payment { event_id, payload, .. } = received_evt else {
                        continue;
                    };

                    if payload.id != Some(p_id.to_string()) {
                        tracing::debug!("handle event, payment id doesn't match");
                        continue;
                    }

                    let amount = payload.proofs.total_amount()?;
                    if amount < expected {
                        tracing::warn!(
                            "Received amount {} is less than expected {}",
                            amount,
                            expected
                        );
                        continue;
                    }

                    let response = <Self as api::WalletApi>::receive_proofs(
                        self,
                        payload.proofs,
                        payload.unit,
                        from_mint_url(&payload.mint),
                        chrono::Utc::now().timestamp() as u64,
                        payload.memo,
                        PaymentType::Cdk18,
                        TransactionStatus::Settled,
                        Some(p_id),
                        None,
                        Some(event_id)
                    )
                        .await;

                    match response {
                        Ok(txid) => {
                            result_callback(Some(txid));
                            return Ok(());
                        },
                        Err(e) => {
                            tracing::error!("Error while handling Nostr event: {e}");
                            continue;
                        },
                    };

                }
            }
        }
    }

    async fn pay(
        &self,
        p_id: Uuid,
        http_cl: &reqwest::Client,
        now: u64,
    ) -> Result<(Uuid, Option<Token>)> {
        let p_ref = self.current_payment.lock().await.take();
        let Some(p_ref) = p_ref else {
            tracing::error!("wallet: No current payment reference found");
            return Err(Error::NoPrepareRef(p_id));
        };
        if p_ref.request_id != p_id {
            tracing::error!(
                "wallet: Payment reference ID mismatch: expected {}, got {}",
                p_ref.request_id,
                p_id
            );
            return Err(Error::NoPrepareRef(p_id));
        }
        let infos = self.get_wallet_mint_keyset_infos().await?;
        let PayReference {
            request_id,
            unit,
            fees,
            ptype,
            memo,
        } = p_ref;
        if unit != self.debit.unit() {
            return Err(Error::InvalidCurrencyUnit(unit.to_string()));
        }
        match ptype {
            WalletPaymentType::Cdk18 { transport, id } => {
                let proofs = self
                    .debit
                    .send_proofs(request_id, &infos, self.client.clone(), self.swap_config())
                    .await?;
                let (ys, proofs): (Vec<cashu::PublicKey>, Vec<cashu::Proof>) =
                    proofs.into_iter().unzip();
                let amount = proofs.total_amount()?;

                let partial_tx = Transaction {
                    id: Uuid::new_v4(),
                    mint_url: to_mint_url(self.client.mint_url()),
                    fees,
                    direction: TransactionDirection::Outgoing,
                    memo,
                    tstamp: now,
                    unit: unit.clone(),
                    ys,
                    amount,
                    quote_id: None,
                    payment_request_id: None,
                    payment_type: PaymentType::Cdk18,
                    status: TransactionStatus::Pending,
                    btc_tx_id: None,
                    nostr_event_id: None,
                    contact_node_id: None,
                    linked_txs: vec![],
                };
                let tx_id = self
                    .pay_nut18(
                        proofs,
                        &self.nostr_transport,
                        http_cl,
                        transport,
                        id,
                        partial_tx,
                    )
                    .await?;
                Ok((tx_id, None))
            }
            WalletPaymentType::Token => {
                // Handle Wallet Mint Offline Case
                match self.is_wallet_mint_offline().await {
                    Ok(is_offline) => {
                        if is_offline {
                            return self
                                .offline_pay_by_token(request_id, unit, fees, memo, now)
                                .await;
                        }
                    }
                    Err(e) => {
                        tracing::error!(
                            "Pay by Token: Error during online check - attempting without offline mode: {e}"
                        );
                    }
                };

                let (proofs, token) = {
                    let p = self
                        .debit
                        .send_proofs(request_id, &infos, self.client.clone(), self.swap_config())
                        .await?;
                    let mut token = BitcrTokenV5::new(
                        self.clowder_node_id(),
                        self.debit.unit(),
                        p.values().map(|p| p.to_owned().into()).collect(),
                    )
                    .with_mint_url(to_mint_url(self.client.mint_url()).to_string());
                    if let Some(ref m) = memo {
                        token = token.with_memo(m.to_string());
                    }
                    (p.clone(), Token::BitcrV5(token))
                };
                let (ys, proofs): (Vec<cashu::PublicKey>, Vec<cashu::Proof>) =
                    proofs.into_iter().unzip();
                let amount = proofs.total_amount()?;

                let partial_tx = Transaction {
                    id: Uuid::new_v4(),
                    mint_url: to_mint_url(self.client.mint_url()),
                    fees,
                    direction: TransactionDirection::Outgoing,
                    memo,
                    tstamp: now,
                    unit: unit.clone(),
                    ys,
                    amount,
                    quote_id: None,
                    payment_request_id: None,
                    payment_type: PaymentType::Token,
                    status: TransactionStatus::Pending,
                    btc_tx_id: None,
                    nostr_event_id: None,
                    contact_node_id: None,
                    linked_txs: vec![],
                };
                let tx_id = self.tx_repo.store_tx(partial_tx).await?;
                Ok((tx_id, Some(token)))
            }
            WalletPaymentType::OnChain => {
                let (btc_tx_id, proofs) = self
                    .debit
                    .pay_onchain_melt(request_id, self.client.clone())
                    .await?;
                let (ys, proofs): (Vec<cashu::PublicKey>, Vec<cashu::Proof>) =
                    proofs.into_iter().unzip();
                let mut amount = proofs.total_amount()?;
                // the proofs are for amount + network_fee + melt_fee, so we need to subtract them
                amount = amount - fees.network - fees.melt;

                let partial_tx = Transaction {
                    id: Uuid::new_v4(),
                    mint_url: to_mint_url(self.client.mint_url()),
                    fees,
                    direction: TransactionDirection::Outgoing,
                    memo,
                    tstamp: now,
                    unit: unit.clone(),
                    ys,
                    amount,
                    quote_id: None,
                    payment_type: PaymentType::OnChain,
                    status: TransactionStatus::Settled,
                    btc_tx_id: Some(btc_tx_id),
                    nostr_event_id: None,
                    contact_node_id: None,
                    payment_request_id: None,
                    linked_txs: vec![],
                };
                let tx_id = self.tx_repo.store_tx(partial_tx).await?;
                Ok((tx_id, None))
            }
            WalletPaymentType::Contact {
                contact_id,
                payment_request_id,
            } => {
                self.refresh_contact_relays(&contact_id).await;
                let Ok(Some(contact)) = self.contact_repo.get_contact(contact_id).await else {
                    return Err(Error::ContactNotFound(contact_id.to_string()));
                };
                if contact.node_id.is_none() {
                    return Err(Error::ContactMustHaveNodeId(contact.id.to_string()));
                }

                let proofs = self
                    .debit
                    .send_proofs(request_id, &infos, self.client.clone(), self.swap_config())
                    .await?;
                let (ys, proofs): (Vec<cashu::PublicKey>, Vec<cashu::Proof>) =
                    proofs.into_iter().unzip();
                let amount = proofs.total_amount()?;

                let partial_tx = Transaction {
                    id: Uuid::new_v4(),
                    mint_url: to_mint_url(self.client.mint_url()),
                    fees,
                    direction: TransactionDirection::Outgoing,
                    memo,
                    tstamp: now,
                    unit: unit.clone(),
                    ys,
                    amount,
                    payment_type: PaymentType::Contact,
                    status: TransactionStatus::Pending,
                    payment_request_id,
                    btc_tx_id: None,
                    quote_id: None,
                    nostr_event_id: None,
                    contact_node_id: contact.node_id.clone(),
                    linked_txs: vec![],
                };
                let tx_id = self
                    .pay_to_contact(
                        proofs,
                        &self.nostr_transport,
                        contact,
                        payment_request_id,
                        partial_tx,
                    )
                    .await?;
                // if it was a payment request - mark as paid
                if let Some(p_req_id) = payment_request_id
                    && let Err(e) = self.mark_payment_request_as_paid(p_req_id, tx_id).await
                {
                    tracing::warn!(
                        "Could not mark payment request {p_req_id} as paid after successful payment: {e}"
                    );
                }
                Ok((tx_id, None))
            }
            WalletPaymentType::SharedPaymentRequest { node_id } => {
                let existing_relays = self.nostr_transport.relays().to_owned();
                let receiver_relays = self
                    .nostr_transport
                    .fetch_relay_list(node_id.npub(), existing_relays)
                    .await?;

                let proofs = self
                    .debit
                    .send_proofs(request_id, &infos, self.client.clone(), self.swap_config())
                    .await?;
                let (ys, proofs): (Vec<cashu::PublicKey>, Vec<cashu::Proof>) =
                    proofs.into_iter().unzip();
                let amount = proofs.total_amount()?;

                let partial_tx = Transaction {
                    id: Uuid::new_v4(),
                    mint_url: to_mint_url(self.client.mint_url()),
                    fees,
                    direction: TransactionDirection::Outgoing,
                    memo,
                    tstamp: now,
                    unit: unit.clone(),
                    ys,
                    amount,
                    payment_type: PaymentType::Contact,
                    status: TransactionStatus::Pending,
                    payment_request_id: None,
                    btc_tx_id: None,
                    quote_id: None,
                    nostr_event_id: None,
                    contact_node_id: Some(node_id.clone()),
                    linked_txs: vec![],
                };
                let tx_id = self
                    .pay_shared_payment_request(
                        node_id,
                        receiver_relays,
                        proofs,
                        &self.nostr_transport,
                        partial_tx,
                    )
                    .await?;

                Ok((tx_id, None))
            }
        }
    }

    async fn mint(&self, amount: bitcoin::Amount) -> Result<MintSummary> {
        let keysets_info = self.get_wallet_mint_keyset_infos().await?;
        let summary = self
            .debit
            .mint_onchain(amount, &keysets_info, self.client.clone())
            .await?;
        Ok(summary)
    }

    async fn check_pending_mints(&self) -> Result<Vec<Uuid>> {
        let mut res = Vec::new();
        let now = chrono::Utc::now();
        let pending_mints_result = self.debit.check_pending_mints(self.client.clone()).await?;

        for (qid, mint_result) in pending_mints_result {
            let tx = Transaction {
                id: Uuid::new_v4(),
                mint_url: to_mint_url(self.client.mint_url()),
                fees: TransactionFees {
                    swap: mint_result.fee, // when minting, we only accrue swap fees
                    ..Default::default()
                },
                direction: TransactionDirection::Incoming,
                memo: None,
                status: TransactionStatus::Settled,
                payment_type: PaymentType::OnChain,
                tstamp: now.timestamp() as u64,
                unit: self.debit_unit(),
                ys: mint_result.ys,
                amount: mint_result.amount,
                quote_id: Some(qid),
                payment_request_id: None,
                btc_tx_id: None,
                nostr_event_id: None,
                contact_node_id: None,
                linked_txs: vec![],
            };
            let tx_id = self.tx_repo.store_tx(tx).await?;
            res.push(tx_id);
        }
        Ok(res)
    }

    async fn check_pending_commitments(&self) -> Result<()> {
        let now = chrono::Utc::now().timestamp() as u64;
        self.debit.check_pending_commitments(now).await
    }

    async fn protest_mint(&self, quote_id: Uuid) -> Result<WalletProtestResult> {
        let ProtestResult { status, result } = self
            .debit
            .protest_mint(quote_id, self.client.clone())
            .await?;

        if let Some((amount, ref ys)) = result {
            let now = chrono::Utc::now();
            let tx = Transaction {
                id: Uuid::new_v4(),
                mint_url: to_mint_url(self.client.mint_url()),
                fees: TransactionFees::default(),
                direction: TransactionDirection::Incoming,
                memo: Some("Mint protest resolved".to_string()),
                tstamp: now.timestamp() as u64,
                unit: self.debit_unit(),
                ys: ys.clone(),
                amount,
                status: TransactionStatus::Settled,
                payment_type: PaymentType::OnChain,
                quote_id: Some(quote_id),
                nostr_event_id: None,
                btc_tx_id: None,
                contact_node_id: None,
                payment_request_id: None,
                linked_txs: vec![],
            };
            self.tx_repo.store_tx(tx).await?;
        }

        Ok(WalletProtestResult { status, result })
    }

    async fn protest_swap(
        &self,
        commitment_sig: bitcoin::secp256k1::schnorr::Signature,
    ) -> Result<WalletProtestResult> {
        let keysets_info = self.get_wallet_mint_keyset_infos().await?;
        let swap_config = self.swap_config();

        let ProtestResult { status, result } = self
            .debit
            .protest_swap(
                commitment_sig,
                &keysets_info,
                self.client.clone(),
                swap_config,
            )
            .await?;

        if let Some((amount, ref ys)) = result {
            let now = chrono::Utc::now();
            let tx = Transaction {
                id: Uuid::new_v4(),
                mint_url: to_mint_url(self.client.mint_url()),
                fees: TransactionFees::default(),
                direction: TransactionDirection::Incoming,
                memo: Some("Swap protest resolved".to_string()),
                tstamp: now.timestamp() as u64,
                unit: self.debit_unit(),
                ys: ys.clone(),
                payment_type: PaymentType::Swap,
                status: TransactionStatus::Settled,
                amount,
                quote_id: None,
                nostr_event_id: None,
                btc_tx_id: None,
                contact_node_id: None,
                payment_request_id: None,
                linked_txs: vec![],
            };
            self.tx_repo.store_tx(tx).await?;
        }

        Ok(WalletProtestResult { status, result })
    }

    async fn protest_melt(&self, quote_id: Uuid) -> Result<WalletProtestResult> {
        let MeltProtestResult {
            base: ProtestResult { status, result },
            txid,
        } = self.debit.protest_melt(quote_id).await?;

        if let Some((amount, ref ys)) = result {
            let now = chrono::Utc::now();

            let tx = Transaction {
                id: Uuid::new_v4(),
                mint_url: to_mint_url(self.client.mint_url()),
                fees: TransactionFees::default(),
                direction: TransactionDirection::Outgoing,
                memo: Some("Melt protest resolved".to_string()),
                tstamp: now.timestamp() as u64,
                unit: self.debit_unit(),
                ys: ys.clone(),
                payment_type: PaymentType::OnChain,
                status: TransactionStatus::Settled,
                amount,
                quote_id: Some(quote_id),
                nostr_event_id: None,
                btc_tx_id: txid,
                contact_node_id: None,
                payment_request_id: None,
                linked_txs: vec![],
            };
            self.tx_repo.store_tx(tx).await?;
        }

        Ok(WalletProtestResult { status, result })
    }

    async fn check_pending_melt_commitments(&self) -> Result<()> {
        const PROTEST_WINDOW_SECS: u64 = 3600;
        let now_ts = chrono::Utc::now().timestamp() as u64;
        let commitments = self.debit.list_melt_commitments().await?;
        tracing::debug!(
            "check pending melt commitments for {} entries",
            commitments.len()
        );
        for (quote_id, expiry) in commitments {
            if expiry.saturating_sub(now_ts) > PROTEST_WINDOW_SECS {
                continue;
            }
            match self.protest_melt(quote_id).await {
                Ok(_) => {}
                Err(e) => tracing::warn!("melt protest for {quote_id} failed: {e}"),
            }
        }
        Ok(())
    }

    async fn receive_proofs(
        &self,
        proofs: Vec<cashu::Proof>,
        unit: CurrencyUnit,
        mint: url::Url,
        tstamp: u64,
        memo: Option<String>,
        payment_type: PaymentType,
        status: TransactionStatus,
        payment_request_id: Option<Uuid>,
        contact_node_id: Option<NodeId>,
        nostr_event_id: Option<EventId>,
    ) -> Result<Uuid> {
        let (intermint_infos, local_alpha_keysets_info) =
            self.get_clowder_path_and_keysets_info(mint.clone()).await?;
        self._receive_proofs(
            &local_alpha_keysets_info,
            proofs,
            unit,
            mint,
            intermint_infos,
            tstamp,
            memo,
            payment_type,
            status,
            payment_request_id,
            contact_node_id,
            nostr_event_id,
        )
        .await
    }

    async fn is_wallet_mint_rabid(&self) -> Result<bool> {
        let betas_count = self.betas().len();
        let mut futures = FuturesUnordered::new();

        for beta in self.betas() {
            let beta_client = self
                .beta_clients
                .get(&beta)
                .ok_or(Error::BetaNotFound(beta.to_string()))?;

            futures.push(async move {
                let status = beta_client.get_alpha_status(self.clowder_id).await?.state;
                Ok::<bool, Error>(matches!(
                    status,
                    wire_clowder::SimpleAlphaState::Rabid(..)
                        | wire_clowder::SimpleAlphaState::ConfiscatedRabid(..)
                ))
            });
        }

        let mut rabid_count = 0;
        while let Some(is_rabid) = futures.next().await {
            if let Ok(true) = is_rabid {
                rabid_count += 1;
                if rabid_count > betas_count / 2 {
                    return Ok(true);
                }
            }
        }

        Ok(rabid_count > betas_count / 2)
    }

    async fn is_wallet_mint_offline(&self) -> Result<bool> {
        let betas_count = self.betas().len();
        let mut futures = FuturesUnordered::new();

        for beta in self.betas() {
            let beta_client = self
                .beta_clients
                .get(&beta)
                .ok_or(Error::BetaNotFound(beta.to_string()))?;

            futures.push(async move {
                let status = beta_client.get_alpha_status(self.clowder_id).await?.state;
                Ok::<bool, Error>(matches!(
                    status,
                    wire_clowder::SimpleAlphaState::Offline(..)
                ))
            });
        }

        let mut offline_count = 0;
        while let Some(is_offline) = futures.next().await {
            if let Ok(true) = is_offline {
                offline_count += 1;
                if offline_count > betas_count / 2 {
                    return Ok(true);
                }
            }
        }

        Ok(offline_count > betas_count / 2)
    }

    async fn mint_substitute(&self) -> Result<Option<url::Url>> {
        let mint_id = self.clowder_id;
        let betas_count = self.betas().len();
        let threshold = betas_count / 2;
        let mut futures = FuturesUnordered::new();

        for beta in self.betas() {
            let beta_client = self
                .beta_clients
                .get(&beta)
                .ok_or(Error::BetaNotFound(beta.to_string()))?;

            futures.push(async move {
                let mint = beta_client.get_alpha_substitute(mint_id).await?.mint;
                Ok::<url::Url, Error>(mint)
            });
        }

        let mut substitute_counts = HashMap::<url::Url, usize>::new();

        while let Some(vote) = futures.next().await {
            let mint = vote?;
            let count = substitute_counts.entry(mint.clone()).or_default();
            *count += 1;

            if *count > threshold {
                return Ok(Some(mint));
            }
        }

        Ok(None)
    }

    fn mint_urls(&self) -> Vec<url::Url> {
        let mut urls = self.betas();
        urls.push(self.client.mint_url().to_owned());
        urls
    }

    fn betas(&self) -> Vec<url::Url> {
        self.beta_clients.keys().cloned().collect()
    }

    fn clowder_node_id(&self) -> NodeId {
        NodeId::new(self.clowder_id, self.network)
    }

    async fn migrate_pockets_substitute(
        &mut self,
        substitute: Arc<dyn ClowderMintConnector>,
    ) -> Result<url::Url> {
        let substitute_clowder_id = substitute.get_clowder_id().await?;
        let debit_proofs = self.debit.delete_proofs().await?;

        // Exchange debit
        let mut exchanged_proofs = Vec::new();

        tracing::info!("Exchanging proofs offline");
        for (keyset_id, proofs) in debit_proofs.into_iter() {
            tracing::info!(
                "Exchanging {} proofs for keyset: {}",
                proofs.len(),
                keyset_id
            );
            for proof in proofs {
                let proof_y = proof.y();
                let proof_amount = proof.amount;
                match self
                    .offline_exchange(substitute.as_ref(), vec![proof], substitute_clowder_id)
                    .await
                {
                    Ok(exchanged) => {
                        exchanged_proofs.extend(exchanged);
                    }
                    Err(e) => {
                        tracing::error!(
                            "Could not exchange proof {proof_y:?} with amount {proof_amount} for keyset {keyset_id} during pocket migration: {e}",
                        );
                    }
                }
            }
        }

        self.client = substitute;
        self.clowder_id = self.client.get_clowder_id().await?;
        let mut beta_clients = HashMap::<url::Url, Arc<dyn ClowderMintConnector>>::new();

        for beta in self.client.as_ref().get_clowder_betas().await? {
            let beta_client = (self.client_factory)(beta.url.clone());
            beta_clients.insert(beta.url, beta_client);
        }
        self.beta_clients = beta_clients;

        let beta_provider = Arc::new(RandomBetaProvider::new(
            self.beta_clients.values().cloned().collect(),
            self.clowder_id,
        )?);

        self.debit.set_beta_provider(beta_provider);

        // Swap intermint exchanged proofs
        tracing::info!("Swapping exchanged proofs");
        let keysets_info = self.get_wallet_mint_keyset_infos().await?;
        self.debit
            .receive_proofs(
                self.client.clone(),
                &keysets_info,
                exchanged_proofs,
                self.swap_config(),
            )
            .await?;
        let balance = self.debit.balance(&keysets_info).await?;

        tracing::info!("Migration successful balance: {:?}", balance);

        Ok(self.client.mint_url().to_owned())
    }

    async fn prepare_pay_by_token(
        &self,
        amount: Amount,
        unit: CurrencyUnit,
        description: Option<String>,
    ) -> Result<PaymentSummary> {
        if unit != self.debit.unit() {
            return Err(Error::InvalidCurrencyUnit(unit.to_string()));
        }
        let infos = self.get_wallet_mint_keyset_infos().await?;

        let s_summary = self.debit.prepare_send(amount, &infos).await?;
        let summary = PaymentSummary::from(s_summary);
        let pref = PayReference {
            request_id: summary.request_id,
            unit: summary.unit.clone(),
            fees: summary.fees,
            ptype: WalletPaymentType::Token,
            memo: description,
        };
        *self.current_payment.lock().await = Some(pref);
        Ok(summary)
    }

    async fn prepare_pay_to_contact(
        &self,
        contact_id: Uuid,
        amount: Amount,
        unit: CurrencyUnit,
        description: Option<String>,
    ) -> Result<PaymentSummary> {
        if unit != self.debit.unit() {
            return Err(Error::InvalidCurrencyUnit(unit.to_string()));
        }

        let Some(contact) = self.contact_repo.get_contact(contact_id).await? else {
            return Err(Error::ContactNotFound(contact_id.to_string()));
        };
        if contact.node_id.is_none() {
            return Err(Error::ContactMustHaveNodeId(contact.id.to_string()));
        }
        self.refresh_contact_relays(&contact.id).await;

        let infos = self.get_wallet_mint_keyset_infos().await?;

        let s_summary = self.debit.prepare_send(amount, &infos).await?;
        let mut summary = PaymentSummary::from(s_summary);
        summary.ptype = PaymentType::Contact;
        let pref = PayReference {
            request_id: summary.request_id,
            unit: summary.unit.clone(),
            fees: summary.fees,
            ptype: WalletPaymentType::Contact {
                contact_id,
                payment_request_id: None,
            },
            memo: description,
        };
        *self.current_payment.lock().await = Some(pref);
        Ok(summary)
    }

    // * Check if our alpha is offline
    // * If it is, determine the substitute
    // * Get proofs for the given amount (including the swap proof), mark them as pendingspent
    // * Do an offline-exchange from our alpha to the substitute (for all the fetched proofs)
    // * Swap the substitute proofs against the substitute beta, to the target amount
    //   * Change is persisted as foreign mint proofs and attempted to be reclaimed regularly
    // * Create Token from swapped target proofs and return Token
    async fn offline_pay_by_token(
        &self,
        request_id: Uuid,
        unit: CurrencyUnit,
        fees: TransactionFees,
        memo: Option<String>,
        now: u64,
    ) -> Result<(Uuid, Option<Token>)> {
        tracing::warn!(
            "Pay by Token: Wallet mint is offline - find substitute and attempt offline exchange for tokens"
        );
        if unit != self.debit.unit() {
            return Err(Error::InvalidCurrencyUnit(unit.to_string()));
        }
        if let Some(substitute) = self.mint_substitute().await? {
            tracing::info!("Substitute found: {}", substitute.to_string());
            // Create substitute client
            let substitute_client = self
                .beta_clients
                .get(&substitute)
                .ok_or(Error::BetaNotFound(substitute.to_string()))?;
            let substitute_clowder_id = substitute_client.get_clowder_id().await?;

            // Create beta provider for substitute to do attestation
            let mut beta_clients = HashMap::<url::Url, Arc<dyn ClowderMintConnector>>::new();

            for beta in substitute_client.as_ref().get_clowder_betas().await? {
                let beta_client = (self.client_factory)(beta.url.clone());
                beta_clients.insert(beta.url, beta_client);
            }

            let beta_provider = RandomBetaProvider::new(
                beta_clients.values().cloned().collect(),
                substitute_clowder_id,
            )?;

            // Get keyset infos from substitute
            // Get local proofs
            tracing::debug!("Offline Pay by Token: Get Local Proofs");
            let (send_amount, local_proofs) = self
                .debit
                .return_proofs_to_send_for_offline_payment(request_id)
                .await?;
            // TODO: just for demo - remove afterwards
            tracing::warn!(
                "Offline Pay by Token - Local Token: {}",
                Token::BitcrV5(
                    BitcrTokenV5::new(
                        self.clowder_node_id(),
                        self.debit.unit(),
                        local_proofs.values().map(|p| p.to_owned().into()).collect()
                    )
                    .with_mint_url(to_mint_url(self.client.mint_url()).to_string())
                )
            );
            tracing::debug!("Offline Pay by Token: Offline Exchange");
            // Do offline exchange
            let substitute_proofs = self
                .offline_exchange(
                    substitute_client.as_ref(),
                    local_proofs.into_values().collect(),
                    substitute_clowder_id,
                )
                .await?;

            // Fetch keyset infos
            let keysets_info: HashMap<cashu::Id, cashu::KeySetInfo> = substitute_client
                .get_mint_keysets()
                .await?
                .into_iter()
                .map(|k| (k.id, k))
                .collect();

            let kids: HashSet<cashu::Id> = substitute_proofs.iter().map(|p| p.keyset_id).collect();
            let mut keysets: HashMap<cashu::Id, KeySet> = HashMap::new();
            for kid in kids.iter() {
                let keyset = substitute_client.get_mint_keyset(*kid).await?;
                keysets.insert(*kid, keyset);
            }
            tracing::debug!("Offline Pay by Token: Swap to unlocked substitute proofs to target.");
            // create swap config for substitute
            let swap_config = SwapConfig {
                expiry: self.swap_expiry,
                alpha_pk: substitute_clowder_id,
            };
            // Swap to unlocked substitute proofs to target
            let unlocked_sending_proofs = self
                .debit
                .swap_to_unlocked_substitute_proofs(
                    substitute_proofs,
                    &keysets_info,
                    keysets,
                    substitute_client.clone(),
                    substitute_clowder_id,
                    beta_provider,
                    send_amount,
                    swap_config,
                )
                .await?;

            // Create Token
            let (ys, proofs): (Vec<cashu::PublicKey>, Vec<cashu::Proof>) = unlocked_sending_proofs
                .into_iter()
                .map(|proof| (proof.y().expect("Hash to curve should not fail"), proof))
                .unzip();
            tracing::debug!("Offline Pay by Token: Create Token");
            let substitute_clowder_node_id = NodeId::new(substitute_clowder_id, self.network());
            let amount = proofs.total_amount()?;
            let mut token = BitcrTokenV5::new(
                substitute_clowder_node_id,
                self.debit.unit(),
                proofs.into_iter().map(|p| p.into()).collect(),
            )
            .with_mint_url(to_mint_url(&substitute.clone()).to_string());
            if let Some(ref m) = memo {
                token = token.with_memo(m.to_string());
            }

            // Create Transaction
            let partial_tx = Transaction {
                id: Uuid::new_v4(),
                mint_url: to_mint_url(&substitute),
                fees,
                direction: TransactionDirection::Outgoing,
                memo,
                tstamp: now,
                unit: unit.clone(),
                ys,
                status: TransactionStatus::Pending,
                payment_type: PaymentType::Token,
                amount,
                quote_id: None,
                payment_request_id: None,
                nostr_event_id: None,
                btc_tx_id: None,
                contact_node_id: None,
                linked_txs: vec![],
            };
            let tx_id = self.tx_repo.store_tx(partial_tx).await?;
            Ok((tx_id, Some(Token::BitcrV5(token))))
        } else {
            Err(Error::NoSubstitute)
        }
    }

    async fn is_nostr_connected(&self) -> bool {
        self.nostr_transport.has_connected_relays().await
            && *self.nostr_consumer_running.lock().await
    }

    async fn fetch_nostr_relays(
        &self,
        npub: nostr::PublicKey,
        relays: Vec<RelayUrl>,
    ) -> Result<Vec<RelayUrl>> {
        let res = self.nostr_transport.fetch_relay_list(npub, relays).await?;
        Ok(res)
    }

    async fn delete(&self) -> Result<()> {
        // shut down nostr client
        self.nostr_transport.shutdown().await;
        // shut down nostr consumer
        self.nostr_shutdown.cancel();
        // delete debit pocket
        if let Err(e) = self.debit.delete().await {
            tracing::error!("Error deleting pocket for wallet {}: {e}", self.id())
        }

        // delete transaction tables
        if let Err(e) = self.tx_repo.delete_repo().await {
            tracing::error!(
                "Error deleting transaction DB for wallet {}: {e}",
                self.id()
            )
        }

        // delete nostr tables
        if let Err(e) = self.nostr_repo.delete_repo().await {
            tracing::error!("Error deleting nostr DB for wallet {}: {e}", self.id())
        }

        // delete contact tables
        if let Err(e) = self.contact_repo.delete_repo().await {
            tracing::error!("Error deleting contact DB for wallet {}: {e}", self.id())
        }

        // delete pending payment request tables
        if let Err(e) = self.payment_request_repo.delete_repo().await {
            tracing::error!(
                "Error deleting pending payment request DB for wallet {}: {e}",
                self.id()
            )
        }

        Ok(())
    }

    async fn create_shareable_remote_payment_request(
        &self,
        amount: Amount,
        unit: CurrencyUnit,
        description: Option<String>,
    ) -> Result<String> {
        let payload = ContactPaymentRequestPayload::new(
            self.node_id(),
            amount,
            unit.clone(),
            description.clone(),
            None,
            to_mint_url(self.client.mint_url()),
        );
        let event: EventEnvelope =
            bcr_wallet_core::event::Event::new_contact_payment_request(payload).try_into()?;
        let encoded_payload = base58::encode(&borsh::to_vec(&event)?);
        Ok(encoded_payload)
    }

    async fn prepare_pay_shared_payment_request(
        &self,
        payment_req: String,
    ) -> Result<PaymentSummary> {
        let infos = self.get_wallet_mint_keyset_infos().await?;

        if let Ok(decoded_event) = base58::decode(&payment_req)
            && let Ok(deserialized_event) = borsh::from_slice::<EventEnvelope>(&decoded_event)
        {
            match deserialized_event.event_type {
                bcr_wallet_core::event::EventType::ContactPaymentRequest => {
                    if let Ok(deserialized_payload) =
                        borsh::from_slice::<ContactPaymentRequestPayload>(&deserialized_event.data)
                    {
                        if deserialized_payload.unit != self.debit.unit() {
                            return Err(Error::InvalidCurrencyUnit(
                                deserialized_payload.unit.to_string(),
                            ));
                        }
                        if deserialized_payload.sender.network() != self.network() {
                            return Err(Error::InvalidNetwork(
                                self.network(),
                                deserialized_payload.sender.network(),
                            ));
                        }
                        let s_summary = self
                            .debit
                            .prepare_send(deserialized_payload.amount, &infos)
                            .await?;
                        let mut summary = PaymentSummary::from(s_summary);
                        summary.ptype = PaymentType::Contact;
                        let pref = PayReference {
                            request_id: summary.request_id,
                            unit: summary.unit.clone(),
                            fees: summary.fees,
                            ptype: WalletPaymentType::SharedPaymentRequest {
                                node_id: deserialized_payload.sender,
                            },
                            memo: deserialized_payload.memo,
                        };
                        *self.current_payment.lock().await = Some(pref);
                        Ok(summary)
                    } else {
                        Err(Error::UnknownPaymentRequest(payment_req))
                    }
                }
                _ => Err(Error::UnknownPaymentRequest(payment_req)),
            }
        } else {
            Err(Error::UnknownPaymentRequest(payment_req))
        }
    }

    async fn request_payment_from_contact(
        &self,
        contact_id: Uuid,
        amount: Amount,
        unit: CurrencyUnit,
        description: Option<String>,
        deadline: Option<u64>,
    ) -> Result<Uuid> {
        self.refresh_contact_relays(&contact_id).await;
        let Ok(Some(contact)) = self.contact_repo.get_contact(contact_id).await else {
            return Err(Error::ContactNotFound(contact_id.to_string()));
        };
        let Some(ref node_id) = contact.node_id else {
            return Err(Error::ContactMustHaveNodeId(contact.id.to_string()));
        };
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
        let target = self.nostr_transport.nip19_for_contact(&contact).await?;
        let Some(target) = target else {
            return Err(Error::ContactMustHaveNodeId(contact.id.to_string()));
        };
        match self
            .nostr_transport
            .send_private_msg(target.clone(), payload.clone())
            .await
        {
            Ok(event_id) => {
                tracing::info!(
                    "Sent contact payment request {} with nostr event_id {event_id}",
                    payment_req_id
                );
            }
            Err(e) => {
                tracing::error!("Failed to send contact payment request, queuing for retry: {e}");
                match e {
                    bcr_wallet_transport::error::Error::NostrSendPrivateMsg(_) => {
                        self.nostr_transport
                            .queue_retry_message(Some(target), payload)
                            .await?;
                    }
                    e => return Err(e.into()),
                }
            }
        };
        let outgoing_payment_request = PaymentRequest {
            id: payment_req_id,
            node_id: node_id.to_owned(),
            amount,
            unit,
            description,
            deadline,
            created_at,
            state: PaymentRequestState::Pending,
            direction: PaymentRequestDirection::Outgoing,
        };

        self.payment_request_repo
            .add_payment_request(outgoing_payment_request)
            .await?;
        Ok(payment_req_id)
    }

    async fn subscribe_to_payment_requests(
        &self,
        cancel_token: CancellationToken,
        item_callback: PendingPaymentSubscriptionCallback,
    ) -> Result<()> {
        tracing::debug!("Subscribing to payment requests from Nostr...");
        let mut nostr_receiver = self.nostr_event_channel.subscribe();
        loop {
            tokio::select! {
                _ = cancel_token.cancelled() => {
                    tracing::info!("subscribe_to_payment_requests cancelled");
                    return Ok(());
                },
                evt = nostr_receiver.recv() => {
                    let received_evt = match evt {
                        Ok(e) => e,
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            tracing::warn!("subscribe_to_payment_requests channel lagged behind");
                            continue;
                        },
                        Err(broadcast::error::RecvError::Closed) => {
                            tracing::warn!("subscribe_to_payment_requests channel closed");
                            return Ok(());
                        },
                    };

                    let NostrWalletEvent::ContactPaymentRequest { event_id, payload, sender } = received_evt else {
                        continue;
                    };
                    tracing::info!("Received contact payment request {} from {sender}, event_id: {event_id}", payload.id);
                    let pending_incoming_payment_request: PaymentRequest = payload.into();
                    let payment_request_id = pending_incoming_payment_request.id;
                    match self.payment_request_repo.add_payment_request(pending_incoming_payment_request).await {
                        Ok(_) => {
                            item_callback(payment_request_id);
                        },
                        Err(bcr_wallet_persistence::error::Error::PaymentRequestAlreadyExists(_)) => {
                            // already had it - either sent again, or already processed - sending it either way and the caller can choose to ignore it
                            item_callback(payment_request_id);
                        },
                        Err(e) => {
                            tracing::error!("Could not store payment request: {e}");
                        }
                    };
                }
            }
        }
    }

    async fn list_payment_requests(
        &self,
        direction: PaymentRequestDirection,
        states: Vec<PaymentRequestState>,
    ) -> Result<Vec<PaymentRequest>> {
        let res = self
            .payment_request_repo
            .list_payment_requests(direction, &states)
            .await?;
        Ok(res)
    }

    async fn get_payment_request(&self, payment_req_id: Uuid) -> Result<Option<PaymentRequest>> {
        let res = self
            .payment_request_repo
            .get_payment_request(payment_req_id)
            .await?;
        Ok(res)
    }

    async fn add_payment_request(
        &self,
        pending_incoming_payment_request: PaymentRequest,
    ) -> Result<()> {
        self.payment_request_repo
            .add_payment_request(pending_incoming_payment_request)
            .await?;
        Ok(())
    }

    async fn prepare_pay_payment_request(&self, payment_req_id: Uuid) -> Result<PaymentSummary> {
        let Some(req) = self
            .payment_request_repo
            .get_payment_request(payment_req_id)
            .await?
        else {
            return Err(Error::PaymentRequestNotFound(payment_req_id));
        };
        // can only pay incoming payment requests
        if req.direction == PaymentRequestDirection::Outgoing {
            return Err(Error::PaymentRequestInWrongState(payment_req_id));
        }
        // can only pay pending payment requests
        if req.state != PaymentRequestState::Pending {
            return Err(Error::PaymentRequestInWrongState(payment_req_id));
        }
        // has to be added to contacts to pay the payment request
        let contacts_by_node_id = self
            .contact_repo
            .get_contacts_by_node_id(req.node_id.clone())
            .await?;
        let Some(first_node_id_contact) = contacts_by_node_id.first() else {
            return Err(Error::ContactNotFound(req.node_id.to_string()));
        };
        let infos = self.get_wallet_mint_keyset_infos().await?;

        let s_summary = self.debit.prepare_send(req.amount, &infos).await?;
        let mut summary = PaymentSummary::from(s_summary);
        summary.ptype = PaymentType::Contact;
        let pref = PayReference {
            request_id: summary.request_id,
            unit: summary.unit.clone(),
            fees: summary.fees,
            ptype: WalletPaymentType::Contact {
                contact_id: first_node_id_contact.id,
                payment_request_id: Some(req.id),
            },
            memo: req.description,
        };
        *self.current_payment.lock().await = Some(pref);
        Ok(summary)
    }

    async fn reject_payment_request(&self, payment_req_id: Uuid) -> Result<()> {
        let Some(req) = self
            .payment_request_repo
            .get_payment_request(payment_req_id)
            .await?
        else {
            return Err(Error::PaymentRequestNotFound(payment_req_id));
        };
        if req.direction != PaymentRequestDirection::Incoming {
            return Err(Error::PaymentRequestInWrongState(payment_req_id));
        }
        if req.state != PaymentRequestState::Pending {
            return Err(Error::PaymentRequestInWrongState(payment_req_id));
        }
        self.payment_request_repo
            .set_payment_request_state(payment_req_id, PaymentRequestState::Rejected)
            .await?;
        Ok(())
    }

    async fn cancel_payment_request(&self, payment_req_id: Uuid) -> Result<()> {
        let Some(req) = self
            .payment_request_repo
            .get_payment_request(payment_req_id)
            .await?
        else {
            return Err(Error::PaymentRequestNotFound(payment_req_id));
        };
        if req.direction != PaymentRequestDirection::Outgoing {
            return Err(Error::PaymentRequestInWrongState(payment_req_id));
        }
        if req.state != PaymentRequestState::Pending {
            return Err(Error::PaymentRequestInWrongState(payment_req_id));
        }
        self.payment_request_repo
            .set_payment_request_state(payment_req_id, PaymentRequestState::Canceled)
            .await?;
        Ok(())
    }

    async fn mark_payment_request_as_paid(&self, payment_req_id: Uuid, tx_id: Uuid) -> Result<()> {
        if self
            .payment_request_repo
            .get_payment_request(payment_req_id)
            .await?
            .is_none()
        {
            return Err(Error::PaymentRequestNotFound(payment_req_id));
        }
        // paid overrides rejected/cancelled, if it's set
        self.payment_request_repo
            .set_payment_request_state(payment_req_id, PaymentRequestState::Paid { tx_id })
            .await?;
        Ok(())
    }
}
