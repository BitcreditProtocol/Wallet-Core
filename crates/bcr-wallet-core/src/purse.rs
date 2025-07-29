// ----- standard library imports
// ----- extra library imports
use async_trait::async_trait;
// ----- local imports
use crate::{error::Result, types::WalletConfig};

// ----- end imports

#[async_trait(?Send)]
pub trait PurseRepository {
    #[allow(dead_code)]
    async fn store_wallet(&self, wallet: WalletConfig) -> Result<()>;
    async fn load_wallet(&self, wallet_id: &str) -> Result<WalletConfig>;
    #[allow(dead_code)]
    async fn delete_wallet(&self, wallet_id: &str) -> Result<()>;
    async fn list_wallets(&self) -> Result<Vec<String>>;
}

pub struct Purse<PurseRepo> {
    pub wallets: PurseRepo,
}

impl<PurseRepo> Purse<PurseRepo>
where
    PurseRepo: PurseRepository,
{
    pub async fn store_wallet(&self, wallet: WalletConfig) -> Result<()> {
        self.wallets.store_wallet(wallet).await
    }
    pub async fn load_wallet(&self, wallet_id: &str) -> Result<WalletConfig> {
        self.wallets.load_wallet(wallet_id).await
    }
    #[allow(dead_code)]
    pub async fn delete_wallet(&self, wallet_id: &str) -> Result<()> {
        self.wallets.delete_wallet(wallet_id).await
    }
    pub async fn list_wallets(&self) -> Result<Vec<String>> {
        self.wallets.list_wallets().await
    }
}
