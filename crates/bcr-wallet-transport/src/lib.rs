use crate::{
    error::Result,
    push::{DeviceMethod, DeviceRequest},
};
use ::nostr::{
    event::EventId,
    key::PublicKey,
    types::{RelayUrl, Timestamp},
};
use async_trait::async_trait;
use bcr_common::cashu::nut18 as cdk18;
use bcr_wallet_core::{
    SendSync,
    event::{ContactPaymentPayload, ContactPaymentRequestPayload, PaymentRequestActionPayload},
};
use tokio::sync::broadcast;

pub mod error;
pub mod nostr;
pub mod push;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SortOrder {
    Asc,
    Desc,
}

#[async_trait]
pub trait TransportApi: SendSync {
    async fn send_private_msg(&self, target: String, payload: String) -> Result<EventId>;
    async fn shutdown(&self);
    fn relays(&self) -> &[RelayUrl];
    async fn has_connected_relays(&self) -> bool;
    async fn retry_messages(&self) -> Result<usize>;
    async fn queue_retry_message(&self, recipient: Option<String>, payload: String) -> Result<()>;
    async fn fetch_relay_list(
        &self,
        npub: PublicKey,
        relays: Vec<RelayUrl>,
    ) -> Result<Vec<RelayUrl>>;
    /// Fetches one gift wrap by id from the relays and decodes it; `None` if absent or not a wallet event.
    async fn fetch_wallet_event(
        &self,
        event_id: EventId,
    ) -> Result<Option<(Timestamp, NostrWalletEvent)>>;
    /// Fetches the wallet events whose gift wrap is not older than `since`, oldest first.
    async fn fetch_wallet_events_since(
        &self,
        since: Timestamp,
    ) -> Result<Vec<(Timestamp, NostrWalletEvent)>>;
    /// Sends one request signed with the wallet's Nostr key to the relay's push endpoint.
    async fn send_device_request(&self, method: DeviceMethod, request: DeviceRequest)
    -> Result<()>;
}

#[async_trait]
pub trait ConsumerApi: SendSync {
    async fn start(&self) -> Result<tokio::task::JoinSet<()>>;
}

#[async_trait]
pub trait ClientApi: SendSync {
    async fn connect(&self) -> Result<()>;
}

#[derive(Clone, Debug)]
pub enum NostrWalletEvent {
    Cdk18Payment {
        event_id: EventId,
        payload: cdk18::PaymentRequestPayload,
        sender: PublicKey,
    },
    ContactPayment {
        event_id: EventId,
        payload: ContactPaymentPayload,
        sender: PublicKey,
    },
    ContactPaymentRequest {
        event_id: EventId,
        payload: ContactPaymentRequestPayload,
        sender: PublicKey,
    },
    PaymentRequestAction {
        event_id: EventId,
        payload: PaymentRequestActionPayload,
        sender: PublicKey,
    },
}

impl NostrWalletEvent {
    pub fn event_id(&self) -> EventId {
        match self {
            Self::Cdk18Payment { event_id, .. }
            | Self::ContactPayment { event_id, .. }
            | Self::ContactPaymentRequest { event_id, .. }
            | Self::PaymentRequestAction { event_id, .. } => *event_id,
        }
    }
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
