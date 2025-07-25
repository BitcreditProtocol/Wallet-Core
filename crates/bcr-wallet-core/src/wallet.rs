// ----- standard library imports
use std::{collections::HashMap, sync::Mutex};
// ----- extra library imports
use async_trait::async_trait;
use bcr_wallet_lib::wallet::Token;
use cashu::{Amount, CurrencyUnit, KeySetInfo, nut00 as cdk00, nut01 as cdk01};
use cdk::wallet::{
    MintConnector,
    types::{Transaction, TransactionDirection, TransactionId},
};
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
    fn unit(&self) -> CurrencyUnit;

    async fn balance(&self) -> Result<Amount>;

    async fn receive_proofs(
        &self,
        client: &dyn MintConnector,
        keysets_info: &[KeySetInfo],
        proofs: Vec<cdk00::Proof>,
    ) -> Result<(Amount, Vec<cdk01::PublicKey>)>;

    async fn prepare_send(&self, amount: Amount, infos: &[KeySetInfo])
    -> Result<PocketSendSummary>;

    async fn send_proofs(
        &self,
        rid: Uuid,
        keysets_info: &[KeySetInfo],
        client: &dyn MintConnector,
    ) -> Result<HashMap<cdk01::PublicKey, cdk00::Proof>>;

    async fn clean_local_proofs(&self, client: &dyn MintConnector)
    -> Result<Vec<cdk01::PublicKey>>;
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

    async fn get_redeemable_proofs(
        &self,
        keysets_info: &[KeySetInfo],
        client: &dyn MintConnector,
    ) -> Result<Vec<cdk00::Proof>>;
}

#[async_trait(?Send)]
pub trait DebitPocket: Pocket {
    async fn reclaim_proofs(
        &self,
        keysets_info: &[KeySetInfo],
        client: &dyn MintConnector,
    ) -> Result<Amount>;
}

#[async_trait(?Send)]
pub trait TransactionRepository {
    async fn store_tx(&self, tx: Transaction) -> Result<TransactionId>;
    async fn load_tx(&self, tx_id: TransactionId) -> Result<Transaction>;
    #[allow(dead_code)]
    async fn delete_tx(&self, tx_id: TransactionId) -> Result<()>;
    async fn list_tx_ids(&self) -> Result<Vec<TransactionId>>;
}

