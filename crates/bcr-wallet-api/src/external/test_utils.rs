pub mod tests {
    use crate::error::Result;
    use async_trait::async_trait;
    use bcr_common::cashu;
    use bcr_common::{
        client::mint::Result as MintResult,
        wire::{keys as wire_keys, melt as wire_melt, mint as wire_mint, swap as wire_swap},
    };
    use bitcoin::secp256k1;

    use bcr_common::wire::clowder::{AlphaStateResponse, ConnectedMintResponse};

    mockall::mock! {
        pub MintConnector {}
        impl std::fmt::Debug for MintConnector {
            fn fmt<'a>(&self, f: &mut std::fmt::Formatter<'a>) -> std::fmt::Result;
        }
        #[async_trait]
        impl crate::ClowderMintConnector for MintConnector {
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
                alpha_proofs: Vec<cashu::Proof>,
                exchange_path: Vec<secp256k1::PublicKey>,
            ) -> MintResult<Vec<cashu::Proof>>;
            async fn get_clowder_id(&self) -> MintResult<secp256k1::PublicKey>;
            async fn post_clowder_path(
                &self,
                origin_mint_url: cashu::MintUrl,
            ) -> MintResult<bcr_common::wire::clowder::ConnectedMintsResponse>;
            async fn get_alpha_keysets(
                &self,
                alpha_id: secp256k1::PublicKey,
            ) -> MintResult<Vec<cashu::KeySet>>;
            async fn get_alpha_offline(&self, alpha_id: secp256k1::PublicKey) -> MintResult<bool>;
            async fn get_alpha_status(&self, alpha_id: secp256k1::PublicKey) -> MintResult<AlphaStateResponse>;
            async fn get_alpha_substitute(&self, alpha_id: secp256k1::PublicKey) -> MintResult<ConnectedMintResponse>;
            async fn post_offline_exchange(
                &self,
                proofs: Vec<wire_keys::ProofFingerprint>,
                locks: Vec<bitcoin::hashes::sha256::Hash>,
                wallet_pubkey: secp256k1::PublicKey,
            ) -> MintResult<Vec<cashu::Proof>>;
            async fn post_swap_commitment(
                &self,
                inputs: Vec<cashu::Proof>,
                outputs: Vec<cashu::BlindedMessage>,
                expiry_seconds: chrono::TimeDelta,
                alpha_pk: secp256k1::PublicKey,
            ) -> Result<crate::external::mint::SwapCommitmentResult>;
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
            ) -> Result<crate::external::mint::MeltQuoteResult>;
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
    }
}
