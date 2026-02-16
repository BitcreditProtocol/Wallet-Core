pub mod api;
pub mod types;
pub mod util;

use crate::{
    ClowderMintConnector,
    config::SameMintSafeMode,
    error::{Error, Result},
    pocket::{credit::CreditPocketApi, debit::DebitPocketApi},
    types::{PAYMENT_TYPE_METADATA_KEY, RedemptionSummary, TRANSACTION_STATUS_METADATA_KEY},
    wallet::types::{PayReference, SafeMode, WalletBalance},
};
use bcr_common::{
    cashu::{
        self, Amount, CurrencyUnit, KeySetInfo, MintUrl, PaymentRequest, Proof, ProofsMethods,
        nut18 as cdk18,
    },
    cdk::wallet::types::{Transaction, TransactionDirection, TransactionId},
    wallet::Token,
    wire::clowder::{ConnectedMintResponse, ConnectedMintsResponse},
};
use bcr_wallet_core::types::{PaymentType, TransactionStatus};
use bcr_wallet_persistence::TransactionRepository;
use bitcoin::{
    hashes::{Hash, sha256::Hash as Sha256},
    secp256k1,
};
use nostr::{nips::nip59::UnwrappedGift, signer::NostrSigner};
use nostr_sdk::nips::nip19::{FromBech32, Nip19Profile};
use std::{collections::HashMap, str::FromStr, sync::Arc};
use tokio::sync::Mutex;
use uuid::Uuid;

pub struct Wallet {
    network: bitcoin::Network,
    client: Box<dyn ClowderMintConnector>,
    mint_keyset_infos: Vec<cashu::KeySetInfo>,
    beta_clients: HashMap<cashu::MintUrl, Box<dyn ClowderMintConnector>>,
    tx_repo: Box<dyn TransactionRepository>,
    debit: Box<dyn DebitPocketApi>,
    credit: Box<dyn CreditPocketApi>,
    name: String,
    id: String,
    pub_key: secp256k1::PublicKey,
    current_payment: Mutex<Option<PayReference>>,
    current_payment_request: Mutex<Option<PaymentRequest>>,
    clowder_id: secp256k1::PublicKey,
    client_factory: Box<dyn Fn(cashu::MintUrl) -> Box<dyn ClowderMintConnector> + Send + Sync>,
    safe_mode: SameMintSafeMode,
}

impl Wallet {
    pub async fn new(
        network: bitcoin::Network,
        client: Box<dyn ClowderMintConnector>,
        mint_keyset_infos: Vec<cashu::KeySetInfo>,
        tx_repo: Box<dyn TransactionRepository>,
        (debit, credit): (Box<dyn DebitPocketApi>, Box<dyn CreditPocketApi>),
        name: String,
        id: String,
        pub_key: secp256k1::PublicKey,
        clowder_id: secp256k1::PublicKey,
        beta_clients: HashMap<cashu::MintUrl, Box<dyn ClowderMintConnector>>,
        client_factory: Box<dyn Fn(cashu::MintUrl) -> Box<dyn ClowderMintConnector> + Send + Sync>,
        safe_mode: SameMintSafeMode,
    ) -> Result<Self> {
        Ok(Self {
            network,
            client,
            mint_keyset_infos,
            tx_repo,
            debit,
            credit,
            name,
            id,
            pub_key,
            current_payment: Mutex::new(None),
            current_payment_request: Mutex::new(None),
            beta_clients,
            clowder_id,
            client_factory,
            safe_mode,
        })
    }

    pub fn name(&self) -> String {
        self.name.clone()
    }
    pub fn credit_unit(&self) -> CurrencyUnit {
        self.credit.unit()
    }

    pub async fn list_redemptions(
        &self,
        payment_window: std::time::Duration,
    ) -> Result<Vec<RedemptionSummary>> {
        let keysets_info = self.get_wallet_mint_keyset_infos().await?;
        self.credit
            .list_redemptions(&keysets_info, payment_window)
            .await
    }

    pub async fn list_tx_ids(&self) -> Result<Vec<TransactionId>> {
        let res = self.tx_repo.list_tx_ids().await?;
        Ok(res)
    }

    pub async fn list_txs(&self) -> Result<Vec<Transaction>> {
        let res = self.tx_repo.list_txs().await?;
        Ok(res)
    }

