// ----- standard library imports
use std::sync::Mutex;
// ----- extra library imports
use async_trait::async_trait;
use bcr_wallet_lib::wallet::Token;
use cashu::{nut00 as cdk00, Amount, CurrencyUnit, KeySetInfo, MintUrl};
use cdk::wallet::MintConnector;
use uuid::Uuid;
// ----- local imports
use crate::{
    error::{Error, Result},
    types::{PocketSendSummary, SendSummary, WalletSendSummary},
};

// ----- end imports

/// trait that represents a single compartment in our wallet where we store proofs/tokens of the
/// same currency emitted by the same mint
#[async_trait(?Send)]
pub trait Pocket {
    fn is_mine(&self, token: &Token) -> bool;
    fn unit(&self) -> CurrencyUnit;

    async fn balance(&self) -> Result<Amount>;

    async fn receive_proofs(
        &self,
        client: &dyn MintConnector,
        keysets_info: &[KeySetInfo],
        proofs: Vec<cdk00::Proof>,
    ) -> Result<Amount>;

    async fn receive(
        &self,
        client: &dyn MintConnector,
        keysets_info: &[KeySetInfo],
        token: Token,
    ) -> Result<Amount>;

    async fn prepare_send(&self, amount: Amount, infos: &[KeySetInfo])
        -> Result<PocketSendSummary>;

    async fn send(
        &self,
        rid: Uuid,
        keysets_info: &[KeySetInfo],
        client: &dyn MintConnector,
        mint_url: MintUrl,
        memo: Option<String>,
    ) -> Result<Token>;
}

#[async_trait(?Send)]
pub trait CreditPocket: Pocket {
    /// returns the amount reclaimed and the proofs that can be redeemed (i.e. unspent proofs with
    /// inactive keysets)
    async fn reclaim_proofs(
        &self,
        keysets_info: &[KeySetInfo],
        client: &dyn MintConnector,
    ) -> Result<(Amount, Vec<cdk00::Proof>)>;
}

#[async_trait(?Send)]
pub trait DebitPocket: Pocket {
    async fn reclaim_proofs(
        &self,
        keysets_info: &[KeySetInfo],
        client: &dyn MintConnector,
    ) -> Result<Amount>;
}

pub struct Wallet<Conn> {
    pub client: Conn,
    pub url: cashu::MintUrl,
    pub debit: Box<dyn DebitPocket>,
    pub credit: Box<dyn CreditPocket>,
    #[allow(dead_code)]
    pub mnemonic: bip39::Mnemonic,
    pub name: String,

    pub current_send: Mutex<Option<WalletSendSummary>>,
}

#[derive(Debug, Clone, Default)]
pub struct WalletBalance {
    pub debit: cashu::Amount,
    pub credit: cashu::Amount,
}

