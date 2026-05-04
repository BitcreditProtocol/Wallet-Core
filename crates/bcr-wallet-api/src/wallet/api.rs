use crate::{
    ClowderMintConnector,
    error::{Error, Result},
    pocket::debit::{MeltProtestResult, ProtestResult},
    types::{
        MintSummary, PAYMENT_TYPE_METADATA_KEY, PaymentSummary, TRANSACTION_STATUS_METADATA_KEY,
        WalletConfig,
    },
    wallet::types::{PayReference, WalletPaymentType, WalletProtestResult},
};
use async_trait::async_trait;
use bcr_common::{
    cashu::{self, Amount, CurrencyUnit, MintUrl, ProofsMethods, nut00 as cdk00, nut18 as cdk18},
    cdk::{
        StreamExt,
        wallet::types::{Transaction, TransactionDirection, TransactionId},
    },
    wallet::Token,
    wire::clowder::{self as wire_clowder},
};
use bcr_wallet_core::{
    SendSync,
    types::{
        BTC_ALPHA_TX_ID_TYPE_METADATA_KEY, BTC_BETA_TX_ID_TYPE_METADATA_KEY, PaymentResultCallback,
        PaymentType, TransactionStatus,
    },
};
use bitcoin::secp256k1;
use futures::stream::FuturesUnordered;
use nostr_sdk::RelayPoolNotification;
use std::{collections::HashMap, str::FromStr, sync::Arc};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait WalletApi: SendSync {
    fn config(&self) -> Result<WalletConfig>;
    fn name(&self) -> String;
    fn id(&self) -> String;
    fn mint_url(&self) -> Result<MintUrl>;
    fn betas(&self) -> Vec<MintUrl>;
    #[allow(dead_code)]
    fn clowder_id(&self) -> secp256k1::PublicKey;
    fn mint_urls(&self) -> Result<Vec<MintUrl>>;
    async fn prepare_melt(
        &self,
        amount: bitcoin::Amount,
        address: bitcoin::Address<bitcoin::address::NetworkUnchecked>,
        description: Option<String>,
    ) -> Result<PaymentSummary>;
    async fn prepare_pay(&self, input: String) -> Result<PaymentSummary>;
    async fn prepare_payment_request(
        &self,
        amount: Amount,
        unit: CurrencyUnit,
        description: Option<String>,
        nostr_transport: cdk18::Transport,
    ) -> Result<cdk18::PaymentRequest>;
    async fn check_received_payment(
        &self,
        max_wait: core::time::Duration,
        p_id: Uuid,
        nostr_cl: &nostr_sdk::Client,
        cancel_token: CancellationToken,
        result_callback: PaymentResultCallback,
    ) -> Result<()>;
    async fn is_wallet_mint_rabid(&self) -> Result<bool>;
    async fn is_wallet_mint_offline(&self) -> Result<bool>;
    async fn mint_substitute(&self) -> Result<Option<MintUrl>>;
    async fn pay(
        &self,
        p_id: Uuid,
        nostr_cl: &nostr_sdk::Client,
        http_cl: &reqwest::Client,
        tstamp: u64,
    ) -> Result<(TransactionId, Option<Token>)>;
    async fn mint(&self, amount: bitcoin::Amount) -> Result<MintSummary>;
    async fn check_pending_mints(&self) -> Result<Vec<TransactionId>>;
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
    ) -> Result<MintUrl>;
    async fn receive_proofs(
        &self,
        proofs: Vec<cdk00::Proof>,
        unit: CurrencyUnit,
        mint: MintUrl,
        tstamp: u64,
        memo: Option<String>,
        metadata: HashMap<String, String>,
    ) -> Result<TransactionId>;
    async fn prepare_pay_by_token(
        &self,
        amount: Amount,
        unit: CurrencyUnit,
        description: Option<String>,
    ) -> Result<PaymentSummary>;
    async fn offline_pay_by_token(
        &self,
        request_id: Uuid,
        unit: CurrencyUnit,
        fees: Amount,
        memo: Option<String>,
        now: u64,
    ) -> Result<(TransactionId, Option<Token>)>;
    async fn cleanup_local_proofs(&self) -> Result<()>;
}

