#![cfg(not(target_arch = "wasm32"))]
// ----- standard library imports
// ----- extra library imports
// ----- local imports

// ----- end imports

#[cfg(test)]
pub mod tests {
    use async_trait::async_trait;
    use cashu::{
        nut02 as cdk02, nut03 as cdk03, nut04 as cdk04, nut05 as cdk05, nut06 as cdk06,
        nut07 as cdk07, nut09 as cdk09, nut23 as cdk23,
    };
    use cdk_common::Error as CDKError;
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
        }
    }
}
