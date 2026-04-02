use crate::error::Result;
use async_trait::async_trait;
use bcr_common::{
    cashu::{self, Proof},
    cdk::{self, Error as CdkError},
    client::clowder::Client as ClowderClient,
    wire::{
        clowder::{self as wire_clowder, ConnectedMintsResponse},
        exchange as wire_exchange, keys as wire_keys, melt as wire_melt, mint as wire_mint,
        swap as wire_swap,
    },
};
use bcr_wallet_core::SendSync;
use bitcoin::base64::prelude::*;
use bitcoin::secp256k1;
use rand::seq::IndexedRandom;
use std::str::FromStr;
use tracing::debug;

pub struct SwapCommitmentResult {
    pub inputs_ys: Vec<cashu::PublicKey>,
    pub outputs: Vec<cashu::BlindedMessage>,
    pub expiry_height: u64,
    pub commitment: secp256k1::schnorr::Signature,
    pub ephemeral_secret: secp256k1::SecretKey,
    pub body_content: String,
    pub wallet_key: cashu::PublicKey,
}

fn clowder_err_to_cdk(e: bcr_common::client::clowder::Error) -> CdkError {
    match e {
        bcr_common::client::clowder::Error::NotFound => {
            CdkError::HttpError(None, "Wildcat Clowder resource not found".to_string())
        }
        bcr_common::client::clowder::Error::Reqwest(e) => CdkError::HttpError(None, e.to_string()),
    }
}

#[async_trait]
pub trait ClowderMintConnector: cdk::wallet::MintConnector + SendSync {
    fn mint_url(&self) -> cashu::MintUrl;
    async fn get_clowder_betas(&self) -> CdkResult<Vec<cashu::MintUrl>>;
    async fn post_online_exchange(
        &self,
        alpha_proofs: Vec<Proof>,
        exchange_path: Vec<secp256k1::PublicKey>,
    ) -> CdkResult<Vec<Proof>>;
    async fn get_clowder_id(&self) -> CdkResult<secp256k1::PublicKey>;
    async fn post_clowder_path(
        &self,
        origin_mint_url: cashu::MintUrl,
    ) -> CdkResult<ConnectedMintsResponse>;
    async fn get_alpha_keysets(
        &self,
        alpha_id: secp256k1::PublicKey,
    ) -> CdkResult<Vec<cashu::KeySet>>;
    async fn get_alpha_offline(&self, alpha_id: secp256k1::PublicKey) -> CdkResult<bool>;
    async fn get_alpha_status(
        &self,
        alpha_id: secp256k1::PublicKey,
    ) -> CdkResult<wire_clowder::AlphaStateResponse>;
    async fn get_alpha_substitute(
        &self,
        alpha_id: secp256k1::PublicKey,
    ) -> CdkResult<wire_clowder::ConnectedMintResponse>;
    async fn post_offline_exchange(
        &self,
        proofs: Vec<wire_keys::ProofFingerprint>,
        locks: Vec<bitcoin::hashes::sha256::Hash>,
        wallet_pubkey: secp256k1::PublicKey,
    ) -> CdkResult<Vec<Proof>>;
    async fn post_swap_commitment(
        &self,
        inputs: Vec<cashu::Proof>,
        outputs: Vec<cashu::BlindedMessage>,
        expiry_seconds: chrono::TimeDelta,
        alpha_pk: secp256k1::PublicKey,
    ) -> Result<SwapCommitmentResult>;
    async fn post_swap_committed(
        &self,
        request: wire_swap::SwapRequest,
    ) -> Result<wire_swap::SwapResponse>;
    async fn post_protest_swap(
        &self,
        req: wire_swap::SwapProtestRequest,
    ) -> Result<wire_swap::SwapProtestResponse>;
    async fn post_melt_quote_onchain(
        &self,
        req: wire_melt::MeltQuoteOnchainRequest,
    ) -> Result<wire_melt::MeltQuoteOnchainResponse>;
    async fn post_melt_onchain(
        &self,
        req: cashu::MeltRequest<String>,
    ) -> Result<wire_melt::MeltOnchainResponse>;
    async fn post_mint_quote_onchain(
        &self,
        req: wire_mint::OnchainMintQuoteRequest,
    ) -> Result<wire_mint::OnchainMintQuoteResponse>;
    async fn get_mint_quote_onchain(
        &self,
        quote_id: String,
    ) -> Result<wire_mint::OnchainMintQuoteResponse>;
    async fn post_mint_onchain(
        &self,
        req: wire_mint::OnchainMintRequest,
    ) -> Result<wire_mint::MintResponse>;
    async fn post_protest_mint(
        &self,
        req: wire_mint::MintProtestRequest,
    ) -> Result<wire_mint::MintProtestResponse>;
}

