use crate::{
    ClientApi, NostrEventChannel, NostrWalletEvent, SortOrder, TransportApi,
    error::{Error, Result},
};
use async_trait::async_trait;
use bcr_common::{cashu::nut18 as cdk18, cdk_common::bitcoin::base58};
use bcr_wallet_core::{
    contact::Contact,
    event::{ContactPaymentPayload, EventEnvelope},
};
use bcr_wallet_persistence::{NostrEventOffset, NostrQueuedMessage, NostrRepository};
use nostr::{
    PublicKey,
    event::{Event, EventBuilder, EventId, Kind, TagKind, TagStandard},
    filter::{Alphabet, Filter, SingleLetterTag},
    nips::{
        nip19::{FromBech32, Nip19Profile, ToBech32},
        nip59::UnwrappedGift,
        nip65::RelayMetadata,
    },
    secp256k1::Keypair,
    signer::NostrSigner,
    types::{RelayUrl, Timestamp},
};
use nostr_sdk::{
    Client as NostrClient, ClientOptions, Keys, RelayPoolNotification, RelayStatus, pool::Output,
};
use std::{
    collections::HashSet,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::task::JoinSet;

#[derive(Debug, Clone)]
pub struct Client {
    client: NostrClient,
    relays: Vec<RelayUrl>,
    signer: Keys,
    connected: Arc<AtomicBool>,
    default_timeout: Duration,
}

impl Client {
    pub async fn new(keypair: &Keypair, relays: Vec<RelayUrl>) -> Result<Self> {
        let signer = Keys::new(keypair.secret_key().into());

        let default_timeout = Duration::from_secs(1);
        let options = ClientOptions::new();
        let client = NostrClient::builder()
            .signer(signer.clone())
            .opts(options)
            .build();

        for nostr_relay in relays.iter() {
            client.add_relay(nostr_relay).await?;
        }

        Ok(Self {
            client,
            relays,
            signer,
            connected: Arc::new(AtomicBool::new(false)),
            default_timeout,
        })
    }

    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    async fn client(&self) -> Result<&NostrClient> {
        if !self.is_connected() {
            self.connect().await?;
        }
        Ok(&self.client)
    }

    async fn subscribe(&self, subscription: Filter) -> Result<()> {
        self.client()
            .await?
            .subscribe(subscription, None)
            .await
            .map_err(|e| {
                tracing::error!("Failed to subscribe to Nostr events: {e}");
                Error::Network("Failed to subscribe to Nostr events".to_string())
            })?;
        Ok(())
    }

    async fn signer(&self) -> Result<Arc<dyn NostrSigner>> {
        let signer = self.client.signer().await?;
        Ok(signer)
    }

    async fn fetch_events(
        &self,
        filter: Filter,
        order: Option<SortOrder>,
        relays: Option<Vec<RelayUrl>>,
    ) -> Result<Vec<Event>> {
        let events = self
            .client()
            .await?
            .fetch_events_from(
                relays.unwrap_or(self.relays.clone()),
                filter,
                self.default_timeout.to_owned(),
            )
            .await
            .map_err(|e| {
                tracing::error!("Failed to fetch Nostr events: {e}");
                Error::Network("Failed to fetch Nostr events".to_string())
            })?;
        let mut events = events.into_iter().collect::<Vec<Event>>();
        if Some(SortOrder::Asc) == order {
            events.reverse();
        }
        Ok(events)
    }

    async fn fetch_relay_list(
        &self,
        npub: PublicKey,
        relays: Vec<RelayUrl>,
    ) -> Result<Vec<RelayUrl>> {
        let filter = Filter::new().author(npub).kind(Kind::RelayList).limit(1);
        let events = self.fetch_events(filter, None, Some(relays)).await?;
        Ok(events
            .first()
            .map(|e| {
                e.tags
                    .filter_standardized(TagKind::SingleLetter(SingleLetterTag::lowercase(
                        Alphabet::R,
                    )))
                    .filter_map(|f| match f {
                        TagStandard::RelayMetadata { relay_url, .. } => Some(relay_url.clone()),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn publish_relay_list(&self, relays: Vec<RelayUrl>) -> Result<()> {
        let signer = self.signer.clone();
        let relay_list: Vec<(RelayUrl, Option<RelayMetadata>)> =
            relays.into_iter().map(|r| (r, None)).collect();

        let event = EventBuilder::relay_list(relay_list)
            .build(signer.public_key())
            .sign(&signer)
            .await
            .map_err(|e| {
                tracing::error!("Failed to sign relay list event: {e}");
                Error::Crypto("Failed to sign relay list event".to_string())
            })?;

        let output = self.client().await?.send_event(&event).await.map_err(|e| {
            tracing::error!("Failed to send relay list with Nostr client: {e}");
            Error::Network("Failed to send relay list with Nostr client".to_string())
        })?;
        check_send_output(output, "publish_relay_list")
    }

    async fn sync_relay_list(&self, relays: Vec<RelayUrl>) -> Result<()> {
        let signer = self.signer.clone();
        let fetched_relays = self
            .fetch_relay_list(signer.public_key(), relays.clone())
            .await?;
        let mut merged: HashSet<RelayUrl> = relays.into_iter().collect();
        merged.extend(fetched_relays);

        self.publish_relay_list(merged.into_iter().collect())
            .await?;
        Ok(())
    }
}

#[async_trait]
impl ClientApi for Client {
    async fn connect(&self) -> Result<()> {
        if self
            .connected
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            self.client.connect().await;

            let self_clone = self.clone();
            tokio::spawn(async move {
                if let Err(e) = self_clone.sync_relay_list(self_clone.relays.clone()).await {
                    tracing::error!(
                        "Failed to publish relay list for {}: {}",
                        self_clone.signer.public_key,
                        e
                    );
                }
            });
        }

        Ok(())
    }
}

#[derive(Clone)]
pub struct Transport {
    client: Arc<Client>,
    nostr_store: Arc<dyn NostrRepository>,
}

impl Transport {
    const MAX_RETRIES: i32 = 5;
    pub fn new(client: Arc<Client>, nostr_store: Arc<dyn NostrRepository>) -> Self {
        Self {
            client,
            nostr_store,
        }
    }
}

#[async_trait]
impl TransportApi for Transport {
    async fn send_private_msg(&self, target: String, payload: String) -> Result<EventId> {
        let receiver = Nip19Profile::from_bech32(&target)?;

        let signer = self.client.client.signer().await?;
        let event: Event =
            EventBuilder::private_msg(&signer, receiver.public_key, payload, []).await?;
        let _ = self
            .client
            .client
            .send_event_to(receiver.relays, &event)
            .await
            .map_err(|e| {
                tracing::error!("send_private_msg failed: {e}");
                Error::NostrSendPrivateMsg(event.id)
            })?;
        Ok(event.id)
    }

    async fn cdk18_transport(&self) -> Result<cdk18::Transport> {
        Ok(cdk18::Transport {
            _type: cdk18::TransportType::Nostr,
            target: Nip19Profile::new(self.client.signer.public_key, self.client.relays.clone())
                .to_bech32()?,
            tags: vec![vec![String::from("n"), String::from("17")]],
        })
    }

    async fn nip19_for_contact(&self, contact: &Contact) -> Result<String> {
        let target =
            Nip19Profile::new(contact.node_id.npub(), contact.nostr_relays.clone()).to_bech32()?;
        Ok(target)
    }

    async fn shutdown(&self) {
        self.client.client.shutdown().await;
    }

    fn relays(&self) -> &[RelayUrl] {
        &self.client.relays
    }

    async fn has_connected_relays(&self) -> bool {
        self.client
            .client
            .relays()
            .await
            .values()
            .any(|relay| relay.status() == RelayStatus::Connected)
    }

    async fn queue_retry_message(&self, recipient: Option<String>, payload: String) -> Result<()> {
        let receiver = match recipient {
            Some(ref r) => Nip19Profile::from_bech32(r)?.public_key.to_string(),
            None => "public broadcast".to_string(),
        };
        let queue_msg = NostrQueuedMessage {
            id: uuid::Uuid::new_v4().to_string(),
            recipient,
            payload,
        };
        self.nostr_store
            .add_retry_message(queue_msg, Self::MAX_RETRIES)
            .await?;
        tracing::debug!("Queued Nostr retry message; triggering immediate retry for {receiver}");

        let self_clone = self.clone();
        tokio::spawn(async move {
            if let Err(e) = self_clone.retry_messages().await {
                tracing::error!("Failed to process Nostr retry queue after enqueue: {e}");
            }
        });
        Ok(())
    }

    async fn retry_messages(&self) -> Result<usize> {
        let mut failed_ids: Vec<String> = vec![];
        let mut retried = 0;
        while let Ok(Some(queued_message)) = self
            .nostr_store
            .get_retry_messages(1)
            .await
            .map(|r| r.first().cloned())
        {
            let result: Result<()> = match &queued_message.recipient {
                Some(target) => {
                    if let Err(e) = serde_json::from_str::<cdk18::PaymentRequestPayload>(
                        &queued_message.payload,
                    ) {
                        tracing::error!("Failed to parse private retry payload: {e}");
                        failed_ids.push(queued_message.id.clone());
                        continue;
                    };

                    match self
                        .send_private_msg(target.to_owned(), queued_message.payload)
                        .await
                    {
                        Ok(_) => Ok(()),
                        Err(e) => Err(e),
                    }
                }
                None => Ok(()), // currently, we only send private events
            };

            match result {
                Ok(()) => {
                    tracing::info!("Successfully sent retry message {}", queued_message.id);
                    if let Err(e) = self.nostr_store.succeed_retry(&queued_message.id).await {
                        tracing::error!("Failed to mark retry message as sent: {e}");
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to send retry message: {e}");
                    failed_ids.push(queued_message.id.clone());
                }
            }
            retried += 1;
        }

        for failed in failed_ids {
            if let Err(e) = self.nostr_store.fail_retry(&failed).await {
                tracing::error!("Failed to store failed retry attempt: {e}");
            }
        }
        Ok(retried)
    }

    async fn fetch_relay_list(
        &self,
        npub: PublicKey,
        relays: Vec<RelayUrl>,
    ) -> Result<Vec<RelayUrl>> {
        self.client.fetch_relay_list(npub, relays).await
    }
}

#[derive(Clone)]
pub struct Consumer {
    client: Arc<Client>,
    nostr_store: Arc<dyn NostrRepository>,
    event_channel: NostrEventChannel,
}

impl Consumer {
    pub fn new(
        client: Arc<Client>,
        nostr_store: Arc<dyn NostrRepository>,
        event_channel: NostrEventChannel,
    ) -> Self {
        Self {
            client,
            nostr_store,
            event_channel,
        }
    }

    pub async fn start(&self) -> Result<JoinSet<()>> {
        let client = self.client.clone();
        let mut tasks = JoinSet::new();

        if !client.is_connected()
            && let Err(e) = client.connect().await
        {
            tracing::error!("Failed to connect Nostr client: {e}");
        }

        let mut earliest_offset = Timestamp::now();
        let offset = get_offset(&self.nostr_store).await;
        if offset != Timestamp::zero() && offset < earliest_offset {
            earliest_offset = offset;
        }

        let nostr_filter = nostr_sdk::Filter::new()
            .kind(nostr_sdk::Kind::GiftWrap)
            .pubkey(client.signer.public_key());

        client.subscribe(nostr_filter).await.map_err(|e| {
            tracing::error!("Failed to subscribe to Nostr public events: {e}");
            Error::Network("Failed to subscribe to Nostr public events".to_string())
        })?;

        let offset_store_clone = self.nostr_store.clone();
        let signer = self.client.signer().await?;
        let event_channel = self.event_channel.clone();
        tasks.spawn(async move {
            client
                .client
                .handle_notifications(move |note| {
                    let offset_store = offset_store_clone.clone();
                    let signer = signer.clone();
                    let event_channel = event_channel.clone();
                    async move {
                        if let RelayPoolNotification::Event { event, .. } = note
                            && should_process(event.clone(), &offset_store, earliest_offset).await
                        {
                            let (success, time) =
                                process_event(event.clone(), signer.clone(), event_channel.clone())
                                    .await?;
                            add_offset(&offset_store, event.id, time, success).await;
                        }
                        Ok(false) // keep looping
                    }
                })
                .await
                .unwrap_or_else(|e| {
                    tracing::error!("Nostr notification handler failed: {e}");
                });
        });

        Ok(tasks)
    }
}

async fn add_offset(
    db: &Arc<dyn NostrRepository>,
    event_id: EventId,
    time: Timestamp,
    success: bool,
) {
    db.add_event(NostrEventOffset {
        event_id: event_id.to_hex(),
        time,
        success,
    })
    .await
    .map_err(|e| tracing::error!("Could not store event offset: {e}"))
    .ok();
}

async fn get_offset(db: &Arc<dyn NostrRepository>) -> Timestamp {
    db.current_offset().await.unwrap_or_else(|e| {
        tracing::error!("Could not get event offset: {e}");
        Timestamp::zero()
    })
}

fn check_send_output(output: Output<EventId>, context: &str) -> Result<()> {
    for (relay, error) in &output.failed {
        tracing::warn!("{context}: relay {relay} failed: {error}");
    }
    if output.success.is_empty() {
        tracing::error!("{context}: all relays failed to accept the event");
        return Err(Error::Network(format!(
            "{context}: all relays failed to accept the event"
        )));
    }
    Ok(())
}

async fn process_event(
    event: Box<Event>,
    signer: Arc<dyn NostrSigner>,
    event_channel: NostrEventChannel,
) -> Result<(bool, Timestamp)> {
    let (success, time) = match event.kind {
        Kind::GiftWrap => {
            tracing::debug!(
                "Processing Nostr nip 17 direct message event with id: {}",
                event.id
            );
            match handle_nip17_direct_message(event.clone(), &signer, event_channel).await {
                Err(e) => {
                    tracing::error!("Failed to handle nip 17 direct message: {e}");
                    (false, Timestamp::zero())
                }
                Ok(_) => (true, event.created_at),
            }
        }
        _ => (true, Timestamp::zero()),
    };

    Ok((success, time))
}

async fn handle_nip17_direct_message<T: NostrSigner>(
    event: Box<Event>,
    signer: &T,
    event_channel: NostrEventChannel,
) -> Result<()> {
    let UnwrappedGift { rumor, sender } = UnwrappedGift::from_gift_wrap(signer, &event).await?;
    let sender_npub = sender.to_bech32();
    let sender_pub_key = sender.to_hex();
    if rumor.kind == nostr_sdk::Kind::PrivateDirectMessage {
        if let Ok(data) = base58::decode(rumor.content.as_str())
            && let Ok(envelope) = borsh::from_slice::<EventEnvelope>(&data)
        {
            tracing::debug!(
                "Processing event: {} {} from {sender_npub:?} (hex: {sender_pub_key})",
                envelope.event_type,
                envelope.version
            );
            match envelope.event_type {
                bcr_wallet_core::event::EventType::ContactPayment => {
                    if let Ok(payload) = borsh::from_slice::<ContactPaymentPayload>(&envelope.data)
                    {
                        event_channel.publish(NostrWalletEvent::ContactPayment {
                            event_id: event.id,
                            payload,
                            sender: event.pubkey,
                        });
                    }
                }
            };
        } else if let Ok(cdk18_payload) =
            serde_json::from_str::<cdk18::PaymentRequestPayload>(&rumor.content)
        {
            event_channel.publish(NostrWalletEvent::Cdk18Payment {
                event_id: event.id,
                payload: cdk18_payload,
                sender: event.pubkey,
            });
        } else {
            tracing::debug!(
                "Nostr nip 17 message with id: {} wasn't of any type we handle - ignoring",
                event.id
            );
        }
    } else {
        tracing::debug!(
            "Nostr nip 17 message with id: {} is not private direct message - ignoring",
            event.id
        );
    }
    Ok(())
}

async fn should_process(
    event: Box<Event>,
    offset_store: &Arc<dyn NostrRepository>,
    since: Timestamp,
) -> bool {
    valid_time(event.kind, event.created_at, since)
        && !offset_store
            .is_processed(&event.id.to_hex())
            .await
            .unwrap_or(false)
}

fn valid_time(kind: Kind, created: Timestamp, since: Timestamp) -> bool {
    if !matches!(kind, Kind::EncryptedDirectMessage | Kind::GiftWrap) {
        created >= since
    } else {
        true
    }
}
