// ----- standard library imports
use std::str::FromStr;
// ----- extra library imports
use async_trait::async_trait;
use cashu::Proof;
use cdk::Error as CdkError;
// ----- local imports
use crate::sync;

// ----- end imports

//* Clowder Models, TODO - later obtain from shared library such
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ConnectedMintsResponse {
    pub mint_urls: Vec<cashu::MintUrl>,
    pub clowder_urls: Vec<reqwest::Url>,
    pub node_ids: Vec<bitcoin::secp256k1::PublicKey>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PathRequest {
    pub origin_mint_url: cashu::MintUrl,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ExchangeRequest {
    pub alpha_proofs: Vec<cashu::Proof>,
    pub exchange_path: Vec<bitcoin::secp256k1::PublicKey>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ExchangeResponse {
    pub beta_proofs: Vec<cashu::Proof>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PublicKeyResponse {
    pub public_key: bitcoin::secp256k1::PublicKey,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ClowderBetasResponse {
    pub betas: Vec<cashu::MintUrl>,
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
pub trait MintConnector: cdk::wallet::MintConnector + sync::SendSync {
    fn mint_url(&self) -> cashu::MintUrl;

    async fn get_clowder_betas(&self) -> CdkResult<Vec<cashu::MintUrl>>;

    async fn post_exchange(
        &self,
        alpha_proofs: Vec<Proof>,
        exchange_path: Vec<bitcoin::secp256k1::PublicKey>,
    ) -> CdkResult<Vec<Proof>>;
    async fn get_clowder_id(&self) -> CdkResult<bitcoin::secp256k1::PublicKey>;
    async fn post_clowder_path(
        &self,
        origin_mint_url: cashu::MintUrl,
    ) -> CdkResult<ConnectedMintsResponse>;
}

#[derive(Debug, Clone)]
pub struct HttpClientExt {
    main: cdk::wallet::HttpClient,
    url: reqwest::Url,
    secondary: reqwest::Client,
}

impl HttpClientExt {
    pub fn new(cdk_url: cashu::MintUrl) -> Self {
        let mint_url = reqwest::Url::parse(&cdk_url.to_string())
            .expect("cashu::MintUrl is as good as reqwest::Url");
        Self {
            main: cdk::wallet::HttpClient::new(cdk_url),
            url: mint_url,
            secondary: reqwest::Client::new(),
        }
    }
}

type CdkResult<T> = std::result::Result<T, cdk::Error>;
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
impl cdk::wallet::MintConnector for HttpClientExt {
    async fn get_mint_keys(&self) -> CdkResult<Vec<cashu::KeySet>> {
        self.main.get_mint_keys().await
    }
    async fn post_restore(
        &self,
        request: cashu::RestoreRequest,
    ) -> CdkResult<cashu::RestoreResponse> {
        self.main.post_restore(request).await
    }
    async fn post_check_state(
        &self,
        request: cashu::CheckStateRequest,
    ) -> CdkResult<cashu::CheckStateResponse> {
        self.main.post_check_state(request).await
    }
    async fn get_mint_keyset(&self, keyset_id: cashu::Id) -> CdkResult<cashu::KeySet> {
        self.main.get_mint_keyset(keyset_id).await
    }
    async fn get_mint_keysets(&self) -> CdkResult<cashu::KeysetResponse> {
        self.main.get_mint_keysets().await
    }
    async fn get_mint_info(&self) -> CdkResult<cashu::MintInfo> {
        self.main.get_mint_info().await
    }
    async fn post_swap(&self, request: cashu::SwapRequest) -> CdkResult<cashu::SwapResponse> {
        self.main.post_swap(request).await
    }
    async fn post_mint(
        &self,
        request: cashu::MintRequest<String>,
    ) -> CdkResult<cashu::MintResponse> {
        self.main.post_mint(request).await
    }
    async fn post_mint_quote(
        &self,
        request: cashu::MintQuoteBolt11Request,
    ) -> CdkResult<cashu::MintQuoteBolt11Response<String>> {
        self.main.post_mint_quote(request).await
    }
    async fn post_melt(
        &self,
        request: cashu::MeltRequest<String>,
    ) -> CdkResult<cashu::MeltQuoteBolt11Response<String>> {
        self.main.post_melt(request).await
    }
    async fn get_melt_quote_status(
        &self,
        quote_id: &str,
    ) -> CdkResult<cashu::MeltQuoteBolt11Response<String>> {
        self.main.get_melt_quote_status(quote_id).await
    }
    async fn post_melt_quote(
        &self,
        request: cashu::MeltQuoteBolt11Request,
    ) -> CdkResult<cashu::MeltQuoteBolt11Response<String>> {
        self.main.post_melt_quote(request).await
    }
    async fn get_mint_quote_status(
        &self,
        quote_id: &str,
    ) -> CdkResult<cashu::MintQuoteBolt11Response<String>> {
        self.main.get_mint_quote_status(quote_id).await
    }

    async fn post_mint_bolt12_quote(
        &self,
        request: cashu::MintQuoteBolt12Request,
    ) -> CdkResult<cashu::MintQuoteBolt12Response<String>> {
        self.main.post_mint_bolt12_quote(request).await
    }
    async fn get_mint_quote_bolt12_status(
        &self,
        quote_id: &str,
    ) -> CdkResult<cashu::MintQuoteBolt12Response<String>> {
        self.main.get_mint_quote_bolt12_status(quote_id).await
    }
    async fn post_melt_bolt12_quote(
        &self,
        request: cashu::MeltQuoteBolt12Request,
    ) -> CdkResult<cashu::MeltQuoteBolt11Response<String>> {
        self.main.post_melt_bolt12_quote(request).await
    }
    async fn get_melt_bolt12_quote_status(
        &self,
        quote_id: &str,
    ) -> CdkResult<cashu::MeltQuoteBolt11Response<String>> {
        self.main.get_melt_bolt12_quote_status(quote_id).await
    }
    async fn post_melt_bolt12(
        &self,
        request: cashu::MeltRequest<String>,
    ) -> CdkResult<cashu::MeltQuoteBolt11Response<String>> {
        self.main.post_melt_bolt12(request).await
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
impl MintConnector for HttpClientExt {
    fn mint_url(&self) -> cashu::MintUrl {
        cashu::MintUrl::from_str(self.url.as_str())
            .expect("cashu::MintUrl is as good as reqwest::Url")
    }

    async fn get_clowder_betas(&self) -> CdkResult<Vec<cashu::MintUrl>> {
        let url = self
            .url
            .join("v1/betas")
            .expect("get_clowder_urls url error");
        let response = self
            .secondary
            .get(url)
            .send()
            .await
            .map_err(|e| CdkError::HttpError(None, e.to_string()))?;
        let response: ConnectedMintsResponse = response
            .json()
            .await
            .map_err(|e| CdkError::Custom(e.to_string()))?;
        Ok(response.mint_urls)
    }

    async fn post_exchange(
        &self,
        alpha_proofs: Vec<Proof>,
        exchange_path: Vec<bitcoin::secp256k1::PublicKey>,
    ) -> CdkResult<Vec<Proof>> {
        let url = self
            .url
            .join("v1/exchange")
            .expect("post clowder exchange url error");
        let request = ExchangeRequest {
            exchange_path,
            alpha_proofs,
        };
        let response = self
            .secondary
            .post(url)
            .json(&request)
            .send()
            .await
            .map_err(|e| CdkError::HttpError(None, e.to_string()))?;
        let response: ExchangeResponse = response
            .json()
            .await
            .map_err(|e| CdkError::Custom(e.to_string()))?;
        Ok(response.beta_proofs)
    }

    async fn get_clowder_id(&self) -> CdkResult<bitcoin::secp256k1::PublicKey> {
        let url = self.url.join("v1/id").expect("get clowder id url error");

        let response = self
            .secondary
            .get(url)
            .send()
            .await
            .map_err(|e| CdkError::HttpError(None, e.to_string()))?;
        let response: PublicKeyResponse = response
            .json()
            .await
            .map_err(|e| CdkError::Custom(e.to_string()))?;
        Ok(response.public_key)
    }

    async fn post_clowder_path(
        &self,
        origin_mint_url: cashu::MintUrl,
    ) -> CdkResult<ConnectedMintsResponse> {
        let url = self.url.join("v1/path").expect("get clowder id url error");
        let request = PathRequest { origin_mint_url };
        let response = self
            .secondary
            .post(url)
            .json(&request)
            .send()
            .await
            .map_err(|e| CdkError::HttpError(None, e.to_string()))?;
        let response: ConnectedMintsResponse = response
            .json()
            .await
            .map_err(|e| CdkError::Custom(e.to_string()))?;
        Ok(response)
    }
}