#[derive(Debug, Clone)]
pub struct HttpClientExt {
    main: cdk::wallet::HttpClient,
    url: reqwest::Url,
    secondary: reqwest::Client,
    clowder: ClowderClient,
}

impl HttpClientExt {
    pub fn new(cdk_url: cashu::MintUrl) -> Self {
        let mint_url = reqwest::Url::parse(&cdk_url.to_string())
            .expect("cashu::MintUrl is as good as reqwest::Url");
        Self {
            main: cdk::wallet::HttpClient::new(cdk_url),
            clowder: ClowderClient::new(mint_url.clone()),
            url: mint_url,
            secondary: reqwest::Client::new(),
        }
    }
}

type CdkResult<T> = std::result::Result<T, cdk::Error>;
#[async_trait]
impl cdk::wallet::MintConnector for HttpClientExt {
    async fn get_mint_keys(&self) -> CdkResult<Vec<cashu::KeySet>> {
        debug!("HTTP call to get_mint_keys");
        self.main.get_mint_keys().await
    }
    async fn post_restore(
        &self,
        request: cashu::RestoreRequest,
    ) -> CdkResult<cashu::RestoreResponse> {
        debug!("HTTP call to post_restore");
        self.main.post_restore(request).await
    }
    async fn post_check_state(
        &self,
        request: cashu::CheckStateRequest,
    ) -> CdkResult<cashu::CheckStateResponse> {
        debug!("HTTP call to post_check_state");
        self.main.post_check_state(request).await
    }
    async fn get_mint_keyset(&self, keyset_id: cashu::Id) -> CdkResult<cashu::KeySet> {
        debug!("HTTP call to get_mint_keyset");
        self.main.get_mint_keyset(keyset_id).await
    }
    async fn get_mint_keysets(&self) -> CdkResult<cashu::KeysetResponse> {
        debug!("HTTP call to get_mint_keysets");
        self.main.get_mint_keysets().await
    }
    async fn get_mint_info(&self) -> CdkResult<cashu::MintInfo> {
        debug!("HTTP call to get_mint_info");
        self.main.get_mint_info().await
    }
    async fn post_swap(&self, request: cashu::SwapRequest) -> CdkResult<cashu::SwapResponse> {
        debug!("HTTP call to post_swap");
        self.main.post_swap(request).await
    }
    async fn post_mint(
        &self,
        request: cashu::MintRequest<String>,
    ) -> CdkResult<cashu::MintResponse> {
        debug!("HTTP call to post_mint");
        self.main.post_mint(request).await
    }
    async fn post_mint_quote(
        &self,
        request: cashu::MintQuoteBolt11Request,
    ) -> CdkResult<cashu::MintQuoteBolt11Response<String>> {
        debug!("HTTP call to post_mint_quote");
        self.main.post_mint_quote(request).await
    }
    async fn post_melt(
        &self,
        request: cashu::MeltRequest<String>,
    ) -> CdkResult<cashu::MeltQuoteBolt11Response<String>> {
        debug!("HTTP call to post_melt");
        self.main.post_melt(request).await
    }
    async fn get_melt_quote_status(
        &self,
        quote_id: &str,
    ) -> CdkResult<cashu::MeltQuoteBolt11Response<String>> {
        debug!("HTTP call to get_melt_quote_status");
        self.main.get_melt_quote_status(quote_id).await
    }
    async fn post_melt_quote(
        &self,
        request: cashu::MeltQuoteBolt11Request,
    ) -> CdkResult<cashu::MeltQuoteBolt11Response<String>> {
        debug!("HTTP call to post_melt_quote");
        self.main.post_melt_quote(request).await
    }
    async fn get_mint_quote_status(
        &self,
        quote_id: &str,
    ) -> CdkResult<cashu::MintQuoteBolt11Response<String>> {
        debug!("HTTP call to get_mint_quote_status");
        self.main.get_mint_quote_status(quote_id).await
    }

