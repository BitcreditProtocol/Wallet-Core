use crate::{ValidationError, email::Email, name::Name};
use bcr_common::core::NodeId;
use nostr::types::RelayUrl;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct Contact {
    pub id: Uuid,
    pub node_id: Option<NodeId>,
    pub email: Option<Email>,
    pub name: Option<Name>,
    pub company: Option<Name>,
    pub nostr_relays: Vec<RelayUrl>,
}

impl Contact {
    /// Creates and validates a new contact
    pub fn new(
        node_id: Option<NodeId>,
        email: Option<Email>,
        name: Option<Name>,
        company: Option<Name>,
        nostr_relays: Vec<RelayUrl>,
    ) -> Result<Self, ValidationError> {
        let contact = Self {
            id: Uuid::new_v4(),
            node_id,
            email,
            name,
            company,
            nostr_relays,
        };
        contact.validate()?;
        Ok(contact)
    }

    /// Validation for a Contact
    /// Either e-mail or node_id need to be set (or both)
    /// Either name or company need to be set (or both)
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.email.is_none() && self.node_id.is_none() {
            return Err(ValidationError::InvalidContact(
                "Either E-Mail or NodeId have to be set".to_string(),
            ));
        }

        if self.name.is_none() && self.company.is_none() {
            return Err(ValidationError::InvalidContact(
                "Either Name or Company have to be set".to_string(),
            ));
        }

        Ok(())
    }
}
