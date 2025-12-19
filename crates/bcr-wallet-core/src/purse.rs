use crate::{
    MintConnector,
    error::{Error, Result},
    persistence::redb::PurseDB,
    sync,
    types::{
        self, PAYMENT_TYPE_METADATA_KEY, PaymentSummary, TRANSACTION_STATUS_METADATA_KEY,
        WalletConfig,
    },
};
use async_trait::async_trait;
use bcr_common::wallet::Token;
use cashu::{Amount, CurrencyUnit, MintUrl, PaymentRequest, nut00 as cdk00, nut18 as cdk18};
use cdk::wallet::types::TransactionId;
use nostr::{nips::nip59::UnwrappedGift, signer::NostrSigner};
use nostr_sdk::nips::nip19::{Nip19Profile, ToBech32};
use std::{collections::HashMap, sync::Arc};
use tokio::{
    sync::{Mutex, RwLock},
    time,
};
use uuid::Uuid;

#[async_trait]
pub trait PurseRepository: sync::SendSync {
    async fn store(&self, wallet: WalletConfig) -> Result<()>;
    async fn load(&self, wallet_id: &str) -> Result<WalletConfig>;
    #[allow(dead_code)]
    async fn delete(&self, wallet_id: &str) -> Result<()>;
    async fn list_ids(&self) -> Result<Vec<String>>;
}

#[async_trait]
pub trait Wallet: sync::SendSync {
    fn config(&self) -> Result<WalletConfig>;
    fn name(&self) -> String;
    fn id(&self) -> String;
    fn mint_url(&self) -> Result<MintUrl>;
    #[allow(dead_code)]
    fn betas(&self) -> Vec<MintUrl>;
    #[allow(dead_code)]
    fn clowder_id(&self) -> bitcoin::secp256k1::PublicKey;
    fn mint_urls(&self) -> Result<Vec<MintUrl>>;
    async fn prepare_pay(&self, input: String, now: u64) -> Result<PaymentSummary>;
    async fn is_wallet_mint_rabid(&self) -> Result<bool>;
    async fn mint_substitute(&self) -> Result<Option<MintUrl>>;
    async fn pay(
        &self,
        p_id: Uuid,
        nostr_cl: &nostr_sdk::Client,
        http_cl: &reqwest::Client,
        tstamp: u64,
    ) -> Result<(TransactionId, Option<Token>)>;

    async fn migrate_pockets_substitute(
        &mut self,
        substitute: Box<dyn MintConnector>,
        tstamp: u64,
    ) -> Result<()>;

    async fn receive_proofs(
        &self,
        proofs: Vec<cdk00::Proof>,
        unit: CurrencyUnit,
        mint: Option<MintUrl>,
        tstamp: u64,
        memo: Option<String>,
        metadata: HashMap<String, String>,
        quote_id: Option<String>,
    ) -> Result<TransactionId>;

    async fn prepare_pay_by_token(
        &self,
        amount: Amount,
        unit: CurrencyUnit,
        description: Option<String>,
    ) -> Result<PaymentSummary>;
}

struct PaymentReference {
    payment_ref: Uuid,
    wallet_idx: usize,
}

pub struct Purse<Wlt> {
    pub repo: Box<dyn PurseRepository>,
    pub wallets: Arc<RwLock<Vec<Arc<RwLock<Wlt>>>>>,
    nostr_cl: Arc<nostr_sdk::Client>,
    myself: Nip19Profile,
    http_cl: Arc<reqwest::Client>,
    current_payment: Mutex<Option<PaymentReference>>,
    current_payment_request: Mutex<Option<PaymentRequest>>,
}
impl<Wlt> Purse<Wlt> {
    pub async fn new(
        repo: PurseDB,
        http_cl: reqwest::Client,
        nostr_cl: nostr_sdk::Client,
        myself: Nip19Profile,
    ) -> Result<Self> {
        Ok(Self {
            repo: Box::new(repo),
            wallets: Arc::new(RwLock::new(Vec::default())),
            nostr_cl: Arc::new(nostr_cl),
            myself,
            http_cl: Arc::new(http_cl),
            current_payment: Mutex::new(None),
            current_payment_request: Mutex::new(None),
        })
    }
}

impl<Wlt> Purse<Wlt> {
    pub async fn load_wallet_config(&self, wallet_id: &str) -> Result<WalletConfig> {
        self.repo.load(wallet_id).await
    }

    pub async fn list_wallets(&self) -> Result<Vec<String>> {
        self.repo.list_ids().await
    }

    pub async fn get_wallet(&self, idx: usize) -> Option<Arc<RwLock<Wlt>>> {
        self.wallets.read().await.get(idx).cloned()
    }

    pub async fn ids(&self) -> Vec<u32> {
        (0..self.wallets.read().await.len() as u32).collect()
    }

