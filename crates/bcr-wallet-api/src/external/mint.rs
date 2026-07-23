use crate::{Error, Result};
use async_trait::async_trait;
use bcr_common::{
    cashu::{self, Proof},
    client::{
        mint::{Client as MintClient, Error as MintError, Result as MintResult},
        treasury::web_ep as TreasuryEp,
    },
    core,
    wire::{
        attestation as wire_attestation,
        clowder::{self as wire_clowder, ConnectedMintsResponse},
        keys::{self as wire_keys, KeysetInfoFilters},
        melt as wire_melt, mint as wire_mint, swap as wire_swap,
    },
};
use bcr_wallet_core::SendSync;
use bitcoin::secp256k1;
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
    pub amount: bitcoin::Amount,
    pub commitment: secp256k1::schnorr::Signature,
    pub ephemeral_secret: secp256k1::SecretKey,
    pub body_content: String,
}

async fn post_swap_commitment_inner(
    client: &MintClient,
    inputs: Vec<cashu::Proof>,
    outputs: Vec<cashu::BlindedMessage>,
    expiry_seconds: chrono::TimeDelta,
    alpha_pk: secp256k1::PublicKey,
    attestation: wire_attestation::IssuanceAttestation,
) -> Result<SwapCommitmentResult> {
    let ephemeral_keypair = core::generate_random_keypair();
    let ephemeral_secret = secp256k1::SecretKey::from_keypair(&ephemeral_keypair);
    let wallet_pk = secp256k1::PublicKey::from_keypair(&ephemeral_keypair);
    let wallet_key = cashu::PublicKey::from(wallet_pk);

    let fingerprints: Vec<_> = inputs
        .into_iter()
        .map(wire_keys::ProofFingerprint::try_from)
        .collect::<std::result::Result<_, cashu::nut00::Error>>()?;
    let inputs_ys = fingerprints.iter().map(|fp| fp.y).collect::<Vec<_>>();
    let sent_digest = wire_attestation::fp_digest(&fingerprints);
    let expiry = (chrono::Utc::now() + expiry_seconds).timestamp() as u64;
    debug!("HTTP call to commit_swap on {}", client.mint_url());
    let (committed_content, commitment) = client
        .commit_swap(
            fingerprints,
            outputs.clone(),
            expiry,
            wallet_pk,
            alpha_pk,
            attestation.clone(),
        )
        .await?;
    let committed_body: wire_swap::SwapCommitmentRequest =
        bcr_common::core::signature::deserialize_borsh_msg(&committed_content)?;
    if committed_body.inputs.attestation != attestation
        || wire_attestation::fp_digest(&committed_body.inputs.inputs) != sent_digest
    {
        return Err(Error::SwapCommitmentMismatch);
    }
    Ok(SwapCommitmentResult {
        inputs_ys,
        outputs,
        expiry,
        commitment,
        ephemeral_secret,
        body_content: committed_content,
        wallet_key,
    })
}

async fn post_melt_quote_onchain_inner(
    client: &MintClient,
    inputs: Vec<cashu::Proof>,
    address: bitcoin::Address<bitcoin::address::NetworkUnchecked>,
    alpha_pk: secp256k1::PublicKey,
    attestation: wire_attestation::IssuanceAttestation,
) -> Result<MeltQuoteResult> {
    let ephemeral_keypair = core::generate_random_keypair();
    let ephemeral_secret = secp256k1::SecretKey::from_keypair(&ephemeral_keypair);
    let wallet_key = cashu::PublicKey::from(secp256k1::PublicKey::from_keypair(&ephemeral_keypair));

    let fingerprints: Vec<_> = inputs
        .into_iter()
        .map(wire_keys::ProofFingerprint::try_from)
        .collect::<std::result::Result<_, cashu::nut00::Error>>()?;
    let sent_digest = wire_attestation::fp_digest(&fingerprints);
    let (content, commitment) = client
        .onchain_melt_quote(
            fingerprints,
            address.clone(),
            wallet_key,
            alpha_pk,
            attestation.clone(),
        )
        .await?;
    let response_body: wire_melt::MeltQuoteOnchainResponseBody =
        bcr_common::core::signature::deserialize_borsh_msg(&content)?;
    let echoed = wire_attestation::fp_digest(&response_body.inputs.inputs) == sent_digest
        && response_body.inputs.attestation == attestation
        && response_body.address == address
        && response_body.wallet_key == wallet_key;
    if !echoed {
        return Err(Error::MeltQuoteMismatch);
    }
    Ok(MeltQuoteResult {
        quote_id: response_body.quote,
        expiry: response_body.expiry,
        amount: response_body.amount,
        commitment,
        ephemeral_secret,
        body_content: content,
    })
}

