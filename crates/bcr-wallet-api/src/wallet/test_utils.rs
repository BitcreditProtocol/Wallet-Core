pub mod tests {
    use async_trait::async_trait;
    use bcr_common::cashu::nut18 as cdk18;
    use bcr_wallet_core::contact::Contact;
    use bcr_wallet_transport::{TransportApi, error::Result};
    use nostr::{PublicKey, RelayUrl, event::EventId};
    mockall::mock! {
        pub Transport {}

        #[async_trait]
        impl TransportApi for Transport {
            async fn send_private_msg(&self, target: String, payload: String) -> Result<EventId>;
            async fn cdk18_transport(&self) -> Result<cdk18::Transport>;
            async fn nip19_for_contact(&self, contact: &Contact) -> Result<String>;
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
        }
    }
}
