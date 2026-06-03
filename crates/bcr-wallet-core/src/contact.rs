use crate::name::Name;
use bcr_common::core::NodeId;
use nostr::types::RelayUrl;

#[derive(Debug, Clone)]
pub struct Contact {
    pub node_id: NodeId,
    pub name: Name,
    pub nostr_relays: Vec<RelayUrl>,
}
