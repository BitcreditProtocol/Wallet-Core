use crate::error::Result;
use async_trait::async_trait;
use bcr_common::{
    cashu::{self, Proof},
    client::{
        core::web_ep as CoreEp,
        mint::{Client as MintClient, Error as MintError, Result as MintResult},
        treasury::web_ep as TreasuryEp,
    },
    wire::{
        clowder::{self as wire_clowder, ConnectedMintsResponse},
        exchange as wire_exchange,
        keys::{self as wire_keys, KeysetInfoFilters},
        melt as wire_melt, mint as wire_mint, swap as wire_swap,
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
    pub expiry: u64,
    pub commitment: secp256k1::schnorr::Signature,
    pub ephemeral_secret: secp256k1::SecretKey,
    pub body_content: String,
    pub wallet_key: cashu::PublicKey,
}

pub struct MeltQuoteResult {
    pub quote_id: uuid::Uuid,
    pub expiry: u64,
    pub commitment: secp256k1::schnorr::Signature,
    pub ephemeral_secret: secp256k1::SecretKey,
    pub body_content: String,
}

async fn post_swap_commitment_inner(
    http_client: &reqwest::Client,
    url: reqwest::Url,
    inputs: Vec<cashu::Proof>,
    outputs: Vec<cashu::BlindedMessage>,
    expiry_seconds: chrono::TimeDelta,
    alpha_pk: secp256k1::PublicKey,
) -> Result<SwapCommitmentResult> {
    let ephemeral_keypair =
        secp256k1::Keypair::new_global(&mut bitcoin::secp256k1::rand::thread_rng());
    let ephemeral_secret = secp256k1::SecretKey::from_keypair(&ephemeral_keypair);
    let wallet_pk = secp256k1::PublicKey::from_keypair(&ephemeral_keypair);
    let wallet_key = cashu::PublicKey::from(wallet_pk);

    let fingerprints: Vec<_> = inputs
        .into_iter()
        .map(wire_keys::ProofFingerprint::try_from)
        .collect::<std::result::Result<_, cashu::nut00::Error>>()?;
    let expiry = (chrono::Utc::now() + expiry_seconds).timestamp() as u64;

    let request = wire_swap::SwapCommitmentRequest {
        inputs: fingerprints,
        outputs,
        expiry,
        wallet_key: wallet_pk,
    };

    let response = http_client.post(url).json(&request).send().await?;
    match response.error_for_status_ref() {
        Ok(_) => {
            let wire_swap::SwapCommitmentResponse {
                content: committed_content,
                commitment,
            } = response.json().await?;

            bcr_common::core::signature::schnorr_verify_b64(
                &committed_content,
                &commitment,
                &alpha_pk.x_only_public_key().0,
            )?;

            let inputs_ys: Vec<cashu::PublicKey> = request.inputs.iter().map(|fp| fp.y).collect();
            Ok(SwapCommitmentResult {
                inputs_ys,
                outputs: request.outputs,
                expiry,
                commitment,
                ephemeral_secret,
                body_content: committed_content,
                wallet_key,
            })
        }
        Err(err) => {
            let status = err.status();
            let body = response.text().await.unwrap_or_default();

            tracing::error!(
                "post_swap_commitment failed: status={:?}, body={}",
                status,
                body
            );

            Err(err.into())
        }
    }
}

async fn post_melt_quote_onchain_inner(
    http_client: &reqwest::Client,
    url: reqwest::Url,
    inputs: Vec<cashu::Proof>,
    address: bitcoin::Address<bitcoin::address::NetworkUnchecked>,
    amount: bitcoin::Amount,
    alpha_pk: secp256k1::PublicKey,
) -> Result<MeltQuoteResult> {
    let ephemeral_keypair =
        secp256k1::Keypair::new_global(&mut bitcoin::secp256k1::rand::thread_rng());
    let ephemeral_secret = secp256k1::SecretKey::from_keypair(&ephemeral_keypair);
    let wallet_key = cashu::PublicKey::from(secp256k1::PublicKey::from_keypair(&ephemeral_keypair));

    let fingerprints: Vec<_> = inputs
        .into_iter()
        .map(wire_keys::ProofFingerprint::try_from)
        .collect::<std::result::Result<_, cashu::nut00::Error>>()?;

    let request = wire_melt::MeltQuoteOnchainRequest {
        inputs: fingerprints,
        address,
        amount,
        wallet_key,
    };

    let response = http_client.post(url).json(&request).send().await?;
    match response.error_for_status_ref() {
        Ok(_) => {
            let wire_melt::MeltQuoteOnchainResponse {
                content: response_content,
                commitment,
            } = response.json().await?;

            bcr_common::core::signature::schnorr_verify_b64(
                &response_content,
                &commitment,
                &alpha_pk.x_only_public_key().0,
            )?;

            let response_body: wire_melt::MeltQuoteOnchainResponseBody =
                bcr_common::core::signature::deserialize_borsh_msg(&response_content)?;

            Ok(MeltQuoteResult {
                quote_id: response_body.quote,
                expiry: response_body.expiry,
                commitment,
                ephemeral_secret,
                body_content: response_content,
            })
        }
        Err(err) => {
            let status = err.status();
            let body = response.text().await.unwrap_or_default();

            tracing::error!(
                "post_melt_quote_onchain failed: status={:?}, body={}",
                status,
                body
            );

            Err(err.into())
        }
    }
}

fn convert_mint_url(mint_url: cashu::MintUrl) -> MintResult<reqwest::Url> {
    reqwest::Url::from_str(&mint_url.to_string()).map_err(|e| MintError::Internal(e.to_string()))
}

#[async_trait]
pub trait ClowderMintConnector: SendSync {
    fn mint_url(&self) -> cashu::MintUrl;
    async fn post_restore(
        &self,
        request: cashu::RestoreRequest,
    ) -> MintResult<Vec<(cashu::BlindedMessage, cashu::BlindSignature)>>;
    async fn post_check_state(
        &self,
        request: cashu::CheckStateRequest,
    ) -> MintResult<Vec<cashu::ProofState>>;
    async fn get_mint_keyset(&self, keyset_id: cashu::Id) -> MintResult<cashu::KeySet>;
    async fn get_mint_keysets(&self) -> MintResult<Vec<cashu::KeySetInfo>>;
    async fn get_clowder_betas(&self) -> MintResult<Vec<cashu::MintUrl>>;
    async fn post_online_exchange(
        &self,
        alpha_proofs: Vec<Proof>,
        exchange_path: Vec<secp256k1::PublicKey>,
    ) -> MintResult<Vec<Proof>>;
    async fn get_clowder_id(&self) -> MintResult<secp256k1::PublicKey>;
    async fn post_clowder_path(
        &self,
        origin_mint_url: cashu::MintUrl,
    ) -> MintResult<ConnectedMintsResponse>;
    async fn get_alpha_keysets(
        &self,
        alpha_id: secp256k1::PublicKey,
    ) -> MintResult<Vec<cashu::KeySet>>;
    async fn get_alpha_offline(&self, alpha_id: secp256k1::PublicKey) -> MintResult<bool>;
    async fn get_alpha_status(
        &self,
        alpha_id: secp256k1::PublicKey,
    ) -> MintResult<wire_clowder::AlphaStateResponse>;
    async fn get_alpha_substitute(
        &self,
        alpha_id: secp256k1::PublicKey,
    ) -> MintResult<wire_clowder::ConnectedMintResponse>;
    async fn post_offline_exchange(
        &self,
        proofs: Vec<wire_keys::ProofFingerprint>,
        locks: Vec<bitcoin::hashes::sha256::Hash>,
        wallet_pubkey: secp256k1::PublicKey,
    ) -> MintResult<Vec<Proof>>;
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
        inputs: Vec<cashu::Proof>,
        address: bitcoin::Address<bitcoin::address::NetworkUnchecked>,
        amount: bitcoin::Amount,
        alpha_pk: secp256k1::PublicKey,
    ) -> Result<MeltQuoteResult>;
    async fn post_melt_onchain(
        &self,
        req: wire_melt::MeltOnchainRequest,
    ) -> Result<wire_melt::MeltOnchainResponse>;
    async fn post_protest_melt(
        &self,
        req: wire_melt::MeltProtestRequest,
    ) -> Result<wire_melt::MeltProtestResponse>;
    async fn post_mint_quote_onchain(
        &self,
        req: wire_mint::OnchainMintQuoteRequest,
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
    main: MintClient,
    url: reqwest::Url,
    secondary: reqwest::Client,
}

impl HttpClientExt {
    pub fn new(cdk_url: cashu::MintUrl) -> Self {
        let mint_url = reqwest::Url::parse(&cdk_url.to_string())
            .expect("cashu::MintUrl is as good as reqwest::Url");
        Self {
            main: MintClient::new(mint_url.clone()),
            url: mint_url,
            secondary: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl ClowderMintConnector for HttpClientExt {
    fn mint_url(&self) -> cashu::MintUrl {
        cashu::MintUrl::from_str(self.url.as_str())
            .expect("cashu::MintUrl is as good as reqwest::Url")
    }

    async fn post_restore(
        &self,
        request: cashu::RestoreRequest,
    ) -> MintResult<Vec<(cashu::BlindedMessage, cashu::BlindSignature)>> {
        debug!("HTTP call to post_restore");
        self.main.restore(request.outputs).await
    }

    async fn post_check_state(
        &self,
        request: cashu::CheckStateRequest,
    ) -> MintResult<Vec<cashu::ProofState>> {
        debug!("HTTP call to post_check_state");
        self.main.check_state(request.ys).await
    }

    async fn get_mint_keyset(&self, keyset_id: cashu::Id) -> MintResult<cashu::KeySet> {
        debug!("HTTP call to get_mint_keyset");
        self.main.keys(keyset_id).await
    }

    async fn get_mint_keysets(&self) -> MintResult<Vec<cashu::KeySetInfo>> {
        debug!("HTTP call to get_mint_keysets");
        self.main
            .list_keyset_info(KeysetInfoFilters::default())
            .await
    }

    /// Active alpha keysets
    async fn get_alpha_keysets(
        &self,
        alpha_id: secp256k1::PublicKey,
    ) -> MintResult<Vec<cashu::KeySet>> {
        debug!("Clowder client call to get_alpha_keysets for {alpha_id}");
        let response = self.main.get_active_keysets(&alpha_id).await?;
        Ok(response.keysets)
    }

    /// Is Alpha Offline
    async fn get_alpha_offline(&self, alpha_id: secp256k1::PublicKey) -> MintResult<bool> {
        debug!("Clowder client call to get_alpha_offline for {alpha_id}");
        let response = self.main.get_offline(&alpha_id).await?;
        Ok(response.offline)
    }

    /// Determines the status of a mint from the view of the requested Beta
    async fn get_alpha_status(
        &self,
        alpha_id: secp256k1::PublicKey,
    ) -> MintResult<wire_clowder::AlphaStateResponse> {
        debug!(
            "Clowder client call to get_alpha_status on {} for {alpha_id}",
            self.mint_url().to_string()
        );
        self.main.get_status(&alpha_id).await
    }

    /// Determines the substitute beta of an alpha mint
    async fn get_alpha_substitute(
        &self,
        alpha_id: secp256k1::PublicKey,
    ) -> MintResult<wire_clowder::ConnectedMintResponse> {
        debug!(
            "Clowder client call to get_alpha_substitute on {} for {alpha_id}",
            self.mint_url().to_string()
        );
        self.main.get_substitute(&alpha_id).await
    }

    async fn get_clowder_betas(&self) -> MintResult<Vec<cashu::MintUrl>> {
        debug!("Clowder client call to get_clowder_betas");
        let response = self.main.get_betas().await?;
        Ok(response.mints.into_iter().map(|m| m.mint).collect())
    }

    async fn post_offline_exchange(
        &self,
        proofs: Vec<wire_keys::ProofFingerprint>,
        locks: Vec<bitcoin::hashes::sha256::Hash>,
        wallet_pubkey: secp256k1::PublicKey,
    ) -> MintResult<Vec<Proof>> {
        debug!("Clowder client call to post_offline_exchange");
        let wallet_pk = cashu::PublicKey::from_slice(&wallet_pubkey.serialize())
            .map_err(|e| MintError::Internal(e.to_string()))?;
        let request = wire_exchange::OfflineExchangeRequest {
            fingerprints: proofs,
            hashes: locks,
            wallet_pk,
        };
        let response = self.main.post_offline_exchange(request).await?;
        let serialized = BASE64_STANDARD
            .decode(&response.content)
            .map_err(|e| MintError::Internal(e.to_string()))?;
        let payload: wire_exchange::OfflineExchangePayload =
            borsh::from_slice(&serialized).map_err(|e| MintError::Internal(e.to_string()))?;
        Ok(payload.proofs)
    }

    async fn post_online_exchange(
        &self,
        alpha_proofs: Vec<Proof>,
        exchange_path: Vec<secp256k1::PublicKey>,
    ) -> MintResult<Vec<Proof>> {
        debug!("Clowder client call to post_online_exchange");
        let request = wire_exchange::OnlineExchangeRequest {
            proofs: alpha_proofs,
            exchange_path,
        };
        let response = self.main.post_online_exchange(request).await?;
        Ok(response.proofs)
    }

    async fn get_clowder_id(&self) -> MintResult<secp256k1::PublicKey> {
        debug!("Clowder client call to get_clowder_id");
        let response = self.main.get_info().await?;
        Ok(*response.node_id)
    }

    async fn post_clowder_path(
        &self,
        origin_mint_url: cashu::MintUrl,
    ) -> MintResult<ConnectedMintsResponse> {
        debug!("Clowder client call to post_clowder_path for mint url {origin_mint_url}");
        self.main
            .post_path(convert_mint_url(origin_mint_url)?)
            .await
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
            .join(CoreEp::SWAP_COMMIT_V1_EXT)
            .expect("post_swap_commitment url error");
        debug!("HTTP call to post_swap_commitment on {url}");
        post_swap_commitment_inner(
            &self.secondary,
            url,
            inputs,
            outputs,
            expiry_seconds,
            alpha_pk,
        )
        .await
    }

    async fn post_swap_committed(
        &self,
        request: wire_swap::SwapRequest,
    ) -> Result<wire_swap::SwapResponse> {
        let url = self
            .url
            .join(CoreEp::SWAP_V1_EXT)
            .expect("post_swap_committed url error");
        debug!("HTTP call to post_swap_committed on {url}");
        let res = self.secondary.post(url).json(&request).send().await?;
        match res.error_for_status_ref() {
            Ok(_) => {
                let response: wire_swap::SwapResponse = res.json().await?;
                Ok(response)
            }
            Err(err) => {
                let status = err.status();
                let body = res.text().await.unwrap_or_default();

                tracing::error!(
                    "post_swap_committed failed: status={:?}, body={}",
                    status,
                    body
                );

                Err(err.into())
            }
        }
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
        let res = self.secondary.post(url).json(&req).send().await?;
        match res.error_for_status_ref() {
            Ok(_) => {
                let response: wire_swap::SwapProtestResponse = res.json().await?;
                Ok(response)
            }
            Err(err) => {
                let status = err.status();
                let body = res.text().await.unwrap_or_default();

                tracing::error!(
                    "post_protest_swap failed: status={:?}, body={}",
                    status,
                    body
                );

                Err(err.into())
            }
        }
    }

    async fn post_melt_quote_onchain(
        &self,
        inputs: Vec<cashu::Proof>,
        address: bitcoin::Address<bitcoin::address::NetworkUnchecked>,
        amount: bitcoin::Amount,
        alpha_pk: secp256k1::PublicKey,
    ) -> Result<MeltQuoteResult> {
        let url = self
            .url
            .join("v1/melt/quote/onchain")
            .expect("melt_quote_onchain url error");
        debug!("HTTP call to melt_quote_onchain on {url}");
        post_melt_quote_onchain_inner(&self.secondary, url, inputs, address, amount, alpha_pk).await
    }

    async fn post_melt_onchain(
        &self,
        req: wire_melt::MeltOnchainRequest,
    ) -> Result<wire_melt::MeltOnchainResponse> {
        let url = self
            .url
            .join("v1/melt/onchain")
            .expect("melt_onchain url error");
        debug!("HTTP call to melt_onchain on {url}");

        let res = self.secondary.post(url).json(&req).send().await?;
        match res.error_for_status_ref() {
            Ok(_) => {
                let response: wire_melt::MeltOnchainResponse = res.json().await?;
                Ok(response)
            }
            Err(err) => {
                let status = err.status();
                let body = res.text().await.unwrap_or_default();

                tracing::error!(
                    "post_melt_onchain failed: status={:?}, body={}",
                    status,
                    body
                );

                Err(err.into())
            }
        }
    }

    async fn post_protest_melt(
        &self,
        req: wire_melt::MeltProtestRequest,
    ) -> Result<wire_melt::MeltProtestResponse> {
        let url = self
            .url
            .join("v1/protest/melt")
            .expect("protest_melt url error");
        debug!("HTTP call to protest_melt on {url}");
        let res = self.secondary.post(url).json(&req).send().await?;
        match res.error_for_status_ref() {
            Ok(_) => {
                let response: wire_melt::MeltProtestResponse = res.json().await?;
                Ok(response)
            }
            Err(err) => {
                let status = err.status();
                let body = res.text().await.unwrap_or_default();

                tracing::error!(
                    "post_protest_melt failed: status={:?}, body={}",
                    status,
                    body
                );

                Err(err.into())
            }
        }
    }

    async fn post_mint_quote_onchain(
        &self,
        req: wire_mint::OnchainMintQuoteRequest,
    ) -> Result<wire_mint::OnchainMintQuoteResponse> {
        let url = self
            .url
            .join(TreasuryEp::MINTQUOTE_ONCHAIN_V1_EXT)
            .expect("mint_quote_onchain url error");
        debug!("HTTP call to mint_quote_onchain on {url}");

        let res = self.secondary.post(url).json(&req).send().await?;
        match res.error_for_status_ref() {
            Ok(_) => {
                let response: wire_mint::OnchainMintQuoteResponse = res.json().await?;
                Ok(response)
            }
            Err(err) => {
                let status = err.status();
                let body = res.text().await.unwrap_or_default();

                tracing::error!(
                    "post_mint_quote_onchain failed: status={:?}, body={}",
                    status,
                    body
                );

                Err(err.into())
            }
        }
    }

    async fn post_mint_onchain(
        &self,
        req: wire_mint::OnchainMintRequest,
    ) -> Result<wire_mint::MintResponse> {
        let url = self
            .url
            .join(TreasuryEp::MINT_ONCHAIN_V1_EXT)
            .expect("mint_onchain url error");
        debug!("HTTP call to mint_onchain on {url}");

        let res = self.secondary.post(url).json(&req).send().await?;

        match res.error_for_status_ref() {
            Ok(_) => {
                let response: wire_mint::MintResponse = res.json().await?;
                Ok(response)
            }
            Err(err) => {
                let status = err.status();
                let body = res.text().await.unwrap_or_default();

                tracing::error!(
                    "post_mint_onchain failed: status={:?}, body={}",
                    status,
                    body
                );

                Err(err.into())
            }
        }
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

        let res = self.secondary.post(url).json(&req).send().await?;
        match res.error_for_status_ref() {
            Ok(_) => {
                let response: wire_mint::MintProtestResponse = res.json().await?;
                Ok(response)
            }
            Err(err) => {
                let status = err.status();
                let body = res.text().await.unwrap_or_default();

                tracing::error!(
                    "post_protest_mint failed: status={:?}, body={}",
                    status,
                    body
                );

                Err(err.into())
            }
        }
    }
}

/// A client wrapper that forwards wallet events to sentinel nodes.
///
/// This client wraps the standard HTTP client and sends monitoring events
/// to randomly selected sentinel nodes after performing mint, swap, and melt operations.
#[derive(Debug, Clone)]
pub struct SentinelClient {
    main: MintClient,
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
impl ClowderMintConnector for SentinelClient {
    fn mint_url(&self) -> cashu::MintUrl {
        cashu::MintUrl::from_str(self.url.as_str())
            .expect("cashu::MintUrl is as good as reqwest::Url")
    }

    async fn post_restore(
        &self,
        request: cashu::RestoreRequest,
    ) -> MintResult<Vec<(cashu::BlindedMessage, cashu::BlindSignature)>> {
        debug!("HTTP call to post_restore on sentinel");
        self.main.restore(request.outputs).await
    }
    async fn post_check_state(
        &self,
        request: cashu::CheckStateRequest,
    ) -> MintResult<Vec<cashu::ProofState>> {
        debug!("HTTP call to post_check_state on sentinel");
        self.main.check_state(request.ys).await
    }
    async fn get_mint_keyset(&self, keyset_id: cashu::Id) -> MintResult<cashu::KeySet> {
        debug!("HTTP call to get_mint_keyset on sentinel");
        self.main.keys(keyset_id).await
    }
    async fn get_mint_keysets(&self) -> MintResult<Vec<cashu::KeySetInfo>> {
        debug!("HTTP call to get_mint_keysets on sentinel");
        self.main
            .list_keyset_info(KeysetInfoFilters::default())
            .await
    }

    /// Active alpha keysets
    async fn get_alpha_keysets(
        &self,
        alpha_id: secp256k1::PublicKey,
    ) -> MintResult<Vec<cashu::KeySet>> {
        debug!("Clowder client call to get_alpha_keysets on sentinel for {alpha_id}");
        let response = self.main.get_active_keysets(&alpha_id).await?;
        Ok(response.keysets)
    }

    /// Is Alpha Offline
    async fn get_alpha_offline(&self, alpha_id: secp256k1::PublicKey) -> MintResult<bool> {
        debug!("Clowder client call to get_alpha_offline on sentinel for {alpha_id}");
        let response = self.main.get_offline(&alpha_id).await?;
        Ok(response.offline)
    }

    /// Determines the status of a mint from the view of the requested Beta
    async fn get_alpha_status(
        &self,
        alpha_id: secp256k1::PublicKey,
    ) -> MintResult<wire_clowder::AlphaStateResponse> {
        debug!("Clowder client call to get_alpha_status on sentinel");
        self.main.get_status(&alpha_id).await
    }

    /// Determines the substitute beta of an alpha mint
    async fn get_alpha_substitute(
        &self,
        alpha_id: secp256k1::PublicKey,
    ) -> MintResult<wire_clowder::ConnectedMintResponse> {
        debug!("Clowder client call to get_alpha_substitute on sentinel");
        self.main.get_substitute(&alpha_id).await
    }

    async fn get_clowder_betas(&self) -> MintResult<Vec<cashu::MintUrl>> {
        debug!("Clowder client call to get_clowder_betas on sentinel");
        let response = self.main.get_betas().await?;
        Ok(response.mints.into_iter().map(|m| m.mint).collect())
    }

    async fn post_offline_exchange(
        &self,
        proofs: Vec<wire_keys::ProofFingerprint>,
        locks: Vec<bitcoin::hashes::sha256::Hash>,
        wallet_pubkey: secp256k1::PublicKey,
    ) -> MintResult<Vec<Proof>> {
        debug!("Clowder client call to post_offline_exchange on sentinel");
        let wallet_pk = cashu::PublicKey::from_slice(&wallet_pubkey.serialize())
            .map_err(|e| MintError::Internal(e.to_string()))?;
        let request = wire_exchange::OfflineExchangeRequest {
            fingerprints: proofs,
            hashes: locks,
            wallet_pk,
        };
        let response = self.main.post_offline_exchange(request).await?;
        let serialized = BASE64_STANDARD
            .decode(&response.content)
            .map_err(|e| MintError::Internal(e.to_string()))?;
        let payload: wire_exchange::OfflineExchangePayload =
            borsh::from_slice(&serialized).map_err(|e| MintError::Internal(e.to_string()))?;
        Ok(payload.proofs)
    }

    async fn post_online_exchange(
        &self,
        alpha_proofs: Vec<Proof>,
        exchange_path: Vec<secp256k1::PublicKey>,
    ) -> MintResult<Vec<Proof>> {
        debug!("Clowder client call to post_online_exchange on sentinel");
        let request = wire_exchange::OnlineExchangeRequest {
            proofs: alpha_proofs,
            exchange_path,
        };
        let response = self.main.post_online_exchange(request).await?;
        Ok(response.proofs)
    }

    async fn get_clowder_id(&self) -> MintResult<secp256k1::PublicKey> {
        debug!("Clowder client call to get_clowder_id on sentinel");
        let response = self.main.get_info().await?;
        Ok(*response.node_id)
    }

    async fn post_clowder_path(
        &self,
        origin_mint_url: cashu::MintUrl,
    ) -> MintResult<ConnectedMintsResponse> {
        debug!(
            "Clowder client call to post_clowder_path on sentinel for mint url {origin_mint_url}"
        );
        self.main
            .post_path(convert_mint_url(origin_mint_url)?)
            .await
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
            .join(CoreEp::SWAP_COMMIT_V1_EXT)
            .expect("post_swap_commitment url error");
        debug!("HTTP call to post_swap_commitment on sentinel {url}");
        post_swap_commitment_inner(
            &self.secondary,
            url,
            inputs,
            outputs,
            expiry_seconds,
            alpha_pk,
        )
        .await
    }

    async fn post_swap_committed(
        &self,
        request: wire_swap::SwapRequest,
    ) -> Result<wire_swap::SwapResponse> {
        let url = self
            .url
            .join(CoreEp::SWAP_V1_EXT)
            .expect("post_swap_committed url error");
        debug!("HTTP call to post_swap_committed on sentinel {url}");
        let response = self.secondary.post(url).json(&request).send().await?;
        match response.error_for_status_ref() {
            Ok(_) => {
                let response: wire_swap::SwapResponse = response.json().await?;

                // Send sentinel event
                if let Some(sentinel_url) = self.random_sentinel() {
                    let event_url = Self::sentinel_ep(sentinel_url);
                    let event = wire_clowder::WalletEvent::Swap {
                        minted: response.signatures.clone(),
                    };
                    let resp = self.secondary.post(event_url).json(&event).send().await;
                    if let Err(e) = resp {
                        tracing::error!(
                            "Failed to send swap event to sentinel {sentinel_url}: {e}"
                        );
                    }
                }
                Ok(response)
            }
            Err(err) => {
                let status = err.status();
                let body = response.text().await.unwrap_or_default();

                tracing::error!(
                    "post_swap_committed failed: status={:?}, body={}",
                    status,
                    body
                );

                Err(err.into())
            }
        }
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
        let res = self.secondary.post(url).json(&req).send().await?;

        match res.error_for_status_ref() {
            Ok(_) => {
                let response: wire_swap::SwapProtestResponse = res.json().await?;
                Ok(response)
            }
            Err(err) => {
                let status = err.status();
                let body = res.text().await.unwrap_or_default();

                tracing::error!(
                    "post_protest_swap failed: status={:?}, body={}",
                    status,
                    body
                );

                Err(err.into())
            }
        }
    }

    async fn post_melt_quote_onchain(
        &self,
        inputs: Vec<cashu::Proof>,
        address: bitcoin::Address<bitcoin::address::NetworkUnchecked>,
        amount: bitcoin::Amount,
        alpha_pk: secp256k1::PublicKey,
    ) -> Result<MeltQuoteResult> {
        let url = self
            .url
            .join(TreasuryEp::MELTQUOTE_ONCHAIN_V1_EXT)
            .expect("melt_quote_onchain url error");
        debug!("HTTP call on sentinel to melt_quote_onchain on {url}");
        post_melt_quote_onchain_inner(&self.secondary, url, inputs, address, amount, alpha_pk).await
    }

    async fn post_melt_onchain(
        &self,
        req: wire_melt::MeltOnchainRequest,
    ) -> Result<wire_melt::MeltOnchainResponse> {
        let url = self
            .url
            .join(TreasuryEp::MELT_ONCHAIN_V1_EXT)
            .expect("melt_onchain url error");
        debug!("HTTP call on sentinel to melt_onchain on {url}");

        let res = self.secondary.post(url).json(&req).send().await?;
        match res.error_for_status_ref() {
            Ok(_) => {
                let response: wire_melt::MeltOnchainResponse = res.json().await?;
                Ok(response)
            }
            Err(err) => {
                let status = err.status();
                let body = res.text().await.unwrap_or_default();

                tracing::error!(
                    "post_melt_onchain failed: status={:?}, body={}",
                    status,
                    body
                );

                Err(err.into())
            }
        }
    }

    async fn post_protest_melt(
        &self,
        req: wire_melt::MeltProtestRequest,
    ) -> Result<wire_melt::MeltProtestResponse> {
        let url = self
            .url
            .join("v1/protest/melt")
            .expect("protest_melt url error");
        debug!("HTTP call on sentinel to protest_melt on {url}");
        let res = self.secondary.post(url).json(&req).send().await?;
        match res.error_for_status_ref() {
            Ok(_) => {
                let response: wire_melt::MeltProtestResponse = res.json().await?;
                Ok(response)
            }
            Err(err) => {
                let status = err.status();
                let body = res.text().await.unwrap_or_default();

                tracing::error!(
                    "post_protest_melt failed: status={:?}, body={}",
                    status,
                    body
                );

                Err(err.into())
            }
        }
    }

    async fn post_mint_quote_onchain(
        &self,
        req: wire_mint::OnchainMintQuoteRequest,
    ) -> Result<wire_mint::OnchainMintQuoteResponse> {
        let url = self
            .url
            .join(TreasuryEp::MINTQUOTE_ONCHAIN_V1_EXT)
            .expect("mint_quote_onchain url error");
        debug!("HTTP call on sentinel to mint_quote_onchain on {url}");

        let res = self.secondary.post(url).json(&req).send().await?;
        match res.error_for_status_ref() {
            Ok(_) => {
                let response: wire_mint::OnchainMintQuoteResponse = res.json().await?;
                Ok(response)
            }
            Err(err) => {
                let status = err.status();
                let body = res.text().await.unwrap_or_default();

                tracing::error!(
                    "post_mint_quote_onchain failed: status={:?}, body={}",
                    status,
                    body
                );

                Err(err.into())
            }
        }
    }

    async fn post_mint_onchain(
        &self,
        req: wire_mint::OnchainMintRequest,
    ) -> Result<wire_mint::MintResponse> {
        let url = self
            .url
            .join(TreasuryEp::MINT_ONCHAIN_V1_EXT)
            .expect("mint_onchain url error");
        debug!("HTTP call on sentinel to mint_onchain on {url}");

        let res = self.secondary.post(url).json(&req).send().await?;
        match res.error_for_status_ref() {
            Ok(_) => {
                let response: wire_mint::MintResponse = res.json().await?;
                Ok(response)
            }
            Err(err) => {
                let status = err.status();
                let body = res.text().await.unwrap_or_default();

                tracing::error!(
                    "post_mint_onchain failed: status={:?}, body={}",
                    status,
                    body
                );

                Err(err.into())
            }
        }
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

        let res = self.secondary.post(url).json(&req).send().await?;
        match res.error_for_status_ref() {
            Ok(_) => {
                let response: wire_mint::MintProtestResponse = res.json().await?;
                Ok(response)
            }
            Err(err) => {
                let status = err.status();
                let body = res.text().await.unwrap_or_default();

                tracing::error!(
                    "post_protest_mint failed: status={:?}, body={}",
                    status,
                    body
                );

                Err(err.into())
            }
        }
    }
}
