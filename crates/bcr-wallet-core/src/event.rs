use bcr_common::{
    cashu::{CurrencyUnit, MintUrl, Proof},
    core::NodeId,
    wire::borsh::{
        deserialize_from_str, deserialize_vecof_cdkproof, serialize_as_str,
        serialize_vecof_cdkproof,
    },
};
use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

const DEFAULT_EVENT_VERSION: &str = "1.0";

fn get_version(_event_type: &EventType) -> String {
    DEFAULT_EVENT_VERSION.into()
}

#[derive(
    strum::VariantArray,
    strum::Display,
    Serialize,
    Deserialize,
    Debug,
    Clone,
    PartialEq,
    BorshSerialize,
    BorshDeserialize,
)]
pub enum EventType {
    ContactPayment,
}

#[derive(Debug, Clone, BorshSerialize)]
pub struct Event<T: BorshSerialize> {
    pub event_type: EventType,
    pub version: String,
    pub data: T,
}

impl<T: BorshSerialize> Event<T> {
    pub fn new(event_type: EventType, data: T) -> Self {
        Self {
            event_type: event_type.to_owned(),
            version: get_version(&event_type),
            data,
        }
    }

    pub fn new_contact_payment(data: T) -> Self {
        Self::new(EventType::ContactPayment, data)
    }
}

impl<T: BorshSerialize> TryFrom<Event<T>> for EventEnvelope {
    type Error = std::io::Error;

    fn try_from(event: Event<T>) -> Result<Self, Self::Error> {
        let serialized = &borsh::to_vec(&event.data)?;
        Ok(Self {
            event_type: event.event_type,
            version: event.version,
            data: serialized.to_vec(),
        })
    }
}

impl<T: BorshDeserialize + BorshSerialize> TryFrom<EventEnvelope> for Event<T> {
    type Error = std::io::Error;
    fn try_from(envelope: EventEnvelope) -> Result<Self, Self::Error> {
        let data: T = borsh::from_slice(&envelope.data)?;
        Ok(Self {
            event_type: envelope.event_type,
            version: envelope.version,
            data,
        })
    }
}

#[derive(BorshSerialize, BorshDeserialize, Debug, Clone)]
pub struct EventEnvelope {
    pub event_type: EventType,
    pub version: String,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub struct ContactPaymentPayload {
    pub sender: NodeId,
    #[borsh(
        serialize_with = "serialize_vecof_cdkproof",
        deserialize_with = "deserialize_vecof_cdkproof"
    )]
    pub proofs: Vec<Proof>,
    pub memo: Option<String>,
    #[borsh(
        serialize_with = "serialize_as_str",
        deserialize_with = "deserialize_from_str"
    )]
    pub mint: MintUrl,
    #[borsh(
        serialize_with = "serialize_as_str",
        deserialize_with = "deserialize_from_str"
    )]
    pub unit: CurrencyUnit,
    pub created_at: u64,
}
