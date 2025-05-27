// ----- standard library imports
use std::marker::PhantomData;
// ----- extra library imports
use anyhow::Result;
use bcr_wdc_webapi::keys::ActivateKeysetRequest;
use bcr_wdc_webapi::quotes::{
    EnquireReply, ResolveOffer, SignedEnquireRequest, StatusReply, UpdateQuoteRequest,
    UpdateQuoteResponse,
};
use cashu::nuts::nut02 as cdk02;
use cashu::{MintBolt11Request, MintBolt11Response};
use reqwest::Client as HttpClient;
use reqwest::Url;
use serde::{de::DeserializeOwned, Serialize};
use uuid::Uuid;
// ----- local modules
// ----- end imports

pub struct RestClient {
    http: HttpClient,
}

impl RestClient {
    pub fn new() -> Self {
        let http = HttpClient::builder().build().unwrap();
        RestClient { http }
    }

    pub async fn get<T: DeserializeOwned>(&self, url: Url) -> Result<T> {
        let resp = self.http.get(url).send().await?.error_for_status()?;
        Ok(resp.json().await?)
    }

    pub async fn post<Req: Serialize, Res: DeserializeOwned>(
        &self,
        url: Url,
        body: &Req,
    ) -> Result<Res> {
        let resp = self
            .http
            .post(url)
            .json(body)
            .send()
            .await?
            .error_for_status()?;
        Ok(resp.json().await?)
    }

    pub async fn post_<Req: Serialize>(&self, url: Url, body: &Req) -> Result<()> {
        self.http
            .post(url)
            .json(body)
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }
}

impl Default for RestClient {
    fn default() -> Self {
        Self::new()
    }
}

pub struct Service<T> {
    base_url: String,
    client: RestClient,
    _marker: PhantomData<T>,
}

impl<T> Service<T> {
    pub fn new(base_url: String) -> Self {
        Self {
            base_url,
            client: RestClient::new(),
            _marker: PhantomData,
        }
    }
    fn url(&self, path: &str) -> Url {
        Url::parse(&format!("{}/{}", self.base_url, path)).unwrap()
    }
}

pub struct UserService {}

impl Service<UserService> {
    pub async fn mint_credit_quote(&self, req: SignedEnquireRequest) -> EnquireReply {
        let url = self.url("v1/mint/credit/quote");
        self.client.post(url, &req).await.unwrap()
    }
    pub async fn lookup_credit_quote(&self, quote_id: Uuid) -> StatusReply {
        let url = self.url(&format!("v1/mint/credit/quote/{quote_id}"));
        self.client.get(url).await.unwrap()
    }
    pub async fn list_keysets(&self) -> cdk02::KeysetResponse {
        let url = self.url("v1/keysets");
        self.client.get(url).await.unwrap()
    }
    pub async fn mint_ebill(&self, req: MintBolt11Request<Uuid>) -> MintBolt11Response {
        let url = self.url("v1/mint/ebill");
        self.client.post(url, &req).await.unwrap()
    }
    pub async fn resolve_quote(&self, quote_id: Uuid, resolution: ResolveOffer) {
        let url = self.url(&format!("v1/mint/credit/quote/{quote_id}"));
        self.client.post_(url, &resolution).await.unwrap()
    }
    pub async fn list_keys(&self, kid: cashu::Id) -> cashu::nut01::KeysResponse {
        let url = self.url(&format!("v1/keys/{kid}"));
        self.client.get(url).await.unwrap()
    }
}
