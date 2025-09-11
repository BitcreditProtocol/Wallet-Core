// ----- standard library imports
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
// ----- extra library imports
use async_trait::async_trait;
use cashu::{Amount, CurrencyUnit, MintUrl, PaymentRequest, nut00 as cdk00, nut18 as cdk18};
use cdk::wallet::types::TransactionId;
use nostr_sdk::nips::nip19::{Nip19Profile, ToBech32};
use tokio_with_wasm::alias::time;
use uuid::Uuid;
// ----- local imports
use crate::{
    error::{Error, Result},
    sync,
    types::{PaymentSummary, WalletConfig},
};

// ----- end imports

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
pub trait PurseRepository: sync::SendSync {
    async fn store(&self, wallet: WalletConfig) -> Result<()>;
    async fn load(&self, wallet_id: &str) -> Result<WalletConfig>;
    #[allow(dead_code)]
    async fn delete(&self, wallet_id: &str) -> Result<()>;
    async fn list_ids(&self) -> Result<Vec<String>>;
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
pub trait Wallet: sync::SendSync {
    fn config(&self) -> WalletConfig;
    fn name(&self) -> String;
    fn mint_url(&self) -> MintUrl;
    fn mint_urls(&self) -> Vec<MintUrl>;
    async fn prepare_pay(&self, input: String, now: u64) -> Result<PaymentSummary>;
    async fn pay(
        &self,
        p_id: Uuid,
        nostr_cl: &nostr_sdk::Client,
        http_cl: &reqwest::Client,
        tstamp: u64,
    ) -> Result<TransactionId>;

