// ----- standard library imports
use std::sync::{Arc, Mutex};
// ----- extra library imports
use async_trait::async_trait;
// ----- local imports
use crate::{error::Result, types::WalletConfig};

// ----- end imports

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
pub trait PurseRepository {
    async fn store(&self, wallet: WalletConfig) -> Result<()>;
    async fn load(&self, wallet_id: &str) -> Result<WalletConfig>;
    #[allow(dead_code)]
    async fn delete(&self, wallet_id: &str) -> Result<()>;
    async fn list_ids(&self) -> Result<Vec<String>>;
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
pub trait Wallet {
    fn config(&self) -> WalletConfig;
    fn name(&self) -> String;
}

pub struct Purse<PurseRepo, Wlt> {
    pub repo: PurseRepo,
    pub wallets: Mutex<Vec<Arc<Wlt>>>,
}
impl<PurseRepo, Wlt> Purse<PurseRepo, Wlt> {
    pub fn new(repo: PurseRepo) -> Self {
        Self {
            repo,
            wallets: Mutex::new(Vec::default()),
        }
    }
}

impl<PurseRepo, Wlt> Purse<PurseRepo, Wlt>
where
    PurseRepo: PurseRepository,
{
    pub async fn load_wallet_config(&self, wallet_id: &str) -> Result<WalletConfig> {
        self.repo.load(wallet_id).await
    }
    pub async fn list_wallets(&self) -> Result<Vec<String>> {
        self.repo.list_ids().await
    }

    pub fn get_wallet(&self, idx: usize) -> Option<Arc<Wlt>> {
        let wallets = self.wallets.lock().unwrap();
        wallets.get(idx).cloned()
    }

    pub fn ids(&self) -> Vec<u32> {
        let w_len = self.wallets.lock().unwrap().len();
        (0..w_len as u32).collect()
    }
}

impl<PurseRepo, Wlt> Purse<PurseRepo, Wlt>
where
    Wlt: Wallet,
{
    pub fn names(&self) -> Vec<String> {
        let wallets = self.wallets.lock().unwrap();
        wallets.iter().map(|w| w.name()).collect()
    }
}

impl<PurseRepo, Wlt> Purse<PurseRepo, Wlt>
where
    PurseRepo: PurseRepository,
    Wlt: Wallet,
{
    pub async fn add_wallet(&self, wallet: Wlt) -> Result<usize> {
        self.repo.store(wallet.config()).await?;
        let mut wallets = self.wallets.lock().unwrap();
        wallets.push(Arc::new(wallet));
        Ok(wallets.len() - 1)
    }
}