    async fn post_mint_bolt12_quote(
        &self,
        request: cashu::MintQuoteBolt12Request,
    ) -> CdkResult<cashu::MintQuoteBolt12Response<String>> {
        debug!("HTTP call to post_mint_bolt12_quote");
        self.main.post_mint_bolt12_quote(request).await
    }
    async fn get_mint_quote_bolt12_status(
        &self,
        quote_id: &str,
    ) -> CdkResult<cashu::MintQuoteBolt12Response<String>> {
        debug!("HTTP call to get_mint_quote_bolt12_status");
        self.main.get_mint_quote_bolt12_status(quote_id).await
    }
    async fn post_melt_bolt12_quote(
        &self,
        request: cashu::MeltQuoteBolt12Request,
    ) -> CdkResult<cashu::MeltQuoteBolt11Response<String>> {
        debug!("HTTP call to post_melt_bolt12_quote");
        self.main.post_melt_bolt12_quote(request).await
    }
    async fn get_melt_bolt12_quote_status(
        &self,
        quote_id: &str,
    ) -> CdkResult<cashu::MeltQuoteBolt11Response<String>> {
        debug!("HTTP call to get_melt_bolt12_quote_status");
        self.main.get_melt_bolt12_quote_status(quote_id).await
    }
    async fn post_melt_bolt12(
        &self,
        request: cashu::MeltRequest<String>,
    ) -> CdkResult<cashu::MeltQuoteBolt11Response<String>> {
        debug!("HTTP call to post_melt_bolt12");
        self.main.post_melt_bolt12(request).await
    }
}

#[async_trait]
impl ClowderMintConnector for HttpClientExt {
    fn mint_url(&self) -> cashu::MintUrl {
        cashu::MintUrl::from_str(self.url.as_str())
            .expect("cashu::MintUrl is as good as reqwest::Url")
    }

    /// Active alpha keysets
    async fn get_alpha_keysets(
        &self,
        alpha_id: secp256k1::PublicKey,
    ) -> CdkResult<Vec<cashu::KeySet>> {
        debug!("Clowder client call to get_alpha_keysets");
        let response = self
            .clowder
            .get_active_keysets(alpha_id)
            .await
            .map_err(clowder_err_to_cdk)?;
        Ok(response.keysets)
    }

    /// Is Alpha Offline
    async fn get_alpha_offline(&self, alpha_id: secp256k1::PublicKey) -> CdkResult<bool> {
        debug!("Clowder client call to get_alpha_offline");
        let response = self
            .clowder
            .get_offline(alpha_id)
            .await
            .map_err(clowder_err_to_cdk)?;
        Ok(response.offline)
    }

    /// Determines the status of a mint from the view of the requested Beta
    async fn get_alpha_status(
        &self,
        alpha_id: secp256k1::PublicKey,
    ) -> CdkResult<wire_clowder::AlphaStateResponse> {
        debug!(
            "Clowder client call to get_alpha_status on {} for {alpha_id}",
            self.mint_url().to_string()
        );
        self.clowder
            .get_status(alpha_id)
            .await
            .map_err(clowder_err_to_cdk)
    }

    /// Determines the substitute beta of an alpha mint
    async fn get_alpha_substitute(
        &self,
        alpha_id: secp256k1::PublicKey,
    ) -> CdkResult<wire_clowder::ConnectedMintResponse> {
        debug!(
            "Clowder client call to get_alpha_substitute on {} for {alpha_id}",
            self.mint_url().to_string()
        );
        self.clowder
            .get_substitute(alpha_id)
            .await
            .map_err(clowder_err_to_cdk)
    }

    async fn get_clowder_betas(&self) -> CdkResult<Vec<cashu::MintUrl>> {
        debug!("Clowder client call to get_clowder_betas");
        let response = self.clowder.get_betas().await.map_err(clowder_err_to_cdk)?;
        Ok(response.mints.into_iter().map(|m| m.mint).collect())
    }

    async fn post_offline_exchange(
        &self,
        proofs: Vec<wire_keys::ProofFingerprint>,
        locks: Vec<bitcoin::hashes::sha256::Hash>,
        wallet_pubkey: secp256k1::PublicKey,
    ) -> CdkResult<Vec<Proof>> {
        debug!("Clowder client call to post_offline_exchange");
        let wallet_pk = cashu::PublicKey::from_slice(&wallet_pubkey.serialize())
            .map_err(|e| CdkError::Custom(e.to_string()))?;
        let request = wire_exchange::OfflineExchangeRequest {
            fingerprints: proofs,
            hashes: locks,
            wallet_pk,
        };
        let response = self
            .clowder
            .post_offline_exchange(request)
            .await
            .map_err(clowder_err_to_cdk)?;
        let serialized = BASE64_STANDARD
            .decode(&response.content)
            .map_err(|e| CdkError::Custom(e.to_string()))?;
        let payload: wire_exchange::OfflineExchangePayload =
            borsh::from_slice(&serialized).map_err(|e| CdkError::Custom(e.to_string()))?;
        Ok(payload.proofs)
    }

    async fn post_online_exchange(
        &self,
        alpha_proofs: Vec<Proof>,
        exchange_path: Vec<secp256k1::PublicKey>,
    ) -> CdkResult<Vec<Proof>> {
        debug!("Clowder client call to post_online_exchange");
        let request = wire_exchange::OnlineExchangeRequest {
            proofs: alpha_proofs,
            exchange_path,
        };
        let response = self
            .clowder
            .post_online_exchange(request)
            .await
            .map_err(clowder_err_to_cdk)?;
        Ok(response.proofs)
    }

