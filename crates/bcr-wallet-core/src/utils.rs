use crate::{MintConnector, TStamp, error::Result};
use bitcoin::secp256k1::PublicKey;
use cdk::Error as CdkError;

type CdkResult<T> = std::result::Result<T, cdk::Error>;

pub fn validate_offline_conditions(
    wallet_pubkey: PublicKey,
    conditions: &cashu::Conditions,
    tstamp: u64,
) -> CdkResult<u64> {
    tracing::info!("Verifying spending conditions {:?}", conditions);

    let lock_time = conditions.locktime.ok_or(CdkError::LocktimeNotProvided)?;
    let num_sigs = conditions.num_sigs.ok_or(CdkError::PubkeyRequired)?;
    let pubkeys = conditions
        .pubkeys
        .as_ref()
        .ok_or(CdkError::PubkeyRequired)?;
    let refund_len = conditions
        .refund_keys
        .as_ref()
        .map(|r| r.len())
        .unwrap_or(0);

    if pubkeys.len() != 1 || num_sigs != 1 {
        return Err(CdkError::PubkeyRequired);
    }
    if refund_len != 0 {
        return Err(CdkError::InvalidSpendConditions(
            "Beta proofs refund not allowed".into(),
        ));
    }
    if *pubkeys[0] != wallet_pubkey {
        return Err(CdkError::InvalidSpendConditions(
            "Pubkey must be wallet pubkey".into(),
        ));
    }
    if lock_time < tstamp + crate::config::LOCK_REDUCTION_SECONDS_PER_HOP {
        return Err(CdkError::InvalidSpendConditions(
            "Lock time too short".into(),
        ));
    }

    Ok(lock_time)
}

pub async fn compel_commitment(
    inputs: Vec<cashu::Proof>,
    outputs: Vec<cashu::BlindedMessage>,
    expiration: chrono::TimeDelta,
    alpha_pk: secp256k1::PublicKey,
    client: &dyn MintConnector,
) -> Result<(
    Vec<cashu::PublicKey>,
    Vec<cashu::BlindedMessage>,
    TStamp,
    secp256k1::schnorr::Signature,
)> {
    let result = client
        .post_commitment(inputs, outputs, expiration, alpha_pk)
        .await;
    match result {
        Ok(response) => Ok(response),
        // TODO: protest
        // Err(Error::BorshSignature(BorshMsgSignatureError::Secp256k1(_))) => {},
        Err(e) => Err(e),
    }
}

#[cfg(test)]
pub mod tests {
    use crate::TStamp;
    use crate::error::Result;
    use async_trait::async_trait;
    use bcr_common::wire::keys as wire_keys;
    use cashu::{
        nut02 as cdk02, nut03 as cdk03, nut04 as cdk04, nut05 as cdk05, nut06 as cdk06,
        nut07 as cdk07, nut09 as cdk09, nut23 as cdk23,
    };
    use cdk_common::Error as CDKError;

    use bcr_common::wire::clowder::{AlphaStateResponse, ConnectedMintResponse};
    type CdkResult<T> = std::result::Result<T, CDKError>;

    mockall::mock! {
            pub MintConnector {
            }
            impl std::fmt::Debug for MintConnector {
                fn fmt<'a>(&self, f: &mut std::fmt::Formatter<'a>) -> std::fmt::Result;
            }

