use crate::{
    ContactStoreApi,
    error::{Error, Result},
};
use async_trait::async_trait;
use bcr_common::core::NodeId;
use bcr_wallet_core::{contact::Contact, name::Name};
use nostr::types::RelayUrl;
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition, TableError};
use std::sync::Arc;
use tokio::task::spawn_blocking;

///////////////////////////////////////////// ContactEntry
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct ContactEntry {
    pub node_id: NodeId,
    pub name: Name,
    pub nostr_relays: Vec<RelayUrl>,
}

impl From<ContactEntry> for Contact {
    fn from(value: ContactEntry) -> Self {
        Self {
            node_id: value.node_id,
            name: value.name,
            nostr_relays: value.nostr_relays,
        }
    }
}

impl From<Contact> for ContactEntry {
    fn from(value: Contact) -> Self {
        Self {
            node_id: value.node_id,
            name: value.name,
            nostr_relays: value.nostr_relays,
        }
    }
}

///////////////////////////////////////////// ContactDB
pub struct ContactDB {
    db: Arc<Database>,
    contact_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
}

impl ContactDB {
    const CONTACT_DB_NAME: &'static str = "contact";

    pub fn new(db: Arc<Database>, wallet_id: &str) -> Result<Self> {
        // Leak once to get static string, because of dynamically generated table names
        let contact_name: &'static str =
            Box::leak(format!("{wallet_id}_{}", Self::CONTACT_DB_NAME).into_boxed_str());

        let contact_table = TableDefinition::new(contact_name);

        Ok(Self { db, contact_table })
    }

    fn add_contact_sync(
        db: Arc<Database>,
        contact_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        contact: ContactEntry,
    ) -> Result<()> {
        let id = contact.node_id.clone();
        let write_txn = db.begin_write()?;

        {
            let mut table = write_txn.open_table(contact_table)?;
            let old_value = table.get(id.to_string().as_bytes())?.map(|v| v.value());

            if old_value.is_some() {
                return Err(Error::ContactAlreadyExists(id.to_string()));
            }
            let mut serialized = Vec::new();
            ciborium::into_writer(&contact, &mut serialized)?;
            table.insert(id.to_string().as_bytes(), serialized)?;
        }

        write_txn.commit()?;
        Ok(())
    }

    fn edit_contact_sync(
        db: Arc<Database>,
        contact_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        node_id: NodeId,
        name: Name,
    ) -> Result<()> {
        let id = node_id.clone();
        let write_txn = db.begin_write()?;

        {
            let mut table = write_txn.open_table(contact_table)?;
            let Some(old_value) = table.get(id.to_string().as_bytes())?.map(|v| v.value()) else {
                return Ok(());
            };
            let mut entry: ContactEntry = ciborium::from_reader(old_value.as_slice())?;
            entry.name = name;
            let mut serialized = Vec::new();
            ciborium::into_writer(&entry, &mut serialized)?;
            table.insert(id.to_string().as_bytes(), serialized)?;
        }

        write_txn.commit()?;
        Ok(())
    }

    fn delete_contact_sync(
        db: Arc<Database>,
        contact_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        node_id: NodeId,
    ) -> Result<()> {
        let write_txn = db.begin_write()?;

        {
            let mut table = write_txn.open_table(contact_table)?;
            table.remove(node_id.to_string().as_bytes())?;
        }

        write_txn.commit()?;
        Ok(())
    }

