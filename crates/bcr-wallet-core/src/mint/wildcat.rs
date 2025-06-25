// ----- standard library imports
// ----- extra library imports
use anyhow::Result;
use async_trait::async_trait;
use cashu::nuts::nut02 as cdk02;
// ----- local modules
use super::connector::{Connector, MintConnector};
use crate::wallet::CreditWallet;
// ----- end imports

// Wildcat endpoints
#[async_trait(?Send)]
impl MintConnector for Connector<CreditWallet> {
    async fn list_keysets(&self) -> Result<cdk02::KeysetResponse> {
        let url = self.url("v1/keysets");
        self.client.get(url).await
    }
    async fn swap(&self, req: cashu::SwapRequest) -> Result<cashu::SwapResponse> {
        let url = self.url("v1/swap");
        self.client.post(url, &req).await
    }
    async fn list_keys(&self, kid: cashu::Id) -> Result<cashu::KeysResponse> {
        let url = self.url(&format!("v1/keys/{kid}"));
        self.client.get(url).await
    }
    async fn restore(&self, req: cashu::RestoreRequest) -> Result<cashu::RestoreResponse> {
        let url = self.url("v1/restore");
        self.client.post(url, &req).await
    }
    async fn checkstate(&self, req: cashu::CheckStateRequest) -> Result<cashu::CheckStateResponse> {
        let url = self.url("v1/checkstate");
        self.client.post(url, &req).await
    }
}