    async fn get_clowder_id(&self) -> CdkResult<secp256k1::PublicKey> {
        debug!("Clowder client call to get_clowder_id");
        let response = self.clowder.get_info().await.map_err(clowder_err_to_cdk)?;
        Ok(*response.node_id)
    }

    async fn post_clowder_path(
        &self,
        origin_mint_url: cashu::MintUrl,
    ) -> CdkResult<ConnectedMintsResponse> {
        debug!("Clowder client call to post_clowder_path");
        self.clowder
            .post_path(origin_mint_url)
            .await
            .map_err(clowder_err_to_cdk)
    }

    async fn post_swap_commitment(
        &self,
        inputs: Vec<cashu::Proof>,
        outputs: Vec<cashu::BlindedMessage>,
        expiry_seconds: chrono::TimeDelta,
        alpha_pk: secp256k1::PublicKey,
    ) -> Result<SwapCommitmentResult> {
        let url = self
            .url
            .join("v1/swap/commitment")
            .expect("post_swap_commitment url error");
        debug!("HTTP call to post_swap_commitment on {url}");

        // Generate ephemeral keypair for this commitment
        let ephemeral_keypair =
            secp256k1::Keypair::new_global(&mut bitcoin::secp256k1::rand::thread_rng());
        let ephemeral_secret = secp256k1::SecretKey::from_keypair(&ephemeral_keypair);

        // Build commitment body
        let fingerprints: Vec<_> = inputs
            .into_iter()
            .map(wire_keys::ProofFingerprint::try_from)
            .collect::<std::result::Result<_, cashu::nut00::Error>>()?;
        let expiry_height = (chrono::Utc::now() + expiry_seconds).timestamp() as u64;
        let body = wire_swap::SwapCommitmentRequestBody {
            inputs: fingerprints,
            outputs,
            expiry_height,
        };

        // Serialize body, sign with ephemeral key
        let (content, wallet_signature) =
            bcr_common::core::signature::serialize_n_schnorr_sign_borsh_msg(
                &body,
                &ephemeral_keypair,
            )?;
        let wallet_key =
            cashu::PublicKey::from(secp256k1::PublicKey::from_keypair(&ephemeral_keypair));

        let request = wire_swap::SwapCommitmentRequest {
            content: content.clone(),
            wallet_key,
            wallet_signature,
        };

        let response = self
            .secondary
            .post(url)
            .json(&request)
            .send()
            .await?
            .error_for_status()?;
        let response: wire_swap::SwapCommitmentResponse = response.json().await?;

        // Verify alpha's commitment signature over the response content
        bcr_common::core::signature::schnorr_verify_b64(
            &response.content,
            &response.commitment,
            &alpha_pk.x_only_public_key().0,
        )?;

        let inputs_ys: Vec<cashu::PublicKey> = body.inputs.into_iter().map(|fp| fp.y).collect();
        Ok(SwapCommitmentResult {
            inputs_ys,
            outputs: body.outputs,
            expiry_height,
            commitment: response.commitment,
            ephemeral_secret,
            body_content: content,
            wallet_key,
        })
    }

    async fn post_swap_committed(
        &self,
        request: wire_swap::SwapRequest,
    ) -> Result<wire_swap::SwapResponse> {
        let url = self.url.join("v1/swap").expect("post_swap_committed url error");
        debug!("HTTP call to post_swap_committed on {url}");
        let res = self
            .secondary
            .post(url)
            .json(&request)
            .send()
            .await?
            .error_for_status()?;
        let response: wire_swap::SwapResponse = res.json().await?;
        Ok(response)
    }

    async fn post_protest_swap(
        &self,
        req: wire_swap::SwapProtestRequest,
    ) -> Result<wire_swap::SwapProtestResponse> {
        let url = self
            .url
            .join("v1/protest/swap")
            .expect("protest_swap url error");
        debug!("HTTP call to protest_swap on {url}");
        let res = self
            .secondary
            .post(url)
            .json(&req)
            .send()
            .await?
            .error_for_status()?;
        let response: wire_swap::SwapProtestResponse = res.json().await?;
        Ok(response)
    }

    async fn post_melt_quote_onchain(
        &self,
        req: wire_melt::MeltQuoteOnchainRequest,
    ) -> Result<wire_melt::MeltQuoteOnchainResponse> {
        let url = self
            .url
            .join("v1/melt/quote/onchain")
            .expect("melt_quote_onchain url error");
        debug!("HTTP call to melt_quote_onchain on {url}");

        let res = self.secondary.post(url).json(&req).send().await?;
        let response: wire_melt::MeltQuoteOnchainResponse = res.json().await?;
        Ok(response)
    }

