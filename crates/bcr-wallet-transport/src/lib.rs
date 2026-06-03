use crate::error::Result;
use ::nostr::{
    PublicKey,
    event::{Event, EventId},
    filter::Filter,
    signer::NostrSigner,
    types::RelayUrl,
};
use async_trait::async_trait;
use bcr_common::cashu::nut18 as cdk18;
use bcr_wallet_core::SendSync;
use std::sync::Arc;
use tokio::sync::broadcast;

pub mod error;
pub mod nostr;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SortOrder {
    Asc,
    Desc,
}

#[async_trait]
pub trait TransportClientApi: SendSync {
    async fn connect(&self) -> Result<()>;
    async fn fetch_events(
        &self,
        filter: Filter,
        order: Option<SortOrder>,
        relays: Option<Vec<RelayUrl>>,
    ) -> Result<Vec<Event>>;
    async fn fetch_relay_list(
        &self,
        npub: PublicKey,
        relays: Vec<RelayUrl>,
    ) -> Result<Vec<RelayUrl>>;
    async fn publish_relay_list(&self, relays: Vec<RelayUrl>) -> Result<()>;
    async fn send_private_msg(&self, target: String, payload: String) -> Result<EventId>;
    async fn subscribe(&self, subscription: Filter) -> Result<()>;
    async fn signer(&self) -> Result<Arc<dyn NostrSigner>>;
    async fn cdk18_transport(&self) -> Result<cdk18::Transport>;
    async fn shutdown(&self);
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NostrWalletEvent {
    Cdk18Payment {
        event_id: EventId,
        payload: cdk18::PaymentRequestPayload,
        sender: PublicKey,
    },
}

#[derive(Clone, Debug)]
pub struct NostrEventChannel {
    sender: broadcast::Sender<NostrWalletEvent>,
}

impl NostrEventChannel {
    pub fn new() -> Self {
        let (sender, _) = broadcast::channel(256);
        Self { sender }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<NostrWalletEvent> {
        self.sender.subscribe()
    }

    pub fn publish(&self, event: NostrWalletEvent) {
        let _ = self.sender.send(event);
    }
}

impl Default for NostrEventChannel {
    fn default() -> Self {
        Self::new()
    }
}
