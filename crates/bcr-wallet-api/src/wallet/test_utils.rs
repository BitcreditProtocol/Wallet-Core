pub mod tests {
    use async_trait::async_trait;
    use bcr_wallet_transport::{
        ConsumerApi, NostrWalletEvent, TransportApi,
        error::Result,
        push::{DeviceMethod, DeviceRequest},
    };
    use nostr::{
        event::{Event, EventId},
        key::PublicKey,
        types::{RelayUrl, Timestamp},
    };

    mockall::mock! {
        pub Transport {}

        #[async_trait]
        impl TransportApi for Transport {
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
            async fn fetch_gift_wraps(&self, ids: Vec<EventId>) -> Result<Vec<Event>>;
            async fn fetch_gift_wraps_since(&self, since: Timestamp) -> Result<Vec<Event>>;
            async fn decode_gift_wrap(&self, event: &Event) -> Result<Option<NostrWalletEvent>>;
            async fn send_device_request(
                &self,
                method: DeviceMethod,
                request: DeviceRequest,
            ) -> Result<()>;
        }
    }

    mockall::mock! {
        pub Consumer {}

        #[async_trait]
        impl ConsumerApi for Consumer {
            async fn start(&self) -> Result<tokio::task::JoinSet<()>>;
        }
    }
}
