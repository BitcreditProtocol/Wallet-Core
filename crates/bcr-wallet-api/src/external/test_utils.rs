pub mod tests {
    use crate::error::Result;
    use async_trait::async_trait;
    use bcr_common::cashu;
    use bcr_common::cdk;
    use bcr_common::{
        cashu::{
            nut02 as cdk02, nut03 as cdk03, nut04 as cdk04, nut05 as cdk05, nut06 as cdk06,
            nut07 as cdk07, nut09 as cdk09, nut23 as cdk23,
        },
        cdk_common::Error as CDKError,
        wire::{keys as wire_keys, melt as wire_melt, mint as wire_mint},
    };
    use bitcoin::secp256k1;

    use bcr_common::wire::clowder::{AlphaStateResponse, ConnectedMintResponse};
    type CdkResult<T> = std::result::Result<T, CDKError>;

    mockall::mock! {
        pub MintConnector {}
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
        impl crate::ClowderMintConnector for MintConnector {
            async fn get_clowder_betas(&self) -> CdkResult<Vec<cashu::MintUrl>>;
            fn mint_url(&self) -> cashu::MintUrl;

            async fn post_online_exchange(
                &self,
                alpha_proofs: Vec<cashu::Proof>,
                exchange_path: Vec<secp256k1::PublicKey>,
            ) -> CdkResult<Vec<cashu::Proof>>;
            async fn get_clowder_id(&self) -> CdkResult<secp256k1::PublicKey>;
            async fn post_clowder_path(
                &self,
                origin_mint_url: cashu::MintUrl,
            ) -> CdkResult<bcr_common::wire::clowder::ConnectedMintsResponse>;
            async fn get_alpha_keysets(
                &self,
                alpha_id: secp256k1::PublicKey,
            ) -> CdkResult<Vec<cashu::KeySet>>;

            async fn get_alpha_offline(&self, alpha_id: secp256k1::PublicKey) -> CdkResult<bool>;
            async fn get_alpha_status(&self, alpha_id: secp256k1::PublicKey) -> CdkResult<AlphaStateResponse>;
            async fn get_alpha_substitute(&self, alpha_id: secp256k1::PublicKey) -> CdkResult<ConnectedMintResponse>;

            async fn post_offline_exchange(
                &self,
                proofs: Vec<wire_keys::ProofFingerprint>,
                locks: Vec<bitcoin::hashes::sha256::Hash>,
                wallet_pubkey: secp256k1::PublicKey,
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
            bcr_wallet_core::types::TStamp,
            secp256k1::schnorr::Signature,
            )>;
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
        }
    }
}
