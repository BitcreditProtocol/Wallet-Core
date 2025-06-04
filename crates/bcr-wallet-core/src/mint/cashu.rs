// ----- standard library imports
// ----- extra library imports
use anyhow::Result;
use cashu::nuts::nut02 as cdk02;
// ----- local modules
use super::connector::{Connector, MintConnector};
use crate::wallet::DebitWallet;
// ----- end imports

// Standard Cashu Interfaces
impl MintConnector for Connector<DebitWallet> {
    async fn list_keysets(&self) -> Result<cdk02::KeysetResponse> {
        todo!()
    }
    async fn swap(&self, req: cashu::SwapRequest) -> Result<cashu::SwapResponse> {
        todo!("{:?}", req);
    }
    async fn list_keys(&self, kid: cashu::Id) -> Result<cashu::KeysResponse> {
        todo!("{:?}", kid);
    }
    async fn restore(&self, req: cashu::RestoreRequest) -> Result<cashu::RestoreResponse> {
        todo!("Restore");
    }
}
