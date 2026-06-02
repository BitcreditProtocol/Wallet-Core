use std::{collections::HashMap, sync::Arc};

use bcr_common::cashu::{Amount, ProofsMethods, nut18 as cdk18};
use bcr_wallet_core::types::{
    PAYMENT_TYPE_METADATA_KEY, PaymentType, TRANSACTION_STATUS_METADATA_KEY, TransactionStatus,
};
use error::Result;
use nostr::{
    event::EventId,
    nips::{
        nip19::{FromBech32, Nip19Profile, ToBech32},
        nip59::UnwrappedGift,
    },
    signer::NostrSigner,
    types::RelayUrl,
};
use nostr_sdk::{Client, Keys, RelayPoolNotification, nips::nip06::FromMnemonic};
use tokio::sync::broadcast;
use uuid::Uuid;

pub mod error;

#[derive(Clone)]
pub struct NostrClient {
    client: Client,
    profile: Nip19Profile,
}

impl NostrClient {
    pub async fn new(mnemonic: &bip39::Mnemonic, nostr_relays: &[RelayUrl]) -> Result<Self> {
        let nostr_cfg = NostrConfig::new(mnemonic.to_owned(), nostr_relays.to_owned())?;
        let nostr_filter = nostr_sdk::Filter::new()
            .kind(nostr_sdk::Kind::GiftWrap)
            .pubkey(nostr_cfg.nostr_signer.public_key());
        let nostr_cl = nostr_sdk::Client::new(nostr_cfg.nostr_signer);
        for nostr_relay in &nostr_cfg.relays {
            nostr_cl.add_relay(nostr_relay).await?;
        }
        nostr_cl.connect().await;

        // create long-running subscription
        nostr_cl.subscribe(nostr_filter, None).await?;

        Ok(Self {
            client: nostr_cl,
            profile: nostr_cfg.nprofile,
        })
    }

    pub async fn send_private_msg(&self, target: String, payload: String) -> Result<EventId> {
        let receiver = Nip19Profile::from_bech32(&target)?;
        let output = self
            .client
            .send_private_msg_to(
                receiver.relays,
                receiver.public_key,
                payload,
                std::iter::empty(),
            )
            .await?;
        Ok(output.id().to_owned())
    }

    pub async fn transport(&self) -> Result<cdk18::Transport> {
        Ok(cdk18::Transport {
            _type: cdk18::TransportType::Nostr,
            target: self.profile.to_bech32()?,
            tags: vec![vec![String::from("n"), String::from("17")]],
        })
    }

    pub async fn signer(&self) -> Result<Arc<dyn NostrSigner>> {
        let signer = self.client.signer().await?;
        Ok(signer)
    }

    pub fn events_channel(&self) -> broadcast::Receiver<RelayPoolNotification> {
        self.client.notifications()
    }

    pub async fn shutdown(&self) {
        self.client.shutdown().await;
    }

    pub async fn handle_event(
        &self,
        received_evt: RelayPoolNotification,
        payment_id: Uuid,
        expected: Amount,
    ) -> Result<Option<(cdk18::PaymentRequestPayload, HashMap<String, String>)>> {
        let signer = self.signer().await?;
        let RelayPoolNotification::Event { event, .. } = received_evt else {
            return Ok(None);
        };
        if event.kind != nostr_sdk::Kind::GiftWrap {
            tracing::debug!("handle event, but no GiftWrap - {}", event.kind);
            return Ok(None);
        }

        let UnwrappedGift { rumor, .. } = match UnwrappedGift::from_gift_wrap(&signer, &event).await
        {
            Ok(gift) => gift,
            Err(e) => {
                tracing::error!("Unwrapping gift wrap failed: {e}");
                return Ok(None);
            }
        };

        let payload = if rumor.kind == nostr_sdk::Kind::PrivateDirectMessage {
            match serde_json::from_str::<cdk18::PaymentRequestPayload>(&rumor.content) {
                Ok(payload) => payload,
                Err(e) => {
                    tracing::error!("Parsing Payment Request failed: {e}");
                    return Ok(None);
                }
            }
        } else {
            tracing::debug!(
                "handle event, but rumor no PrivateDirectMessage - {}",
                rumor.kind
            );
            return Ok(None);
        };

        if payload.id != Some(payment_id.to_string()) {
            tracing::debug!("handle event, payment id doesn't match");
            return Ok(None);
        }

        let amount = payload.proofs.total_amount()?;
        if amount < expected {
            tracing::warn!(
                "Received amount {} is less than expected {}",
                amount,
                expected
            );
            return Ok(None);
        }
        let meta = HashMap::from([
            (String::from("sender"), event.pubkey.to_string()),
            (String::from("payment_id"), payment_id.to_string()),
            (String::from("nostr_event_id"), event.id.to_string()),
            (
                String::from(PAYMENT_TYPE_METADATA_KEY),
                PaymentType::Cdk18.to_string(),
            ),
            (
                String::from(TRANSACTION_STATUS_METADATA_KEY),
                TransactionStatus::Settled.to_string(),
            ),
        ]);

        Ok(Some((payload, meta)))
    }
}

#[derive(Debug, Clone)]
pub struct NostrConfig {
    pub nprofile: Nip19Profile,
    pub nostr_signer: Keys,
    pub relays: Vec<RelayUrl>,
}

impl NostrConfig {
    pub fn new(mnemonic: bip39::Mnemonic, nostr_relays: Vec<RelayUrl>) -> Result<Self> {
        let keys = Keys::from_mnemonic(mnemonic.to_string(), None)?;

        Ok(Self {
            nprofile: Nip19Profile::new(keys.public_key, nostr_relays.clone()),
            nostr_signer: keys,
            relays: nostr_relays,
        })
    }
}
