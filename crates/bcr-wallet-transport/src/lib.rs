use crate::error::Result;
use ::nostr::{PublicKey, event::EventId, types::RelayUrl};
use async_trait::async_trait;
use bcr_common::cashu::nut18 as cdk18;
use bcr_wallet_core::SendSync;
use tokio::sync::broadcast;

pub mod error;
pub mod nostr;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SortOrder {
    Asc,
    Desc,
}

#[async_trait]
pub trait TransportApi: SendSync {
    async fn send_private_msg(&self, target: String, payload: String) -> Result<EventId>;
    async fn cdk18_transport(&self) -> Result<cdk18::Transport>;
    async fn shutdown(&self);
    fn relays(&self) -> &[RelayUrl];
    async fn has_connected_relays(&self) -> bool;
    async fn retry_messages(&self) -> Result<usize>;
    async fn queue_retry_message(&self, recipient: Option<String>, payload: String) -> Result<()>;
}

#[async_trait]
pub trait ClientApi: SendSync {
    async fn connect(&self) -> Result<()>;
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
