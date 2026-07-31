use crate::{
    error::{Error, Result},
    wallet::{api::WalletApi, util::update_optional_field},
};
use bcr_common::core::NodeId;
use bcr_wallet_core::{contact::Contact, email::Email, name::Name, types::WalletConfig};
use bcr_wallet_persistence::{ContactStoreApi, PurseRepository, redb::purse::PurseDB};
use nostr::types::RelayUrl;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use tokio::sync::RwLock;
use uuid::Uuid;

pub struct Purse<Wlt> {
    pub repo: Box<dyn PurseRepository>,
    pub contact_repos: HashMap<bitcoin::Network, Arc<dyn ContactStoreApi>>,
    pub wallets: Arc<RwLock<HashMap<String, Arc<RwLock<Wlt>>>>>,
}

impl<Wlt> Purse<Wlt> {
    pub async fn new(
        repo: PurseDB,
        contact_repos: HashMap<bitcoin::Network, Arc<dyn ContactStoreApi>>,
    ) -> Result<Self> {
        Ok(Self {
            repo: Box::new(repo),
            contact_repos,
            wallets: Arc::new(RwLock::new(HashMap::default())),
        })
    }

    pub fn get_contact_repo(&self, network: bitcoin::Network) -> Arc<dyn ContactStoreApi> {
        self.contact_repos
            .get(&network)
            .expect("there is a contact repo for each network")
            .clone()
    }

    pub async fn load_wallet_config(&self, wallet_id: &str) -> Result<WalletConfig> {
        let res = self.repo.load(wallet_id).await?;
        Ok(res)
    }

    pub async fn list_wallets(&self) -> Result<Vec<String>> {
        let mut res = self.repo.list_ids().await?;
        res.sort();
        Ok(res)
    }

    pub async fn get_wallet(&self, id: &str) -> Option<Arc<RwLock<Wlt>>> {
        self.wallets.read().await.get(id).cloned()
    }

    pub async fn ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.wallets.read().await.keys().cloned().collect();
        ids.sort();
        ids
    }
}

