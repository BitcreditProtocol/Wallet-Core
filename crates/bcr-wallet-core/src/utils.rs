// ----- standard library imports
use std::collections::HashMap;
// ----- extra library imports
use bitcoin::secp256k1::{PublicKey, SECP256K1};
use cashu::{BlindSignature, Proof};
use cdk::Error as CdkError;
// ----- local imports
use crate::mint::MintConnector;
use crate::mint::SecretlessProof;
// ----- end imports

type CdkResult<T> = std::result::Result<T, cdk::Error>;

pub async fn proofs_to_secretless(
    alpha_id: PublicKey,
    substitute_client: &dyn crate::mint::MintConnector,
    proofs: Vec<Proof>,
) -> CdkResult<(Vec<SecretlessProof>, Vec<cashu::secret::Secret>)> {
    let alpha_keysets = substitute_client
        .get_alpha_keysets(alpha_id)
        .await
        .map_err(|err| CdkError::HttpError(None, err.to_string()))?;

    let keys: HashMap<cashu::Id, cashu::KeySet> = alpha_keysets
        .iter()
        .map(|keyset| (keyset.id, keyset.clone()))
        .collect();

    let mut secrets = Vec::with_capacity(proofs.len());
    let mut secret_less = Vec::with_capacity(proofs.len());

    for p in proofs.iter() {
        let pubkey = keys
            .get(&p.keyset_id)
            .ok_or(CdkError::UnknownKeySet)?
            .keys
            .amount_key(p.amount)
            .ok_or(CdkError::AmountKey)?;

        let dleq = p.dleq.as_ref().ok_or(CdkError::DleqProofNotProvided)?;
        let r = bitcoin::secp256k1::Scalar::from(*dleq.r);
        let r_bigk: PublicKey = pubkey
            .mul_tweak(SECP256K1, &r)
            .map_err(|err| CdkError::Custom(err.to_string()))?;
        let signature =
            p.c.combine(&r_bigk)
                .map_err(|err| CdkError::Custom(err.to_string()))?;

        let dleq = p.dleq.clone().ok_or(CdkError::DleqProofNotProvided)?;
        secrets.push(p.secret.clone());

        let signature = BlindSignature {
            amount: p.amount,
            keyset_id: p.keyset_id,
            c: signature.into(),
            dleq: None,
        };
        secret_less.push(SecretlessProof {
            signature,
            dleq,
            y: *p.y()?,
        });
    }

    Ok((secret_less, secrets))
}

#[cfg(all(test, not(target_arch = "wasm32")))]
pub mod tests {
    use async_trait::async_trait;
    use cashu::{
        nut02 as cdk02, nut03 as cdk03, nut04 as cdk04, nut05 as cdk05, nut06 as cdk06,
        nut07 as cdk07, nut09 as cdk09, nut23 as cdk23,
    };
    use cdk_common::Error as CDKError;

    use crate::mint::SecretlessProof;
    type CdkResult<T> = Result<T, CDKError>;

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
        ) -> CdkResult<crate::mint::ConnectedMintsResponse>;
        async fn get_alpha_keysets(
            &self,
            alpha_id: bitcoin::secp256k1::PublicKey,
        ) -> CdkResult<Vec<cashu::KeySet>>;

        async fn get_alpha_offline(&self, alpha_id: bitcoin::secp256k1::PublicKey) -> CdkResult<bool>;

        async fn post_exchange_substitute(
            &self,
            proofs: Vec<SecretlessProof>,
            locks: Vec<bitcoin::hashes::sha256::Hash>,
            wallet_pubkey: bitcoin::secp256k1::PublicKey,
        ) -> CdkResult<Vec<cashu::Proof>>;

        }
    }
}
