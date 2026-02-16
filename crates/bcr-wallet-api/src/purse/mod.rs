use crate::{
    error::{Error, Result},
    wallet::api::WalletApi,
};
use bcr_common::cashu::MintUrl;
use bcr_wallet_core::types::WalletConfig;
use bcr_wallet_persistence::{PurseRepository, redb::purse::PurseDB};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::RwLock;

pub struct Purse<Wlt> {
    pub repo: Box<dyn PurseRepository>,
    pub wallets: Arc<RwLock<Vec<Arc<RwLock<Wlt>>>>>,
}

impl<Wlt> Purse<Wlt> {
    pub async fn new(repo: PurseDB) -> Result<Self> {
        Ok(Self {
            repo: Box::new(repo),
            wallets: Arc::new(RwLock::new(Vec::default())),
        })
    }

    pub async fn load_wallet_config(&self, wallet_id: &str) -> Result<WalletConfig> {
        let res = self.repo.load(wallet_id).await?;
        Ok(res)
    }

    pub async fn list_wallets(&self) -> Result<Vec<String>> {
        let res = self.repo.list_ids().await?;
        Ok(res)
    }

    pub async fn get_wallet(&self, idx: usize) -> Option<Arc<RwLock<Wlt>>> {
        self.wallets.read().await.get(idx).cloned()
    }

    pub async fn ids(&self) -> Vec<u32> {
        (0..self.wallets.read().await.len() as u32).collect()
    }

    // Current limitation to 1 wallet
    pub async fn can_add_wallet(&self) -> bool {
        self.wallets.read().await.is_empty()
    }
}

impl<Wlt> Purse<Wlt>
where
    Wlt: WalletApi,
{
    pub async fn add_wallet(&self, wallet: Wlt) -> Result<usize> {
        self.repo.store(wallet.config()?).await?;
        let mut wallets = self.wallets.write().await;
        wallets.push(Arc::new(RwLock::new(wallet)));
        Ok(wallets.len() - 1)
    }

    pub async fn delete_wallet(&self, idx: usize) -> Result<()> {
        let Some(wlt) = self.get_wallet(idx).await else {
            return Err(Error::WalletNotFound(idx));
        };
        let id = wlt.read().await.id();
        wlt.read().await.cleanup_local_proofs().await?;
        self.repo.delete(&id).await?;
        self.wallets.write().await.remove(idx);
        Ok(())
    }

    pub async fn migrate_rabid_wallets(&self) -> Result<HashMap<String, MintUrl>> {
        let mut res = HashMap::new();
        let wlts = self.wallets.read().await;
        for wlt in wlts.iter() {
            let wallet_id = wlt.read().await.id();
            tracing::info!("Checking if alpha is rabid..");
            let is_rabid = wlt.read().await.is_wallet_mint_rabid().await?;
            if is_rabid {
                tracing::warn!("Alpha is rabid - finding substitute");
                let substitute_url = wlt.read().await.mint_substitute().await?;

                let wallet_name = wlt.read().await.name();
                if let Some(substitute_url) = substitute_url {
                    tracing::info!(
                        "Wallet {} is found rabid, migrating to substitute beta {}",
                        wallet_name,
                        substitute_url
                    );
                    let substitute_client =
                        crate::external::mint::HttpClientExt::new(substitute_url);
                    let new_mint_url = wlt
                        .write()
                        .await
                        .migrate_pockets_substitute(Box::new(substitute_client))
                        .await?;
                    res.insert(wallet_id, new_mint_url);
                    self.repo.store(wlt.read().await.config()?).await?;
                }
            } else {
                tracing::info!("Alpha is not rabid - nothing to migrate.");
            }
        }

        Ok(res)
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use bcr_common::cashu::CurrencyUnit;
    use bcr_wallet_persistence::{MockPurseRepository, test_utils::tests::test_pub_key};

    use super::*;
    use crate::wallet::api::MockWalletApi;

    fn purse(db: Box<dyn PurseRepository>) -> super::Purse<MockWalletApi> {
        Purse {
            repo: db,
            wallets: Arc::new(RwLock::new(Vec::default())),
        }
    }

    fn wlt_cfg() -> WalletConfig {
        WalletConfig {
            wallet_id: "wlt-1".to_owned(),
            name: "wallet-1".to_owned(),
            network: bitcoin::Network::Testnet,
            mint: MintUrl::from_str("https://example.com").unwrap(),
            mint_keyset_infos: vec![],
            clowder_id: test_pub_key(),
            debit: CurrencyUnit::Sat,
            credit: CurrencyUnit::Custom("crsat".to_owned()),
            pub_key: test_pub_key(),
            betas: vec![],
        }
    }

    #[tokio::test]
    async fn test_wallet_lifecycle() {
        let mut db = MockPurseRepository::new();
        db.expect_load().times(1).returning(|_| Ok(wlt_cfg()));
        db.expect_store().times(1).returning(|_| Ok(()));
        db.expect_delete().times(1).returning(|_| Ok(()));
        db.expect_list_ids()
            .times(1)
            .returning(|| Ok(vec!["wallet-1".to_owned()]));
        let purse = purse(Box::new(db));

        let mut wlt = MockWalletApi::new();
        wlt.expect_id().returning(|| "wlt-1".to_owned());
        wlt.expect_config().times(1).returning(|| Ok(wlt_cfg()));
        wlt.expect_cleanup_local_proofs().returning(|| Ok(()));

        let new_wlt_id = wlt.id();
        assert!(purse.can_add_wallet().await);
        let wlt_id = purse.add_wallet(wlt).await.expect("can create wallet");
        assert_eq!(wlt_id, 0);
        let wallets = purse.list_wallets().await.expect("list wallets works");
        assert_eq!(wallets.len(), 1);
        assert!(!purse.can_add_wallet().await);
        let cfg = purse
            .load_wallet_config(&wlt_id.to_string())
            .await
            .expect("load cfg works");
        assert_eq!(cfg.name, wlt_cfg().name);
        let ids = purse.ids().await;
        assert_eq!(ids[0], wlt_id as u32);
        let gotten = purse.get_wallet(wlt_id).await.expect("get wallet works");
        assert_eq!(gotten.read().await.id(), new_wlt_id);

        purse.delete_wallet(wlt_id).await.expect("delete works");
        assert!(purse.can_add_wallet().await);
    }

    #[tokio::test]
    async fn test_migrate_rabid_baseline() {
        let mut db = MockPurseRepository::new();
        db.expect_store().times(2).returning(|_| Ok(()));
        let purse = purse(Box::new(db));
        let mut wlt = MockWalletApi::new();
        wlt.expect_id().times(1).returning(|| "wlt-1".to_owned());
        wlt.expect_name()
            .times(1)
            .returning(|| "wallet-1".to_owned());
        wlt.expect_config().times(2).returning(|| Ok(wlt_cfg()));
        wlt.expect_is_wallet_mint_rabid()
            .times(1)
            .returning(|| Ok(true));
        wlt.expect_mint_substitute().times(1).returning(|| {
            Ok(Some(
                MintUrl::from_str("https://substitute.example.com").unwrap(),
            ))
        });
        wlt.expect_migrate_pockets_substitute()
            .returning(|_| Ok(MintUrl::from_str("https://substitute.example.com").unwrap()));

        let _wlt_id = purse.add_wallet(wlt).await.expect("can create wallet");

        let migrated = purse
            .migrate_rabid_wallets()
            .await
            .expect("migrate rabid wallets works");
        assert!(!migrated.is_empty());
    }
}