pub struct Wallet<Conn, TxRepo> {
    pub client: Conn,
    pub tx_repo: TxRepo,
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

impl<Conn, TxRepo> Wallet<Conn, TxRepo> {
    pub async fn balance(&self) -> Result<WalletBalance> {
        let debit = self.debit.balance().await?;
        let credit = self.credit.balance().await?;
        Ok(WalletBalance { debit, credit })
    }
    async fn prepare_send_with_pocket(
        amount: Amount,
        keysets_info: &[KeySetInfo],
        pocket: &dyn Pocket,
    ) -> Result<(WalletSendSummary, SendSummary)> {
        let pocket_summary = pocket.prepare_send(amount, keysets_info).await?;
        let reference = WalletSendSummary {
            request_id: Uuid::new_v4(),
            amount,
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
}

impl<Conn, TxRepo> Wallet<Conn, TxRepo>
where
    Conn: MintConnector,
{
    pub async fn prepare_send(
        &self,
        amount: Amount,
        unit: Option<CurrencyUnit>,
    ) -> Result<SendSummary> {
        let keysets_info = self.client.get_mint_keysets().await?.keysets;
        match unit {
            Some(unit) if unit == self.credit.unit() => {
                let (refer, summary) =
                    Self::prepare_send_with_pocket(amount, &keysets_info, self.credit.as_ref())
                        .await?;
                *self.current_send.lock().unwrap() = Some(refer);
                Ok(summary)
            }
            Some(unit) if unit == self.debit.unit() => {
                let (refer, summary) =
                    Self::prepare_send_with_pocket(amount, &keysets_info, self.debit.as_ref())
                        .await?;
                *self.current_send.lock().unwrap() = Some(refer);
                Ok(summary)
            }
            Some(unit) => Err(Error::UnknownCurrencyUnit(unit)),
            None => {
                // first we try to pay with credit
                let credit_balance = self.credit.balance().await?;
                if credit_balance >= amount {
                    let (refer, summary) =
                        Self::prepare_send_with_pocket(amount, &keysets_info, self.credit.as_ref())
                            .await?;
                    *self.current_send.lock().unwrap() = Some(refer);
                    return Ok(summary);
                }
                // and then fall back to debit
                let debit_balance = self.debit.balance().await?;
                if debit_balance >= amount {
                    let (refer, summary) =
                        Self::prepare_send_with_pocket(amount, &keysets_info, self.debit.as_ref())
                            .await?;
                    *self.current_send.lock().unwrap() = Some(refer);
                    return Ok(summary);
                }
                Err(Error::InsufficientFunds)
            }
        }
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
        let (debit_redeemed, _) = self
            .debit
            .receive_proofs(&self.client, &keysets_info, reedemable_credit_proofs)
            .await?;
        Ok(WalletBalance {
            credit: credit_reclaimed,
            debit: debit_reclaimed + debit_redeemed,
        })
    }

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
}

impl<Conn, TxRepo> Wallet<Conn, TxRepo>
where
    TxRepo: TransactionRepository,
{
    pub async fn load_tx(&self, tx_id: TransactionId) -> Result<Transaction> {
        self.tx_repo.load_tx(tx_id).await
    }

    pub async fn list_tx_ids(&self) -> Result<Vec<TransactionId>> {
        self.tx_repo.list_tx_ids().await
    }
}

impl<Conn, TxRepo> Wallet<Conn, TxRepo>
where
    Conn: MintConnector,
    TxRepo: TransactionRepository,
{
    pub async fn receive_token(&self, token: Token, tstamp: u64) -> Result<TransactionId> {
        let token_teaser = token.to_string().chars().take(20).collect::<String>();
        if token.mint_url() != self.url {
            return Err(Error::InvalidToken(token_teaser));
        }
        let keysets_info = self.client.get_mint_keysets().await?.keysets;
        let proofs = token.proofs(&keysets_info)?;
        let received_amount = proofs
            .iter()
            .fold(Amount::ZERO, |acc, proof| acc + proof.amount);

        if proofs.is_empty() {
            return Err(Error::EmptyToken(token_teaser));
        }

        let (stored_amount, unit, ys) = if matches!(token, Token::CashuV4(..)) {
            tracing::debug!("import debit token");
            if token.unit().is_some() && token.unit() != Some(self.debit.unit()) {
                return Err(Error::CurrencyUnitMismatch(
                    token.unit().unwrap(),
                    self.debit.unit(),
                ));
            }
            let (amount, ys) = self
                .debit
                .receive_proofs(&self.client, &keysets_info, proofs)
                .await?;
            (amount, self.debit.unit(), ys)
        } else if matches!(token, Token::BitcrV4(..)) {
            tracing::debug!("import credit token");
            if token.unit().is_some() && token.unit() != Some(self.credit.unit()) {
                return Err(Error::CurrencyUnitMismatch(
                    token.unit().unwrap(),
                    self.credit.unit(),
                ));
            }
            let (amount, ys) = self
                .credit
                .receive_proofs(&self.client, &keysets_info, proofs)
                .await?;
            (amount, self.credit.unit(), ys)
        } else {
            return Err(Error::InvalidToken(token_teaser));
        };
        let tx = Transaction {
            mint_url: self.url.clone(),
            direction: TransactionDirection::Incoming,
            fee: received_amount
                .checked_sub(stored_amount)
                .expect("fee cannot be negative"),
            amount: received_amount,
            memo: token.memo().clone(),
            metadata: HashMap::new(),
            timestamp: tstamp,
            unit,
            ys,
        };
        let txid = self.tx_repo.store_tx(tx).await?;
        Ok(txid)
    }

    pub async fn send(
        &self,
        rid: Uuid,
        memo: Option<String>,
        tstamp: u64,
    ) -> Result<(Token, TransactionId)> {
        let current_send = self.current_send.lock().unwrap().take();
        if current_send.is_none() {
            return Err(Error::NoPrepareSendRef(rid));
        };
        let current_ref = current_send.unwrap();
        if current_ref.request_id != rid {
            return Err(Error::NoPrepareSendRef(rid));
        }

        let keysets_info = self.client.get_mint_keysets().await?.keysets;

        let (token, sent_amount, unit, ys) = if current_ref.unit == self.credit.unit() {
            let proofs_map = self
                .credit
                .send_proofs(current_ref.internal_rid, &keysets_info, &self.client)
                .await?;
            let (ys, proofs): (Vec<cdk01::PublicKey>, Vec<cdk00::Proof>) =
                proofs_map.into_iter().unzip();
            let sent_amount = proofs
                .iter()
                .fold(Amount::ZERO, |acc, proof| acc + proof.amount);
            let token =
                Token::new_bitcr(self.url.clone(), proofs, memo.clone(), self.credit.unit());
            (token, sent_amount, self.credit.unit(), ys)
        } else if current_ref.unit == self.debit.unit() {
            let proofs_map = self
                .debit
                .send_proofs(current_ref.internal_rid, &keysets_info, &self.client)
                .await?;
            let (ys, proofs): (Vec<cdk01::PublicKey>, Vec<cdk00::Proof>) =
                proofs_map.into_iter().unzip();
            let sent_amount = proofs
                .iter()
                .fold(Amount::ZERO, |acc, proof| acc + proof.amount);
            let token = Token::new_cashu(self.url.clone(), proofs, memo.clone(), self.debit.unit());
            (token, sent_amount, self.debit.unit(), ys)
        } else {
            return Err(Error::UnknownCurrencyUnit(current_ref.unit));
        };

        let tx = Transaction {
            mint_url: self.url.clone(),
            amount: current_ref.amount,
            fee: sent_amount
                .checked_sub(current_ref.amount)
                .expect("fee cannot be negative"),
            direction: TransactionDirection::Outgoing,
            memo,
            ys,
            unit,
            timestamp: tstamp,
            metadata: HashMap::new(),
        };
        let tx_id = self.tx_repo.store_tx(tx).await?;
        Ok((token, tx_id))
    }
}
