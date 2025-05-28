// ----- standard library imports
use std::marker::PhantomData;
// ----- extra library imports
use anyhow::Result;
use async_trait::async_trait;
use cashu::nuts::nut02 as cdk02;
use reqwest::Client as HttpClient;
use reqwest::Url;
use serde::{Serialize, de::DeserializeOwned};
// ----- local modules
use super::DebitWallet;
use super::WalletType;
use crate::wallet::CreditWallet;
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
}

impl Default for RestClient {
    fn default() -> Self {
        Self::new()
    }
}

pub struct Connector<T: WalletType> {
    base_url: String,
    client: RestClient,
    _marker: PhantomData<T>,
}

impl<T: WalletType> Connector<T> {
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

#[async_trait]
pub trait MintConnector {
    async fn list_keysets(&self) -> Result<cdk02::KeysetResponse>;
    async fn swap(&self, req: cashu::SwapRequest) -> Result<cashu::SwapResponse>;
    async fn list_keys(&self, kid: cashu::Id) -> Result<cashu::KeysResponse>;
}

#[async_trait]
impl MintConnector for Connector<CreditWallet> {
    async fn list_keysets(&self) -> Result<cdk02::KeysetResponse> {
        let url = self.url("v1/keysets");
        self.client.get(url).await
    }
    async fn swap(&self, req: cashu::SwapRequest) -> Result<cashu::SwapResponse> {
        let url = self.url("v1/swap");
        self.client.post(url, &req).await
    }
    async fn list_keys(&self, kid: cashu::Id) -> Result<cashu::KeysResponse> {
        let url = self.url(&format!("v1/keys/{kid}"));
        self.client.get(url).await
    }
}

#[async_trait]
impl MintConnector for Connector<DebitWallet> {
    async fn list_keysets(&self) -> Result<cdk02::KeysetResponse> {
        todo!()
    }
    async fn swap(&self, req: cashu::SwapRequest) -> Result<cashu::SwapResponse> {
        todo!("{:?}", req);
    }
    async fn list_keys(&self, kid: cashu::Id) -> Result<cashu::KeysResponse> {
        todo!("{:?}", kid);
    }
}