    // Current limitation to 1 wallet
    pub async fn can_add_wallet(&self) -> bool {
        self.wallets.read().await.is_empty()
    }
}

impl<Wlt> Purse<Wlt>
where
    Wlt: Wallet,
{
    pub async fn add_wallet(&self, wallet: Wlt) -> Result<usize> {
        self.repo.store(wallet.config()?).await?;
        let mut wallets = self.wallets.write().await;
        wallets.push(Arc::new(RwLock::new(wallet)));
        Ok(wallets.len() - 1)
    }

    pub async fn delete_wallet(&self, idx: usize) -> Result<()> {
        let Some(wlt) = self.get_wallet(idx).await else {
            return Err(Error::WalletNotFound(idx));
        };
        let id = wlt.read().await.id();
        self.repo.delete(&id).await?;
        self.wallets.write().await.remove(idx);
        Ok(())
    }

    pub async fn prepare_pay(&self, idx: usize, input: String, now: u64) -> Result<PaymentSummary> {
        let Some(wlt) = self.get_wallet(idx).await else {
            return Err(Error::WalletNotFound(idx));
        };
        let summary = wlt.read().await.prepare_pay(input, now).await?;
        let pref = PaymentReference {
            payment_ref: summary.request_id,
            wallet_idx: idx,
        };
        *self.current_payment.lock().await = Some(pref);
        Ok(summary)
    }

    pub async fn pay(&self, p_id: Uuid, tstamp: u64) -> Result<TransactionId> {
        let p_ref = self.current_payment.lock().await.take();
        let Some(pref) = p_ref else {
            tracing::error!("No current payment reference found");
            return Err(Error::NoPrepareRef(p_id));
        };
        if pref.payment_ref != p_id {
            tracing::error!(
                "Payment reference ID mismatch: expected {}, got {}",
                pref.payment_ref,
                p_id
            );
            return Err(Error::NoPrepareRef(p_id));
        }
        let Some(wlt) = self.get_wallet(pref.wallet_idx).await else {
            return Err(Error::Internal(String::from(
                "Wallet not found for payment",
            )));
        };
        let (txid, _) = wlt
            .read()
            .await
            .pay(p_id, &self.nostr_cl, &self.http_cl, tstamp)
            .await?;
        Ok(txid)
    }

    pub async fn prepare_payment_request(
        &self,
        amount: Amount,
        unit: Option<CurrencyUnit>,
        description: Option<String>,
    ) -> Result<cdk18::PaymentRequest> {
        let mints = {
            let wlts = self.wallets.read().await;
            let mut mints = Vec::with_capacity(wlts.len());
            for wlt in wlts.iter() {
                mints.extend(wlt.read().await.mint_urls()?);
            }
            mints
        };
        let nostr_transport = cdk18::Transport {
            _type: cdk18::TransportType::Nostr,
            target: self.myself.to_bech32()?,
            tags: Some(vec![vec![String::from("n"), String::from("17")]]),
        };
        let request = cdk18::PaymentRequest {
            payment_id: Some(Uuid::new_v4().to_string()),
            amount: Some(amount),
            mints: Some(mints),
            unit,
            single_use: Some(true),
            description,
            nut10: None,
            transports: vec![nostr_transport],
        };
        *self.current_payment_request.lock().await = Some(request.clone());
        Ok(request)
    }

    pub async fn check_received_payment(
        &self,
        max_wait: core::time::Duration,
        p_id: Uuid,
    ) -> Result<Option<TransactionId>> {
        let current_request = self.current_payment_request.lock().await.take();
        let Some(req) = current_request else {
            return Err(Error::NoPrepareRef(p_id));
        };
        if req.payment_id != Some(p_id.to_string()) {
            return Err(Error::NoPrepareRef(p_id));
        }
        // we split max timeout into 3 intervals.
        // each interval is split into 2 30/70-sized sub-intervals: the fetch_events timeout, and
        // the wait time in-between fetches.
        const TIMEOUT_SPLIT_SIZE: u32 = 3;
        const TIMEOUT_SUBSPLIT_WAIT: f64 = 0.7;
        let interval = max_wait
            .checked_div(TIMEOUT_SPLIT_SIZE)
            .expect("TIMEOUT_SPLIT_SIZE should be > 0");
        let sleep_time = interval.mul_f64(TIMEOUT_SUBSPLIT_WAIT);
        let fetch_timeout = interval
            .checked_sub(sleep_time)
            .expect("sleep_time should be < interval");

        let filter = nostr_sdk::Filter::new()
            .kind(nostr_sdk::Kind::GiftWrap)
            .pubkey(self.myself.public_key);

        let signer = self.nostr_cl.signer().await?;
        for _ in 0..TIMEOUT_SPLIT_SIZE {
            tracing::debug!("Checking events from Nostr...");
            let events = self
                .nostr_cl
                .fetch_events(filter.clone(), fetch_timeout)
                .await?;
            for event in events {
                if let Some(txid) = handle_event(
                    event,
                    signer.clone(),
                    &self.wallets,
                    p_id,
                    req.amount.unwrap_or_default(),
                )
                .await?
                {
                    return Ok(Some(txid));
                }
            }
            time::sleep(sleep_time).await;
        }
        Ok(None)
    }

    pub async fn migrate_rabid_wallets(&self, tstamp: u64) -> Result<()> {
        let wlts = self.wallets.read().await;
        for wlt in wlts.iter() {
            let is_rabid = wlt.read().await.is_wallet_mint_rabid().await?;
            let substitute_url = wlt.read().await.mint_substitute().await?;

            let wallet_name = wlt.read().await.name();
            if is_rabid && let Some(substitute_url) = substitute_url {
                tracing::info!(
                    "Wallet {} is found rabid, migrating to substitute beta {}",
                    wallet_name,
                    substitute_url
                );
                let substitute_client = crate::mint::HttpClientExt::new(substitute_url);
                wlt.write()
                    .await
                    .migrate_pockets_substitute(Box::new(substitute_client), tstamp)
                    .await?;
                self.repo.store(wlt.read().await.config()?).await?;
            }
        }

        Ok(())
    }

    pub async fn prepare_pay_by_token(
        &self,
        idx: usize,
        amount: Amount,
        unit: CurrencyUnit,
        description: Option<String>,
    ) -> Result<PaymentSummary> {
        let Some(wlt) = self.get_wallet(idx).await else {
            return Err(Error::WalletNotFound(idx));
        };

        let summary = wlt
            .read()
            .await
            .prepare_pay_by_token(amount, unit, description)
            .await?;

        let pref = PaymentReference {
            payment_ref: summary.request_id,
            wallet_idx: idx,
        };

        *self.current_payment.lock().await = Some(pref);

        Ok(summary)
    }

    pub async fn pay_by_token(&self, p_id: Uuid, tstamp: u64) -> Result<(TransactionId, Token)> {
        let p_ref = self.current_payment.lock().await.take();

        let Some(pref) = p_ref else {
            tracing::error!("No current payment reference found");
            return Err(Error::NoPrepareRef(p_id));
        };

        if pref.payment_ref != p_id {
            tracing::error!(
                "Payment reference ID mismatch: expected {}, got {}",
                pref.payment_ref,
                p_id
            );
            return Err(Error::NoPrepareRef(p_id));
        }

        let Some(wlt) = self.get_wallet(pref.wallet_idx).await else {
            return Err(Error::Internal(String::from(
                "Wallet not found for payment",
            )));
        };

        let (tx_id, token) = wlt
            .read()
            .await
            .pay(p_id, &self.nostr_cl, &self.http_cl, tstamp)
            .await?;

        Ok((tx_id, token.expect("pay by token returns a token")))
    }
}