    async fn post_melt_onchain(
        &self,
        req: cashu::MeltRequest<String>,
    ) -> Result<wire_melt::MeltOnchainResponse> {
        let url = self
            .url
            .join("v1/melt/onchain")
            .expect("melt_onchain url error");
        debug!("HTTP call to melt_onchain on {url}");

        let res = self.secondary.post(url).json(&req).send().await?;
        let response: wire_melt::MeltOnchainResponse = res.json().await?;
        Ok(response)
    }

    async fn post_mint_quote_onchain(
        &self,
        req: wire_mint::OnchainMintQuoteRequest,
    ) -> Result<wire_mint::OnchainMintQuoteResponse> {
        let url = self
            .url
            .join("v1/mint/quote/onchain")
            .expect("mint_quote_onchain url error");
        debug!("HTTP call to mint_quote_onchain on {url}");

        let res = self.secondary.post(url).json(&req).send().await?;
        let response: wire_mint::OnchainMintQuoteResponse = res.json().await?;
        Ok(response)
    }

    async fn get_mint_quote_onchain(
        &self,
        quote_id: String,
    ) -> Result<wire_mint::OnchainMintQuoteResponse> {
        let url = self
            .url
            .join(&format!("v1/mint/quote/onchain/{quote_id}"))
            .expect("get mint_onchain url error");
        debug!("HTTP call to get mint_quote_onchain on {url}");

        let res = self.secondary.get(url).send().await?;
        let response: wire_mint::OnchainMintQuoteResponse = res.json().await?;
        Ok(response)
    }

    async fn post_mint_onchain(
        &self,
        req: wire_mint::OnchainMintRequest,
    ) -> Result<wire_mint::MintResponse> {
        let url = self
            .url
            .join("v1/mint/onchain")
            .expect("mint_onchain url error");
        debug!("HTTP call to mint_onchain on {url}");

        let res = self.secondary.post(url).json(&req).send().await?;
        let response: wire_mint::MintResponse = res.json().await?;
        Ok(response)
    }

    async fn post_protest_mint(
        &self,
        req: wire_mint::MintProtestRequest,
    ) -> Result<wire_mint::MintProtestResponse> {
        let url = self
            .url
            .join("v1/protest/mint")
            .expect("protest_mint url error");
        debug!("HTTP call to protest_mint on {url}");

        let res = self
            .secondary
            .post(url)
            .json(&req)
            .send()
            .await?
            .error_for_status()?;
        let response: wire_mint::MintProtestResponse = res.json().await?;
        Ok(response)
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
    clowder: ClowderClient,
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
            clowder,
        } = client;
        Self {
            main,
            url,
            secondary,
            clowder,
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
        debug!("HTTP call to get_mint_keys on sentinel");
        self.main.get_mint_keys().await
    }
    async fn post_restore(
        &self,
        request: cashu::RestoreRequest,
    ) -> CdkResult<cashu::RestoreResponse> {
        debug!("HTTP call to post_restore on sentinel");
        self.main.post_restore(request).await
    }
    async fn post_check_state(
        &self,
        request: cashu::CheckStateRequest,
    ) -> CdkResult<cashu::CheckStateResponse> {
        debug!("HTTP call to post_check_state on sentinel");
        self.main.post_check_state(request).await
    }
    async fn get_mint_keyset(&self, keyset_id: cashu::Id) -> CdkResult<cashu::KeySet> {
        debug!("HTTP call to get_mint_keyset on sentinel");
        self.main.get_mint_keyset(keyset_id).await
    }
    async fn get_mint_keysets(&self) -> CdkResult<cashu::KeysetResponse> {
        debug!("HTTP call to get_mint_keysets on sentinel");
        self.main.get_mint_keysets().await
    }
    async fn get_mint_info(&self) -> CdkResult<cashu::MintInfo> {
        debug!("HTTP call to get_mint_info on sentinel");
        self.main.get_mint_info().await
    }

    async fn post_swap(&self, request: cashu::SwapRequest) -> CdkResult<cashu::SwapResponse> {
        debug!("HTTP call to post_swap on sentinel");
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
        debug!("HTTP call to post_mint on sentinel");
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
        debug!("HTTP call to post_mint_quote on sentinel");
        self.main.post_mint_quote(request).await
    }

