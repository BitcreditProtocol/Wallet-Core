pub mod api;
pub mod types;
pub mod util;

use crate::{
    ClowderMintConnector,
    error::{Error, Result},
    pocket::debit::DebitPocketApi,
    types::{PAYMENT_TYPE_METADATA_KEY, TRANSACTION_STATUS_METADATA_KEY},
    wallet::types::{PayReference, SwapConfig, WalletBalance},
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
    client: Arc<dyn ClowderMintConnector>,
    mint_keyset_infos: Vec<cashu::KeySetInfo>,
    beta_clients: HashMap<cashu::MintUrl, Arc<dyn ClowderMintConnector>>,
    tx_repo: Box<dyn TransactionRepository>,
    debit: Box<dyn DebitPocketApi>,
    name: String,
    id: String,
    pub_key: secp256k1::PublicKey,
    current_payment: Mutex<Option<PayReference>>,
    current_payment_request: Mutex<Option<PaymentRequest>>,
    clowder_id: secp256k1::PublicKey,
    client_factory: Box<dyn Fn(cashu::MintUrl) -> Arc<dyn ClowderMintConnector> + Send + Sync>,
    swap_expiry: chrono::TimeDelta,
}

impl Wallet {
    pub async fn new(
        network: bitcoin::Network,
        client: Arc<dyn ClowderMintConnector>,
        mint_keyset_infos: Vec<cashu::KeySetInfo>,
        tx_repo: Box<dyn TransactionRepository>,
        debit: Box<dyn DebitPocketApi>,
        name: String,
        id: String,
        pub_key: secp256k1::PublicKey,
        clowder_id: secp256k1::PublicKey,
        beta_clients: HashMap<cashu::MintUrl, Arc<dyn ClowderMintConnector>>,
        client_factory: Box<dyn Fn(cashu::MintUrl) -> Arc<dyn ClowderMintConnector> + Send + Sync>,
        swap_expiry: chrono::TimeDelta,
    ) -> Result<Self> {
        Ok(Self {
            network,
            client,
            mint_keyset_infos,
            tx_repo,
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
        })
    }

