use crate::types::{PaymentRequestDirection, PaymentRequestState};
use bcr_common::{
    cashu::{self, Amount, CurrencyUnit, MintUrl, Proof},
    core::NodeId,
    wire::borsh::{
        deserialize_from_str, deserialize_from_u64, deserialize_vecof_cdkproof, serialize_as_str,
        serialize_as_u64, serialize_vecof_cdkproof,
    },
};
use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const DEFAULT_EVENT_VERSION: &str = "1.0";

fn get_version(_event_type: &EventType) -> String {
    DEFAULT_EVENT_VERSION.into()
}

/// Append only: borsh encodes each variant by its declaration order.
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
    PaymentRequestAction,
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

    pub fn new_payment_request_action(data: T) -> Self {
        Self::new(EventType::PaymentRequestAction, data)
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
            created_at: time::OffsetDateTime::now_utc().unix_timestamp() as u64,
        }
    }
}

#[derive(Debug, Clone, PartialEq, BorshSerialize, BorshDeserialize)]
pub enum PaymentRequestActionKind {
    Cancel,
    Reject,
}

impl PaymentRequestActionKind {
    pub fn actor_transition(&self) -> (PaymentRequestState, PaymentRequestDirection) {
        match self {
            Self::Cancel => (
                PaymentRequestState::Canceled,
                PaymentRequestDirection::Outgoing,
            ),
            Self::Reject => (
                PaymentRequestState::Rejected,
                PaymentRequestDirection::Incoming,
            ),
        }
    }
}

/// `actor` must match the Nostr seal sender.
#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub struct PaymentRequestActionPayload {
    pub payment_request_id: Uuid,
    pub action: PaymentRequestActionKind,
    pub actor: NodeId,
    pub acted_at: u64,
    pub reason: Option<String>,
}

impl PaymentRequestActionPayload {
    pub fn new(
        payment_request_id: Uuid,
        action: PaymentRequestActionKind,
        actor: NodeId,
        reason: Option<String>,
    ) -> Self {
        Self {
            payment_request_id,
            action,
            actor,
            acted_at: time::OffsetDateTime::now_utc().unix_timestamp() as u64,
            reason,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn test_event_type_existing_variants_keep_their_borsh_discriminant() {
        assert_eq!(
            borsh::to_vec(&EventType::ContactPayment).unwrap(),
            vec![0u8]
        );
        assert_eq!(
            borsh::to_vec(&EventType::ContactPaymentRequest).unwrap(),
            vec![1u8]
        );
        assert_eq!(
            borsh::to_vec(&EventType::PaymentRequestAction).unwrap(),
            vec![2u8]
        );
    }

    #[test]
    fn test_unknown_event_type_discriminant_fails_to_decode() {
        let unknown_discriminant = [42u8];
        assert!(borsh::from_slice::<EventType>(&unknown_discriminant).is_err());
    }

    #[test]
    fn test_payment_request_action_payload_round_trips_through_borsh() {
        let payload = PaymentRequestActionPayload::new(
            Uuid::new_v4(),
            PaymentRequestActionKind::Cancel,
            NodeId::from_str(
                "bitcrt03205b8dec12bc9e879f5b517aa32192a2550e88adcee3e54ec2c7294802568fef",
            )
            .unwrap(),
            Some("no longer needed".to_string()),
        );
        let event: EventEnvelope = Event::new_payment_request_action(payload.clone())
            .try_into()
            .unwrap();
        assert_eq!(event.event_type, EventType::PaymentRequestAction);

        let decoded: PaymentRequestActionPayload = borsh::from_slice(&event.data).unwrap();
        assert_eq!(decoded.payment_request_id, payload.payment_request_id);
        assert_eq!(decoded.action, payload.action);
        assert_eq!(decoded.actor, payload.actor);
        assert_eq!(decoded.reason, payload.reason);
    }
}