    // Returns (Option<(clowder_path, intermint_alpha_keyset)>, local_alpha_keyset)
    async fn get_clowder_path_and_keysets_info(
        &self,
        mint_url: MintUrl,
    ) -> Result<(
        Option<(ConnectedMintsResponse, Vec<KeySetInfo>)>,
        Vec<KeySetInfo>,
    )> {
        let local_keysets_info = self.get_wallet_mint_keyset_infos().await?;
        if mint_url == self.client.mint_url() {
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
            let substitute_client = if beta_mint == self.client.mint_url() {
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

            // The endpoint only returns active keysets and Clowder/Wildcat don't have fees
            let intermint_alpha_infos: Vec<cashu::KeySetInfo> = alpha_keysets
                .iter()
                .map(|keyset| cashu::KeySetInfo {
                    id: keyset.id,
                    unit: keyset.unit.clone(),
                    active: true,
                    input_fee_ppk: 0,
                    final_expiry: keyset.final_expiry,
                })
                .collect();
            Ok((Some((path, intermint_alpha_infos)), local_keysets_info))
        }
    }

    async fn get_wallet_mint_keyset_infos(&self) -> Result<Vec<KeySetInfo>> {
        Ok(match self.client.get_mint_keysets().await {
            Ok(infos) => infos.keysets,
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
        let debit = self.debit.balance().await?;
        let credit = self.credit.balance().await?;
        Ok(WalletBalance { debit, credit })
    }

    async fn check_nut18_request(
        &self,
        req: &cashu::PaymentRequest,
    ) -> Result<(Amount, CurrencyUnit, cashu::Transport)> {
        if let Some(mints) = &req.mints
            && !mints.contains(&self.client.mint_url())
        {
            return Err(Error::InterMint);
        }
        if req.nut10.is_some() {
            return Err(Error::SpendingConditions);
        }
        let Some(amount) = req.amount else {
            return Err(Error::MissingAmount);
        };
        let unit = if let Some(unit) = &req.unit {
            if *unit != self.debit.unit() && *unit != self.credit.unit() {
                return Err(Error::CurrencyUnitMismatch(self.debit.unit(), unit.clone()));
            }
            unit.clone()
        } else if amount <= self.credit.balance().await? {
            self.credit.unit()
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

    pub async fn redeem_credit(&self) -> Result<Amount> {
        let keysets_info = self.get_wallet_mint_keyset_infos().await?;
        let credit_proofs: Vec<cashu::Proof> = self
            .credit
            .get_redeemable_proofs(&keysets_info, self.client.as_ref())
            .await?;
        let amount = self
            .redeem_credit_proofs(credit_proofs, &keysets_info)
            .await?;
        Ok(amount)
    }

    async fn redeem_credit_proofs(
        &self,
        credit_proofs: Vec<cashu::Proof>,
        keysets_info: &[KeySetInfo],
    ) -> Result<Amount> {
        if credit_proofs.is_empty() {
            Ok(Amount::ZERO)
        } else {
            let (amount, _) = self
                .debit
                .receive_proofs(
                    self.client.as_ref(),
                    keysets_info,
                    credit_proofs,
                    SafeMode::new(self.safe_mode, self.clowder_id),
                )
                .await?;
            Ok(amount)
        }
    }

    pub async fn restore_local_proofs(&self) -> Result<()> {
        let keysets_info = self.get_wallet_mint_keyset_infos().await?;
        let (debit, credit) = futures::join!(
            self.debit
                .restore_local_proofs(&keysets_info, self.client.as_ref()),
            self.credit
                .restore_local_proofs(&keysets_info, self.client.as_ref())
        );
        debit?;
        credit?;
        Ok(())
    }

    pub async fn load_tx(&self, tx_id: TransactionId) -> Result<Transaction> {
        let tx = self.tx_repo.load_tx(tx_id).await?;
        Ok(tx)
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
            .states
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

    pub async fn reclaim_tx(&self, tx_id: TransactionId) -> Result<Amount> {
        let infos = self.get_wallet_mint_keyset_infos().await?;
        self.refresh_tx(tx_id).await?;
        let tx = self.load_tx(tx_id).await?;

        // Only Outgoing and Pending transactions can be reclaimed
        if !util::tx_can_be_refreshed(&tx) {
            return Err(Error::TransactionCantBeReclaimed(tx_id));
        }

        // Reclaim proofs
        let amount = if tx.unit == self.debit.unit() {
            tracing::debug!("Reclaim Debit Transaction {tx_id}");
            self.debit
                .reclaim_proofs(
                    &tx.ys,
                    &infos,
                    self.client.as_ref(),
                    SafeMode::new(self.safe_mode, self.clowder_id),
                )
                .await?
        } else if tx.unit == self.credit.unit() {
            tracing::debug!("Reclaim Credit Transaction {tx_id}");
            let (reclaimed_amount, redeemable_proofs) = self
                .credit
                .reclaim_proofs(
                    &tx.ys,
                    &infos,
                    self.client.as_ref(),
                    SafeMode::new(self.safe_mode, self.clowder_id),
                )
                .await?;

            let redeemed_amount = self.redeem_credit_proofs(redeemable_proofs, &infos).await?;
            tracing::debug!(
                "Reclaimed/Redeemed Credit Transaction {tx_id} - Reclaimed: {reclaimed_amount}, Redeemed: {redeemed_amount}"
            );
            reclaimed_amount + redeemed_amount
        } else {
            return Err(Error::CurrencyUnitMismatch(self.debit.unit(), tx.unit));
        };

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
            if amount != tx.amount {
                tracing::warn!(
                    "Reclaimed amount does not match the transaction amount for {tx_id}: {amount} vs. {}",
                    tx.amount
                );
            }

            // Set reclaimed transaction to canceled
            self.tx_repo
                .update_metadata(
                    tx_id,
                    String::from(TRANSACTION_STATUS_METADATA_KEY),
                    TransactionStatus::Canceled.to_string(),
                )
                .await?;
        }

        Ok(amount)
    }

    async fn _receive_proofs(
        &self,
        local_alpha_keysets_info: &[KeySetInfo],
        proofs: Vec<cashu::Proof>,
        unit: CurrencyUnit,
        mint: MintUrl,
        intermint_infos: Option<(ConnectedMintsResponse, Vec<KeySetInfo>)>,
        tstamp: u64,
        memo: Option<String>,
        metadata: HashMap<String, String>,
    ) -> Result<TransactionId> {
        let mut proofs = proofs;
        if mint != self.client.mint_url() {
            if let Some((clowder_path, _)) = intermint_infos {
                let alpha_id = clowder_path.mints[0].node_id;
                let alpha_client = (self.client_factory)(mint.clone());
                let substitute_beta_mint = clowder_path.mints[1].mint.clone();

                // In the direct exchange case this is the same as the Wallet's mint
                let substitute_client = if substitute_beta_mint == self.client.mint_url() {
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
                            unit.clone(),
                            tstamp,
                        )
                        .await?;
                } else {
                    tracing::debug!("Offline exchange from {}", mint.to_string());
                    let substitute_proofs = self
                        .offline_exchange(substitute_client.as_ref(), proofs)
                        .await?;

                    // log for debugging
                    tracing::debug!(
                        "Offline Exchanged token: {}",
                        cashu::Token::new(
                            substitute_beta_mint.clone(),
                            substitute_proofs.clone(),
                            None,
                            cashu::CurrencyUnit::Sat,
                        )
                    );

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
                            unit.clone(),
                            tstamp,
                        )
                        .await?;
                }
            } else {
                // different mint, but no clowder-path set
                return Err(Error::InterMintButNoClowderPath);
            };
        }

        let received_amount = proofs.total_amount()?;
        let (stored_amount, ys) = if unit == self.debit.unit() {
            self.debit
                .receive_proofs(
                    self.client.as_ref(),
                    local_alpha_keysets_info,
                    proofs,
                    SafeMode::new(self.safe_mode, self.clowder_id),
                )
                .await?
        } else if unit == self.credit.unit() {
            self.credit
                .receive_proofs(
                    self.client.as_ref(),
                    local_alpha_keysets_info,
                    proofs,
                    SafeMode::new(self.safe_mode, self.clowder_id),
                )
                .await?
        } else {
            return Err(Error::CurrencyUnitMismatch(self.debit.unit(), unit));
        };
        let tx = Transaction {
            mint_url: self.client.mint_url(),
            direction: TransactionDirection::Incoming,
            fee: received_amount
                .checked_sub(stored_amount)
                .expect("fee cannot be negative"),
            amount: received_amount,
            memo,
            metadata,
            timestamp: tstamp,
            unit,
            ys,
            quote_id: None,
        };
        let txid = self.tx_repo.store_tx(tx).await?;
        Ok(txid)
    }

    async fn offline_exchange(
        &self,
        substitute_client: &dyn ClowderMintConnector,
        proofs: Vec<Proof>,
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
        alpha_url: MintUrl,
        alpha_client: &dyn ClowderMintConnector,
        path: Vec<ConnectedMintResponse>,
        unit: CurrencyUnit,
        tstamp: u64,
    ) -> Result<Vec<Proof>> {
        tracing::debug!(alpha_url=?alpha_url, "intermint exchange from ");
        // Already proofs on our mint
        if alpha_url == self.client.mint_url() {
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

        let preimage = format!("CLWDR {}", cashu::SecretKey::generate().to_secret_hex());
        let hash_lock = Sha256::hash(preimage.as_bytes());

        let is_credit = unit == self.credit.unit();

        let locked_alpha_proofs = util::htlc_lock(
            unit,
            tstamp,
            alpha_client,
            is_credit,
            alpha_proofs,
            hash_lock,
            key_locks,
            *wallet_pk.public_key(),
            SafeMode::new(self.safe_mode, self.clowder_id),
        )
        .await?;

        // log for debugging
        tracing::debug!(
            "Locked alpha token: {}",
            cashu::Token::new(
                alpha_url.clone(),
                locked_alpha_proofs.clone(),
                None,
                cashu::CurrencyUnit::Sat,
            )
        );

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
        // log for debugging
        tracing::debug!(
            "Unlocked beta token: {}",
            cashu::Token::new(
                self.client.mint_url(),
                beta_proofs.clone(),
                None,
                cashu::CurrencyUnit::Sat,
            )
        );
        tracing::debug!("Returning same mint proofs");
        Ok(beta_proofs)
    }

    pub async fn receive_token(&self, token: Token, tstamp: u64) -> Result<TransactionId> {
        let token_teaser = token.to_string().chars().take(20).collect::<String>();
        let (intermint_infos, keysets_info) = self
            .get_clowder_path_and_keysets_info(token.mint_url())
            .await?;

        let proofs = if token.mint_url() == self.client.mint_url() {
            token.proofs(&keysets_info)?
        } else if let Some((_, ref intermint_alpha_infos)) = intermint_infos {
            token.proofs(intermint_alpha_infos)?
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
                token.mint_url(),
                intermint_infos,
                tstamp,
                token.memo().clone(),
                metadata,
            )
            .await?
        } else if token.unit().is_some() && token.unit() == Some(self.credit.unit()) {
            tracing::debug!("import credit token");

            self._receive_proofs(
                &keysets_info,
                proofs,
                self.credit.unit(),
                token.mint_url(),
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

    async fn pay_nut18(
        &self,
        proofs: Vec<cashu::Proof>,
        nostr_cl: &nostr_sdk::Client,
        http_cl: &reqwest::Client,
        transport: cashu::Transport,
        p_id: Option<String>,
        mut partial_tx: Transaction,
    ) -> Result<TransactionId> {
        let payload = cashu::PaymentRequestPayload {
            id: p_id,
            memo: partial_tx.memo.clone(),
            unit: partial_tx.unit.clone(),
            mint: self.client.mint_url(),
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
                let receiver = Nip19Profile::from_bech32(&transport.target)?;
                let output = nostr_cl
                    .send_private_msg_to(
                        receiver.relays,
                        receiver.public_key,
                        payload,
                        std::iter::empty(),
                    )
                    .await?;
                partial_tx
                    .metadata
                    .insert(String::from("nostr::event_id"), output.id().to_string());
            }
        }
        let txid = self.tx_repo.store_tx(partial_tx).await?;
        Ok(txid)
    }

    pub async fn handle_event(
        &self,
        event: nostr_sdk::Event,
        signer: Arc<dyn NostrSigner>,
        payment_id: Uuid,
        expected: Amount,
    ) -> Result<Option<TransactionId>> {
        if event.kind != nostr_sdk::Kind::GiftWrap {
            tracing::debug!("handle event, but no GiftWrap - {}", event.kind);
            return Ok(None);
        }

        let payload = match UnwrappedGift::from_gift_wrap(&signer, &event).await {
            Ok(UnwrappedGift { rumor, .. }) => {
                if rumor.kind == nostr_sdk::Kind::PrivateDirectMessage {
                    match serde_json::from_str::<cdk18::PaymentRequestPayload>(&rumor.content) {
                        Ok(payload) => payload,
                        Err(e) => {
                            tracing::error!("Parsing Payment Request failed: {e}");
                            return Ok(None);
                        }
                    }
                } else {
                    tracing::debug!(
                        "handle event, but rumor no PrivateDirectMessage - {}",
                        rumor.kind
                    );
                    return Ok(None);
                }
            }
            Err(e) => {
                tracing::error!("Unwrapping gift wrap failed: {e}");
                return Ok(None);
            }
        };

        if payload.id.unwrap_or_default() != payment_id.to_string() {
            tracing::debug!("handle event, payment id doesn't match");
            return Ok(None);
        }

        let amount = payload.proofs.total_amount()?;
        if amount < expected {
            tracing::warn!(
                "Received amount {} is less than expected {}",
                amount,
                expected
            );
            return Ok(None);
        }
        let meta = HashMap::from([
            (String::from("sender"), event.pubkey.to_string()),
            (String::from("payment_id"), payment_id.to_string()),
            (String::from("nostr_event_id"), event.id.to_string()),
            (
                String::from(PAYMENT_TYPE_METADATA_KEY),
                PaymentType::Cdk18.to_string(),
            ),
            (
                String::from(TRANSACTION_STATUS_METADATA_KEY),
                TransactionStatus::Settled.to_string(),
            ),
        ]);
        let response = <Self as api::WalletApi>::receive_proofs(
            self,
            payload.proofs,
            payload.unit,
            payload.mint,
            chrono::Utc::now().timestamp() as u64,
            payload.memo,
            meta,
        )
        .await;
        match response {
            Ok(txid) => Ok(Some(txid)),
            Err(e) => Err(e),
        }
    }
}