    pub fn name(&self) -> String {
        self.name.clone()
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
        Ok(WalletBalance { debit })
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
        if unit != self.debit.unit() {
            return Err(Error::InvalidCurrencyUnit(unit.to_string()));
        }
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
        let (stored_amount, ys) = self
            .debit
            .receive_proofs(
                self.client.clone(),
                local_alpha_keysets_info,
                proofs,
                self.swap_config(),
            )
            .await?;
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

        let locked_alpha_proofs = util::htlc_lock(
            unit,
            tstamp,
            alpha_client,
            alpha_proofs,
            hash_lock,
            key_locks,
            *wallet_pk.public_key(),
            self.swap_config(),
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

#[cfg(test)]
mod tests {
    use bcr_common::wire::clowder as wire_clowder;
    use bcr_wallet_core::types::{MintSummary, PaymentResultCallback};
    use bcr_wallet_persistence::{
        MockTransactionRepository,
        test_utils::tests::{test_pub_key, valid_payment_address_testnet},
    };
    use nostr::nips::nip19::ToBech32;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::{
        external::{mint::HttpClientExt, test_utils::tests::MockMintConnector},
        pocket::test_utils::tests::MockDebitPocket,
        wallet::{api::WalletApi, types::WalletPaymentType},
    };

    struct MockWalletCtx {
        pub client: MockMintConnector,
        pub tx_repo: MockTransactionRepository,
        pub debit: MockDebitPocket,
    }

    fn wallet_ctx() -> MockWalletCtx {
        let mut client = MockMintConnector::new();
        client.expect_fmt().returning(|_| Ok(()));
        MockWalletCtx {
            client,
            tx_repo: MockTransactionRepository::new(),
            debit: MockDebitPocket::new(),
        }
    }

    fn wallet(ctx: MockWalletCtx) -> Wallet {
        let arc_client: Arc<dyn ClowderMintConnector> = Arc::new(ctx.client);
        Wallet {
            network: bitcoin::Network::Testnet,
            client: arc_client,
            mint_keyset_infos: vec![],
            beta_clients: HashMap::new(),
            tx_repo: Box::new(ctx.tx_repo),
            debit: Box::new(ctx.debit),
            name: "wallet-1".to_owned(),
            id: "w-1".to_owned(),
            pub_key: test_pub_key(),
            current_payment: Mutex::new(None),
            current_payment_request: Mutex::new(None),
            clowder_id: test_pub_key(),
            client_factory: Box::new(|url| Arc::new(HttpClientExt::new(url))),
            swap_expiry: chrono::TimeDelta::seconds(60),
        }
    }

    fn wallet_with_betas(
        mut w: Wallet,
        betas: Vec<(cashu::MintUrl, Arc<dyn ClowderMintConnector>)>,
    ) -> Wallet {
        let mut map = HashMap::new();
        for (url, cl) in betas {
            map.insert(url, cl);
        }
        w.beta_clients = map;
        w
    }

    #[tokio::test]
    async fn test_config_builds_expected_config() {
        let mut ctx = wallet_ctx();

        ctx.debit
            .expect_unit()
            .times(1)
            .returning(|| CurrencyUnit::Sat);

        ctx.client
            .expect_mint_url()
            .times(1)
            .returning(|| cashu::MintUrl::from_str("https://mint.example").unwrap());

        let wlt = wallet(ctx);
        let cfg = wlt.config().expect("config works");

        assert_eq!(cfg.wallet_id, "w-1");
        assert_eq!(cfg.name, "wallet-1");
        assert_eq!(cfg.network, bitcoin::Network::Testnet);
        assert_eq!(cfg.debit, CurrencyUnit::Sat);
        assert_eq!(cfg.mint.to_string(), "https://mint.example");
        assert_eq!(cfg.pub_key, test_pub_key());
        assert_eq!(cfg.clowder_id, test_pub_key());
        assert!(cfg.betas.is_empty());
    }

    #[tokio::test]
    async fn test_name() {
        let ctx = wallet_ctx();
        let wlt = wallet(ctx);

        let res = wlt.name();
        assert_eq!(res, "wallet-1".to_owned());
    }

    #[tokio::test]
    async fn test_id() {
        let ctx = wallet_ctx();
        let wlt = wallet(ctx);
        assert_eq!(wlt.id(), "w-1".to_string());
    }

    #[tokio::test]
    async fn test_mint_url() {
        let mut ctx = wallet_ctx();
        ctx.client
            .expect_mint_url()
            .times(1)
            .returning(|| cashu::MintUrl::from_str("https://mint.example").unwrap());

        let wlt = wallet(ctx);
        let url = wlt.mint_url().unwrap();
        assert_eq!(url.to_string(), "https://mint.example");
    }

    #[tokio::test]
    async fn test_betas_and_mint_urls() {
        let mut ctx = wallet_ctx();
        ctx.client
            .expect_mint_url()
            .times(1)
            .returning(|| cashu::MintUrl::from_str("https://mint.example").unwrap());
        let mut wlt = wallet(ctx);

        let b1 = cashu::MintUrl::from_str("https://beta1.example").unwrap();
        let b2 = cashu::MintUrl::from_str("https://beta2.example").unwrap();

        let beta1: Arc<dyn ClowderMintConnector> = Arc::new(MockMintConnector::new());
        let beta2: Arc<dyn ClowderMintConnector> = Arc::new(MockMintConnector::new());

        wlt = wallet_with_betas(wlt, vec![(b1.clone(), beta1), (b2.clone(), beta2)]);

        let betas = wlt.betas();
        assert_eq!(betas.len(), 2);
        assert!(betas.contains(&b1));
        assert!(betas.contains(&b2));

        let urls = wlt.mint_urls().unwrap();
        assert!(urls.contains(&b1));
        assert!(urls.contains(&b2));
        assert!(urls.contains(&cashu::MintUrl::from_str("https://mint.example").unwrap()));
        assert_eq!(urls.len(), 3);
    }

    #[tokio::test]
    async fn test_clowder_id() {
        let ctx = wallet_ctx();
        let wlt = wallet(ctx);
        assert_eq!(wlt.clowder_id(), test_pub_key());
    }

    #[tokio::test]
    async fn test_prepare_pay_unknown_payment_request() {
        let mut ctx = wallet_ctx();
        ctx.client
            .expect_get_mint_keysets()
            .times(1)
            .returning(|| Ok(cashu::KeysetResponse { keysets: vec![] }));
        let wlt = wallet(ctx);

        let err = wlt
            .prepare_pay("not-a-request".to_string())
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

        ctx.client
            .expect_mint_url()
            .times(1)
            .returning(|| cashu::MintUrl::from_str("https://mint.example").unwrap());

        let wlt = wallet(ctx);
        let nostr_transport = cdk18::Transport {
            _type: cdk18::TransportType::Nostr,
            target: nostr::PublicKey::from(test_pub_key().x_only_public_key().0)
                .to_bech32()
                .unwrap(),
            tags: Some(vec![vec![String::from("n"), String::from("17")]]),
        };

        let req = wlt
            .prepare_payment_request(
                cashu::Amount::from(123),
                CurrencyUnit::Sat,
                Some("hello".to_string()),
                nostr_transport,
            )
            .await
            .unwrap();

        let stored = wlt.current_payment_request.lock().await.clone();
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
        let wlt = wallet(ctx);

        let nostr_cl = nostr_sdk::Client::new(nostr_sdk::Keys::generate());

        let callback: PaymentResultCallback = Arc::new(move |_| {});
        let cancel_token = CancellationToken::new();

        let pid = Uuid::new_v4();
        let err = wlt
            .check_received_payment(
                std::time::Duration::from_millis(1),
                pid,
                &nostr_cl,
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
        let wlt = wallet(ctx);

        let res = wlt.debit_unit();
        assert_eq!(res, CurrencyUnit::Sat);
    }

    #[tokio::test]
    async fn test_balance() {
        let mut ctx = wallet_ctx();
        ctx.debit
            .expect_balance()
            .times(1)
            .returning(|| Ok(Amount::ZERO));
        let wlt = wallet(ctx);

        let res = wlt.balance().await.expect("balance works");
        assert_eq!(res.debit, Amount::ZERO);
    }

    #[tokio::test]
    async fn test_list_tx_ids() {
        let mut ctx = wallet_ctx();
        ctx.tx_repo
            .expect_list_tx_ids()
            .times(1)
            .returning(|| Ok(vec![]));
        let wlt = wallet(ctx);

        let res = wlt.list_tx_ids().await.unwrap();
        assert!(res.is_empty());
    }

    #[tokio::test]
    async fn test_list_txs() {
        let mut ctx = wallet_ctx();
        ctx.tx_repo
            .expect_list_txs()
            .times(1)
            .returning(|| Ok(vec![]));
        let wlt = wallet(ctx);

        let res = wlt.list_txs().await.unwrap();
        assert!(res.is_empty());
    }

    #[tokio::test]
    async fn test_cleanup_local_proofs_calls_both_pockets() {
        let mut ctx = wallet_ctx();

        ctx.debit
            .expect_cleanup_local_proofs()
            .times(1)
            .returning(|_client| Ok(vec![]));

        let wlt = wallet(ctx);
        wlt.cleanup_local_proofs().await.unwrap();
    }

    #[tokio::test]
    async fn test_is_wallet_mint_offline_majority_true() {
        let ctx = wallet_ctx();
        let mut wlt = wallet(ctx);

        let b1 = cashu::MintUrl::from_str("https://b1.example").unwrap();
        let b2 = cashu::MintUrl::from_str("https://b2.example").unwrap();
        let b3 = cashu::MintUrl::from_str("https://b3.example").unwrap();

        let mut m1 = MockMintConnector::new();
        let mut m2 = MockMintConnector::new();
        let mut m3 = MockMintConnector::new();

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
        );

        let res = wlt.is_wallet_mint_offline().await.unwrap();
        assert!(res);
    }

    #[tokio::test]
    async fn test_is_wallet_mint_rabid_majority_false() {
        let ctx = wallet_ctx();
        let mut wlt = wallet(ctx);

        let b1 = cashu::MintUrl::from_str("https://b1.example").unwrap();
        let b2 = cashu::MintUrl::from_str("https://b2.example").unwrap();

        let mut m1 = MockMintConnector::new();
        let mut m2 = MockMintConnector::new();

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

        wlt = wallet_with_betas(wlt, vec![(b1, Arc::new(m1)), (b2, Arc::new(m2))]);

        let res = wlt.is_wallet_mint_rabid().await.unwrap();
        assert!(!res);
    }

    #[tokio::test]
    async fn test_mint_substitute_returns_some_on_majority_vote() {
        let ctx = wallet_ctx();
        let mut wlt = wallet(ctx);

        let b1 = cashu::MintUrl::from_str("https://b1.example").unwrap();
        let b2 = cashu::MintUrl::from_str("https://b2.example").unwrap();
        let b3 = cashu::MintUrl::from_str("https://b3.example").unwrap();

        let substitute = cashu::MintUrl::from_str("https://sub.example").unwrap();
        let other = cashu::MintUrl::from_str("https://other.example").unwrap();

        let mut m1 = MockMintConnector::new();
        let mut m2 = MockMintConnector::new();
        let mut m3 = MockMintConnector::new();

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
        );

        let res = wlt.mint_substitute().await.unwrap();
        assert_eq!(res, Some(substitute));
    }

    #[tokio::test]
    async fn test_offline_pay_by_token_errors_if_no_substitute() {
        // no betas = no substitute
        let mut ctx = wallet_ctx();
        ctx.debit
            .expect_unit()
            .times(1)
            .returning(|| CurrencyUnit::Sat);
        let wlt = wallet(ctx);

        let err = wlt
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
            .returning(|| Ok(cashu::KeysetResponse { keysets: vec![] }));
        ctx.client
            .expect_mint_url()
            .times(2) // token creation + tx mint_url
            .returning(|| cashu::MintUrl::from_str("https://mint.example").unwrap());

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

        let wlt = wallet(ctx);
        *wlt.current_payment.lock().await = Some(PayReference {
            request_id: pid,
            unit: CurrencyUnit::Sat,
            fees: cashu::Amount::ZERO,
            ptype: WalletPaymentType::Token,
            memo: Some("memo".to_string()),
        });

        let nostr_cl = nostr_sdk::Client::new(nostr_sdk::Keys::generate());
        let http_cl = reqwest::Client::new();

        let (_txid, token) = wlt.pay(pid, &nostr_cl, &http_cl, 123).await.unwrap();

        assert!(token.is_some());
    }

    #[tokio::test]
    async fn test_mint_uses_debit() {
        let mut ctx = wallet_ctx();

        ctx.client
            .expect_get_mint_keysets()
            .times(1)
            .returning(|| Ok(cashu::KeysetResponse { keysets: vec![] }));

        ctx.debit.expect_mint_onchain().times(1).returning(
            |_amount, _keysets_info, _client, _clowder_id| {
                Ok(MintSummary {
                    quote_id: Uuid::new_v4(),
                    amount: bitcoin::Amount::from_sat(1000),
                    address: valid_payment_address_testnet(),
                    expiry: 0,
                })
            },
        );

        let wlt = wallet(ctx);
        let _ = wlt.mint(bitcoin::Amount::from_sat(1000)).await.unwrap();
    }
}