    async fn receive_proofs(
        &self,
        proofs: Vec<cdk00::Proof>,
        unit: CurrencyUnit,
        mint: MintUrl,
        tstamp: u64,
        memo: Option<String>,
        metadata: HashMap<String, String>,
    ) -> Result<TransactionId>;
}

struct PaymentReference {
    payment_ref: Uuid,
    wallet_idx: usize,
}

pub struct Purse<PurseRepo, Wlt> {
    pub repo: PurseRepo,
    pub wallets: Arc<Mutex<Vec<Arc<Wlt>>>>,
    nostr_cl: Arc<nostr_sdk::Client>,
    myself: Nip19Profile,
    http_cl: Arc<reqwest::Client>,
    current_payment: Mutex<Option<PaymentReference>>,
    current_payment_request: Mutex<Option<PaymentRequest>>,
}
impl<PurseRepo, Wlt> Purse<PurseRepo, Wlt> {
    pub async fn new(
        repo: PurseRepo,
        http_cl: reqwest::Client,
        nostr_cl: nostr_sdk::Client,
        myself: Nip19Profile,
    ) -> Result<Self> {
        Ok(Self {
            repo,
            wallets: Arc::new(Mutex::new(Vec::default())),
            nostr_cl: Arc::new(nostr_cl),
            myself,
            http_cl: Arc::new(http_cl),
            current_payment: Mutex::new(None),
            current_payment_request: Mutex::new(None),
        })
    }
}

impl<PurseRepo, Wlt> Purse<PurseRepo, Wlt>
where
    PurseRepo: PurseRepository,
{
    pub async fn load_wallet_config(&self, wallet_id: &str) -> Result<WalletConfig> {
        self.repo.load(wallet_id).await
    }
    pub async fn list_wallets(&self) -> Result<Vec<String>> {
        self.repo.list_ids().await
    }

    pub fn get_wallet(&self, idx: usize) -> Option<Arc<Wlt>> {
        let wallets = self.wallets.lock().unwrap();
        wallets.get(idx).cloned()
    }

    pub fn ids(&self) -> Vec<u32> {
        let w_len = self.wallets.lock().unwrap().len();
        (0..w_len as u32).collect()
    }
}

impl<PurseRepo, Wlt> Purse<PurseRepo, Wlt>
where
    Wlt: Wallet,
{
    pub fn names(&self) -> Vec<String> {
        let wallets = self.wallets.lock().unwrap();
        wallets.iter().map(|w| w.name()).collect()
    }
}

impl<PurseRepo, Wlt> Purse<PurseRepo, Wlt>
where
    PurseRepo: PurseRepository,
    Wlt: Wallet,
{
    pub async fn add_wallet(&self, wallet: Wlt) -> Result<usize> {
        self.repo.store(wallet.config()).await?;
        let mut wallets = self.wallets.lock().unwrap();
        wallets.push(Arc::new(wallet));
        Ok(wallets.len() - 1)
    }

    pub async fn prepare_pay(&self, idx: usize, input: String, now: u64) -> Result<PaymentSummary> {
        let Some(wlt) = self.wallets.lock().unwrap().get(idx).cloned() else {
            return Err(Error::WalletNotFound(idx));
        };
        let summary = wlt.prepare_pay(input, now).await?;
        let pref = PaymentReference {
            payment_ref: summary.request_id,
            wallet_idx: idx,
        };
        *self.current_payment.lock().unwrap() = Some(pref);
        Ok(summary)
    }

    pub async fn pay(&self, p_id: Uuid, tstamp: u64) -> Result<TransactionId> {
        let p_ref = self.current_payment.lock().unwrap().take();
        let Some(pref) = p_ref else {
            return Err(Error::NoPrepareRef(p_id));
        };
        if pref.payment_ref != p_id {
            return Err(Error::NoPrepareRef(p_id));
        }
        let Some(wlt) = self.wallets.lock().unwrap().get(pref.wallet_idx).cloned() else {
            return Err(Error::Internal(String::from(
                "Wallet not found for payment",
            )));
        };
        let txid = wlt.pay(p_id, &self.nostr_cl, &self.http_cl, tstamp).await?;
        Ok(txid)
    }

    pub fn prepare_payment_request(
        &self,
        amount: Amount,
        unit: Option<CurrencyUnit>,
        description: Option<String>,
    ) -> Result<cdk18::PaymentRequest> {
        let mints = {
            let wlts = self.wallets.lock().unwrap();
            let mut mints = Vec::with_capacity(wlts.len());
            for wlt in wlts.iter() {
                mints.extend(wlt.mint_urls());
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
            transports: Some(vec![nostr_transport]),
        };
        *self.current_payment_request.lock().unwrap() = Some(request.clone());
        Ok(request)
    }

    pub async fn check_received_payment(
        &self,
        max_wait: core::time::Duration,
        p_id: Uuid,
    ) -> Result<Option<TransactionId>> {
        let current_request = self.current_payment_request.lock().unwrap().take();
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

        for _ in 0..TIMEOUT_SPLIT_SIZE {
            let events = self
                .nostr_cl
                .fetch_events(filter.clone(), fetch_timeout)
                .await?;
            for event in events {
                if let Some(txid) =
                    handle_event(event, &self.wallets, p_id, req.amount.unwrap_or_default()).await?
                {
                    return Ok(Some(txid));
                }
            }
            time::sleep(sleep_time).await;
        }
        Ok(None)
    }
}

async fn handle_event<T>(
    event: nostr_sdk::Event,
    wlts: &Mutex<Vec<Arc<T>>>,
    payment_id: Uuid,
    expected: Amount,
) -> Result<Option<TransactionId>>
where
    T: Wallet,
{
    if event.kind != nostr_sdk::Kind::PrivateDirectMessage {
        return Ok(None);
    }
    let Ok(payload) = serde_json::from_str::<cdk18::PaymentRequestPayload>(&event.content) else {
        return Ok(None);
    };
    if payload.id.unwrap_or_default() != payment_id.to_string() {
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
        let locked = wlts.lock().unwrap();
        let mut best_wlt: Option<Arc<T>> = None;
        for wlt in locked.iter() {
            if wlt.mint_url() == payload.mint {
                best_wlt.replace(wlt.clone());
                break;
            }
            if wlt.mint_urls().contains(&payload.mint) {
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
    ]);
    let response = wlt
        .receive_proofs(
            payload.proofs,
            payload.unit,
            payload.mint,
            event.created_at.as_u64(),
            payload.memo,
            meta,
        )
        .await;
    match response {
        Ok(txid) => Ok(Some(txid)),
        Err(e) => Err(e),
    }
}
