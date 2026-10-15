use crate::{
    error::{Error, Result},
    wallet::{api::WalletApi, util::update_optional_field},
};
use bcr_common::core::NodeId;
use bcr_wallet_core::{contact::Contact, email::Email, name::Name, types::WalletConfig};
use bcr_wallet_persistence::{ContactStoreApi, PurseRepository, redb::purse::PurseDB};
use nostr::types::RelayUrl;
use std::{
    collections::{BTreeSet, HashMap},
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

    pub async fn rename_wallet(&self, id: &str, new_name: String) -> Result<()> {
        let Some(wlt) = self.get_wallet(id).await else {
            return Err(Error::WalletNotFound(id.to_owned()));
        };
        let info = wlt.read().await.info();
        // renaming to the same name is a no-op
        if info.name == new_name {
            return Ok(());
        }

        let network = info.network;
        if self.names_by_network(network).await.contains(&new_name) {
            return Err(Error::WalletUniqueName(new_name.clone(), network));
        }

        self.repo.rename(id, new_name.clone()).await?;
        wlt.write().await.rename(new_name);
        Ok(())
    }

    pub async fn migrate_rabid_wallets(&self) -> Result<HashMap<String, url::Url>> {
        let wlts: Vec<_> = {
            let wlts = self.wallets.read().await;
            wlts.iter().map(|(id, w)| (id.clone(), w.clone())).collect()
        };
        let mut res = HashMap::new();
        let mut failure = None;
        for (wallet_id, wlt) in wlts {
            match self.migrate_rabid_wallet(&wallet_id, &wlt).await {
                Ok(Some(new_mint_url)) => {
                    res.insert(wallet_id, new_mint_url);
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::error!("Migration of wallet {wallet_id} failed: {e}");
                    failure.get_or_insert(e);
                }
            }
        }
        match failure {
            Some(e) => Err(e),
            None => Ok(res),
        }
    }

    async fn migrate_rabid_wallet(
        &self,
        wallet_id: &str,
        wlt: &RwLock<Wlt>,
    ) -> Result<Option<url::Url>> {
        let rebuilt = {
            let wlt = wlt.read().await;
            tracing::info!("Checking if alpha is rabid..");
            if !wlt.is_wallet_mint_rabid().await? {
                if wlt.has_migration_entries().await? {
                    tracing::info!(
                        "Finishing a migrated wallet whose config or journal was not saved"
                    );
                    self.repo.store(wlt.config()?).await?;
                    wlt.finish_migration().await?;
                } else {
                    tracing::info!("Alpha is not rabid - nothing to migrate.");
                }
                return Ok(None);
            }
            tracing::warn!("Alpha is rabid - finding substitute");
            let Some(substitute_url) = wlt.mint_substitute().await? else {
                return Ok(None);
            };
            let wallet_name = wlt.name();
            tracing::info!(
                "Wallet {} is found rabid, migrating to substitute beta {}",
                wallet_name,
                substitute_url
            );
            let cfg = wlt.exchange_to_substitute(substitute_url).await?;
            let rebuilt = wlt.rebuild(cfg.clone()).await?;
            self.repo.store(cfg).await?;
            rebuilt
        };
        let listener = wlt.read().await.take_nostr_listener();
        let handed_over = match listener {
            Some(listener) => Some(listener.stop().await),
            None => None,
        };
        self.wallets
            .write()
            .await
            .insert(wallet_id.to_owned(), rebuilt.clone());
        let rebuilt = rebuilt.read().await;
        rebuilt.start_nostr_listener(handed_over).await;
        rebuilt.finish_migration().await?;
        Ok(Some(rebuilt.mint_url()))
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
        let mut res = BTreeSet::new();
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
            wlt.read()
                .await
                .fetch_nostr_relays(node_id.npub(), my_relays)
                .await
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
    use std::sync::atomic::{AtomicBool, Ordering};

    use bcr_common::cashu::CurrencyUnit;
    use bcr_wallet_persistence::{
        MockContactStoreApi, MockPurseRepository, test_utils::tests::test_pub_key,
    };
    use mockall::predicate::eq;

    use super::*;
    use crate::wallet::{
        Wallet,
        api::MockWalletApi,
        tests::{
            ExchangeGate, RepoWrite, SwapFixture, contact_payment, next_received, settle,
        },
        types::WalletInfo,
    };
    use bcr_common::cashu::Amount;
    use bcr_wallet_core::{types::PaymentType, util::to_mint_url};
    use nostr::event::EventId;
    use std::time::Duration;

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
            swap_guards: Default::default(),
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
        db.expect_rename().times(1).returning(|_, _| Ok(()));
        db.expect_list_ids()
            .times(1)
            .returning(|| Ok(vec!["wallet-1".to_owned()]));
        let purse = purse(Box::new(db));

        let mut wlt = MockWalletApi::new();
        wlt.expect_id().returning(|| "wlt-1".to_owned());
        wlt.expect_rename()
            .with(eq("new_name".to_owned()))
            .once()
            .return_const(());
        wlt.expect_info().returning(|| WalletInfo {
            name: wlt_cfg().name,
            node_id: NodeId::new(wlt_cfg().pub_key, wlt_cfg().network),
            network: wlt_cfg().network,
            default_mint_url: wlt_cfg().mint,
            nostr_relays: vec![],
        });
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

        purse
            .rename_wallet(&wlt_id, "new_name".to_owned())
            .await
            .expect("rename works");

        purse.delete_wallet(&wlt_id).await.expect("delete works");
    }

    #[tokio::test]
    async fn test_migrate_rabid_baseline() {
        let mut db = MockPurseRepository::new();
        db.expect_store().times(2).returning(|_| Ok(()));
        let purse = purse(Box::new(db));
        let mut wlt = rabid_wallet("wlt-1", 0);
        wlt.expect_id().times(1).returning(|| "wlt-1".to_owned());
        wlt.expect_config().times(1).returning(|| Ok(wlt_cfg()));

        let wallet = Arc::new(RwLock::new(wlt));
        let _wlt_id = purse.add_wallet(wallet).await.expect("can create wallet");

        let migrated = purse
            .migrate_rabid_wallets()
            .await
            .expect("migrate rabid wallets works");
        assert!(!migrated.is_empty());
    }

    fn substitute_url() -> url::Url {
        url::Url::from_str("https://substitute.example.com").unwrap()
    }

    fn migrated_wallet() -> MockWalletApi {
        let mut wlt = MockWalletApi::new();
        wlt.expect_mint_url().returning(substitute_url);
        wlt.expect_is_wallet_mint_rabid().returning(|| Ok(false));
        wlt.expect_has_migration_entries().returning(|| Ok(false));
        wlt.expect_start_nostr_listener().return_const(());
        wlt.expect_finish_migration().returning(|| Ok(()));
        wlt
    }

    fn rabid_wallet(id: &'static str, failed_rebuilds: usize) -> MockWalletApi {
        let mut wlt = MockWalletApi::new();
        wlt.expect_name().returning(move || id.to_owned());
        wlt.expect_is_wallet_mint_rabid().returning(|| Ok(true));
        wlt.expect_mint_substitute()
            .returning(|| Ok(Some(substitute_url())));
        wlt.expect_exchange_to_substitute().returning(move |mint| {
            Ok(WalletConfig {
                wallet_id: id.to_owned(),
                mint,
                ..wlt_cfg()
            })
        });
        let mut failures = failed_rebuilds;
        wlt.expect_rebuild().returning(move |cfg| {
            assert_eq!(cfg.mint, substitute_url());
            if failures > 0 {
                failures -= 1;
                return Err(Error::NoBetas);
            }
            Ok(Arc::new(RwLock::new(migrated_wallet())))
        });
        wlt.expect_take_nostr_listener().returning(|| None);
        wlt.expect_start_nostr_listener().never();
        wlt.expect_finish_migration().never();
        wlt
    }

    fn recording_repo() -> (
        MockPurseRepository,
        Arc<std::sync::Mutex<Vec<WalletConfig>>>,
    ) {
        let mut db = MockPurseRepository::new();
        let stored = Arc::new(std::sync::Mutex::new(Vec::new()));
        let st = stored.clone();
        db.expect_store().returning(move |cfg| {
            st.lock().unwrap().push(cfg);
            Ok(())
        });
        (db, stored)
    }

    async fn insert_wallet(
        purse: &Purse<MockWalletApi>,
        wlt: MockWalletApi,
    ) -> Arc<RwLock<MockWalletApi>> {
        let id = wlt.name();
        let wlt = Arc::new(RwLock::new(wlt));
        purse.wallets.write().await.insert(id, wlt.clone());
        wlt
    }

    #[tokio::test]
    async fn migration_rebuild_produces_a_distinct_wallet_instance() {
        let (db, stored) = recording_repo();
        let purse = purse(Box::new(db));
        let old = insert_wallet(&purse, rabid_wallet("wlt-1", 0)).await;

        let reader = old.read().await;
        let migrated = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            purse.migrate_rabid_wallets(),
        )
        .await
        .expect("the migration never waits for exclusive access to the old instance")
        .unwrap();
        drop(reader);

        assert_eq!(migrated["wlt-1"], substitute_url());
        let current = purse.get_wallet("wlt-1").await.unwrap();
        assert!(!Arc::ptr_eq(&current, &old));
        assert_eq!(current.read().await.mint_url(), substitute_url());
        let stored = stored.lock().unwrap().clone();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].mint, substitute_url());
    }

    #[tokio::test]
    async fn migration_resumes_after_crash_between_exchange_and_rebuild() {
        let (db, stored) = recording_repo();
        let purse = purse(Box::new(db));
        let old = insert_wallet(&purse, rabid_wallet("wlt-1", 1)).await;

        purse
            .migrate_rabid_wallets()
            .await
            .expect_err("the failed rebuild is reported");
        assert!(stored.lock().unwrap().is_empty());
        assert!(Arc::ptr_eq(&purse.get_wallet("wlt-1").await.unwrap(), &old));

        purse
            .migrate_rabid_wallets()
            .await
            .expect("the retry rebuilds the wallet");
        assert_eq!(stored.lock().unwrap().len(), 1);
        let current = purse.get_wallet("wlt-1").await.unwrap();
        assert!(!Arc::ptr_eq(&current, &old));
    }

    #[tokio::test]
    async fn migrate_rabid_wallets_retries_a_failed_config_store() {
        let mut db = MockPurseRepository::new();
        let failed = Arc::new(AtomicBool::new(false));
        let stored = Arc::new(std::sync::Mutex::new(Vec::new()));
        let st = stored.clone();
        db.expect_store().returning(move |cfg| {
            if cfg.wallet_id == "wlt-1" && !failed.swap(true, Ordering::SeqCst) {
                return Err(bcr_wallet_persistence::error::Error::Custom(
                    "disk full".into(),
                ));
            }
            st.lock().unwrap().push(cfg.wallet_id);
            Ok(())
        });
        let purse = purse(Box::new(db));
        let old = insert_wallet(&purse, rabid_wallet("wlt-1", 0)).await;
        insert_wallet(&purse, rabid_wallet("wlt-2", 0)).await;

        purse
            .migrate_rabid_wallets()
            .await
            .expect_err("the failed config store is reported");
        assert_eq!(*stored.lock().unwrap(), vec!["wlt-2".to_owned()]);
        assert!(Arc::ptr_eq(&purse.get_wallet("wlt-1").await.unwrap(), &old));

        purse
            .migrate_rabid_wallets()
            .await
            .expect("retry stores the config");
        assert_eq!(
            *stored.lock().unwrap(),
            vec!["wlt-2".to_owned(), "wlt-1".to_owned()]
        );
        assert!(!Arc::ptr_eq(
            &purse.get_wallet("wlt-1").await.unwrap(),
            &old
        ));
    }

    fn substitute_betas() -> Vec<url::Url> {
        vec![url::Url::from_str("https://sub-beta.example").unwrap()]
    }

    async fn total(wlt: &RwLock<Wallet>) -> Amount {
        wlt.read().await.balance().await.unwrap().total
    }

    async fn spawn_migration(f: &SwapFixture) -> (ExchangeGate, MigrationTask) {
        let mut gate = f.gate_exchange();
        let purse = f.purse.clone();
        let migration = tokio::spawn(async move { purse.migrate_rabid_wallets().await });
        gate.entered().await;
        (gate, migration)
    }

    type MigrationTask = tokio::task::JoinHandle<Result<HashMap<String, url::Url>>>;

    #[tokio::test]
    async fn migrate_rabid_wallet_keeps_total_proof_value_during_concurrent_receive() {
        let mut f = SwapFixture::new(substitute_betas()).await;
        let (gate, migration) = spawn_migration(&f).await;

        f.channel.publish(contact_payment(20, &f.substitute_url));
        let token = f.substitute_token(8);
        let purse = f.purse.clone();
        let receive = tokio::spawn(async move {
            let wlt = purse.hold_wallet("w-1").await.unwrap();
            wlt.read().await.receive_token(token, 123).await
        });
        settle().await;
        assert!(f.old_received.try_recv().is_err());
        assert!(f.new_received.try_recv().is_err());
        assert!(!receive.is_finished());
        assert!(f.txs().is_empty());

        gate.release();
        let migrated = migration.await.unwrap().unwrap();
        assert_eq!(migrated["w-1"], f.substitute_url);
        let new = f.purse.get_wallet("w-1").await.unwrap();
        assert!(!Arc::ptr_eq(&new, &f.old));
        receive.await.unwrap().unwrap();
        f.channel.publish(contact_payment(10, &f.substitute_url));

        let mut received = Vec::new();
        for _ in 0..3 {
            received.push(u64::from(next_received(&mut f.new_received).await));
        }
        received.sort();
        assert_eq!(received, vec![8, 10, 20]);
        settle().await;
        assert!(f.new_received.try_recv().is_err());
        assert!(f.old_received.try_recv().is_err());
        assert_eq!(total(&new).await, Amount::from(100 + 20 + 10 + 8));
        let txs = f.txs();
        assert!(
            txs.iter()
                .all(|tx| tx.mint_url == to_mint_url(&f.substitute_url))
        );
        let mut contact_events: Vec<_> = txs
            .iter()
            .filter(|tx| tx.payment_type == PaymentType::Contact)
            .map(|tx| tx.nostr_event_id)
            .collect();
        contact_events.sort();
        assert_eq!(
            contact_events,
            vec![
                Some(EventId::from_byte_array([10; 32])),
                Some(EventId::from_byte_array([20; 32]))
            ]
        );
        assert_eq!(f.consumer_starts(), 1);
    }

    #[tokio::test]
    async fn failed_migration_resumes_the_old_listener_with_the_payment_that_waited() {
        let mut f = SwapFixture::new(substitute_betas()).await;
        let (gate, migration) = spawn_migration(&f).await;
        f.channel.publish(contact_payment(20, &f.rabid_url));
        settle().await;
        assert!(f.old_received.try_recv().is_err());

        gate.fail();
        migration
            .await
            .unwrap()
            .expect_err("the substitute fails the exchange");
        assert!(Arc::ptr_eq(
            &f.purse.get_wallet("w-1").await.unwrap(),
            &f.old
        ));
        assert_eq!(next_received(&mut f.old_received).await, Amount::from(20));
        settle().await;
        assert!(f.old_received.try_recv().is_err());
        assert!(f.new_received.try_recv().is_err());
        assert_eq!(total(&f.old).await, Amount::from(120));
        assert!(f.stored().is_empty());
        assert_eq!(f.consumer_starts(), 1);
    }

    #[tokio::test]
    async fn aborted_migration_resumes_the_old_listener_with_the_payment_that_waited() {
        let mut f = SwapFixture::new(substitute_betas()).await;
        let (_gate, migration) = spawn_migration(&f).await;
        f.channel.publish(contact_payment(20, &f.rabid_url));
        settle().await;
        assert!(f.old_received.try_recv().is_err());

        migration.abort();
        assert!(migration.await.unwrap_err().is_cancelled());
        assert_eq!(next_received(&mut f.old_received).await, Amount::from(20));
        assert!(Arc::ptr_eq(
            &f.purse.get_wallet("w-1").await.unwrap(),
            &f.old
        ));
        assert_eq!(total(&f.old).await, Amount::from(120));
        assert_eq!(f.consumer_starts(), 1);
        tokio::time::timeout(Duration::from_secs(5), f.purse.hold_wallet("w-1"))
            .await
            .expect("the aborted migration released the wallet")
            .unwrap();
    }

    #[tokio::test]
    async fn migrate_rabid_wallets_twice_concurrently_runs_the_exchange_once() {
        let f = SwapFixture::new(substitute_betas()).await;
        let (gate, first) = spawn_migration(&f).await;
        let purse = f.purse.clone();
        let second = tokio::spawn(async move { purse.migrate_rabid_wallets().await });
        settle().await;
        assert!(!second.is_finished());

        gate.release();
        assert_eq!(first.await.unwrap().unwrap()["w-1"], f.substitute_url);
        assert!(second.await.unwrap().unwrap().is_empty());
        assert_eq!(f.stored().len(), 1);
    }

    #[tokio::test]
    async fn migration_queues_behind_an_in_flight_read_without_deadlock() {
        let f = SwapFixture::new(substitute_betas()).await;
        let mut gate = f.gate_exchange();
        let held = f.purse.hold_wallet("w-1").await.unwrap();
        let purse = f.purse.clone();
        let migration = tokio::spawn(async move { purse.migrate_rabid_wallets().await });
        settle().await;
        assert!(!gate.has_entered());

        let balance = tokio::time::timeout(Duration::from_secs(5), async {
            held.read().await.balance().await
        })
        .await
        .expect("the call in flight finishes while the migration waits")
        .unwrap();
        assert_eq!(balance.total, Amount::from(100));
        drop(held);

        gate.entered().await;
        gate.release();
        let migrated = tokio::time::timeout(Duration::from_secs(5), migration)
            .await
            .expect("the migration runs once the call in flight is done")
            .unwrap()
            .unwrap();
        assert_eq!(migrated["w-1"], f.substitute_url);
    }

    #[tokio::test]
    async fn rename_during_migration_keeps_the_new_name() {
        let f = SwapFixture::new(substitute_betas()).await;
        let (gate, migration) = spawn_migration(&f).await;
        let purse = f.purse.clone();
        let rename =
            tokio::spawn(async move { purse.rename_wallet("w-1", "renamed".to_owned()).await });
        settle().await;
        assert!(!rename.is_finished());

        gate.release();
        migration.await.unwrap().unwrap();
        rename.await.unwrap().unwrap();
        let current = f.purse.get_wallet("w-1").await.unwrap();
        assert!(!Arc::ptr_eq(&current, &f.old));
        assert_eq!(current.read().await.name(), "renamed");
        assert_eq!(
            f.repo_writes(),
            vec![
                RepoWrite::Store("wallet-1".to_owned()),
                RepoWrite::Rename("renamed".to_owned())
            ]
        );
    }

    #[tokio::test]
    async fn delete_during_migration_does_not_resurrect_the_wallet() {
        let f = SwapFixture::new(substitute_betas()).await;
        let (gate, migration) = spawn_migration(&f).await;
        let purse = f.purse.clone();
        let delete = tokio::spawn(async move { purse.delete_wallet("w-1").await });
        settle().await;
        assert!(!delete.is_finished());

        gate.release();
        migration.await.unwrap().unwrap();
        delete.await.unwrap().unwrap();
        assert!(f.purse.get_wallet("w-1").await.is_none());
        assert_eq!(
            f.repo_writes(),
            vec![RepoWrite::Store("wallet-1".to_owned()), RepoWrite::Delete]
        );
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
