use crate::{
    ContactStoreApi,
    error::{Error, Result},
};
use async_trait::async_trait;
use bcr_common::{
    core::NodeId,
    wire::borsh::{deserialize_vec_of_strs, serialize_vec_of_strs},
};
use bcr_wallet_core::{contact::Contact, email::Email, name::Name};
use borsh::{BorshDeserialize, BorshSerialize};
use nostr::types::RelayUrl;
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition, TableError};
use std::sync::Arc;
use tokio::task::spawn_blocking;
use uuid::Uuid;

/// StoredContact is a versioned, borsh-serialized contact
#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub(super) enum StoredContact {
    V1(StoredContactPayloadV1),
}

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub struct StoredContactPayloadV1 {
    pub id: Uuid,
    pub node_id: Option<NodeId>,
    pub email: Option<Email>,
    pub name: Option<Name>,
    pub company: Option<Name>,
    #[borsh(
        serialize_with = "serialize_vec_of_strs",
        deserialize_with = "deserialize_vec_of_strs"
    )]
    pub nostr_relays: Vec<RelayUrl>,
}

impl From<StoredContactPayloadV1> for Contact {
    fn from(value: StoredContactPayloadV1) -> Self {
        Self {
            id: value.id,
            node_id: value.node_id,
            email: value.email,
            name: value.name,
            company: value.company,
            nostr_relays: value.nostr_relays,
        }
    }
}