impl<Conn> Wallet<Conn>
where
    Conn: MintConnector,
{
    pub async fn balance(&self) -> Result<WalletBalance> {
        let debit = self.debit.balance().await?;
        let credit = self.credit.balance().await?;
        Ok(WalletBalance { debit, credit })
    }

    pub async fn receive(&self, token: Token) -> Result<cashu::Amount> {
        let keysets_info = self.client.get_mint_keysets().await?.keysets;
        if self.credit.is_mine(&token) {
            tracing::debug!("import credit token");
            self.credit
                .receive(&self.client, &keysets_info, token)
                .await
        } else if self.debit.is_mine(&token) {
            tracing::debug!("import debit token");
            self.debit.receive(&self.client, &keysets_info, token).await
        } else {
            let teaser = token.to_string().chars().take(20).collect::<String>();
            return Err(Error::InvalidToken(teaser));
        }
    }

    async fn prepare_send_with_pocket(
        amount: Amount,
        infos: &[KeySetInfo],
        pocket: &dyn Pocket,
    ) -> Result<(WalletSendSummary, SendSummary)> {
        let pocket_summary = pocket.prepare_send(amount, infos).await?;
        let reference = WalletSendSummary {
            request_id: Uuid::new_v4(),
            unit: pocket.unit(),
            internal_rid: pocket_summary.request_id,
        };
        let summary = SendSummary {
            request_id: reference.request_id,
            unit: pocket.unit(),
            send_fees: pocket_summary.send_fees,
            swap_fees: pocket_summary.swap_fees,
        };
        Ok((reference, summary))
    }

    pub async fn prepare_send(
        &self,
        amount: Amount,
        unit: Option<CurrencyUnit>,
    ) -> Result<SendSummary> {
        let infos = self.client.get_mint_keysets().await?.keysets;
        match unit {
            Some(unit) if unit == self.credit.unit() => {
                let (refer, summary) =
                    Self::prepare_send_with_pocket(amount, &infos, self.credit.as_ref()).await?;
                *self.current_send.lock().unwrap() = Some(refer);
                Ok(summary)
            }
            Some(unit) if unit == self.debit.unit() => {
                let (refer, summary) =
                    Self::prepare_send_with_pocket(amount, &infos, self.debit.as_ref()).await?;
                *self.current_send.lock().unwrap() = Some(refer);
                Ok(summary)
            }
            Some(unit) => Err(Error::UnknownCurrencyUnit(unit)),
            None => {
                // first we try to pay with credit
                let credit_balance = self.credit.balance().await?;
                if credit_balance >= amount {
                    let (refer, summary) =
                        Self::prepare_send_with_pocket(amount, &infos, self.credit.as_ref())
                            .await?;
                    *self.current_send.lock().unwrap() = Some(refer);
                    return Ok(summary);
                }
                // and then fall back to debit
                let debit_balance = self.debit.balance().await?;
                if debit_balance >= amount {
                    let (refer, summary) =
                        Self::prepare_send_with_pocket(amount, &infos, self.debit.as_ref()).await?;
                    *self.current_send.lock().unwrap() = Some(refer);
                    return Ok(summary);
                }
                Err(Error::InsufficientFunds)
            }
        }
    }

    pub async fn send(&self, rid: Uuid, memo: Option<String>) -> Result<Token> {
        let current_send = self.current_send.lock().unwrap().take();
        if current_send.is_none() {
            return Err(Error::NoPrepareSendRef(rid));
        };
        let current_ref = current_send.unwrap();
        if current_ref.request_id != rid {
            return Err(Error::NoPrepareSendRef(rid));
        }

        let keysets_info = self.client.get_mint_keysets().await?.keysets;

        if current_ref.unit == self.credit.unit() {
            let token = self
                .credit
                .send(
                    current_ref.internal_rid,
                    &keysets_info,
                    &self.client,
                    self.url.clone(),
                    memo,
                )
                .await?;
            return Ok(token);
        }
        if current_ref.unit == self.debit.unit() {
            let token = self
                .debit
                .send(
                    current_ref.internal_rid,
                    &keysets_info,
                    &self.client,
                    self.url.clone(),
                    memo,
                )
                .await?;
            return Ok(token);
        }
        Err(Error::UnknownCurrencyUnit(current_ref.unit))
    }

    pub async fn reclaim_funds(&self) -> Result<WalletBalance> {
        let keysets_info = self.client.get_mint_keysets().await?.keysets;
        let debit_reclaimed = self
            .debit
            .reclaim_proofs(&keysets_info, &self.client)
            .await?;
        let (credit_reclaimed, reedemable_credit_proofs) = self
            .credit
            .reclaim_proofs(&keysets_info, &self.client)
            .await?;
        let debit_redeemed = self
            .debit
            .receive_proofs(&self.client, &keysets_info, reedemable_credit_proofs)
            .await?;
        Ok(WalletBalance {
            credit: credit_reclaimed,
            debit: debit_reclaimed + debit_redeemed,
        })
    }
}