#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait ClowderMintConnector: SendSync + std::fmt::Debug {
    fn mint_url(&self) -> &url::Url;
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
    async fn get_clowder_betas(&self) -> MintResult<Vec<url::Url>>;
    async fn post_online_exchange(
        &self,
        alpha_proofs: Vec<Proof>,
        exchange_path: Vec<secp256k1::PublicKey>,
    ) -> MintResult<Vec<Proof>>;
    async fn get_clowder_id(&self) -> MintResult<secp256k1::PublicKey>;
    async fn post_clowder_path(
        &self,
        origin_mint_url: url::Url,
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
        mint_pk: secp256k1::PublicKey,
    ) -> MintResult<Vec<Proof>>;
    async fn post_swap_commitment(
        &self,
        inputs: Vec<cashu::Proof>,
        outputs: Vec<cashu::BlindedMessage>,
        expiry_seconds: chrono::TimeDelta,
        alpha_pk: secp256k1::PublicKey,
        attestation: wire_attestation::IssuanceAttestation,
    ) -> Result<SwapCommitmentResult>;
    async fn post_swap_committed(
        &self,
        inputs: Vec<cashu::Proof>,
        outputs: Vec<cashu::BlindedMessage>,
        commitment: secp256k1::schnorr::Signature,
    ) -> Result<Vec<cashu::BlindSignature>>;
    async fn post_protest_swap(
        &self,
        req: wire_swap::SwapProtestRequest,
    ) -> Result<wire_swap::SwapProtestResponse>;
    async fn post_melt_quote_onchain(
        &self,
        inputs: Vec<cashu::Proof>,
        address: bitcoin::Address<bitcoin::address::NetworkUnchecked>,
        alpha_pk: secp256k1::PublicKey,
        attestation: wire_attestation::IssuanceAttestation,
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
    ) -> Result<wire_mint::OnchainMintResponse>;
    async fn post_protest_mint(
        &self,
        req: wire_mint::MintProtestRequest,
    ) -> Result<wire_mint::MintProtestResponse>;
    async fn post_attest_issuance(
        &self,
        request: wire_attestation::IssuanceAttestationRequest,
    ) -> Result<wire_attestation::IssuanceAttestation>;
}

#[derive(Debug, Clone)]
pub struct HttpClientExt {
    main: MintClient,
    secondary: reqwest::Client,
}