    fn get_contact_sync(
        db: Arc<Database>,
        contact_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        node_id: NodeId,
    ) -> Result<Option<ContactEntry>> {
        let read_txn = db.begin_read()?;

        match read_txn.open_table(contact_table) {
            Ok(table) => {
                let entry = table.get(node_id.to_string().as_bytes())?;
                match entry {
                    Some(e) => {
                        let contact: ContactEntry = ciborium::from_reader(e.value().as_slice())?;
                        Ok(Some(contact))
                    }
                    None => Ok(None),
                }
            }
            Err(TableError::TableDoesNotExist(_)) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn list_contacts_sync(
        db: Arc<Database>,
        contact_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        search_term: Option<String>,
    ) -> Result<Vec<ContactEntry>> {
        let read_txn = db.begin_read()?;

        match read_txn.open_table(contact_table) {
            Ok(table) => {
                let mut res = Vec::new();
                for (_, v) in table.range::<&[u8]>(..)?.flatten() {
                    let entry: ContactEntry = ciborium::from_reader(v.value().as_slice())?;
                    res.push(entry);
                }
                let search_term = search_term.map(|st| st.to_lowercase());
                match search_term {
                    Some(st) => Ok(res
                        .into_iter()
                        .filter(|ct| ct.name.as_str().to_lowercase().contains(&st))
                        .collect()),
                    None => Ok(res),
                }
            }
            Err(TableError::TableDoesNotExist(_)) => Ok(vec![]),
            Err(e) => Err(e.into()),
        }
    }

    fn delete_repo_sync(
        db: Arc<Database>,
        contact_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
    ) -> Result<()> {
        let write_txn = db.begin_write()?;

        {
            if write_txn.open_table(contact_table).is_ok() {
                write_txn.delete_table(contact_table)?;
            }
        }

        write_txn.commit()?;
        Ok(())
    }
}

#[async_trait]
impl ContactStoreApi for ContactDB {
    async fn add_contact(&self, contact: Contact) -> Result<()> {
        let db_clone = self.db.clone();
        let table = self.contact_table;
        spawn_blocking(move || Self::add_contact_sync(db_clone, table, contact.into())).await?
    }

    async fn edit_contact(&self, node_id: NodeId, name: Name) -> Result<()> {
        let db_clone = self.db.clone();
        let table = self.contact_table;
        spawn_blocking(move || Self::edit_contact_sync(db_clone, table, node_id, name)).await?
    }

    async fn delete_contact(&self, node_id: NodeId) -> Result<()> {
        let db_clone = self.db.clone();
        let table = self.contact_table;
        spawn_blocking(move || Self::delete_contact_sync(db_clone, table, node_id)).await??;
        Ok(())
    }

    async fn get_contact(&self, node_id: NodeId) -> Result<Option<Contact>> {
        let db_clone = self.db.clone();
        let table = self.contact_table;
        let res =
            spawn_blocking(move || Self::get_contact_sync(db_clone, table, node_id)).await??;
        Ok(res.map(|c| c.into()))
    }

    async fn list_contacts(&self, search_term: Option<String>) -> Result<Vec<Contact>> {
        let db_clone = self.db.clone();
        let table = self.contact_table;
        let res = spawn_blocking(move || Self::list_contacts_sync(db_clone, table, search_term))
            .await??;
        Ok(res.into_iter().map(|entry| entry.into()).collect())
    }

    async fn delete_repo(&self) -> Result<()> {
        let db_clone = self.db.clone();
        let table = self.contact_table;
        spawn_blocking(move || Self::delete_repo_sync(db_clone, table)).await?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use redb::{Builder, backends::InMemoryBackend};
    use std::str::FromStr;

    const NODE_ID_1: &str =
        "bitcrt03205b8dec12bc9e879f5b517aa32192a2550e88adcee3e54ec2c7294802568fef";

    const NODE_ID_2: &str =
        "bitcrt0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";

    const NODE_ID_3: &str =
        "bitcrt03f9f94d1fdc2090d46f3524807e3f58618c36988e69577d70d5d4d1e9e9645a4f";

    fn get_db(wallet_id: &str) -> ContactDB {
        let in_mem = InMemoryBackend::new();
        let db = Arc::new(
            Builder::new()
                .create_with_backend(in_mem)
                .expect("can create in-memory redb"),
        );

        ContactDB::new(db, wallet_id).expect("can create ContactDB")
    }

    fn node_id(value: &str) -> NodeId {
        value.parse().expect("valid node id")
    }

    fn name(value: &str) -> Name {
        Name::from_str(value).expect("valid name")
    }

    fn contact(node_id: &str, name: &str) -> Contact {
        Contact {
            node_id: self::node_id(node_id),
            name: self::name(name),
            nostr_relays: vec![],
        }
    }

    #[tokio::test]
    async fn test_get_contact_empty_returns_none() {
        let repo = get_db("wallet-empty-contact");

        let result = repo
            .get_contact(node_id(NODE_ID_1))
            .await
            .expect("get_contact works");

        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_list_contacts_empty_returns_empty_vec() {
        let repo = get_db("wallet-empty-list");

        let result = repo.list_contacts(None).await.expect("list_contacts works");

        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn test_add_contact_can_be_retrieved() {
        let repo = get_db("wallet-store-contact");
        let node_id = node_id(NODE_ID_1);

        repo.add_contact(Contact {
            node_id: node_id.clone(),
            name: name("Alice"),
            nostr_relays: vec![],
        })
        .await
        .expect("add_contact works");

        let result = repo
            .get_contact(node_id)
            .await
            .expect("get_contact works")
            .expect("contact exists");

        assert_eq!(result.name, name("Alice"));
    }

    #[tokio::test]
    async fn test_edot_contact_overwrites_existing_contact_for_same_node_id() {
        let repo = get_db("wallet-overwrite-contact");
        let node_id = node_id(NODE_ID_1);

        repo.add_contact(Contact {
            node_id: node_id.clone(),
            name: name("Alice"),
            nostr_relays: vec![],
        })
        .await
        .expect("add first contact works");

        repo.edit_contact(node_id.clone(), name("Alice Updated"))
            .await
            .expect("edit updated contact works");

        let result = repo
            .get_contact(node_id)
            .await
            .expect("get_contact works")
            .expect("contact exists");

        assert_eq!(result.name, name("Alice Updated"));
    }

    #[tokio::test]
    async fn test_get_contact_returns_none_for_unknown_node_id() {
        let repo = get_db("wallet-missing-contact");

        repo.add_contact(contact(NODE_ID_1, "Alice"))
            .await
            .expect("add_contact works");

        let result = repo
            .get_contact(node_id(NODE_ID_2))
            .await
            .expect("get_contact works");

        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_list_contacts_returns_all_contacts() {
        let repo = get_db("wallet-list-contacts");

        repo.add_contact(contact(NODE_ID_1, "Alice"))
            .await
            .expect("store Alice works");

        repo.add_contact(contact(NODE_ID_2, "Bob"))
            .await
            .expect("store Bob works");

        let mut result = repo.list_contacts(None).await.expect("list_contacts works");

        result.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));

        assert_eq!(result.len(), 2);
        assert_eq!(result[0].name, name("Alice"));
        assert_eq!(result[1].name, name("Bob"));
    }

    #[tokio::test]
    async fn test_list_contacts_filters_by_search_term() {
        let repo = get_db("wallet-search-contacts");

        repo.add_contact(contact(NODE_ID_1, "Alice"))
            .await
            .expect("store Alice works");

        repo.add_contact(contact(NODE_ID_2, "Bob"))
            .await
            .expect("store Bob works");

        repo.add_contact(contact(NODE_ID_3, "Alicia"))
            .await
            .expect("store Alicia works");

        let mut result = repo
            .list_contacts(Some("Ali".to_string()))
            .await
            .expect("list_contacts with search term works");

        result.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));

        assert_eq!(result.len(), 2);
        assert_eq!(result[0].name, name("Alice"));
        assert_eq!(result[1].name, name("Alicia"));
    }

    #[tokio::test]
    async fn test_list_contacts_filters_by_search_term_case_insensitive() {
        let repo = get_db("wallet-search-contacts-case-insensitive");

        repo.add_contact(contact(NODE_ID_1, "Alice"))
            .await
            .expect("store Alice works");

        repo.add_contact(contact(NODE_ID_2, "Bob"))
            .await
            .expect("store Bob works");

        repo.add_contact(contact(NODE_ID_3, "Alicia"))
            .await
            .expect("store Alicia works");

        let mut result = repo
            .list_contacts(Some("ali".to_string()))
            .await
            .expect("list_contacts with lowercase search term works");

        result.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));

        assert_eq!(result.len(), 2);
        assert_eq!(result[0].name, name("Alice"));
        assert_eq!(result[1].name, name("Alicia"));
    }