    async fn post_melt(
        &self,
        request: cashu::MeltRequest<String>,
    ) -> CdkResult<cashu::MeltQuoteBolt11Response<String>> {
        debug!("HTTP call to post_melt on sentinel");
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
        debug!("HTTP call to get_melt_quote_status on sentinel");
        self.main.get_melt_quote_status(quote_id).await
    }
    async fn post_melt_quote(
        &self,
        request: cashu::MeltQuoteBolt11Request,
    ) -> CdkResult<cashu::MeltQuoteBolt11Response<String>> {
        debug!("HTTP call to post_melt_quote on sentinel");
        self.main.post_melt_quote(request).await
    }
    async fn get_mint_quote_status(
        &self,
        quote_id: &str,
    ) -> CdkResult<cashu::MintQuoteBolt11Response<String>> {
        debug!("HTTP call to get_mint_quote_status on sentinel");
        self.main.get_mint_quote_status(quote_id).await
    }

    async fn post_mint_bolt12_quote(
        &self,
        request: cashu::MintQuoteBolt12Request,
    ) -> CdkResult<cashu::MintQuoteBolt12Response<String>> {
        debug!("HTTP call to post_mint_bolt12_quote on sentinel");
        self.main.post_mint_bolt12_quote(request).await
    }
    async fn get_mint_quote_bolt12_status(
        &self,
        quote_id: &str,
    ) -> CdkResult<cashu::MintQuoteBolt12Response<String>> {
        debug!("HTTP call to get_mint_quote_bolt12_status on sentinel");
        self.main.get_mint_quote_bolt12_status(quote_id).await
    }
    async fn post_melt_bolt12_quote(
        &self,
        request: cashu::MeltQuoteBolt12Request,
    ) -> CdkResult<cashu::MeltQuoteBolt11Response<String>> {
        debug!("HTTP call to post_melt_bolt12_quote on sentinel");
        self.main.post_melt_bolt12_quote(request).await
    }
    async fn get_melt_bolt12_quote_status(
        &self,
        quote_id: &str,
    ) -> CdkResult<cashu::MeltQuoteBolt11Response<String>> {
        debug!("HTTP call to get_melt_bolt12_quote_status on sentinel");
        self.main.get_melt_bolt12_quote_status(quote_id).await
    }
    async fn post_melt_bolt12(
        &self,
        request: cashu::MeltRequest<String>,
    ) -> CdkResult<cashu::MeltQuoteBolt11Response<String>> {
        debug!("HTTP call to post_melt_bolt12 on sentinel");
        self.main.post_melt_bolt12(request).await
    }
}

#[async_trait]
impl ClowderMintConnector for SentinelClient {
    fn mint_url(&self) -> cashu::MintUrl {
        cashu::MintUrl::from_str(self.url.as_str())
            .expect("cashu::MintUrl is as good as reqwest::Url")
    }

    /// Active alpha keysets
    async fn get_alpha_keysets(
        &self,
        alpha_id: secp256k1::PublicKey,
    ) -> CdkResult<Vec<cashu::KeySet>> {
        debug!("Clowder client call to get_alpha_keysets on sentinel");
        let response = self
            .clowder
            .get_active_keysets(alpha_id)
            .await
            .map_err(clowder_err_to_cdk)?;
        Ok(response.keysets)
    }

    /// Is Alpha Offline
    async fn get_alpha_offline(&self, alpha_id: secp256k1::PublicKey) -> CdkResult<bool> {
        debug!("Clowder client call to get_alpha_offline on sentinel");
        let response = self
            .clowder
            .get_offline(alpha_id)
            .await
            .map_err(clowder_err_to_cdk)?;
        Ok(response.offline)
    }

    /// Determines the status of a mint from the view of the requested Beta
    async fn get_alpha_status(
        &self,
        alpha_id: secp256k1::PublicKey,
    ) -> CdkResult<wire_clowder::AlphaStateResponse> {
        debug!("Clowder client call to get_alpha_status on sentinel");
        self.clowder
            .get_status(alpha_id)
            .await
            .map_err(clowder_err_to_cdk)
    }

    /// Determines the substitute beta of an alpha mint
    async fn get_alpha_substitute(
        &self,
        alpha_id: secp256k1::PublicKey,
    ) -> CdkResult<wire_clowder::ConnectedMintResponse> {
        debug!("Clowder client call to get_alpha_substitute on sentinel");
        self.clowder
            .get_substitute(alpha_id)
            .await
            .map_err(clowder_err_to_cdk)
    }

    async fn get_clowder_betas(&self) -> CdkResult<Vec<cashu::MintUrl>> {
        debug!("Clowder client call to get_clowder_betas on sentinel");
        let response = self.clowder.get_betas().await.map_err(clowder_err_to_cdk)?;
        Ok(response.mints.into_iter().map(|m| m.mint).collect())
    }

