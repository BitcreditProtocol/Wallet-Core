// ----- standard library imports
use std::{collections::HashMap, str::FromStr, sync::Mutex};
// ----- extra library imports
use crate::{clowder_models::AlphaState, utils::proofs_to_fingerprints};
use async_trait::async_trait;
use bcr_wallet_lib::wallet::Token;
use bitcoin::hashes::{Hash, sha256::Hash as Sha256};
use cashu::{
    Amount, Bolt11Invoice, CurrencyUnit, KeySetInfo, MintUrl, Proof, nut00 as cdk00,
    nut01 as cdk01, nut07 as cdk07, nut18 as cdk18,
};
// use cdk::wallet::MintConnector as _;
use cdk::wallet::types::{Transaction, TransactionDirection, TransactionId};
use nostr_sdk::nips::nip19::{FromBech32, Nip19Profile};
use uuid::Uuid;
// ----- local imports
use crate::{
    MintConnector,
    error::{Error, Result},
    purse::{self},
    sync,
    types::{self, MeltSummary, PaymentSummary, RedemptionSummary, SendSummary, WalletConfig},
};

// ----- end imports

/// trait that represents a single compartment in our wallet where we store proofs/tokens of the
/// same currency emitted by the same mint
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
pub trait Pocket: sync::SendSync {
    fn unit(&self) -> CurrencyUnit;
    async fn balance(&self) -> Result<Amount>;
    async fn receive_proofs(
        &self,
        client: &dyn MintConnector,
        keysets_info: &[KeySetInfo],
        proofs: Vec<cdk00::Proof>,
    ) -> Result<(Amount, Vec<cdk01::PublicKey>)>;
    async fn prepare_send(&self, amount: Amount, infos: &[KeySetInfo]) -> Result<SendSummary>;
    async fn send_proofs(
        &self,
        rid: Uuid,
        keysets_info: &[KeySetInfo],
        client: &dyn MintConnector,
    ) -> Result<HashMap<cdk01::PublicKey, cdk00::Proof>>;
    async fn clean_local_proofs(&self, client: &dyn MintConnector)
    -> Result<Vec<cdk01::PublicKey>>;
    async fn restore_local_proofs(
        &self,
        keysets_info: &[KeySetInfo],
        client: &dyn MintConnector,
    ) -> Result<usize>;
    async fn delete_proofs(&self) -> Result<HashMap<cashu::Id, Vec<cdk00::Proof>>>;
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
pub trait CreditPocket: Pocket {
    fn maybe_unit(&self) -> Option<CurrencyUnit>;
    /// returns the amount reclaimed and the proofs that can be redeemed (i.e. unspent proofs with
    /// inactive keysets)
    async fn reclaim_proofs(
        &self,
        keysets_info: &[KeySetInfo],
        client: &dyn MintConnector,
    ) -> Result<(Amount, Vec<cdk00::Proof>)>;
    async fn get_redeemable_proofs(
        &self,
        keysets_info: &[KeySetInfo],
        client: &dyn MintConnector,
    ) -> Result<Vec<cdk00::Proof>>;
    async fn list_redemptions(
        &self,
        keysets_info: &[KeySetInfo],
        payment_window: std::time::Duration,
    ) -> Result<Vec<RedemptionSummary>>;
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
pub trait DebitPocket: Pocket {
    async fn reclaim_proofs(
        &self,
        keysets_info: &[KeySetInfo],
        client: &dyn MintConnector,
    ) -> Result<Amount>;
    async fn prepare_melt(
        &self,
        invoice: Bolt11Invoice,
        keysets_info: &[KeySetInfo],
        client: &dyn MintConnector,
    ) -> Result<MeltSummary>;
    async fn pay_melt(
        &self,
        rid: Uuid,
        keysets_info: &[KeySetInfo],
        client: &dyn MintConnector,
    ) -> Result<HashMap<cdk01::PublicKey, cdk00::Proof>>;
    async fn check_pending_melts(&self, client: &dyn MintConnector) -> Result<Amount>;
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
pub trait TransactionRepository: sync::SendSync {
    async fn store_tx(&self, tx: Transaction) -> Result<TransactionId>;
    async fn load_tx(&self, tx_id: TransactionId) -> Result<Transaction>;
    #[allow(dead_code)]
    async fn delete_tx(&self, tx_id: TransactionId) -> Result<()>;
    async fn list_tx_ids(&self) -> Result<Vec<TransactionId>>;
    async fn update_metadata(
        &self,
        tx_id: TransactionId,
        key: String,
        value: String,
    ) -> Result<Option<String>>;
}

pub struct SendReference {
    pub request_id: Uuid,
    pub amount: Amount,
    pub unit: CurrencyUnit,
}
pub enum PaymentType {
    Cdk18 {
        transport: cdk18::Transport,
        id: Option<String>,
    },
    Bolt11,
}
pub struct PayReference {
    request_id: Uuid,
    unit: CurrencyUnit,
    fees: Amount,
    ptype: PaymentType,
    memo: Option<String>,
}
pub struct Wallet<TxRepo, DebtPck> {
    network: bitcoin::Network,
    client: Box<dyn MintConnector>,
    beta_clients: HashMap<cashu::MintUrl, Box<dyn MintConnector>>,
    tx_repo: TxRepo,
    debit: DebtPck,
    credit: Box<dyn CreditPocket>,
    name: String,
    id: String,
    mnemonic: bip39::Mnemonic,
    current_payment: Mutex<Option<PayReference>>,
    clowder_id: bitcoin::secp256k1::PublicKey,
    client_factory: Box<dyn Fn(cashu::MintUrl) -> Box<dyn MintConnector> + Send + Sync>,
}

#[derive(Debug, Clone, Default)]
pub struct WalletBalance {
    pub debit: cashu::Amount,
    pub credit: cashu::Amount,
}

impl<TxRepo, DebtPck> Wallet<TxRepo, DebtPck> {
    pub async fn new(
        network: bitcoin::Network,
        client: Box<dyn MintConnector>,
        tx_repo: TxRepo,
        (debit, credit): (DebtPck, Box<dyn CreditPocket>),
        name: String,
        id: String,
        mnemonic: bip39::Mnemonic,
        beta_clients: HashMap<cashu::MintUrl, Box<dyn MintConnector>>,
        client_factory: Box<dyn Fn(cashu::MintUrl) -> Box<dyn MintConnector> + Send + Sync>,
    ) -> Result<Self> {
        let clowder_id = client.get_clowder_id().await?;
        Ok(Self {
            network,
            client,
            tx_repo,
            debit,
            credit,
            name,
            id,
            mnemonic,
            current_payment: Mutex::new(None),
            beta_clients,
            clowder_id,
            client_factory,
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
        let keysets_info = self.client.get_mint_keysets().await?.keysets;
        self.credit
            .list_redemptions(&keysets_info, payment_window)
            .await
    }
}

impl<TxRepo, DebtPck> Wallet<TxRepo, DebtPck>
where
    DebtPck: DebitPocket,
{
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
        req: &cdk18::PaymentRequest,
    ) -> Result<(Amount, CurrencyUnit, cdk18::Transport)> {
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
            .partition(|t| matches!(t._type, cdk18::TransportType::Nostr));
        if !http_transports.is_empty() {
            Ok((amount, unit, http_transports[0].clone()))
        } else if !nostr_transports.is_empty() {
            Ok((amount, unit, nostr_transports[0].clone()))
        } else {
            Err(Error::NoTransport)
        }
    }

    fn check_bolt11_invoice(&self, invoice: &Bolt11Invoice, now: u64) -> Result<()> {
        if invoice.network() != self.network {
            return Err(Error::InvalidNetwork(self.network, invoice.network()));
        }
        let now = std::time::Duration::from_secs(now);
        if now > invoice.duration_since_epoch() + invoice.expiry_time() {
            return Err(Error::PaymentExpired);
        }
        if invoice.amount_milli_satoshis().is_none() {
            return Err(Error::MissingAmount);
        };
        Ok(())
    }
}

impl<TxRepo, DebtPck> Wallet<TxRepo, DebtPck>
where
    DebtPck: DebitPocket,
{
    pub async fn clean_local_db(&self) -> Result<u32> {
        let credit_ys = self.credit.clean_local_proofs(self.client.as_ref()).await?;
        let debit_ys = self.debit.clean_local_proofs(self.client.as_ref()).await?;
        let total = credit_ys.len() + debit_ys.len();
        Ok(total as u32)
    }

    pub async fn redeem_credit(&self) -> Result<Amount> {
        let keysets_info = self.client.get_mint_keysets().await?.keysets;
        let credit_proofs: Vec<cdk00::Proof> = self
            .credit
            .get_redeemable_proofs(&keysets_info, self.client.as_ref())
            .await?;
        if credit_proofs.is_empty() {
            Ok(Amount::ZERO)
        } else {
            let (amount, _) = self
                .debit
                .receive_proofs(self.client.as_ref(), &keysets_info, credit_proofs)
                .await?;
            Ok(amount)
        }
    }

    pub async fn restore_local_proofs(&self) -> Result<()> {
        let keysets_info = self.client.get_mint_keysets().await?.keysets;
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

    pub async fn check_pending_melts(&self) -> Result<Amount> {
        self.debit.check_pending_melts(self.client.as_ref()).await
    }
}

impl<TxRepo, DebtPck> Wallet<TxRepo, DebtPck>
where
    TxRepo: TransactionRepository,
{
    pub async fn list_tx_ids(&self) -> Result<Vec<TransactionId>> {
        self.tx_repo.list_tx_ids().await
    }
}

impl<TxRepo, DebtPck> Wallet<TxRepo, DebtPck>
where
    TxRepo: TransactionRepository,
    DebtPck: DebitPocket,
{
    pub async fn load_tx(&self, tx_id: TransactionId) -> Result<Transaction> {
        let mut tx = self.tx_repo.load_tx(tx_id).await?;
        let p_status = types::get_transaction_status(&tx.metadata);
        if !matches!(p_status, types::TransactionStatus::Pending) {
            return Ok(tx);
        }
        let request = cdk07::CheckStateRequest { ys: tx.ys.clone() };
        let response = self.client.post_check_state(request).await?;
        let is_any_spent = response
            .states
            .iter()
            .any(|s| matches!(s.state, cdk07::State::Spent));
        if is_any_spent {
            self.tx_repo
                .update_metadata(
                    tx_id,
                    String::from(types::TRANSACTION_STATUS_METADATA_KEY),
                    types::TransactionStatus::CashedIn.to_string(),
                )
                .await?;
            tx = self.tx_repo.load_tx(tx_id).await?;
        }
        Ok(tx)
    }

    async fn _receive_proofs(
        &self,
        keysets_info: &[KeySetInfo],
        proofs: Vec<cdk00::Proof>,
        unit: CurrencyUnit,
        mint: Option<MintUrl>,
        tstamp: u64,
        memo: Option<String>,
        metadata: HashMap<String, String>,
        quote_id: Option<String>,
    ) -> Result<TransactionId> {
        let mut proofs = proofs;
        if let Some(mint) = mint
            && mint != self.client.mint_url()
        {
            // Determine path from current mint to origin
            let path = self.client.post_clowder_path(mint.clone()).await?;
            tracing::debug!("Receive intermint proofs path {:?}", path);
            if path.node_ids.len() < 3 {
                return Err(Error::InvalidClowderPath);
            }
            let alpha_id = path.node_ids[0];

            let alpha_client = (self.client_factory)(mint.clone());
            // The path goes through the substitute Beta if the Alpha origin mint is offline
            let beta_mint = path.mint_urls[1].clone();
            // Replace Beta instantiation here with stored MintConnectors for each Beta
            let substitute_client = self
                .beta_clients
                .get(&beta_mint)
                .ok_or(Error::BetaNotFound(beta_mint))?;

            let is_alpha_offline = substitute_client.get_alpha_offline(alpha_id).await?;

            if !is_alpha_offline {
                tracing::debug!("Online exchange");
                proofs = self
                    .online_exchange(
                        proofs,
                        mint,
                        alpha_client.as_ref(),
                        path.node_ids,
                        unit.clone(),
                        tstamp,
                    )
                    .await?;
            } else {
                tracing::debug!("Offline exchange");
                let substitute_proofs = self
                    .offline_exchange(substitute_client.as_ref(), proofs, tstamp)
                    .await?;
                // Alpha proofs -> Beta proofs is done, so we only need the path from Beta to the Wallet Mint
                let path = path.node_ids[1..].to_vec();
                proofs = self
                    .online_exchange(
                        substitute_proofs,
                        mint,
                        substitute_client.as_ref(),
                        path,
                        unit.clone(),
                        tstamp,
                    )
                    .await?;
            }
        }

        let received_amount = proofs
            .iter()
            .fold(Amount::ZERO, |acc, proof| acc + proof.amount);
        let (stored_amount, ys) = if unit == self.debit.unit() {
            tracing::debug!("receive into debit pocket");
            self.debit
                .receive_proofs(self.client.as_ref(), keysets_info, proofs)
                .await?
        } else if unit == self.credit.unit() {
            tracing::debug!("receive into credit pocket");
            self.credit
                .receive_proofs(self.client.as_ref(), keysets_info, proofs)
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
            quote_id,
        };
        let txid = self.tx_repo.store_tx(tx).await?;
        Ok(txid)
    }

    async fn htlc_lock(
        unit: CurrencyUnit,
        tstamp: u64,
        client: &dyn MintConnector,
        is_credit: bool,
        proofs: Vec<cashu::Proof>,
        hash_lock: Sha256,
        key_locks: Vec<bitcoin::secp256k1::PublicKey>,
        wallet_pubkey: bitcoin::secp256k1::PublicKey,
    ) -> Result<Vec<cashu::Proof>> {
        let amount = proofs
            .iter()
            .fold(cashu::Amount::ZERO, |acc, x| acc + x.amount);

        let key_locks: Vec<cashu::PublicKey> = key_locks.into_iter().map(|k| k.into()).collect();

        // total hops * time per hop + 2 hops buffer
        let lock_time =
            tstamp + (key_locks.len() as u64 + 2) * crate::config::LOCK_REDUCTION_SECONDS_PER_HOP;

        let infos = client.get_mint_keysets().await?.keysets;

        let active_keyset_id = if is_credit {
            proofs.first().ok_or(Error::NoActiveKeyset)?.keyset_id
        } else {
            infos
                .iter()
                .find(|info| info.active && info.unit == unit)
                .ok_or(Error::NoActiveKeyset)?
                .id
        };

        let n = key_locks.len() as u64;
        let p2pk = cashu::Conditions::new(
            Some(lock_time),
            Some(key_locks),
            Some(vec![wallet_pubkey.into()]),
            Some(n),
            None,
            Some(1),
        )?;
        let htlc = cashu::SpendingConditions::new_htlc_hash(&hash_lock.to_string(), Some(p2pk))?;
        let split_target = cashu::amount::SplitTarget::None;
        let premints =
            cashu::PreMintSecrets::with_conditions(active_keyset_id, amount, &split_target, &htlc)?;

        let swap_request = cashu::SwapRequest::new(proofs, premints.blinded_messages());
        let swap = client.post_swap(swap_request).await?;

        let keyset = client.get_mint_keyset(active_keyset_id).await?;
        let proofs = crate::pocket::unblind_proofs(&keyset, &swap.signatures, &premints);

        Ok(proofs)
    }

    async fn offline_exchange(
        &self,
        substitute_client: &dyn MintConnector,
        proofs: Vec<Proof>,
        tstamp: u64,
    ) -> Result<Vec<Proof>> {
        // Ephemeral P2PK secret
        let wallet_pk = cashu::SecretKey::generate();

        let (fingerprints, secrets) = proofs_to_fingerprints(proofs)?;
        let hash_locks: Vec<Sha256> = secrets
            .iter()
            .map(|secret| Sha256::hash(&secret.to_bytes()))
            .collect();
        let mut beta_proofs = substitute_client
            .post_exchange_substitute(
                fingerprints.clone(),
                hash_locks.clone(),
                *wallet_pk.public_key(),
            )
            .await?;
        // TODO - Verify Beta Proofs don't have additional locks preventing the wallet from using it
        for ((p, h), s) in beta_proofs.iter_mut().zip(hash_locks).zip(secrets) {
            let msg: Vec<u8> = p.secret.to_bytes();

            // Verify spending conditions
            // let hashed = Sha256::hash(&msg);
            // if hashed != h {
            //     return Err(Error::InvalidHashLock(h, hashed));
            // }
            let secret: cashu::nuts::nut10::Secret = p
                .secret
                .clone()
                .try_into()
                .map_err(|_| Error::SpendingConditions)?;
            let conditions: cashu::Conditions = secret
                .secret_data()
                .tags()
                .and_then(|c| c.clone().try_into().ok())
                .ok_or(Error::SpendingConditions)?;

            if secret.secret_data().data() != h.to_string() {
                return Err(Error::InvalidHashLock(
                    h,
                    secret.secret_data().data().to_string(),
                ));
            }

            crate::utils::validate_offline_conditions(
                *wallet_pk.public_key(),
                &conditions,
                tstamp,
            )?;

            let signature: bitcoin::secp256k1::schnorr::Signature = wallet_pk.sign(&msg)?;
            let signatures = vec![signature.to_string()];

            p.witness = Some(cashu::Witness::HTLCWitness(cashu::HTLCWitness {
                preimage: s.to_string(),
                signatures: Some(signatures),
            }));
        }
        Ok(beta_proofs)
    }

    pub async fn online_exchange(
        &self,
        alpha_proofs: Vec<cashu::Proof>,
        alpha_url: MintUrl,
        alpha_client: &dyn MintConnector,
        path: Vec<bitcoin::secp256k1::PublicKey>,
        unit: CurrencyUnit,
        tstamp: u64,
    ) -> Result<Vec<Proof>> {
        tracing::debug!(alpha_url=?alpha_url, "intermint exchange");

        // Ephemeral P2PK secret
        let wallet_pk = cashu::SecretKey::generate();

        // TODO make factory

        // Require all intermediate mints to sign
        // Exclude alpha origin from p2pk lock as it doesn't need to sign its own eCash
        tracing::debug!("Origin {}", path[0]);
        let key_locks: Vec<bitcoin::secp256k1::PublicKey> =
            path.clone().into_iter().skip(1).collect();
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

        let locked_alpha_proofs = Self::htlc_lock(
            unit,
            tstamp,
            alpha_client,
            is_credit,
            alpha_proofs,
            hash_lock,
            key_locks,
            *wallet_pk.public_key(),
        )
        .await?;

        let mut exchange_path = path.clone();
        // Include wallet pubkey as last to be p2pk
        exchange_path.push(*wallet_pk.public_key());

        // Multiple attempts as beta might not immediately have the signatures recorded
        let mut beta_proofs = {
            let mut attempts = 0;
            loop {
                attempts += 1;
                match self
                    .client
                    .post_exchange(locked_alpha_proofs.clone(), exchange_path.clone())
                    .await
                {
                    Ok(proofs) => break Ok(proofs),
                    Err(err) if attempts < crate::config::MAX_INTERMINT_ATTEMPTS => {
                        tracing::warn!("Failed to exchange HTLC proofs: {}", err);
                        tokio_with_wasm::alias::time::sleep(std::time::Duration::from_secs(1))
                            .await;
                    }
                    // TODO - Store the proofs and refund after time lock
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

        for proof in beta_proofs.iter_mut() {
            let msg: Vec<u8> = proof.secret.to_bytes();
            let signature: bitcoin::secp256k1::schnorr::Signature = wallet_pk.sign(&msg)?;

            let signatures = vec![signature.to_string()];

            proof.witness = Some(cashu::Witness::HTLCWitness(cashu::HTLCWitness {
                preimage: preimage.to_string(),
                signatures: Some(signatures),
            }));
        }
        tracing::debug!("Returning same mint proofs");
        Ok(beta_proofs)
    }

    pub async fn receive_token(&self, token: Token, tstamp: u64) -> Result<TransactionId> {
        let token_teaser = token.to_string().chars().take(20).collect::<String>();
        let keysets_info = self.client.get_mint_keysets().await?.keysets;
        let proofs = if token.mint_url() == self.client.mint_url() {
            token.proofs(&keysets_info)?
        } else {
            let path = self.client.post_clowder_path(token.mint_url()).await?;
            tracing::debug!("Receive intermint proofs path {:?}", path);
            if path.node_ids.len() < 3 {
                return Err(Error::InvalidClowderPath);
            }
            let alpha_id = path.node_ids[0];
            // The path goes through the substitute Beta if the Alpha origin mint is offline
            let beta_mint = path.mint_urls[1].clone();
            // In the direct exchange case this is the same as the Wallet's mint
            let substitute_client = self
                .beta_clients
                .get(&beta_mint)
                .ok_or(Error::BetaNotFound(beta_mint))?;

            // In the offline case we can only ask the substitute, in the online case we can ask the mint
            // The Beta mint (after Alpha in the path) should have it in any case
            // This can be revised based on some criteria ?
            let alpha_keysets = substitute_client.get_alpha_keysets(alpha_id).await?;

            // The endpoint only returns active keysets and Clowder/Wildcat don't have fees
            let alpha_infos: Vec<cashu::KeySetInfo> = alpha_keysets
                .iter()
                .map(|keyset| cashu::KeySetInfo {
                    id: keyset.id,
                    unit: keyset.unit.clone(),
                    active: true,
                    input_fee_ppk: 0,
                    final_expiry: keyset.final_expiry,
                })
                .collect();

            token.proofs(&alpha_infos)?
        };
        if proofs.is_empty() {
            return Err(Error::EmptyToken(token_teaser));
        }

        let tx_id = if token.unit().is_some() && token.unit() == Some(self.debit.unit()) {
            tracing::debug!("import debit token");

            self._receive_proofs(
                &keysets_info,
                proofs,
                self.debit.unit(),
                Some(token.mint_url()),
                tstamp,
                token.memo().clone(),
                HashMap::default(),
                None,
            )
            .await?
        } else if token.unit().is_some() && token.unit() == Some(self.credit.unit()) {
            tracing::debug!("import credit token");

            self._receive_proofs(
                &keysets_info,
                proofs,
                self.credit.unit(),
                Some(token.mint_url()),
                tstamp,
                token.memo().clone(),
                HashMap::default(),
                None,
            )
            .await?
        } else {
            return Err(Error::InvalidToken(token_teaser));
        };
        Ok(tx_id)
    }

    async fn pay_nut18(
        &self,
        proofs: Vec<cdk00::Proof>,
        nostr_cl: &nostr_sdk::Client,
        http_cl: &reqwest::Client,
        transport: cdk18::Transport,
        p_id: Option<String>,
        mut partial_tx: Transaction,
    ) -> Result<TransactionId> {
        let payload = cdk18::PaymentRequestPayload {
            id: p_id,
            memo: partial_tx.memo.clone(),
            unit: partial_tx.unit.clone(),
            mint: self.client.mint_url(),
            proofs,
        };
        match transport._type {
            cdk18::TransportType::HttpPost => {
                let url = reqwest::Url::from_str(&transport.target)?;
                let response = http_cl.post(url).json(&payload).send().await?;
                response.error_for_status()?;
            }
            cdk18::TransportType::Nostr => {
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
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
impl<TxRepo, DebtPck> purse::Wallet for Wallet<TxRepo, DebtPck>
where
    TxRepo: TransactionRepository,
    DebtPck: DebitPocket,
{
    fn config(&self) -> Result<WalletConfig> {
        Ok(WalletConfig {
            wallet_id: self.id.clone(),
            name: self.name.clone(),
            network: self.network,
            debit: self.debit.unit(),
            credit: self.credit.maybe_unit(),
            mint: self.client.mint_url(),
            mnemonic: self.mnemonic.clone(),
        })
    }
    fn name(&self) -> String {
        self.name.clone()
    }
    fn mint_url(&self) -> Result<cashu::MintUrl> {
        Ok(self.client.mint_url())
    }
    async fn prepare_pay(&self, input: String, now: u64) -> Result<PaymentSummary> {
        let infos = { self.client.get_mint_keysets() }.await?.keysets;

        if let Ok(request) = cdk18::PaymentRequest::from_str(&input) {
            let (amount, unit, transport) = self.check_nut18_request(&request).await?;
            let s_summary = if self.credit.unit() == unit {
                self.credit.prepare_send(amount, &infos).await?
            } else if self.debit.unit() == unit {
                self.debit.prepare_send(amount, &infos).await?
            } else {
                return Err(Error::CurrencyUnitMismatch(self.debit.unit(), unit));
            };
            let summary = PaymentSummary::from(s_summary);
            let pref = PayReference {
                request_id: summary.request_id,
                unit: summary.unit.clone(),
                fees: summary.fees,
                ptype: PaymentType::Cdk18 {
                    transport,
                    id: request.payment_id,
                },
                memo: request.description,
            };
            *self.current_payment.lock().unwrap() = Some(pref);
            Ok(summary)
        } else if let Ok(invoice) = Bolt11Invoice::from_str(&input) {
            self.check_bolt11_invoice(&invoice, now)?;
            let m_summary = self
                .debit
                .prepare_melt(invoice.clone(), &infos, self.client.as_ref())
                .await?;
            let summary = PaymentSummary::from(m_summary);
            let pref = PayReference {
                request_id: summary.request_id,
                unit: summary.unit.clone(),
                fees: summary.fees,
                ptype: PaymentType::Bolt11,
                memo: Some(invoice.description().to_string()),
            };
            *self.current_payment.lock().unwrap() = Some(pref);
            Ok(summary)
        } else {
            Err(Error::UnknownPaymentRequest(input))
        }
    }
    async fn pay(
        &self,
        p_id: Uuid,
        nostr_cl: &nostr_sdk::Client,
        http_cl: &reqwest::Client,
        now: u64,
    ) -> Result<TransactionId> {
        let p_ref = self.current_payment.lock().unwrap().take();
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
        let infos = self.client.get_mint_keysets().await?.keysets;
        let PayReference {
            request_id,
            unit,
            fees,
            ptype,
            memo,
        } = p_ref;
        match ptype {
            PaymentType::Cdk18 { transport, id } => {
                let proofs = if unit == self.credit.unit() {
                    self.credit
                        .send_proofs(request_id, &infos, self.client.as_ref())
                        .await?
                } else if unit == self.debit.unit() {
                    self.debit
                        .send_proofs(request_id, &infos, self.client.as_ref())
                        .await?
                } else {
                    return Err(Error::Internal(String::from("currency unit mismatch")));
                };
                let (ys, proofs): (Vec<cdk01::PublicKey>, Vec<cdk00::Proof>) =
                    proofs.into_iter().unzip();
                let amount = proofs
                    .iter()
                    .fold(Amount::ZERO, |acc, proof| acc + proof.amount);
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
                    metadata: HashMap::default(),
                    quote_id: None,
                };
                let tx_id = self
                    .pay_nut18(proofs, nostr_cl, http_cl, transport, id, partial_tx)
                    .await?;
                return Ok(tx_id);
            }
            PaymentType::Bolt11 => {
                let proofs = self
                    .debit
                    .pay_melt(request_id, &infos, self.client.as_ref())
                    .await?;
                let (ys, proofs): (Vec<cdk01::PublicKey>, Vec<cdk00::Proof>) =
                    proofs.into_iter().unzip();
                let amount = proofs
                    .iter()
                    .fold(Amount::ZERO, |acc, proof| acc + proof.amount);
                let partial_tx = Transaction {
                    mint_url: self.client.mint_url(),
                    fee: fees,
                    direction: TransactionDirection::Outgoing,
                    memo,
                    timestamp: now,
                    unit: unit.clone(),
                    ys,
                    amount,
                    metadata: HashMap::default(),
                    quote_id: None,
                };
                let tx_id = self.tx_repo.store_tx(partial_tx).await?;
                return Ok(tx_id);
            }
        }
    }
    async fn receive_proofs(
        &self,
        proofs: Vec<cdk00::Proof>,
        unit: CurrencyUnit,
        mint: Option<MintUrl>,
        tstamp: u64,
        memo: Option<String>,
        metadata: HashMap<String, String>,
        quote_id: Option<String>,
    ) -> Result<TransactionId> {
        let keysets_info = self.client.get_mint_keysets().await?.keysets;
        self._receive_proofs(
            &keysets_info,
            proofs,
            unit,
            mint,
            tstamp,
            memo,
            metadata,
            quote_id,
        )
        .await
    }

    async fn is_wallet_mint_rabid(&self) -> Result<bool> {
        let mut rabid_count = 0;
        for beta in self.betas() {
            let beta_client = self
                .beta_clients
                .get(&beta)
                .ok_or(Error::BetaNotFound(beta))?;

            let status = beta_client.get_alpha_status(self.clowder_id).await?;
            if matches!(status, AlphaState::Rabid(..)) {
                rabid_count += 1;
            }
        }
        Ok(rabid_count > self.beta_clients.len() / 2)
    }

    async fn mint_substitute(&self) -> Result<Option<MintUrl>> {
        let mint_id = self.clowder_id;

        let mut substitute_counts = HashMap::<MintUrl, usize>::new();

        for beta in self.betas() {
            let beta_client = self
                .beta_clients
                .get(&beta)
                .ok_or(Error::BetaNotFound(beta))?;

            let substitute_vote = beta_client.get_alpha_substitute(mint_id).await?.mint_url;
            *substitute_counts.entry(substitute_vote).or_default() += 1;
        }

        let threshold = self.beta_clients.len() / 2;
        for (beta_mint, &count) in substitute_counts.iter() {
            if count > threshold {
                return Ok(Some(beta_mint.clone()));
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
        self.beta_clients.iter().map(|(k, _v)| k.clone()).collect()
    }

    fn clowder_id(&self) -> bitcoin::secp256k1::PublicKey {
        self.clowder_id
    }

    async fn migrate_pockets_substitute(
        &mut self,
        substitute: Box<dyn MintConnector>,
        tstamp: u64,
    ) -> Result<()> {
        let debit_proofs = self.debit.delete_proofs().await?;
        let credit_proofs = self.credit.delete_proofs().await?;

        // Exchange debit
        let mut exchanged_debit = Vec::new();
        let mut exchanged_credit = Vec::new();

        // TODO, handle partial exchanges

        for (_, proofs) in debit_proofs.iter() {
            let exchanged = self
                .offline_exchange(substitute.as_ref(), proofs.clone(), tstamp)
                .await?;
            exchanged_debit.extend(exchanged);
        }
        let keysets_info = self.client.get_mint_keysets().await?.keysets;
        self.debit
            .receive_proofs(substitute.as_ref(), &keysets_info, exchanged_debit)
            .await?;

        for (_, proofs) in credit_proofs.iter() {
            let exchanged = self
                .offline_exchange(substitute.as_ref(), proofs.clone(), tstamp)
                .await?;
            exchanged_credit.extend(exchanged);
        }
        self.credit
            .receive_proofs(substitute.as_ref(), &keysets_info, exchanged_credit)
            .await?;

        self.client = substitute;
        Ok(())
    }
}