#[async_trait]
impl WalletApi for super::Wallet {
    fn config(&self) -> Result<WalletConfig> {
        Ok(WalletConfig {
            wallet_id: self.id.clone(),
            name: self.name.clone(),
            network: self.network,
            debit: self.debit.unit(),
            mint: self.client.mint_url(),
            mint_keyset_infos: self.mint_keyset_infos.clone(),
            clowder_id: self.clowder_id,
            pub_key: self.pub_key,
            betas: self.betas(),
        })
    }

    fn name(&self) -> String {
        self.name.clone()
    }

    fn id(&self) -> String {
        self.id.clone()
    }

    fn mint_url(&self) -> Result<cashu::MintUrl> {
        Ok(self.client.mint_url())
    }

    async fn prepare_melt(
        &self,
        amount: bitcoin::Amount,
        address: bitcoin::Address<bitcoin::address::NetworkUnchecked>,
        description: Option<String>,
    ) -> Result<PaymentSummary> {
        let infos = self.get_wallet_mint_keyset_infos().await?;

        let m_summary = self
            .debit
            .prepare_onchain_melt(
                address.assume_checked().to_string(),
                amount.to_sat(),
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

    async fn prepare_pay(&self, input: String) -> Result<PaymentSummary> {
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

    async fn prepare_payment_request(
        &self,
        amount: Amount,
        unit: CurrencyUnit,
        description: Option<String>,
        nostr_transport: cdk18::Transport,
    ) -> Result<cdk18::PaymentRequest> {
        let mints = self.mint_urls()?;
        let request = cdk18::PaymentRequest {
            payment_id: Some(Uuid::new_v4().to_string()),
            amount: Some(amount),
            mints: Some(mints),
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
        nostr_cl: &nostr_sdk::Client,
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

        let start = tokio::time::Instant::now();
        let signer = nostr_cl.signer().await?;

        tracing::debug!("Subscribing to events from Nostr...");
        let mut events = nostr_cl.notifications();
        let deadline = start + max_wait;

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
                evt = events.recv() => {
                    let Ok(received_evt) = evt else {
                        tracing::warn!("check_received_payment channel closed: {p_id}");
                        result_callback(None);
                        return Ok(());
                    };
                    if let RelayPoolNotification::Event { event, .. } = received_evt {
                    match self
                        .handle_event(*event, signer.clone(), p_id, req.amount.unwrap_or_default())
                        .await
                        {
                            Ok(None) => {
                                // do nothing
                                continue;
                            }
                            Ok(Some(tx_id)) => {
                                result_callback(Some(tx_id));
                                return Ok(());
                            }
                            Err(e) => {
                                tracing::error!("Error while handling Nostr event: {e}");
                                continue;
                            }
                        };
                    }
                }
            }
        }
    }

    async fn pay(
        &self,
        p_id: Uuid,
        nostr_cl: &nostr_sdk::Client,
        http_cl: &reqwest::Client,
        now: u64,
    ) -> Result<(TransactionId, Option<Token>)> {
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
                let mut metadata = HashMap::default();
                metadata.insert(
                    PAYMENT_TYPE_METADATA_KEY.to_owned(),
                    PaymentType::Cdk18.to_string(),
                );
                metadata.insert(
                    TRANSACTION_STATUS_METADATA_KEY.to_owned(),
                    TransactionStatus::Pending.to_string(),
                );

                let partial_tx = Transaction {
                    mint_url: self.client.mint_url(),
                    fee: fees,
                    direction: TransactionDirection::Outgoing,
                    memo,
                    timestamp: now,
                    unit: unit.clone(),
                    ys,
                    amount,
                    // payments might need to fill some extra metadata later
                    metadata,
                    quote_id: None,
                };
                let tx_id = self
                    .pay_nut18(proofs, nostr_cl, http_cl, transport, id, partial_tx)
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
                    (
                        p.clone(),
                        Token::new_cashu(
                            self.client.mint_url(),
                            p.into_values().collect(),
                            memo.clone(),
                            self.debit.unit(),
                        ),
                    )
                };
                let (ys, proofs): (Vec<cashu::PublicKey>, Vec<cashu::Proof>) =
                    proofs.into_iter().unzip();
                let amount = proofs.total_amount()?;
                let mut metadata = HashMap::default();
                metadata.insert(
                    PAYMENT_TYPE_METADATA_KEY.to_owned(),
                    PaymentType::Token.to_string(),
                );
                metadata.insert(
                    TRANSACTION_STATUS_METADATA_KEY.to_owned(),
                    TransactionStatus::Pending.to_string(),
                );

                let partial_tx = Transaction {
                    mint_url: self.client.mint_url(),
                    fee: fees,
                    direction: TransactionDirection::Outgoing,
                    memo,
                    timestamp: now,
                    unit: unit.clone(),
                    ys,
                    amount,
                    metadata,
                    quote_id: None,
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
                let amount = proofs.total_amount()?;
                let mut metadata = HashMap::default();
                metadata.insert(
                    PAYMENT_TYPE_METADATA_KEY.to_owned(),
                    PaymentType::OnChain.to_string(),
                );
                metadata.insert(
                    TRANSACTION_STATUS_METADATA_KEY.to_owned(),
                    TransactionStatus::Settled.to_string(),
                );
                if let Some(alpha_tx_id) = btc_tx_id.alpha_txid {
                    metadata.insert(
                        BTC_ALPHA_TX_ID_TYPE_METADATA_KEY.to_owned(),
                        alpha_tx_id.to_string(),
                    );
                }
                if let Some(beta_tx_id) = btc_tx_id.beta_txid {
                    metadata.insert(
                        BTC_BETA_TX_ID_TYPE_METADATA_KEY.to_owned(),
                        beta_tx_id.to_string(),
                    );
                }

                let partial_tx = Transaction {
                    mint_url: self.client.mint_url(),
                    fee: fees,
                    direction: TransactionDirection::Outgoing,
                    memo,
                    timestamp: now,
                    unit: unit.clone(),
                    ys,
                    amount,
                    metadata,
                    quote_id: None,
                };
                let tx_id = self.tx_repo.store_tx(partial_tx).await?;
                Ok((tx_id, None))
            }
        }
    }