    async fn post_offline_exchange(
        &self,
        proofs: Vec<wire_keys::ProofFingerprint>,
        locks: Vec<bitcoin::hashes::sha256::Hash>,
        wallet_pubkey: secp256k1::PublicKey,
    ) -> CdkResult<Vec<Proof>> {
        debug!("Clowder client call to post_offline_exchange on sentinel");
        let wallet_pk = cashu::PublicKey::from_slice(&wallet_pubkey.serialize())
            .map_err(|e| CdkError::Custom(e.to_string()))?;
        let request = wire_exchange::OfflineExchangeRequest {
            fingerprints: proofs,
            hashes: locks,
            wallet_pk,
        };
        let response = self
            .clowder
            .post_offline_exchange(request)
            .await
            .map_err(clowder_err_to_cdk)?;
        let serialized = BASE64_STANDARD
            .decode(&response.content)
            .map_err(|e| CdkError::Custom(e.to_string()))?;
        let payload: wire_exchange::OfflineExchangePayload =
            borsh::from_slice(&serialized).map_err(|e| CdkError::Custom(e.to_string()))?;
        Ok(payload.proofs)
    }

    async fn post_online_exchange(
        &self,
        alpha_proofs: Vec<Proof>,
        exchange_path: Vec<secp256k1::PublicKey>,
    ) -> CdkResult<Vec<Proof>> {
        debug!("Clowder client call to post_online_exchange on sentinel");
        let request = wire_exchange::OnlineExchangeRequest {
            proofs: alpha_proofs,
            exchange_path,
        };
        let response = self
            .clowder
            .post_online_exchange(request)
            .await
            .map_err(clowder_err_to_cdk)?;
        Ok(response.proofs)
    }

    async fn get_clowder_id(&self) -> CdkResult<secp256k1::PublicKey> {
        debug!("Clowder client call to get_clowder_id on sentinel");
        let response = self.clowder.get_info().await.map_err(clowder_err_to_cdk)?;
        Ok(*response.node_id)
    }

    async fn post_clowder_path(
        &self,
        origin_mint_url: cashu::MintUrl,
    ) -> CdkResult<ConnectedMintsResponse> {
        debug!("Clowder client call to post_clowder_path on sentinel");
        self.clowder
            .post_path(origin_mint_url)
            .await
            .map_err(clowder_err_to_cdk)
    }

    async fn post_swap_commitment(
        &self,
        inputs: Vec<cashu::Proof>,
        outputs: Vec<cashu::BlindedMessage>,
        expiry_seconds: chrono::TimeDelta,
        alpha_pk: secp256k1::PublicKey,
    ) -> Result<SwapCommitmentResult> {
        let url = self
            .url
            .join("v1/swap/commitment")
            .expect("post_swap_commitment url error");
        debug!("HTTP call to post_swap_commitment on sentinel {url}");

        let ephemeral_keypair =
            secp256k1::Keypair::new_global(&mut bitcoin::secp256k1::rand::thread_rng());
        let ephemeral_secret = secp256k1::SecretKey::from_keypair(&ephemeral_keypair);

        let fingerprints: Vec<_> = inputs
            .into_iter()
            .map(wire_keys::ProofFingerprint::try_from)
            .collect::<std::result::Result<_, cashu::nut00::Error>>()?;
        let expiry_height = (chrono::Utc::now() + expiry_seconds).timestamp() as u64;
        let body = wire_swap::SwapCommitmentRequestBody {
            inputs: fingerprints,
            outputs,
            expiry_height,
        };

        let (content, wallet_signature) =
            bcr_common::core::signature::serialize_n_schnorr_sign_borsh_msg(
                &body,
                &ephemeral_keypair,
            )?;
        let wallet_key =
            cashu::PublicKey::from(secp256k1::PublicKey::from_keypair(&ephemeral_keypair));

        let request = wire_swap::SwapCommitmentRequest {
            content: content.clone(),
            wallet_key,
            wallet_signature,
        };

        let response = self
            .secondary
            .post(url)
            .json(&request)
            .send()
            .await?
            .error_for_status()?;
        let response: wire_swap::SwapCommitmentResponse = response.json().await?;

        bcr_common::core::signature::schnorr_verify_b64(
            &response.content,
            &response.commitment,
            &alpha_pk.x_only_public_key().0,
        )?;

        let inputs_ys: Vec<cashu::PublicKey> = body.inputs.into_iter().map(|fp| fp.y).collect();
        Ok(SwapCommitmentResult {
            inputs_ys,
            outputs: body.outputs,
            expiry_height,
            commitment: response.commitment,
            ephemeral_secret,
            body_content: content,
            wallet_key,
        })
    }