impl HttpClientExt {
    pub fn new(cdk_url: url::Url) -> Self {
        Self {
            main: MintClient::new(cdk_url.clone()),
            secondary: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl ClowderMintConnector for HttpClientExt {
    fn mint_url(&self) -> &url::Url {
        self.main.mint_url()
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

    async fn get_clowder_betas(&self) -> MintResult<Vec<url::Url>> {
        debug!("Clowder client call to get_clowder_betas");
        let response = self.main.get_betas().await?;
        Ok(response.mints.into_iter().map(|m| m.mint).collect())
    }

    async fn post_offline_exchange(
        &self,
        proofs: Vec<wire_keys::ProofFingerprint>,
        locks: Vec<bitcoin::hashes::sha256::Hash>,
        wallet_pubkey: secp256k1::PublicKey,
        mint_pk: secp256k1::PublicKey,
    ) -> MintResult<Vec<Proof>> {
        debug!("Clowder client call to post_offline_exchange");
        let wallet_pk = cashu::PublicKey::from_slice(&wallet_pubkey.serialize())
            .map_err(|e| MintError::Internal(e.to_string()))?;
        let response = self
            .main
            .exchange_offline(proofs, locks, wallet_pk, mint_pk)
            .await?;
        Ok(response.0)
    }

    async fn post_online_exchange(
        &self,
        alpha_proofs: Vec<Proof>,
        exchange_path: Vec<secp256k1::PublicKey>,
    ) -> MintResult<Vec<Proof>> {
        debug!("Clowder client call to post_online_exchange");
        let proofs = self
            .main
            .exchange_online(alpha_proofs, exchange_path)
            .await?;
        Ok(proofs)
    }

    async fn get_clowder_id(&self) -> MintResult<secp256k1::PublicKey> {
        debug!("Clowder client call to get_clowder_id");
        let response = self.main.get_info().await?;
        Ok(*response.node_id)
    }

    async fn post_clowder_path(
        &self,
        origin_mint_url: url::Url,
    ) -> MintResult<ConnectedMintsResponse> {
        debug!("Clowder client call to post_clowder_path for mint url {origin_mint_url}");
        self.main.post_path(origin_mint_url).await
    }

    async fn post_swap_commitment(
        &self,
        inputs: Vec<cashu::Proof>,
        outputs: Vec<cashu::BlindedMessage>,
        expiry_seconds: chrono::TimeDelta,
        alpha_pk: secp256k1::PublicKey,
        attestation: wire_attestation::IssuanceAttestation,
    ) -> Result<SwapCommitmentResult> {
        post_swap_commitment_inner(
            &self.main,
            inputs,
            outputs,
            expiry_seconds,
            alpha_pk,
            attestation,
        )
        .await
    }

    async fn post_swap_committed(
        &self,
        inputs: Vec<cashu::Proof>,
        outputs: Vec<cashu::BlindedMessage>,
        commitment: secp256k1::schnorr::Signature,
    ) -> Result<Vec<cashu::BlindSignature>> {
        debug!("HTTP call to post_swap_committed on {}", self.mint_url());
        let signatures = self.main.swap(inputs, outputs, commitment).await?;
        Ok(signatures)
    }

    async fn post_protest_swap(
        &self,
        req: wire_swap::SwapProtestRequest,
    ) -> Result<wire_swap::SwapProtestResponse> {
        let url = self
            .mint_url()
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
        alpha_pk: secp256k1::PublicKey,
        attestation: wire_attestation::IssuanceAttestation,
    ) -> Result<MeltQuoteResult> {
        post_melt_quote_onchain_inner(&self.main, inputs, address, alpha_pk, attestation).await
    }

    async fn post_melt_onchain(
        &self,
        req: wire_melt::MeltOnchainRequest,
    ) -> Result<wire_melt::MeltOnchainResponse> {
        let url = self
            .mint_url()
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
            .mint_url()
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
            .mint_url()
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
    ) -> Result<wire_mint::OnchainMintResponse> {
        let url = self
            .mint_url()
            .join(TreasuryEp::MINT_ONCHAIN_V1_EXT)
            .expect("mint_onchain url error");
        debug!("HTTP call to mint_onchain on {url}");

        let res = self.secondary.post(url).json(&req).send().await?;

        match res.error_for_status_ref() {
            Ok(_) => {
                let response: wire_mint::OnchainMintResponse = res.json().await?;
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
            .mint_url()
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

    async fn post_attest_issuance(
        &self,
        request: wire_attestation::IssuanceAttestationRequest,
    ) -> Result<wire_attestation::IssuanceAttestation> {
        debug!(
            "HTTP call to post_attest_issuance on {}",
            self.main.mint_url()
        );
        Ok(self.main.post_attest_issuance(&request).await?)
    }
}

/// A client wrapper that forwards wallet events to sentinel nodes.
///
/// This client wraps the standard HTTP client and sends monitoring events
/// to randomly selected sentinel nodes after performing mint, swap, and melt operations.
#[derive(Debug, Clone)]
pub struct SentinelClient {
    main: MintClient,
    secondary: reqwest::Client,
}

impl SentinelClient {
    pub fn new(client: HttpClientExt) -> Self {
        let HttpClientExt { main, secondary } = client;
        Self { main, secondary }
    }
}

#[async_trait]
impl ClowderMintConnector for SentinelClient {
    fn mint_url(&self) -> &url::Url {
        self.main.mint_url()
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

    async fn get_clowder_betas(&self) -> MintResult<Vec<url::Url>> {
        debug!("Clowder client call to get_clowder_betas on sentinel");
        let response = self.main.get_betas().await?;
        Ok(response.mints.into_iter().map(|m| m.mint).collect())
    }

    async fn post_offline_exchange(
        &self,
        proofs: Vec<wire_keys::ProofFingerprint>,
        locks: Vec<bitcoin::hashes::sha256::Hash>,
        wallet_pubkey: secp256k1::PublicKey,
        mint_pk: secp256k1::PublicKey,
    ) -> MintResult<Vec<Proof>> {
        debug!("Clowder client call to post_offline_exchange on sentinel");
        let wallet_pk = cashu::PublicKey::from_slice(&wallet_pubkey.serialize())
            .map_err(|e| MintError::Internal(e.to_string()))?;
        let response = self
            .main
            .exchange_offline(proofs, locks, wallet_pk, mint_pk)
            .await?;
        Ok(response.0)
    }

    async fn post_online_exchange(
        &self,
        alpha_proofs: Vec<Proof>,
        exchange_path: Vec<secp256k1::PublicKey>,
    ) -> MintResult<Vec<Proof>> {
        debug!("Clowder client call to post_online_exchange on sentinel");
        let proofs = self
            .main
            .exchange_online(alpha_proofs, exchange_path)
            .await?;
        Ok(proofs)
    }

    async fn get_clowder_id(&self) -> MintResult<secp256k1::PublicKey> {
        debug!("Clowder client call to get_clowder_id on sentinel");
        let response = self.main.get_info().await?;
        Ok(*response.node_id)
    }

    async fn post_clowder_path(
        &self,
        origin_mint_url: url::Url,
    ) -> MintResult<ConnectedMintsResponse> {
        debug!(
            "Clowder client call to post_clowder_path on sentinel for mint url {origin_mint_url}"
        );
        self.main.post_path(origin_mint_url).await
    }

    async fn post_swap_commitment(
        &self,
        inputs: Vec<cashu::Proof>,
        outputs: Vec<cashu::BlindedMessage>,
        expiry_seconds: chrono::TimeDelta,
        alpha_pk: secp256k1::PublicKey,
        attestation: wire_attestation::IssuanceAttestation,
    ) -> Result<SwapCommitmentResult> {
        post_swap_commitment_inner(
            &self.main,
            inputs,
            outputs,
            expiry_seconds,
            alpha_pk,
            attestation,
        )
        .await
    }

    async fn post_swap_committed(
        &self,
        inputs: Vec<cashu::Proof>,
        outputs: Vec<cashu::BlindedMessage>,
        commitment: secp256k1::schnorr::Signature,
    ) -> Result<Vec<cashu::BlindSignature>> {
        debug!("HTTP call to post_swap_committed on {}", self.mint_url());
        let signatures = self.main.swap(inputs, outputs, commitment).await?;
        Ok(signatures)
    }

    async fn post_protest_swap(
        &self,
        req: wire_swap::SwapProtestRequest,
    ) -> Result<wire_swap::SwapProtestResponse> {
        let url = self
            .mint_url()
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
        alpha_pk: secp256k1::PublicKey,
        attestation: wire_attestation::IssuanceAttestation,
    ) -> Result<MeltQuoteResult> {
        post_melt_quote_onchain_inner(&self.main, inputs, address, alpha_pk, attestation).await
    }

    async fn post_melt_onchain(
        &self,
        req: wire_melt::MeltOnchainRequest,
    ) -> Result<wire_melt::MeltOnchainResponse> {
        let url = self
            .mint_url()
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
            .mint_url()
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
            .mint_url()
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
    ) -> Result<wire_mint::OnchainMintResponse> {
        let url = self
            .mint_url()
            .join(TreasuryEp::MINT_ONCHAIN_V1_EXT)
            .expect("mint_onchain url error");
        debug!("HTTP call on sentinel to mint_onchain on {url}");

        let res = self.secondary.post(url).json(&req).send().await?;
        match res.error_for_status_ref() {
            Ok(_) => {
                let response: wire_mint::OnchainMintResponse = res.json().await?;
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
            .mint_url()
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

    async fn post_attest_issuance(
        &self,
        request: wire_attestation::IssuanceAttestationRequest,
    ) -> Result<wire_attestation::IssuanceAttestation> {
        debug!(
            "HTTP call to post_attest_issuance on sentinel {}",
            self.main.mint_url()
        );
        Ok(self.main.post_attest_issuance(&request).await?)
    }
}