    async fn mint(&self, amount: bitcoin::Amount) -> Result<MintSummary> {
        let keysets_info = self.get_wallet_mint_keyset_infos().await?;
        let summary = self
            .debit
            .mint_onchain(amount, &keysets_info, self.client.clone(), self.clowder_id)
            .await?;
        Ok(summary)
    }

    async fn check_pending_mints(&self) -> Result<Vec<TransactionId>> {
        let mut res = Vec::new();
        let keysets_info = self.get_wallet_mint_keyset_infos().await?;
        let now = chrono::Utc::now();
        let pending_mints_result = self
            .debit
            .check_pending_mints(
                &keysets_info,
                self.client.clone(),
                now.timestamp() as u64,
                self.swap_config(),
                self.clowder_id,
            )
            .await?;

        for (qid, (amount, ys)) in pending_mints_result {
            let mut metadata = HashMap::default();
            metadata.insert(
                PAYMENT_TYPE_METADATA_KEY.to_owned(),
                PaymentType::OnChain.to_string(),
            );
            metadata.insert(
                TRANSACTION_STATUS_METADATA_KEY.to_owned(),
                TransactionStatus::Settled.to_string(),
            );

            let tx = Transaction {
                mint_url: self.client.mint_url(),
                fee: cashu::Amount::ZERO,
                direction: TransactionDirection::Incoming,
                memo: None,
                timestamp: now.timestamp() as u64,
                unit: self.debit_unit(),
                ys,
                amount,
                metadata,
                quote_id: Some(qid.to_string()),
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
        let keysets_info = self.get_wallet_mint_keyset_infos().await?;
        let ProtestResult { status, result } = self
            .debit
            .protest_mint(
                quote_id,
                &keysets_info,
                self.client.clone(),
                self.swap_config(),
                self.clowder_id,
            )
            .await?;

        if let Some((amount, ref ys)) = result {
            let now = chrono::Utc::now();
            let mut metadata = HashMap::default();
            metadata.insert(
                PAYMENT_TYPE_METADATA_KEY.to_owned(),
                PaymentType::OnChain.to_string(),
            );
            metadata.insert(
                TRANSACTION_STATUS_METADATA_KEY.to_owned(),
                TransactionStatus::Settled.to_string(),
            );

            let tx = Transaction {
                mint_url: self.client.mint_url(),
                fee: cashu::Amount::ZERO,
                direction: TransactionDirection::Incoming,
                memo: Some("Mint protest resolved".to_string()),
                timestamp: now.timestamp() as u64,
                unit: self.debit_unit(),
                ys: ys.clone(),
                amount,
                metadata,
                quote_id: Some(quote_id.to_string()),
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

        // Pick a beta client
        let beta_url = self.betas().into_iter().next().ok_or(Error::NoBetas)?;
        let beta_client = self
            .beta_clients
            .get(&beta_url)
            .ok_or(Error::BetaNotFound(beta_url))?
            .clone();

        let ProtestResult { status, result } = self
            .debit
            .protest_swap(
                commitment_sig,
                &keysets_info,
                self.client.clone(),
                beta_client,
                self.clowder_id,
                swap_config,
            )
            .await?;

        if let Some((amount, ref ys)) = result {
            let now = chrono::Utc::now();
            let mut metadata = HashMap::default();
            metadata.insert(
                PAYMENT_TYPE_METADATA_KEY.to_owned(),
                PaymentType::Swap.to_string(),
            );
            metadata.insert(
                TRANSACTION_STATUS_METADATA_KEY.to_owned(),
                TransactionStatus::Settled.to_string(),
            );

            let tx = Transaction {
                mint_url: self.client.mint_url(),
                fee: cashu::Amount::ZERO,
                direction: TransactionDirection::Incoming,
                memo: Some("Swap protest resolved".to_string()),
                timestamp: now.timestamp() as u64,
                unit: self.debit_unit(),
                ys: ys.clone(),
                amount,
                metadata,
                quote_id: None,
            };
            self.tx_repo.store_tx(tx).await?;
        }

        Ok(WalletProtestResult { status, result })
    }

    async fn protest_melt(&self, quote_id: Uuid) -> Result<WalletProtestResult> {
        let beta_url = self.betas().into_iter().next().ok_or(Error::NoBetas)?;
        let beta_client = self
            .beta_clients
            .get(&beta_url)
            .ok_or(Error::BetaNotFound(beta_url))?
            .clone();

        let MeltProtestResult {
            base: ProtestResult { status, result },
            txid,
        } = self
            .debit
            .protest_melt(quote_id, beta_client, self.clowder_id)
            .await?;

        if let Some((amount, ref ys)) = result {
            let now = chrono::Utc::now();
            let mut metadata = HashMap::default();
            metadata.insert(
                PAYMENT_TYPE_METADATA_KEY.to_owned(),
                PaymentType::OnChain.to_string(),
            );
            metadata.insert(
                TRANSACTION_STATUS_METADATA_KEY.to_owned(),
                TransactionStatus::Settled.to_string(),
            );
            if let Some(ref melt_tx) = txid {
                if let Some(alpha_tx_id) = melt_tx.alpha_txid {
                    metadata.insert(
                        BTC_ALPHA_TX_ID_TYPE_METADATA_KEY.to_owned(),
                        alpha_tx_id.to_string(),
                    );
                }
                if let Some(beta_tx_id) = melt_tx.beta_txid {
                    metadata.insert(
                        BTC_BETA_TX_ID_TYPE_METADATA_KEY.to_owned(),
                        beta_tx_id.to_string(),
                    );
                }
            }

            let tx = Transaction {
                mint_url: self.client.mint_url(),
                fee: cashu::Amount::ZERO,
                direction: TransactionDirection::Outgoing,
                memo: Some("Melt protest resolved".to_string()),
                timestamp: now.timestamp() as u64,
                unit: self.debit_unit(),
                ys: ys.clone(),
                amount,
                metadata,
                quote_id: Some(quote_id.to_string()),
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
        mint: MintUrl,
        tstamp: u64,
        memo: Option<String>,
        metadata: HashMap<String, String>,
    ) -> Result<TransactionId> {
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
            metadata,
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
                .ok_or(Error::BetaNotFound(beta))?;

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
                .ok_or(Error::BetaNotFound(beta))?;

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

    async fn mint_substitute(&self) -> Result<Option<MintUrl>> {
        let mint_id = self.clowder_id;
        let betas_count = self.betas().len();
        let threshold = betas_count / 2;
        let mut futures = FuturesUnordered::new();

        for beta in self.betas() {
            let beta_client = self
                .beta_clients
                .get(&beta)
                .ok_or(Error::BetaNotFound(beta))?;

            futures.push(async move {
                let mint = beta_client.get_alpha_substitute(mint_id).await?.mint;
                Ok::<MintUrl, Error>(mint)
            });
        }

        let mut substitute_counts = HashMap::<MintUrl, usize>::new();

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

    fn mint_urls(&self) -> Result<Vec<cashu::MintUrl>> {
        let mut urls = self.betas();
        urls.push(self.client.mint_url());
        Ok(urls)
    }

    fn betas(&self) -> Vec<cashu::MintUrl> {
        self.beta_clients.keys().cloned().collect()
    }

    fn clowder_id(&self) -> secp256k1::PublicKey {
        self.clowder_id
    }

    async fn migrate_pockets_substitute(
        &mut self,
        substitute: Arc<dyn ClowderMintConnector>,
    ) -> Result<MintUrl> {
        let debit_proofs = self.debit.delete_proofs().await?;

        // Exchange debit
        let mut exchanged_debit = Vec::new();

        tracing::info!("Exchanging debit offline");
        for (_, proofs) in debit_proofs.iter() {
            match self
                .offline_exchange(substitute.as_ref(), proofs.clone())
                .await
            {
                Ok(exchanged) => {
                    exchanged_debit.extend(exchanged);
                }
                Err(e) => {
                    tracing::error!(
                        "Could not exchange {} proofs during pocket migration: {e}",
                        proofs.len()
                    );
                }
            }
        }

        self.client = substitute;
        self.clowder_id = self.client.get_clowder_id().await?;
        let mut beta_clients = HashMap::<cashu::MintUrl, Arc<dyn ClowderMintConnector>>::new();

        for beta in self.client.as_ref().get_clowder_betas().await? {
            let beta_client = (self.client_factory)(beta.clone());
            beta_clients.insert(beta, beta_client);
        }
        self.beta_clients = beta_clients;

        // Swap intermint exchanged proofs
        tracing::info!("Swapping exchanged proofs");
        let keysets_info = self.get_wallet_mint_keyset_infos().await?;
        self.debit
            .receive_proofs(
                self.client.clone(),
                &keysets_info,
                exchanged_debit,
                self.swap_config(),
            )
            .await?;
        let balance = self.debit.balance(&keysets_info).await?;

        tracing::info!("Migration successful balance: {:?}", balance);

        Ok(self.client.mint_url())
    }

    async fn prepare_pay_by_token(
        &self,
        amount: Amount,
        unit: CurrencyUnit,
        description: Option<String>,
    ) -> Result<PaymentSummary> {
        let infos = self.get_wallet_mint_keyset_infos().await?;
        if unit != self.debit.unit() {
            return Err(Error::InvalidCurrencyUnit(unit.to_string()));
        }

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

    // This is a temporary solution for demoing the concept, which has some gaping holes
    // The process is:
    // * Check if our alpha is offline
    // * If it is, determine the substitute
    // * Get proofs for the given amount (including the swap proof), mark them as pendingspent
    // * Do an offline-exchange from our alpha to the substitute (for all the fetched proofs)
    // * Swap the substitute proofs against the substitute beta, to the target amount
    //   * => This means, that overlap from swapping to target is currently lost, since there's no good way to store other-mint-proofs in the Wallet for now
    //   * => In the future, we could persist them in a special storage and, once our alpha is back online, attempt to swap them back
    // * Create Token from swapped target proofs and return Token
    async fn offline_pay_by_token(
        &self,
        request_id: Uuid,
        unit: CurrencyUnit,
        fees: Amount,
        memo: Option<String>,
        now: u64,
    ) -> Result<(TransactionId, Option<Token>)> {
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
                .ok_or(Error::BetaNotFound(substitute.clone()))?;
            // Get keyset infos from substitute
            // Get local proofs
            tracing::debug!("Offline Pay by Token: Get Local Proofs");
            let (send_amount, local_proofs) = self
                .debit
                .return_proofs_to_send_for_offline_payment(request_id)
                .await?;
            tracing::debug!("Offline Pay by Token: Offline Exchange");
            // Do offline exchange
            let substitute_proofs = self
                .offline_exchange(
                    substitute_client.as_ref(),
                    local_proofs.into_values().collect(),
                )
                .await?;

            // Fetch keyset infos
            let keysets_info = substitute_client.get_mint_keysets().await?;
            tracing::debug!("Offline Pay by Token: Swap to unlocked substitute proofs to target.");
            // Swap to unlocked substitute proofs to target
            let unlocked_sending_proofs = self
                .debit
                .swap_to_unlocked_substitute_proofs(
                    substitute_proofs,
                    &keysets_info,
                    substitute_client.clone(),
                    send_amount,
                    self.swap_config(),
                )
                .await?;

            // Create Token
            let (ys, proofs): (Vec<cashu::PublicKey>, Vec<cashu::Proof>) = unlocked_sending_proofs
                .into_iter()
                .map(|proof| (proof.y().expect("Hash to curve should not fail"), proof))
                .unzip();
            tracing::debug!("Offline Pay by Token: Create Token");
            let token = Token::new_cashu(
                substitute.clone(),
                proofs.clone(),
                memo.clone(),
                self.debit.unit(),
            );

            let amount = proofs.total_amount()?;
            let mut metadata = HashMap::default();
            metadata.insert(
                PAYMENT_TYPE_METADATA_KEY.to_owned(),
                PaymentType::Token.to_string(),
            );
            metadata.insert(
                TRANSACTION_STATUS_METADATA_KEY.to_owned(),
                TransactionStatus::Pending.to_string(),
            );

            // Create Transaction
            let partial_tx = Transaction {
                mint_url: substitute,
                fee: fees,
                direction: TransactionDirection::Outgoing,
                memo,
                timestamp: now,
                unit: unit.clone(),
                ys,
                amount,
                metadata,
                quote_id: None,
            };
            let tx_id = self.tx_repo.store_tx(partial_tx).await?;
            Ok((tx_id, Some(token)))
        } else {
            Err(Error::NoSubstitute)
        }
    }

    async fn cleanup_local_proofs(&self) -> Result<()> {
        self.debit.cleanup_local_proofs(self.client.clone()).await?;
        Ok(())
    }
}