async fn handle_event<T>(
    event: nostr_sdk::Event,
    signer: Arc<dyn NostrSigner>,
    wlts: &RwLock<Vec<Arc<RwLock<T>>>>,
    payment_id: Uuid,
    expected: Amount,
) -> Result<Option<TransactionId>>
where
    T: Wallet,
{
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

    let amount = payload
        .proofs
        .iter()
        .fold(Amount::ZERO, |total, p| total + p.amount);
    if amount < expected {
        tracing::warn!(
            "Received amount {} is less than expected {}",
            amount,
            expected
        );
        return Ok(None);
    }
    let wlt = {
        let wallets = wlts.read().await;
        let mut best_wlt: Option<Arc<RwLock<T>>> = None;
        for wlt in wallets.iter() {
            if wlt.read().await.mint_url()? == payload.mint {
                best_wlt.replace(wlt.clone());
                break;
            }
            if wlt.read().await.mint_urls()?.contains(&payload.mint) {
                best_wlt.replace(wlt.clone());
            }
        }
        if best_wlt.is_none() {
            return Err(Error::UnknownMint(payload.mint));
        }
        best_wlt.expect("should be found")
    };
    let meta = HashMap::from([
        (String::from("sender"), event.pubkey.to_string()),
        (String::from("payment_id"), payment_id.to_string()),
        (String::from("nostr_event_id"), event.id.to_string()),
        (
            String::from(PAYMENT_TYPE_METADATA_KEY),
            types::PaymentType::Cdk18.to_string(),
        ),
        (
            String::from(TRANSACTION_STATUS_METADATA_KEY),
            types::TransactionStatus::CashedIn.to_string(),
        ),
    ]);
    let response = wlt
        .read()
        .await
        .receive_proofs(
            payload.proofs,
            payload.unit,
            Some(payload.mint),
            event.created_at.as_u64(),
            payload.memo,
            meta,
            None,
        )
        .await;
    match response {
        Ok(txid) => Ok(Some(txid)),
        Err(e) => Err(e),
    }
}
