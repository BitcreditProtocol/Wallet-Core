// ----- standard library imports
use std::str::FromStr;
// ----- extra library imports
use async_trait::async_trait;
use bcr_common::wire::{keys as wire_keys, swap as wire_swap};
use bitcoin::base64::prelude::*;
use cashu::Proof;
use cdk::Error as CdkError;
// ----- local imports
use crate::{
    TStamp,
    clowder_models::{
        AlphaStateResponse, ConnectedMintResponse, ConnectedMintsResponse, ExchangeRequest,
        ExchangeResponse, OfflineResponse, PathRequest, ProofFingerprint, PublicKeyResponse,
        SubstituteExchangeRequest, SubstituteExchangeResponse,
    },
    error::Result,
    sync,
};
// ----- end imports

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
    async fn get_alpha_keysets(
        &self,
        alpha_id: bitcoin::secp256k1::PublicKey,
    ) -> CdkResult<Vec<cashu::KeySet>>;

    async fn get_alpha_offline(&self, alpha_id: bitcoin::secp256k1::PublicKey) -> CdkResult<bool>;
    async fn get_alpha_status(
        &self,
        alpha_id: bitcoin::secp256k1::PublicKey,
    ) -> CdkResult<AlphaStateResponse>;
    async fn get_alpha_substitute(
        &self,
        alpha_id: bitcoin::secp256k1::PublicKey,
    ) -> CdkResult<ConnectedMintResponse>;

    async fn post_exchange_substitute(
        &self,
        proofs: Vec<ProofFingerprint>,
        locks: Vec<bitcoin::hashes::sha256::Hash>,
        wallet_pubkey: bitcoin::secp256k1::PublicKey,
    ) -> CdkResult<Vec<Proof>>;

    async fn post_commitment(
        &self,
        inputs: Vec<cashu::Proof>,
        outputs: Vec<cashu::BlindedMessage>,
        expiration: chrono::TimeDelta,
        alpha_pk: secp256k1::PublicKey,
    ) -> Result<(
        Vec<cashu::PublicKey>,
        Vec<cashu::BlindedMessage>,
        TStamp,
        secp256k1::schnorr::Signature,
    )>;
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

    /// Active alpha keysets
    async fn get_alpha_keysets(
        &self,
        alpha_id: bitcoin::secp256k1::PublicKey,
    ) -> CdkResult<Vec<cashu::KeySet>> {
        let url = self.url.join(&format!("v1/alpha/keysets/{alpha_id}"))?;
        let response = self
            .secondary
            .get(url)
            .send()
            .await
            .map_err(|e| CdkError::HttpError(None, e.to_string()))?;
        let response: cashu::nuts::KeysResponse = response
            .json()
            .await
            .map_err(|e| CdkError::HttpError(None, e.to_string()))?;
        Ok(response.keysets)
    }

    /// Is Alpha Offline
    async fn get_alpha_offline(&self, alpha_id: bitcoin::secp256k1::PublicKey) -> CdkResult<bool> {
        let url = self.url.join(&format!("v1/alpha/offline/{alpha_id}"))?;
        let response = self
            .secondary
            .get(url)
            .send()
            .await
            .map_err(|e| CdkError::HttpError(None, e.to_string()))?;
        let response: OfflineResponse = response
            .json()
            .await
            .map_err(|e| CdkError::HttpError(None, e.to_string()))?;
        Ok(response.offline)
    }

    /// Determines the status of a mint from the view of the requested Beta
    async fn get_alpha_status(
        &self,
        alpha_id: bitcoin::secp256k1::PublicKey,
    ) -> CdkResult<AlphaStateResponse> {
        let url = self.url.join(&format!("v1/alpha/status/{alpha_id}"))?;
        let response = self
            .secondary
            .get(url)
            .send()
            .await
            .map_err(|e| CdkError::HttpError(None, e.to_string()))?;
        Ok(response
            .json()
            .await
            .map_err(|e| CdkError::HttpError(None, e.to_string()))?)
    }

    /// Determines the substitute beta of an alpha mint
    async fn get_alpha_substitute(
        &self,
        alpha_id: bitcoin::secp256k1::PublicKey,
    ) -> CdkResult<ConnectedMintResponse> {
        let url = self.url.join(&format!("v1/alpha/substitute/{alpha_id}"))?;
        let response = self
            .secondary
            .get(url)
            .send()
            .await
            .map_err(|e| CdkError::HttpError(None, e.to_string()))?;
        Ok(response
            .json()
            .await
            .map_err(|e| CdkError::HttpError(None, e.to_string()))?)
    }

    async fn get_clowder_betas(&self) -> CdkResult<Vec<cashu::MintUrl>> {
        let url = self
            .url
            .join("v1/betas")
            .expect("get_clowder_betas url error");
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

    async fn post_exchange_substitute(
        &self,
        proofs: Vec<ProofFingerprint>,
        locks: Vec<bitcoin::hashes::sha256::Hash>,
        wallet_pubkey: bitcoin::secp256k1::PublicKey,
    ) -> CdkResult<Vec<Proof>> {
        let url = self.url.join("v1/exchange/substitute")?;
        let request = SubstituteExchangeRequest {
            proofs,
            locks,
            wallet_pubkey,
        };

        let response = self
            .secondary
            .post(url)
            .json(&request)
            .send()
            .await
            .map_err(|e| CdkError::HttpError(None, e.to_string()))?;
        let response: SubstituteExchangeResponse = response
            .json()
            .await
            .map_err(|e| CdkError::Custom(e.to_string()))?;
        Ok(response.outputs)
    }

    async fn post_exchange(
        &self,
        alpha_proofs: Vec<Proof>,
        exchange_path: Vec<bitcoin::secp256k1::PublicKey>,
    ) -> CdkResult<Vec<Proof>> {
        let url = self
            .url
            .join("v1/exchange")
            .expect("post_clowder_exchange url error");
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
        let url = self.url.join("v1/id").expect("get_clowder_id url error");

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
        let url = self
            .url
            .join("v1/path")
            .expect("post_clowder_path url error");
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

    async fn post_commitment(
        &self,
        inputs: Vec<cashu::Proof>,
        outputs: Vec<cashu::BlindedMessage>,
        expiration: chrono::TimeDelta,
        alpha_pk: secp256k1::PublicKey,
    ) -> Result<(
        Vec<cashu::PublicKey>,
        Vec<cashu::BlindedMessage>,
        TStamp,
        secp256k1::schnorr::Signature,
    )> {
        let url = self
            .url
            .join("v1/commitment")
            .expect("post_commitment url error");
        let inputs: Vec<_> = inputs
            .into_iter()
            .map(wire_keys::ProofFingerprint::try_from)
            .collect::<std::result::Result<_, cashu::nut00::Error>>()?;
        let now = chrono::Utc::now();
        let payload = wire_swap::CommitmentContent {
            inputs,
            outputs,
            expiration: now + expiration,
        };
        let borshed = borsh::to_vec(&payload)?;
        let content = BASE64_STANDARD.encode(borshed);
        let request = wire_swap::CommitmentRequest {
            content: content.clone(),
        };
        let response = self.secondary.post(url).json(&request).send().await?;
        let response: wire_swap::CommitmentResponse = response.json().await?;
        bcr_common::core::signature::schnorr_verify_b64(
            &content,
            &response.commitment,
            &alpha_pk.x_only_public_key().0,
        )?;
        let inputs: Vec<cashu::PublicKey> = payload.inputs.into_iter().map(|fp| fp.y).collect();
        Ok((
            inputs,
            payload.outputs,
            payload.expiration,
            response.commitment,
        ))
    }
}