impl<Wlt> Purse<Wlt>
where
    Wlt: WalletApi,
{
    pub async fn names_by_network(&self, network: bitcoin::Network) -> Vec<String> {
        let wallets: Vec<_> = {
            let wlts = self.wallets.read().await;
            wlts.values().cloned().collect()
        };
        let mut res: Vec<String> = Vec::new();
        for wlt in wallets.iter() {
            let info = wlt.read().await.info();
            if info.network == network {
                res.push(info.name);
            }
        }
        res
    }

    pub async fn add_wallet(&self, wallet: Arc<RwLock<Wlt>>) -> Result<String> {
        let wallet_id = wallet.read().await.id();
        self.repo.store(wallet.read().await.config()?).await?;
        let mut wallets = self.wallets.write().await;
        wallets.insert(wallet_id.clone(), wallet);
        Ok(wallet_id)
    }

    pub async fn delete_wallet(&self, id: &str) -> Result<()> {
        let Some(wlt) = self.get_wallet(id).await else {
            return Err(Error::WalletNotFound(id.to_owned()));
        };
        wlt.read().await.delete().await?;
        self.repo.delete(id).await?;
        self.wallets.write().await.remove(id);
        Ok(())
    }

    pub async fn migrate_rabid_wallets(&self) -> Result<HashMap<String, url::Url>> {
        let mut res = HashMap::new();
        let wlts = self.wallets.read().await;
        for (wallet_id, wlt) in wlts.iter() {
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
                        .migrate_pockets_substitute(Arc::new(substitute_client))
                        .await?;
                    res.insert(wallet_id.clone(), new_mint_url);
                    self.repo.store(wlt.read().await.config()?).await?;
                }
            } else {
                tracing::info!("Alpha is not rabid - nothing to migrate.");
            }
        }

        Ok(res)
    }

    pub async fn wallets_nostr_connected(&self) -> HashMap<String, bool> {
        let mut res = HashMap::new();
        let wallets: HashMap<_, _> = {
            let wlts = self.wallets.read().await;
            wlts.clone()
        };
        for (wallet_id, wlt) in wallets.iter() {
            res.insert(
                wallet_id.to_owned(),
                wlt.read().await.is_nostr_connected().await,
            );
        }
        res
    }

    async fn first_wallet_for_network(&self, network: bitcoin::Network) -> Option<String> {
        let wallets: HashMap<_, _> = {
            let wlts = self.wallets.read().await;
            wlts.clone()
        };
        for (wallet_id, wlt) in wallets.iter() {
            let info = wlt.read().await.info();
            if info.network == network {
                return Some(wallet_id.to_string());
            }
        }
        None
    }

    // collect nostr relays of all wallets of this network
    async fn nostr_relays(&self, network: bitcoin::Network) -> Vec<RelayUrl> {
        let mut res = HashSet::new();
        let wallets: HashMap<_, _> = {
            let wlts = self.wallets.read().await;
            wlts.clone()
        };
        for wlt in wallets.values() {
            let info = wlt.read().await.info();
            if info.network == network {
                res.extend(info.nostr_relays);
            }
        }
        res.into_iter().collect()
    }

    async fn relay_list_for_node_id(
        &self,
        node_id: Option<NodeId>,
        network: bitcoin::Network,
    ) -> Vec<RelayUrl> {
        // fetch relay list for the contact from nostr if node id is set, falling back to our relays
        let my_relays = self.nostr_relays(network).await;

        if let Some(ref node_id) = node_id
            && let Some(first_wallet_for_network) = self.first_wallet_for_network(network).await
            && let Some(wlt) = self.get_wallet(&first_wallet_for_network).await
        {
            let mut fetched_relay_list = wlt
                .read()
                .await
                .fetch_nostr_relays(node_id.npub(), my_relays.clone())
                .await
                .unwrap_or(my_relays.clone());

            if fetched_relay_list.is_empty() {
                fetched_relay_list = my_relays;
            }
            fetched_relay_list
        } else {
            my_relays
        }
    }

    pub async fn add_contact(
        &self,
        network: bitcoin::Network,
        node_id: Option<NodeId>,
        email: Option<Email>,
        name: Option<Name>,
        company: Option<Name>,
    ) -> Result<Uuid> {
        let relay_list_for_node_id = self.relay_list_for_node_id(node_id.clone(), network).await;
        if let Some(ref node_id) = node_id
            && node_id.network() != network
        {
            return Err(Error::InvalidNetwork(network, node_id.network()));
        }
        let contact = Contact::new(node_id, email, name, company, relay_list_for_node_id)?;
        match self.get_contact_repo(network).add_contact(contact).await {
            Ok(id) => Ok(id),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn edit_contact(
        &self,
        network: bitcoin::Network,
        contact_id: Uuid,
        node_id: Option<NodeId>,
        email: Option<Email>,
        name: Option<Name>,
        company: Option<Name>,
    ) -> Result<()> {
        let Some(mut contact_to_update) = self
            .get_contact_repo(network)
            .get_contact(contact_id)
            .await?
        else {
            return Err(Error::ContactNotFound(contact_id.to_string()));
        };
        if let Some(ref node_id) = node_id
            && node_id.network() != network
        {
            return Err(Error::InvalidNetwork(network, node_id.network()));
        }

        let mut changed = false;

        update_optional_field(&mut contact_to_update.node_id, &node_id, &mut changed);
        // if node id is set/changed, fetch and set/update relays
        if changed && node_id.is_some() {
            let relay_list_for_node_id =
                self.relay_list_for_node_id(node_id.clone(), network).await;
            contact_to_update.nostr_relays = relay_list_for_node_id;
        }
        update_optional_field(&mut contact_to_update.email, &email, &mut changed);
        update_optional_field(&mut contact_to_update.name, &name, &mut changed);
        update_optional_field(&mut contact_to_update.company, &company, &mut changed);

        if !changed {
            return Ok(());
        }

        contact_to_update.validate()?;

        match self
            .get_contact_repo(network)
            .edit_contact(contact_id, contact_to_update)
            .await
        {
            Ok(()) => Ok(()),
            Err(bcr_wallet_persistence::error::Error::ContactNotFound(_)) => {
                Err(Error::ContactNotFound(contact_id.to_string()))
            }
            Err(e) => Err(e.into()),
        }
    }

    pub async fn delete_contact(&self, network: bitcoin::Network, contact_id: Uuid) -> Result<()> {
        if self
            .get_contact_repo(network)
            .get_contact(contact_id)
            .await?
            .is_some()
        {
            self.get_contact_repo(network)
                .delete_contact(contact_id)
                .await?;
        } else {
            return Err(Error::ContactNotFound(contact_id.to_string()));
        }
        Ok(())
    }

    pub async fn get_contact(
        &self,
        network: bitcoin::Network,
        contact_id: Uuid,
    ) -> Result<Option<Contact>> {
        let contact = self
            .get_contact_repo(network)
            .get_contact(contact_id)
            .await?;
        Ok(contact)
    }

    pub async fn list_contacts(
        &self,
        network: bitcoin::Network,
        search_term: Option<String>,
    ) -> Result<Vec<Contact>> {
        let contacts = self
            .get_contact_repo(network)
            .list_contacts(search_term)
            .await?;
        Ok(contacts)
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use bcr_common::cashu::CurrencyUnit;
    use bcr_wallet_persistence::{
        MockContactStoreApi, MockPurseRepository, test_utils::tests::test_pub_key,
    };

    use super::*;
    use crate::wallet::api::MockWalletApi;

    fn purse(db: Box<dyn PurseRepository>) -> super::Purse<MockWalletApi> {
        let mut contact_dbs = HashMap::new();
        contact_dbs.insert(
            bitcoin::Network::Testnet,
            Arc::new(MockContactStoreApi::new()),
        );
        contact_dbs.insert(
            bitcoin::Network::Testnet4,
            Arc::new(MockContactStoreApi::new()),
        );
        contact_dbs.insert(
            bitcoin::Network::Regtest,
            Arc::new(MockContactStoreApi::new()),
        );
        contact_dbs.insert(
            bitcoin::Network::Signet,
            Arc::new(MockContactStoreApi::new()),
        );
        contact_dbs.insert(
            bitcoin::Network::Bitcoin,
            Arc::new(MockContactStoreApi::new()),
        );
        let contact_repos: HashMap<bitcoin::Network, Arc<dyn ContactStoreApi>> = contact_dbs
            .into_iter()
            .map(|(network, db)| (network, db as Arc<dyn ContactStoreApi>))
            .collect();
        Purse {
            repo: db,
            contact_repos,
            wallets: Arc::new(RwLock::new(HashMap::default())),
        }
    }

    fn wlt_cfg() -> WalletConfig {
        WalletConfig {
            wallet_id: "wlt-1".to_owned(),
            name: "wallet-1".to_owned(),
            network: bitcoin::Network::Testnet,
            mint: url::Url::from_str("https://example.com").unwrap(),
            mint_keyset_infos: HashMap::new(),
            clowder_id: test_pub_key(),
            debit: CurrencyUnit::Sat,
            pub_key: test_pub_key(),
            betas: vec![],
            nostr_relays: vec![],
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
        wlt.expect_delete().times(1).returning(|| Ok(()));

        let new_wlt_id = wlt.id();
        let wallet = Arc::new(RwLock::new(wlt));
        let wlt_id = purse.add_wallet(wallet).await.expect("can create wallet");
        assert_eq!(wlt_id, "wlt-1".to_owned());
        let wallets = purse.list_wallets().await.expect("list wallets works");
        assert_eq!(wallets.len(), 1);
        let cfg = purse
            .load_wallet_config(&wlt_id.to_string())
            .await
            .expect("load cfg works");
        assert_eq!(cfg.name, wlt_cfg().name);
        let ids = purse.ids().await;
        assert_eq!(ids[0], wlt_id);
        let gotten = purse.get_wallet(&wlt_id).await.expect("get wallet works");
        assert_eq!(gotten.read().await.id(), new_wlt_id);

        purse.delete_wallet(&wlt_id).await.expect("delete works");
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
                url::Url::from_str("https://substitute.example.com").unwrap(),
            ))
        });
        wlt.expect_migrate_pockets_substitute()
            .returning(|_| Ok(url::Url::from_str("https://substitute.example.com").unwrap()));

        let wallet = Arc::new(RwLock::new(wlt));
        let _wlt_id = purse.add_wallet(wallet).await.expect("can create wallet");

        let migrated = purse
            .migrate_rabid_wallets()
            .await
            .expect("migrate rabid wallets works");
        assert!(!migrated.is_empty());
    }

    const NODE_ID_1: &str =
        "bitcrt03205b8dec12bc9e879f5b517aa32192a2550e88adcee3e54ec2c7294802568fef";

    fn test_contact(id: Uuid) -> Contact {
        Contact {
            id,
            node_id: Some(NodeId::from_str(NODE_ID_1).unwrap()),
            email: None,
            name: Some(Name::from_str("Minka").unwrap()),
            company: None,
            nostr_relays: vec![],
        }
    }

    #[tokio::test]
    async fn test_contacts() {
        let network = bitcoin::Network::Testnet;
        let db = MockPurseRepository::new();
        let mut contact_repo = MockContactStoreApi::new();
        let ct_id = Uuid::new_v4();
        contact_repo
            .expect_add_contact()
            .times(1)
            .returning(move |_| Ok(ct_id));
        contact_repo
            .expect_edit_contact()
            .times(1)
            .returning(|_, _| Ok(()));
        contact_repo
            .expect_delete_contact()
            .times(1)
            .returning(|_| Ok(()));
        // one for edit, one for get
        contact_repo
            .expect_get_contact()
            .times(2)
            .returning(move |_| Ok(Some(test_contact(ct_id))));
        contact_repo
            .expect_list_contacts()
            .times(1)
            .returning(move |_| Ok(vec![test_contact(ct_id)]));
        let mut purse = purse(Box::new(db));
        *purse.contact_repos.get_mut(&network).unwrap() = Arc::new(contact_repo);

        purse
            .add_contact(
                network,
                Some(NodeId::from_str(NODE_ID_1).unwrap()),
                None,
                Some(Name::from_str("Minka").unwrap()),
                None,
            )
            .await
            .expect("create contact works");

        purse
            .edit_contact(
                network,
                ct_id,
                Some(NodeId::from_str(NODE_ID_1).unwrap()),
                None,
                Some(Name::from_str("Nala").unwrap()),
                None,
            )
            .await
            .expect("edit contact works");

        purse
            .delete_contact(network, ct_id)
            .await
            .expect("delete contact works");
        let cts = purse
            .list_contacts(network, None)
            .await
            .expect("list contacts works");
        assert!(cts.len() == 1);
    }
}
