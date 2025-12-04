use crate::{TStamp, error::Result, sync};
use async_trait::async_trait;
use bcr_common::wire::{clowder as wire_clowder, keys as wire_keys, swap as wire_swap};
use bitcoin::base64::prelude::*;
use cashu::Proof;
use cdk::Error as CdkError;
use rand::seq::IndexedRandom;
use std::str::FromStr;

#[async_trait]
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
    ) -> CdkResult<wire_clowder::ConnectedMintsResponse>;
    async fn get_alpha_keysets(
        &self,
        alpha_id: bitcoin::secp256k1::PublicKey,
    ) -> CdkResult<Vec<cashu::KeySet>>;

    async fn get_alpha_offline(&self, alpha_id: bitcoin::secp256k1::PublicKey) -> CdkResult<bool>;
    async fn get_alpha_status(
        &self,
        alpha_id: bitcoin::secp256k1::PublicKey,
    ) -> CdkResult<wire_clowder::AlphaStateResponse>;
    async fn get_alpha_substitute(
        &self,
        alpha_id: bitcoin::secp256k1::PublicKey,
    ) -> CdkResult<wire_clowder::ConnectedMintResponse>;

    async fn post_exchange_substitute(
        &self,
        proofs: Vec<wire_keys::ProofFingerprint>,
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
#[async_trait]
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

#[async_trait]
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
        let response: wire_clowder::OfflineResponse = response
            .json()
            .await
            .map_err(|e| CdkError::HttpError(None, e.to_string()))?;
        Ok(response.offline)
    }

    /// Determines the status of a mint from the view of the requested Beta
    async fn get_alpha_status(
        &self,
        alpha_id: bitcoin::secp256k1::PublicKey,
    ) -> CdkResult<wire_clowder::AlphaStateResponse> {
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
    ) -> CdkResult<wire_clowder::ConnectedMintResponse> {
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
        let response: wire_clowder::ConnectedMintsResponse = response
            .json()
            .await
            .map_err(|e| CdkError::Custom(e.to_string()))?;
        Ok(response.mints.into_iter().map(|m| m.mint).collect())
    }

    async fn post_exchange_substitute(
        &self,
        proofs: Vec<wire_keys::ProofFingerprint>,
        locks: Vec<bitcoin::hashes::sha256::Hash>,
        wallet_pubkey: bitcoin::secp256k1::PublicKey,
    ) -> CdkResult<Vec<Proof>> {
        let url = self.url.join("v1/exchange/substitute")?;
        let request = wire_clowder::SubstituteExchangeRequest {
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
        let response: wire_clowder::SubstituteExchangeResponse = response
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
        let request = wire_clowder::ExchangeRequest {
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
        let response: wire_clowder::ExchangeResponse = response
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
        let response: wire_clowder::PublicKeyResponse = response
            .json()
            .await
            .map_err(|e| CdkError::Custom(e.to_string()))?;
        Ok(response.public_key)
    }

    async fn post_clowder_path(
        &self,
        origin_mint_url: cashu::MintUrl,
    ) -> CdkResult<wire_clowder::ConnectedMintsResponse> {
        let url = self
            .url
            .join("v1/path")
            .expect("post_clowder_path url error");
        let request = wire_clowder::PathRequest { origin_mint_url };
        let response = self
            .secondary
            .post(url)
            .json(&request)
            .send()
            .await
            .map_err(|e| CdkError::HttpError(None, e.to_string()))?;
        let response: wire_clowder::ConnectedMintsResponse = response
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

/// A client wrapper that forwards wallet events to sentinel nodes.
///
/// This client wraps the standard HTTP client and sends monitoring events
/// to randomly selected sentinel nodes after performing mint, swap, and melt operations.
#[derive(Debug, Clone)]
pub struct SentinelClient {
    main: cdk::wallet::HttpClient,
    url: reqwest::Url,
    secondary: reqwest::Client,
    sentinels: Vec<reqwest::Url>,
}

impl SentinelClient {
    pub fn new(client: HttpClientExt, sentinels: Vec<cashu::MintUrl>) -> Self {
        let sentinels = sentinels
            .iter()
            .map(|url| {
                reqwest::Url::parse(&url.to_string())
                    .expect("cashu::MintUrl is as good as reqwest::Url")
            })
            .collect();

        let HttpClientExt {
            main,
            url,
            secondary,
        } = client;
        Self {
            main,
            url,
            secondary,
            sentinels,
        }
    }
    /// Returns a randomly selected sentinel URL from the configured list, or `None` if no sentinels are configured.
    fn random_sentinel(&self) -> Option<&reqwest::Url> {
        self.sentinels.choose(&mut rand::rng())
    }
    /// Constructs the sentinel event endpoint URL from a base sentinel URL.
    fn sentinel_ep(base_url: &reqwest::Url) -> reqwest::Url {
        base_url
            .join("v1/wallet/event")
            .expect("wallet event url error")
    }
}

#[async_trait]
impl cdk::wallet::MintConnector for SentinelClient {
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
        let response = self.main.post_swap(request).await?;
        let Some(sentinel_url) = self.random_sentinel() else {
            return Ok(response);
        };
        let event_url = Self::sentinel_ep(sentinel_url);
        let event = wire_clowder::WalletEvent::Swap {
            minted: response.signatures.clone(),
        };
        let resp = self.secondary.post(event_url).json(&event).send().await;
        if let Err(e) = resp {
            tracing::error!("Failed to send swap event to sentinel {sentinel_url}: {e}");
        }
        Ok(response)
    }

    async fn post_mint(
        &self,
        request: cashu::MintRequest<String>,
    ) -> CdkResult<cashu::MintResponse> {
        let response = self.main.post_mint(request).await?;
        let Some(sentinel_url) = self.random_sentinel() else {
            return Ok(response);
        };
        let event_url = Self::sentinel_ep(sentinel_url);
        let event = wire_clowder::WalletEvent::Mint {
            minted: response.signatures.clone(),
        };
        let resp = self.secondary.post(event_url).json(&event).send().await;
        if let Err(e) = resp {
            tracing::error!("Failed to send mint event to sentinel {sentinel_url}: {e}");
        }
        Ok(response)
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
        let fps = request
            .inputs()
            .iter()
            .map(|p| p.y())
            .collect::<std::result::Result<Vec<_>, cashu::nut00::Error>>()?;
        let qid = request.quote().clone();
        let response = self.main.post_melt(request).await?;
        let Some(sentinel_url) = self.random_sentinel() else {
            return Ok(response);
        };
        let event_url = Self::sentinel_ep(sentinel_url);
        let event = wire_clowder::WalletEvent::Melt { burned: fps, qid };
        let resp = self.secondary.post(event_url).json(&event).send().await;
        if let Err(e) = resp {
            tracing::error!("Failed to send melt event to sentinel {sentinel_url}: {e}");
        }
        Ok(response)
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

#[async_trait]
impl MintConnector for SentinelClient {
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
        let response: wire_clowder::OfflineResponse = response
            .json()
            .await
            .map_err(|e| CdkError::HttpError(None, e.to_string()))?;
        Ok(response.offline)
    }

    /// Determines the status of a mint from the view of the requested Beta
    async fn get_alpha_status(
        &self,
        alpha_id: bitcoin::secp256k1::PublicKey,
    ) -> CdkResult<wire_clowder::AlphaStateResponse> {
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
    ) -> CdkResult<wire_clowder::ConnectedMintResponse> {
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
        let response: wire_clowder::ConnectedMintsResponse = response
            .json()
            .await
            .map_err(|e| CdkError::Custom(e.to_string()))?;
        Ok(response.mints.into_iter().map(|m| m.mint).collect())
    }

    async fn post_exchange_substitute(
        &self,
        proofs: Vec<wire_keys::ProofFingerprint>,
        locks: Vec<bitcoin::hashes::sha256::Hash>,
        wallet_pubkey: bitcoin::secp256k1::PublicKey,
    ) -> CdkResult<Vec<Proof>> {
        let url = self.url.join("v1/exchange/substitute")?;
        let request = wire_clowder::SubstituteExchangeRequest {
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
        let response: wire_clowder::SubstituteExchangeResponse = response
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
        let request = wire_clowder::ExchangeRequest {
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
        let response: wire_clowder::ExchangeResponse = response
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
        let response: wire_clowder::PublicKeyResponse = response
            .json()
            .await
            .map_err(|e| CdkError::Custom(e.to_string()))?;
        Ok(response.public_key)
    }

    async fn post_clowder_path(
        &self,
        origin_mint_url: cashu::MintUrl,
    ) -> CdkResult<wire_clowder::ConnectedMintsResponse> {
        let url = self
            .url
            .join("v1/path")
            .expect("post_clowder_path url error");
        let request = wire_clowder::PathRequest { origin_mint_url };
        let response = self
            .secondary
            .post(url)
            .json(&request)
            .send()
            .await
            .map_err(|e| CdkError::HttpError(None, e.to_string()))?;
        let response: wire_clowder::ConnectedMintsResponse = response
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