impl From<Contact> for StoredContactPayloadV1 {
    fn from(value: Contact) -> Self {
        Self {
            id: value.id,
            node_id: value.node_id,
            email: value.email,
            name: value.name,
            company: value.company,
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

    pub fn contact_table_name(bitcoin_network: bitcoin::Network) -> String {
        format!("{bitcoin_network}_{}", Self::CONTACT_DB_NAME)
    }

    pub fn new(db: Arc<Database>, bitcoin_network: bitcoin::Network) -> Result<Self> {
        // Leak once to get static string, because of dynamically generated table names
        let contact_name: &'static str =
            Box::leak(Self::contact_table_name(bitcoin_network).into_boxed_str());

        let contact_table = TableDefinition::new(contact_name);

        Ok(Self { db, contact_table })
    }

    fn add_contact_sync(
        db: Arc<Database>,
        contact_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        contact: Contact,
    ) -> Result<Uuid> {
        let id = contact.id;
        let entry = StoredContact::V1(contact.into());
        let write_txn = db.begin_write()?;

        {
            let mut table = write_txn.open_table(contact_table)?;

            let serialized =
                borsh::to_vec(&entry).map_err(|e| Error::BorshSerialization(e.to_string()))?;
            table.insert(id.as_bytes().as_slice(), serialized)?;
        }

        write_txn.commit()?;
        Ok(id)
    }

    fn edit_contact_sync(
        db: Arc<Database>,
        contact_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        id: Uuid,
        contact: Contact,
    ) -> Result<()> {
        let write_txn = db.begin_write()?;

        {
            let mut table = write_txn.open_table(contact_table)?;
            let Some(old_value) = table.get(id.as_bytes().as_slice())?.map(|v| v.value()) else {
                return Err(Error::ContactNotFound(id.to_string()));
            };

            let deserialized: StoredContact = borsh::from_slice(&old_value)
                .map_err(|e| Error::BorshSerialization(e.to_string()))?;
            let StoredContact::V1(mut entry) = deserialized;

            if entry.id != id {
                return Err(Error::ContactNotFound(id.to_string()));
            }

            entry.node_id = contact.node_id;
            entry.email = contact.email;
            entry.name = contact.name;
            entry.company = contact.company;
            entry.nostr_relays = contact.nostr_relays;

            let serialized = borsh::to_vec(&StoredContact::V1(entry))
                .map_err(|e| Error::BorshSerialization(e.to_string()))?;

            table.insert(id.as_bytes().as_slice(), serialized)?;
        }

        write_txn.commit()?;
        Ok(())
    }

    fn edit_contact_relays_sync(
        db: Arc<Database>,
        contact_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        id: Uuid,
        relays: Vec<RelayUrl>,
    ) -> Result<()> {
        let write_txn = db.begin_write()?;

        {
            let mut table = write_txn.open_table(contact_table)?;
            let Some(old_value) = table.get(id.as_bytes().as_slice())?.map(|v| v.value()) else {
                return Err(Error::ContactNotFound(id.to_string()));
            };

            let deserialized: StoredContact = borsh::from_slice(&old_value)
                .map_err(|e| Error::BorshSerialization(e.to_string()))?;
            let StoredContact::V1(mut entry) = deserialized;

            if entry.id != id {
                return Err(Error::ContactNotFound(id.to_string()));
            }

            entry.nostr_relays = relays;

            let serialized = borsh::to_vec(&StoredContact::V1(entry))
                .map_err(|e| Error::BorshSerialization(e.to_string()))?;

            table.insert(id.as_bytes().as_slice(), serialized)?;
        }

        write_txn.commit()?;
        Ok(())
    }

    fn delete_contact_sync(
        db: Arc<Database>,
        contact_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        id: Uuid,
    ) -> Result<()> {
        let write_txn = db.begin_write()?;

        {
            let mut table = write_txn.open_table(contact_table)?;
            table.remove(id.as_bytes().as_slice())?;
        }

        write_txn.commit()?;
        Ok(())
    }

    fn get_contact_sync(
        db: Arc<Database>,
        contact_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        id: Uuid,
    ) -> Result<Option<Contact>> {
        let read_txn = db.begin_read()?;

        match read_txn.open_table(contact_table) {
            Ok(table) => {
                let entry = table.get(id.as_bytes().as_slice())?;
                match entry {
                    Some(e) => {
                        let deserialized: StoredContact =
                            borsh::from_slice(e.value().as_slice())
                                .map_err(|e| Error::BorshSerialization(e.to_string()))?;
                        let StoredContact::V1(contact) = deserialized;
                        Ok(Some(contact.into()))
                    }
                    None => Ok(None),
                }
            }
            Err(TableError::TableDoesNotExist(_)) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn get_contacts_by_node_id_sync(
        db: Arc<Database>,
        contact_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        node_id: NodeId,
    ) -> Result<Vec<Contact>> {
        let read_txn = db.begin_read()?;
        let to_find = Some(node_id);

        match read_txn.open_table(contact_table) {
            Ok(table) => {
                let mut res: Vec<Contact> = Vec::new();
                for item in table.range::<&[u8]>(..)? {
                    let (_, v) = item?;
                    let deserialized: StoredContact = borsh::from_slice(v.value().as_slice())
                        .map_err(|e| Error::BorshSerialization(e.to_string()))?;
                    let StoredContact::V1(contact) = deserialized;
                    if contact.node_id == to_find {
                        res.push(contact.into());
                    }
                }
                Ok(res)
            }
            Err(TableError::TableDoesNotExist(_)) => Ok(vec![]),
            Err(e) => Err(e.into()),
        }
    }

    fn list_contacts_sync(
        db: Arc<Database>,
        contact_table: TableDefinition<'static, &'static [u8], Vec<u8>>,
        search_term: Option<String>,
    ) -> Result<Vec<Contact>> {
        let read_txn = db.begin_read()?;

        match read_txn.open_table(contact_table) {
            Ok(table) => {
                let mut res: Vec<Contact> = Vec::new();
                for item in table.range::<&[u8]>(..)? {
                    let (_, v) = item?;
                    let deserialized: StoredContact = borsh::from_slice(v.value().as_slice())
                        .map_err(|e| Error::BorshSerialization(e.to_string()))?;
                    let StoredContact::V1(contact) = deserialized;
                    res.push(contact.into());
                }
                let search_term = search_term.map(|st| st.to_lowercase());
                // search in name and company
                match search_term {
                    Some(st) => Ok(res
                        .into_iter()
                        .filter(|ct| {
                            ct.name
                                .as_ref()
                                .is_some_and(|name| name.as_str().to_lowercase().contains(&st))
                                || ct.company.as_ref().is_some_and(|company| {
                                    company.as_str().to_lowercase().contains(&st)
                                })
                        })
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
    async fn add_contact(&self, contact: Contact) -> Result<Uuid> {
        let db_clone = self.db.clone();
        let table = self.contact_table;
        let res =
            spawn_blocking(move || Self::add_contact_sync(db_clone, table, contact)).await??;
        Ok(res)
    }

    async fn edit_contact(&self, id: Uuid, contact: Contact) -> Result<()> {
        let db_clone = self.db.clone();
        let table = self.contact_table;
        spawn_blocking(move || Self::edit_contact_sync(db_clone, table, id, contact)).await?
    }

    async fn edit_contact_relays(&self, id: Uuid, relays: Vec<RelayUrl>) -> Result<()> {
        let db_clone = self.db.clone();
        let table = self.contact_table;
        spawn_blocking(move || Self::edit_contact_relays_sync(db_clone, table, id, relays)).await?
    }

    async fn delete_contact(&self, id: Uuid) -> Result<()> {
        let db_clone = self.db.clone();
        let table = self.contact_table;
        spawn_blocking(move || Self::delete_contact_sync(db_clone, table, id)).await??;
        Ok(())
    }

    async fn get_contact(&self, id: Uuid) -> Result<Option<Contact>> {
        let db_clone = self.db.clone();
        let table = self.contact_table;
        let res = spawn_blocking(move || Self::get_contact_sync(db_clone, table, id)).await??;
        Ok(res)
    }

    async fn get_contacts_by_node_id(&self, node_id: NodeId) -> Result<Vec<Contact>> {
        let db_clone = self.db.clone();
        let table = self.contact_table;
        let res =
            spawn_blocking(move || Self::get_contacts_by_node_id_sync(db_clone, table, node_id))
                .await??;
        Ok(res)
    }

    async fn list_contacts(&self, search_term: Option<String>) -> Result<Vec<Contact>> {
        let db_clone = self.db.clone();
        let table = self.contact_table;
        let res = spawn_blocking(move || Self::list_contacts_sync(db_clone, table, search_term))
            .await??;
        Ok(res)
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

    fn get_db(bitcoin_network: bitcoin::Network) -> ContactDB {
        let in_mem = InMemoryBackend::new();
        let db = Arc::new(
            Builder::new()
                .create_with_backend(in_mem)
                .expect("can create in-memory redb"),
        );

        ContactDB::new(db, bitcoin_network).expect("can create ContactDB")
    }

    fn node_id(value: &str) -> NodeId {
        value.parse().expect("valid node id")
    }

    fn name(value: &str) -> Name {
        Name::from_str(value).expect("valid name")
    }

    fn contact(node_id: &str, name: &str) -> Contact {
        Contact {
            id: Uuid::new_v4(),
            node_id: Some(self::node_id(node_id)),
            email: None,
            name: Some(self::name(name)),
            company: None,
            nostr_relays: vec![],
        }
    }

    #[tokio::test]
    async fn test_get_contact_empty_returns_none() {
        let repo = get_db(bitcoin::Network::Testnet);

        let result = repo
            .get_contact(Uuid::new_v4())
            .await
            .expect("get_contact works");

        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_list_contacts_empty_returns_empty_vec() {
        let repo = get_db(bitcoin::Network::Testnet);

        let result = repo.list_contacts(None).await.expect("list_contacts works");

        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn test_add_contact_can_be_retrieved() {
        let repo = get_db(bitcoin::Network::Testnet);
        let node_id = node_id(NODE_ID_1);
        let id = Uuid::new_v4();

        repo.add_contact(Contact {
            id,
            node_id: Some(node_id.clone()),
            email: None,
            name: Some(name("Alice")),
            company: None,
            nostr_relays: vec![],
        })
        .await
        .expect("add_contact works");

        let result = repo
            .get_contact(id)
            .await
            .expect("get_contact works")
            .expect("contact exists");

        assert_eq!(result.name, Some(name("Alice")));
    }

    #[tokio::test]
    async fn test_edit_contact_overwrites_existing_contact_for_same_id() {
        let repo = get_db(bitcoin::Network::Testnet);
        let node_id = node_id(NODE_ID_1);
        let id = Uuid::new_v4();
        let c = Contact {
            id,
            node_id: Some(node_id.clone()),
            email: None,
            name: Some(name("Alice")),
            company: None,
            nostr_relays: vec![],
        };

        repo.add_contact(c.clone())
            .await
            .expect("add first contact works");
        let mut updated = c.clone();
        updated.name = Some(name("Alice Updated"));

        repo.edit_contact(id, updated)
            .await
            .expect("edit updated contact works");

        let result = repo
            .get_contact(id)
            .await
            .expect("get_contact works")
            .expect("contact exists");

        assert_eq!(result.name, Some(name("Alice Updated")));
    }

    #[tokio::test]
    async fn test_edit_contact_relays_overwrites_existing_contact_relays_for_same_id() {
        let repo = get_db(bitcoin::Network::Testnet);
        let node_id = node_id(NODE_ID_1);
        let id = Uuid::new_v4();

        repo.add_contact(Contact {
            id,
            node_id: Some(node_id.clone()),
            email: None,
            name: Some(name("Alice")),
            company: None,
            nostr_relays: vec![],
        })
        .await
        .expect("add contact works");

        let new_relays = vec![RelayUrl::from_str("wss://test2.example").unwrap()];

        repo.edit_contact_relays(id, new_relays)
            .await
            .expect("edit updated contact relays works");

        let result = repo
            .get_contact(id)
            .await
            .expect("get_contact works")
            .expect("contact exists");

        assert!(
            result
                .nostr_relays
                .contains(&RelayUrl::from_str("wss://test2.example").unwrap())
        );
        assert!(result.nostr_relays.len() == 1);
    }

    #[tokio::test]
    async fn test_get_contact_returns_none_for_unknown_id() {
        let repo = get_db(bitcoin::Network::Testnet);

        repo.add_contact(contact(NODE_ID_1, "Alice"))
            .await
            .expect("add_contact works");

        let result = repo
            .get_contact(Uuid::new_v4())
            .await
            .expect("get_contact works");

        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_get_contacts_by_node_id_returns_all_contacts() {
        let repo = get_db(bitcoin::Network::Testnet);

        repo.add_contact(contact(NODE_ID_1, "Alice"))
            .await
            .expect("store Alice works");

        repo.add_contact(contact(NODE_ID_1, "Bob"))
            .await
            .expect("store Bob works");

        let mut result = repo
            .get_contacts_by_node_id(node_id(NODE_ID_1))
            .await
            .expect("works works");

        result.sort_by(|a, b| a.name.cmp(&b.name));

        assert_eq!(result.len(), 2);
        assert_eq!(result[0].name, Some(name("Alice")));
        assert_eq!(result[1].name, Some(name("Bob")));
    }

    #[tokio::test]
    async fn test_list_contacts_returns_all_contacts() {
        let repo = get_db(bitcoin::Network::Testnet);

        repo.add_contact(contact(NODE_ID_1, "Alice"))
            .await
            .expect("store Alice works");

        repo.add_contact(contact(NODE_ID_2, "Bob"))
            .await
            .expect("store Bob works");

        let mut result = repo.list_contacts(None).await.expect("list_contacts works");

        result.sort_by(|a, b| a.name.cmp(&b.name));

        assert_eq!(result.len(), 2);
        assert_eq!(result[0].name, Some(name("Alice")));
        assert_eq!(result[1].name, Some(name("Bob")));
    }

    #[tokio::test]
    async fn test_list_contacts_filters_by_search_term() {
        let repo = get_db(bitcoin::Network::Testnet);

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

        result.sort_by(|a, b| a.name.cmp(&b.name));

        assert_eq!(result.len(), 2);
        assert_eq!(result[0].name, Some(name("Alice")));
        assert_eq!(result[1].name, Some(name("Alicia")));
    }

    #[tokio::test]
    async fn test_list_contacts_filters_by_search_term_case_insensitive() {
        let repo = get_db(bitcoin::Network::Testnet);

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

        result.sort_by(|a, b| a.name.cmp(&b.name));

        assert_eq!(result.len(), 2);
        assert_eq!(result[0].name, Some(name("Alice")));
        assert_eq!(result[1].name, Some(name("Alicia")));
    }

    #[tokio::test]
    async fn test_list_contacts_search_term_without_matches_returns_empty_vec() {
        let repo = get_db(bitcoin::Network::Testnet);

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
        let repo = get_db(bitcoin::Network::Testnet);
        let node_id = node_id(NODE_ID_1);
        let id = Uuid::new_v4();

        repo.add_contact(Contact {
            id,
            email: None,
            node_id: Some(node_id.clone()),
            name: Some(name("Alice")),
            company: None,
            nostr_relays: vec![],
        })
        .await
        .expect("add_contact works");

        repo.delete_contact(id).await.expect("delete_contact works");

        let result = repo.get_contact(id).await.expect("get_contact works");

        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_delete_contact_does_not_remove_other_contacts() {
        let repo = get_db(bitcoin::Network::Testnet);
        let c_1 = contact(NODE_ID_1, "Alice");
        let c_2 = contact(NODE_ID_2, "Bob");

        repo.add_contact(c_1.clone())
            .await
            .expect("store Alice works");

        repo.add_contact(c_2.clone())
            .await
            .expect("store Bob works");

        repo.delete_contact(c_1.id)
            .await
            .expect("delete_contact works");

        let deleted = repo
            .get_contact(c_1.id)
            .await
            .expect("get deleted contact works");

        let remaining = repo
            .get_contact(c_2.id)
            .await
            .expect("get remaining contact works")
            .expect("Bob remains");

        assert!(deleted.is_none());
        assert_eq!(remaining.name, Some(name("Bob")));
    }

    #[tokio::test]
    async fn test_delete_contact_ignores_missing_contact() {
        let repo = get_db(bitcoin::Network::Testnet);

        repo.delete_contact(Uuid::new_v4())
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

        let repo_a =
            ContactDB::new(db.clone(), bitcoin::Network::Testnet).expect("can create repo testnet");
        let repo_b = ContactDB::new(db.clone(), bitcoin::Network::Testnet4)
            .expect("can create repo testnet4");

        let c_1 = contact(NODE_ID_1, "Alice");

        repo_a
            .add_contact(c_1.clone())
            .await
            .expect("store contact in repo a works");

        assert!(
            repo_a
                .get_contact(c_1.id)
                .await
                .expect("get_contact repo a works")
                .is_some()
        );

        assert!(
            repo_b
                .get_contact(c_1.id)
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