    async fn post_swap_committed(
        &self,
        request: wire_swap::SwapRequest,
    ) -> Result<wire_swap::SwapResponse> {
        let url = self.url.join("v1/swap").expect("post_swap_committed url error");
        debug!("HTTP call to post_swap_committed on sentinel {url}");
        let response = self
            .secondary
            .post(url)
            .json(&request)
            .send()
            .await?
            .error_for_status()?;
        let response: wire_swap::SwapResponse = response.json().await?;

        // Send sentinel event
        if let Some(sentinel_url) = self.random_sentinel() {
            let event_url = Self::sentinel_ep(sentinel_url);
            let event = wire_clowder::WalletEvent::Swap {
                minted: response.signatures.clone(),
            };
            let resp = self.secondary.post(event_url).json(&event).send().await;
            if let Err(e) = resp {
                tracing::error!("Failed to send swap event to sentinel {sentinel_url}: {e}");
            }
        }
        Ok(response)
    }

    async fn post_protest_swap(
        &self,
        req: wire_swap::SwapProtestRequest,
    ) -> Result<wire_swap::SwapProtestResponse> {
        let url = self
            .url
            .join("v1/protest/swap")
            .expect("protest_swap url error");
        debug!("HTTP call on sentinel to protest_swap on {url}");
        let res = self
            .secondary
            .post(url)
            .json(&req)
            .send()
            .await?
            .error_for_status()?;
        let response: wire_swap::SwapProtestResponse = res.json().await?;
        Ok(response)
    }

    async fn post_melt_quote_onchain(
        &self,
        req: wire_melt::MeltQuoteOnchainRequest,
    ) -> Result<wire_melt::MeltQuoteOnchainResponse> {
        let url = self
            .url
            .join("v1/melt/quote/onchain")
            .expect("melt_quote_onchain url error");
        debug!("HTTP call on sentinel to melt_quote_onchain on {url}");

        let res = self.secondary.post(url).json(&req).send().await?;
        let response: wire_melt::MeltQuoteOnchainResponse = res.json().await?;
        Ok(response)
    }

    async fn post_melt_onchain(
        &self,
        req: cashu::MeltRequest<String>,
    ) -> Result<wire_melt::MeltOnchainResponse> {
        let url = self
            .url
            .join("v1/melt/onchain")
            .expect("melt_onchain url error");
        debug!("HTTP call on sentinel to melt_onchain on {url}");

        let res = self.secondary.post(url).json(&req).send().await?;
        let response: wire_melt::MeltOnchainResponse = res.json().await?;
        Ok(response)
    }

    async fn post_mint_quote_onchain(
        &self,
        req: wire_mint::OnchainMintQuoteRequest,
    ) -> Result<wire_mint::OnchainMintQuoteResponse> {
        let url = self
            .url
            .join("v1/mint/quote/onchain")
            .expect("mint_quote_onchain url error");
        debug!("HTTP call on sentinel to mint_quote_onchain on {url}");

        let res = self.secondary.post(url).json(&req).send().await?;
        let response: wire_mint::OnchainMintQuoteResponse = res.json().await?;
        Ok(response)
    }

    async fn get_mint_quote_onchain(
        &self,
        quote_id: String,
    ) -> Result<wire_mint::OnchainMintQuoteResponse> {
        let url = self
            .url
            .join(&format!("v1/mint/quote/onchain/{quote_id}"))
            .expect("get mint_onchain url error");
        debug!("HTTP call on sentinel to get mint_quote_onchain on {url}");

        let res = self.secondary.get(url).send().await?;
        let response: wire_mint::OnchainMintQuoteResponse = res.json().await?;
        Ok(response)
    }

    async fn post_mint_onchain(
        &self,
        req: wire_mint::OnchainMintRequest,
    ) -> Result<wire_mint::MintResponse> {
        let url = self
            .url
            .join("v1/mint/onchain")
            .expect("mint_onchain url error");
        debug!("HTTP call on sentinel to mint_onchain on {url}");

        let res = self.secondary.post(url).json(&req).send().await?;
        let response: wire_mint::MintResponse = res.json().await?;
        Ok(response)
    }

    async fn post_protest_mint(
        &self,
        req: wire_mint::MintProtestRequest,
    ) -> Result<wire_mint::MintProtestResponse> {
        let url = self
            .url
            .join("v1/protest/mint")
            .expect("protest_mint url error");
        debug!("HTTP call on sentinel to protest_mint on {url}");

        let res = self
            .secondary
            .post(url)
            .json(&req)
            .send()
            .await?
            .error_for_status()?;
        let response: wire_mint::MintProtestResponse = res.json().await?;
        Ok(response)
    }
}