    #[tokio::test]
    async fn test_list_contacts_search_term_without_matches_returns_empty_vec() {
        let repo = get_db("wallet-search-no-matches");

        repo.add_contact(contact(NODE_ID_1, "Alice"))
            .await
            .expect("store Alice works");

        repo.add_contact(contact(NODE_ID_2, "Bob"))
            .await
            .expect("store Bob works");

        let result = repo
            .list_contacts(Some("Charlie".to_string()))
            .await
            .expect("list_contacts with search term works");

        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn test_delete_contact_removes_existing_contact() {
        let repo = get_db("wallet-delete-contact");
        let node_id = node_id(NODE_ID_1);

        repo.add_contact(Contact {
            node_id: node_id.clone(),
            name: name("Alice"),
            nostr_relays: vec![],
        })
        .await
        .expect("add_contact works");

        repo.delete_contact(node_id.clone())
            .await
            .expect("delete_contact works");

        let result = repo.get_contact(node_id).await.expect("get_contact works");

        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_delete_contact_does_not_remove_other_contacts() {
        let repo = get_db("wallet-delete-other-contact");

        repo.add_contact(contact(NODE_ID_1, "Alice"))
            .await
            .expect("store Alice works");

        repo.add_contact(contact(NODE_ID_2, "Bob"))
            .await
            .expect("store Bob works");

        repo.delete_contact(node_id(NODE_ID_1))
            .await
            .expect("delete_contact works");

        let deleted = repo
            .get_contact(node_id(NODE_ID_1))
            .await
            .expect("get deleted contact works");

        let remaining = repo
            .get_contact(node_id(NODE_ID_2))
            .await
            .expect("get remaining contact works")
            .expect("Bob remains");

        assert!(deleted.is_none());
        assert_eq!(remaining.name, name("Bob"));
    }

    #[tokio::test]
    async fn test_delete_contact_ignores_missing_contact() {
        let repo = get_db("wallet-delete-missing-contact");

        repo.delete_contact(node_id(NODE_ID_1))
            .await
            .expect("delete_contact ignores missing contacts");
    }

    #[tokio::test]
    async fn test_wallets_are_isolated() {
        let in_mem = InMemoryBackend::new();
        let db = Arc::new(
            Builder::new()
                .create_with_backend(in_mem)
                .expect("can create in-memory redb"),
        );

        let repo_a = ContactDB::new(db.clone(), "wallet-a").expect("can create repo a");
        let repo_b = ContactDB::new(db.clone(), "wallet-b").expect("can create repo b");

        repo_a
            .add_contact(contact(NODE_ID_1, "Alice"))
            .await
            .expect("store contact in repo a works");

        assert!(
            repo_a
                .get_contact(node_id(NODE_ID_1))
                .await
                .expect("get_contact repo a works")
                .is_some()
        );

        assert!(
            repo_b
                .get_contact(node_id(NODE_ID_1))
                .await
                .expect("get_contact repo b works")
                .is_none()
        );

        assert_eq!(
            repo_a
                .list_contacts(None)
                .await
                .expect("list_contacts repo a works")
                .len(),
            1
        );

        assert!(
            repo_b
                .list_contacts(None)
                .await
                .expect("list_contacts repo b works")
                .is_empty()
        );
    }
}
