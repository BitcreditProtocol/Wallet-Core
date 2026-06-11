use bcr_common::{
    cashu::{self, Amount, CurrencyUnit, MintUrl, Proof},
    core::NodeId,
    wire::borsh::{
        deserialize_from_str, deserialize_from_u64, deserialize_vecof_cdkproof, serialize_as_str,
        serialize_as_u64, serialize_vecof_cdkproof,
    },
};
use borsh::{BorshDeserialize, BorshSerialize};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

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
    ContactPaymentRequest,
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

    pub fn new_contact_payment_request(data: T) -> Self {
        Self::new(EventType::ContactPaymentRequest, data)
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
    pub payment_request_id: Option<Uuid>,
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

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub struct ContactPaymentRequestPayload {
    pub id: Uuid,
    pub sender: NodeId,
    pub memo: Option<String>,
    #[borsh(
        serialize_with = "serialize_as_str",
        deserialize_with = "deserialize_from_str"
    )]
    pub mint: MintUrl,
    #[borsh(
        serialize_with = "serialize_as_u64",
        deserialize_with = "deserialize_from_u64"
    )]
    pub amount: cashu::Amount,
    #[borsh(
        serialize_with = "serialize_as_str",
        deserialize_with = "deserialize_from_str"
    )]
    pub unit: CurrencyUnit,
    pub deadline: Option<u64>,
    pub created_at: u64,
}

impl ContactPaymentRequestPayload {
    pub fn new(
        node_id: NodeId,
        amount: Amount,
        unit: CurrencyUnit,
        memo: Option<String>,
        deadline: Option<u64>,
        mint: MintUrl,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            sender: node_id,
            memo,
            mint,
            amount,
            unit,
            deadline,
            created_at: Utc::now().timestamp() as u64,
        }
    }
}
