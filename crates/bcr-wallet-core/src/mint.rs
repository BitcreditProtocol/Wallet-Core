// ----- standard library imports
// ----- extra library imports
use async_trait::async_trait;
use cdk::Error as CdkError;
// ----- local imports
use crate::sync;

// ----- end imports

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IntermintSwapRequest {
    pub input_mint: cashu::MintUrl,
    pub inputs: Vec<cashu::Proof>,
    pub outputs: Vec<cashu::BlindedMessage>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ClowderBetasResponse {
    pub betas: Vec<cashu::MintUrl>,
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
pub trait MintConnector: cdk::wallet::MintConnector + sync::SendSync {
    async fn post_intermintswap(
        &self,
        request: IntermintSwapRequest,
    ) -> CdkResult<cashu::SwapResponse>;
    async fn get_clowder_betas(&self) -> CdkResult<Vec<cashu::MintUrl>>;
}

#[derive(Debug, Clone)]
pub struct HttpClientExt {
    main: cdk::wallet::HttpClient,
    url: reqwest::Url,
    secondary: reqwest::Client,
}

impl HttpClientExt {
    pub fn new(cdk_url: cashu::MintUrl) -> Self {
        let mint_url = reqwest::Url::parse(&cdk_url.to_string()).expect("Invalid mint URL");
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
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
impl MintConnector for HttpClientExt {
    async fn post_intermintswap(
        &self,
        request: IntermintSwapRequest,
    ) -> CdkResult<cashu::SwapResponse> {
        let url = self
            .url
            .join("intermint_swap")
            .expect("post_intermintswap url error");
        let response = self
            .secondary
            .post(url)
            .json(&request)
            .send()
            .await
            .map_err(|e| CdkError::HttpError(e.to_string()))?;
        let response: cashu::SwapResponse = response
            .json()
            .await
            .map_err(|e| CdkError::Custom(e.to_string()))?;
        Ok(response)
    }

    async fn get_clowder_betas(&self) -> CdkResult<Vec<cashu::MintUrl>> {
        let url = self
            .url
            .join("clowder/urls")
            .expect("get_clowder_urls url error");
        let response = self
            .secondary
            .get(url)
            .send()
            .await
            .map_err(|e| CdkError::HttpError(e.to_string()))?;
        let response: ClowderBetasResponse = response
            .json()
            .await
            .map_err(|e| CdkError::Custom(e.to_string()))?;
        Ok(response.betas)
    }
}