            #[async_trait]
            impl cdk::wallet::MintConnector for MintConnector {
            async fn get_mint_keys(&self) -> CdkResult<Vec<cdk02::KeySet>>;
            async fn get_mint_keyset(&self, keyset_id: cdk02::Id) -> CdkResult<cdk02::KeySet>;
            async fn get_mint_keysets(&self) -> CdkResult<cdk02::KeysetResponse>;
            async fn post_mint_quote(
                &self,
                request: cdk23::MintQuoteBolt11Request,
            ) -> CdkResult<cdk23::MintQuoteBolt11Response<String>>;
            async fn get_mint_quote_status(
                &self,
                quote_id: &str,
            ) -> CdkResult<cdk23::MintQuoteBolt11Response<String>>;
            async fn post_mint(&self, request: cdk04::MintRequest<String>) -> CdkResult<cdk04::MintResponse>;
            async fn post_melt_quote(
                &self,
                request: cdk23::MeltQuoteBolt11Request,
            ) -> CdkResult<cdk23::MeltQuoteBolt11Response<String>>;
            async fn get_melt_quote_status(
                &self,
                quote_id: &str,
            ) -> CdkResult<cdk23::MeltQuoteBolt11Response<String>>;
            async fn post_melt(
                &self,
                request: cdk05::MeltRequest<String>,
            ) -> CdkResult<cdk23::MeltQuoteBolt11Response<String>>;
            async fn post_swap(&self, request: cdk03::SwapRequest) -> CdkResult<cdk03::SwapResponse>;
            async fn get_mint_info(&self) -> CdkResult<cdk06::MintInfo>;
            async fn post_check_state(
                &self,
                request: cdk07::CheckStateRequest,
            ) -> CdkResult<cdk07::CheckStateResponse>;
            async fn post_restore(&self, request: cdk09::RestoreRequest) -> CdkResult<cdk09::RestoreResponse>;
    async fn post_mint_bolt12_quote(
            &self,
            request: cashu::MintQuoteBolt12Request,
        ) -> CdkResult<cashu::MintQuoteBolt12Response<String>>;
        async fn get_mint_quote_bolt12_status(
            &self,
            quote_id: &str,
        ) -> CdkResult<cashu::MintQuoteBolt12Response<String>>;
        async fn post_melt_bolt12_quote(
            &self,
            request: cashu::MeltQuoteBolt12Request,
        ) -> CdkResult<cashu::MeltQuoteBolt11Response<String>>;
        async fn get_melt_bolt12_quote_status(
            &self,
            quote_id: &str,
        ) -> CdkResult<cashu::MeltQuoteBolt11Response<String>>;
        async fn post_melt_bolt12(
            &self,
            request: cashu::MeltRequest<String>,
        ) -> CdkResult<cashu::MeltQuoteBolt11Response<String>>;
            }
        #[async_trait]
        impl crate::MintConnector for MintConnector {
        async fn get_clowder_betas(&self) -> CdkResult<Vec<cashu::MintUrl>>;
        fn mint_url(&self) -> cashu::MintUrl;

        async fn post_exchange(
            &self,
            alpha_proofs: Vec<cashu::Proof>,
            exchange_path: Vec<bitcoin::secp256k1::PublicKey>,
        ) -> CdkResult<Vec<cashu::Proof>>;
        async fn get_clowder_id(&self) -> CdkResult<bitcoin::secp256k1::PublicKey>;
        async fn post_clowder_path(
            &self,
            origin_mint_url: cashu::MintUrl,
        ) -> CdkResult<bcr_common::wire::clowder::ConnectedMintsResponse>;
        async fn get_alpha_keysets(
            &self,
            alpha_id: bitcoin::secp256k1::PublicKey,
        ) -> CdkResult<Vec<cashu::KeySet>>;

        async fn get_alpha_offline(&self, alpha_id: bitcoin::secp256k1::PublicKey) -> CdkResult<bool>;
        async fn get_alpha_status(&self, alpha_id: bitcoin::secp256k1::PublicKey) -> CdkResult<AlphaStateResponse>;
        async fn get_alpha_substitute(&self, alpha_id: bitcoin::secp256k1::PublicKey) -> CdkResult<ConnectedMintResponse>;

        async fn post_exchange_substitute(
            &self,
            proofs: Vec<wire_keys::ProofFingerprint>,
            locks: Vec<bitcoin::hashes::sha256::Hash>,
            wallet_pubkey: bitcoin::secp256k1::PublicKey,
        ) -> CdkResult<Vec<cashu::Proof>>;

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
    }
}
