// ----- standard library imports
use std::{collections::HashMap, str::FromStr, sync::Mutex};
// ----- extra library imports
use async_trait::async_trait;
use bcr_wallet_lib::wallet::Token;
use cashu::{
    Amount, Bolt11Invoice, CurrencyUnit, KeySetInfo, nut00 as cdk00, nut01 as cdk01,
    nut07 as cdk07, nut18 as cdk18,
};
use cdk::wallet::types::{Transaction, TransactionDirection, TransactionId};
use nostr_sdk::nips::nip19::{FromBech32, Nip19Profile};
use uuid::Uuid;
// ----- local imports
use crate::{
    MintConnector,
    error::{Error, Result},
    purse, sync,
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
pub struct Wallet<Conn, TxRepo, DebtPck> {
    pub network: bitcoin::Network,
    pub client: Conn,
    pub tx_repo: TxRepo,
    pub mint_url: cashu::MintUrl,
    pub debit: DebtPck,
    pub credit: Box<dyn CreditPocket>,
    pub name: String,
    pub id: String,
    pub mnemonic: bip39::Mnemonic,
    pub current_payment: Mutex<Option<PayReference>>,
}

#[derive(Debug, Clone, Default)]
pub struct WalletBalance {
    pub debit: cashu::Amount,
    pub credit: cashu::Amount,
}

impl<Conn, TxRepo, DebtPck> Wallet<Conn, TxRepo, DebtPck>
where
    DebtPck: DebitPocket,
{
    pub async fn balance(&self) -> Result<WalletBalance> {
        let debit = self.debit.balance().await?;
        let credit = self.credit.balance().await?;
        Ok(WalletBalance { debit, credit })
    }

    async fn check_nut18_request(
        &self,
        req: &cdk18::PaymentRequest,
    ) -> Result<(Amount, CurrencyUnit, cdk18::Transport)> {
        if let Some(mints) = &req.mints {
            if !mints.contains(&self.mint_url) {
                return Err(Error::InterMint);
            }
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
        let empty = vec![];
        let transports = req.transports.as_ref().unwrap_or(&empty);
        let (nostr_transports, http_transports): (Vec<_>, Vec<_>) = transports
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

impl<Conn, TxRepo, DebtPck> Wallet<Conn, TxRepo, DebtPck>
where
    Conn: MintConnector,
{
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

impl<Conn, TxRepo, DebtPck> Wallet<Conn, TxRepo, DebtPck>
where
    Conn: MintConnector,
    DebtPck: DebitPocket,
{
    pub async fn clean_local_db(&self) -> Result<u32> {
        let credit_ys = self.credit.clean_local_proofs(&self.client).await?;
        let debit_ys = self.debit.clean_local_proofs(&self.client).await?;
        let total = credit_ys.len() + debit_ys.len();
        Ok(total as u32)
    }

    pub async fn redeem_credit(&self) -> Result<Amount> {
        let keysets_info = self.client.get_mint_keysets().await?.keysets;
        let credit_proofs: Vec<cdk00::Proof> = self
            .credit
            .get_redeemable_proofs(&keysets_info, &self.client)
            .await?;
        if credit_proofs.is_empty() {
            Ok(Amount::ZERO)
        } else {
            let (amount, _) = self
                .debit
                .receive_proofs(&self.client, &keysets_info, credit_proofs)
                .await?;
            Ok(amount)
        }
    }

    pub async fn restore_local_proofs(&self) -> Result<()> {
        let keysets_info = self.client.get_mint_keysets().await?.keysets;
        let (debit, credit) = futures::join!(
            self.debit.restore_local_proofs(&keysets_info, &self.client),
            self.credit
                .restore_local_proofs(&keysets_info, &self.client)
        );
        debit?;
        credit?;
        Ok(())
    }

    pub async fn check_pending_melts(&self) -> Result<Amount> {
        self.debit.check_pending_melts(&self.client).await
    }
}

impl<Conn, TxRepo, DebtPck> Wallet<Conn, TxRepo, DebtPck>
where
    TxRepo: TransactionRepository,
{
    pub async fn list_tx_ids(&self) -> Result<Vec<TransactionId>> {
        self.tx_repo.list_tx_ids().await
    }
}

impl<Conn, TxRepo, DebtPck> Wallet<Conn, TxRepo, DebtPck>
where
    Conn: MintConnector,
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
        tstamp: u64,
        memo: Option<String>,
        metadata: HashMap<String, String>,
    ) -> Result<TransactionId> {
        let received_amount = proofs
            .iter()
            .fold(Amount::ZERO, |acc, proof| acc + proof.amount);
        let (stored_amount, ys) = if unit == self.debit.unit() {
            tracing::debug!("receive into debit pocket");
            self.debit
                .receive_proofs(&self.client, keysets_info, proofs)
                .await?
        } else if unit == self.credit.unit() {
            tracing::debug!("receive into credit pocket");
            self.credit
                .receive_proofs(&self.client, keysets_info, proofs)
                .await?
        } else {
            return Err(Error::CurrencyUnitMismatch(self.debit.unit(), unit));
        };
        let tx = Transaction {
            mint_url: self.mint_url.clone(),
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
        };
        let txid = self.tx_repo.store_tx(tx).await?;
        Ok(txid)
    }

    pub async fn receive_token(&self, token: Token, tstamp: u64) -> Result<TransactionId> {
        let token_teaser = token.to_string().chars().take(20).collect::<String>();
        if token.mint_url() != self.mint_url {
            return Err(Error::InvalidToken(token_teaser));
        }
        let keysets_info = self.client.get_mint_keysets().await?.keysets;
        let proofs = token.proofs(&keysets_info)?;
        if proofs.is_empty() {
            return Err(Error::EmptyToken(token_teaser));
        }

        let tx_id = if matches!(token, Token::CashuV4(..)) {
            tracing::debug!("import debit token");
            if token.unit().is_some() && token.unit() != Some(self.debit.unit()) {
                return Err(Error::CurrencyUnitMismatch(
                    token.unit().unwrap(),
                    self.debit.unit(),
                ));
            }
            self._receive_proofs(
                &keysets_info,
                proofs,
                self.debit.unit(),
                tstamp,
                token.memo().clone(),
                HashMap::default(),
            )
            .await?
        } else if matches!(token, Token::BitcrV4(..)) {
            tracing::debug!("import credit token");
            if token.unit().is_some() && token.unit() != Some(self.credit.unit()) {
                return Err(Error::CurrencyUnitMismatch(
                    token.unit().unwrap(),
                    self.credit.unit(),
                ));
            }

            self._receive_proofs(
                &keysets_info,
                proofs,
                self.credit.unit(),
                tstamp,
                token.memo().clone(),
                HashMap::default(),
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
            mint: self.mint_url.clone(),
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
impl<Conn, TxRepo, DebtPck> purse::Wallet for Wallet<Conn, TxRepo, DebtPck>
where
    TxRepo: TransactionRepository,
    Conn: MintConnector,
    DebtPck: DebitPocket,
{
    fn config(&self) -> WalletConfig {
        WalletConfig {
            wallet_id: self.id.clone(),
            name: self.name.clone(),
            network: self.network,
            debit: self.debit.unit(),
            credit: self.credit.maybe_unit(),
            mint: self.mint_url.clone(),
            mnemonic: self.mnemonic.clone(),
        }
    }
    fn name(&self) -> String {
        self.name.clone()
    }
    fn mint_url(&self) -> cashu::MintUrl {
        self.mint_url.clone()
    }
    async fn prepare_pay(&self, input: String, now: u64) -> Result<PaymentSummary> {
        let infos = self.client.get_mint_keysets().await?.keysets;
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
                .prepare_melt(invoice.clone(), &infos, &self.client)
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
            return Err(Error::NoPrepareRef(p_id));
        };
        if p_ref.request_id != p_id {
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
                        .send_proofs(request_id, &infos, &self.client)
                        .await?
                } else if unit == self.debit.unit() {
                    self.debit
                        .send_proofs(request_id, &infos, &self.client)
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
                    mint_url: self.mint_url.clone(),
                    fee: fees,
                    direction: TransactionDirection::Outgoing,
                    memo,
                    timestamp: now,
                    unit: unit.clone(),
                    ys,
                    amount,
                    // payments might need to fill some extra metadata later
                    metadata: HashMap::default(),
                };
                let tx_id = self
                    .pay_nut18(proofs, nostr_cl, http_cl, transport, id, partial_tx)
                    .await?;
                return Ok(tx_id);
            }
            PaymentType::Bolt11 => {
                let proofs = self
                    .debit
                    .pay_melt(request_id, &infos, &self.client)
                    .await?;
                let (ys, proofs): (Vec<cdk01::PublicKey>, Vec<cdk00::Proof>) =
                    proofs.into_iter().unzip();
                let amount = proofs
                    .iter()
                    .fold(Amount::ZERO, |acc, proof| acc + proof.amount);
                let partial_tx = Transaction {
                    mint_url: self.mint_url.clone(),
                    fee: fees,
                    direction: TransactionDirection::Outgoing,
                    memo,
                    timestamp: now,
                    unit: unit.clone(),
                    ys,
                    amount,
                    metadata: HashMap::default(),
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
        tstamp: u64,
        memo: Option<String>,
        metadata: HashMap<String, String>,
    ) -> Result<TransactionId> {
        let keysets_info = self.client.get_mint_keysets().await?.keysets;
        self._receive_proofs(&keysets_info, proofs, unit, tstamp, memo, metadata)
            .await
    }
}
