pub mod error;
#[cfg(feature = "redb")]
pub mod redb;
#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;

use crate::error::Result;
use async_trait::async_trait;
use bcr_common::cashu::{self, nut00 as cdk00, nut01 as cdk01, nut07 as cdk07};
use bcr_common::core::NodeId;
use bcr_wallet_core::contact::Contact;
use bcr_wallet_core::types::{
    ForeignMintProof, PaymentRequestDirection, PaymentRequestState, Transaction,
    TransactionLinkReason, TransactionStatus,
};
use bcr_wallet_core::{
    SendSync,
    types::{PaymentRequest, WalletConfig},
};
use bitcoin::secp256k1;
use nostr::{RelayUrl, types::Timestamp};
use std::collections::HashMap;
use uuid::Uuid;

///////////////////////////////////////////// SwapCommitmentRecord
#[derive(Debug, Clone)]
pub struct SwapCommitmentRecord {
    pub inputs: Vec<cashu::PublicKey>,
    pub outputs: Vec<cashu::BlindedMessage>,
    pub expiry: u64,
    pub commitment: secp256k1::schnorr::Signature,
    pub ephemeral_secret: secp256k1::SecretKey,
    pub body_content: String,
    pub wallet_key: cashu::PublicKey,
    pub premints: HashMap<cashu::Id, cdk00::PreMintSecrets>,
}

///////////////////////////////////////////// MeltCommitmentRecord
#[derive(Debug, Clone)]
pub struct MeltCommitmentRecord {
    pub quote_id: Uuid,
    pub expiry: u64,
    pub commitment: secp256k1::schnorr::Signature,
    pub ephemeral_secret: secp256k1::SecretKey,
    pub body_content: String,
}

///////////////////////////////////////////// PocketRepository
#[cfg_attr(any(test, feature = "test-utils"), mockall::automock)]
#[async_trait]
pub trait PocketRepository: SendSync {
    async fn store_new(&self, proof: cdk00::Proof) -> Result<cdk01::PublicKey>;
    async fn store_pendingspent(&self, proof: cdk00::Proof) -> Result<cdk01::PublicKey>;
    async fn load_proof(&self, y: cdk01::PublicKey) -> Result<(cdk00::Proof, cdk07::State)>;
    async fn load_proofs(
        &self,
        ys: &[cdk01::PublicKey],
    ) -> Result<HashMap<cdk01::PublicKey, cdk00::Proof>>;
    async fn delete_proof(
        &self,
        y: cdk01::PublicKey,
    ) -> Result<Option<(cdk00::Proof, cdk07::State)>>;
    async fn list_unspent(&self) -> Result<HashMap<cdk01::PublicKey, cdk00::Proof>>;
    async fn list_pending(&self) -> Result<HashMap<cdk01::PublicKey, cdk00::Proof>>;
    async fn list_reserved(&self) -> Result<HashMap<cdk01::PublicKey, cdk00::Proof>>;
    async fn list_spent(&self) -> Result<HashMap<cdk01::PublicKey, cdk00::Proof>>;
    async fn list_all(&self) -> Result<Vec<cdk01::PublicKey>>;
    async fn mark_as_pendingspent(&self, y: cdk01::PublicKey) -> Result<cdk00::Proof>;
    async fn mark_pending_as_spent(&self, y: cdk01::PublicKey) -> Result<cdk00::Proof>;
    async fn revert_pendingspent_to_unspent(&self, y: cdk01::PublicKey) -> Result<cdk00::Proof>;

    async fn counter(&self, kid: cashu::Id) -> Result<u32>;
    async fn increment_counter(&self, kid: cashu::Id, old: u32, increment: u32) -> Result<()>;

    async fn store_commitment(&self, record: SwapCommitmentRecord) -> Result<()>;
    async fn load_commitment(
        &self,
        commitment: secp256k1::schnorr::Signature,
    ) -> Result<SwapCommitmentRecord>;
    async fn delete_commitment(&self, commitment: secp256k1::schnorr::Signature) -> Result<()>;
    async fn list_commitments(&self) -> Result<Vec<SwapCommitmentRecord>>;
    async fn delete_repo(&self) -> Result<()>;

    async fn store_foreign_mint_proof(
        &self,
        foreign_mint_proof: ForeignMintProof,
    ) -> Result<cdk01::PublicKey>;
    async fn load_foreign_mint_proofs(&self) -> Result<Vec<ForeignMintProof>>;
    async fn delete_foreign_mint_proofs(
        &self,
        clowder_id: secp256k1::PublicKey,
        ys: Vec<cdk01::PublicKey>,
    ) -> Result<()>;
}

///////////////////////////////////////////// PurseRepository
#[cfg_attr(any(test, feature = "test-utils"), mockall::automock)]
#[async_trait]
pub trait PurseRepository: SendSync {
    async fn store(&self, wallet: WalletConfig) -> Result<()>;
    async fn load(&self, wallet_id: &str) -> Result<WalletConfig>;
    async fn delete(&self, wallet_id: &str) -> Result<()>;
    async fn list_ids(&self) -> Result<Vec<String>>;
}

///////////////////////////////////////////// TransactionRepository
#[cfg_attr(any(test, feature = "test-utils"), mockall::automock)]
#[async_trait]
pub trait TransactionRepository: SendSync {
    async fn store_tx(&self, tx: Transaction) -> Result<Uuid>;
    async fn load_tx(&self, tx_id: Uuid) -> Result<Transaction>;
    async fn list_tx_ids(&self) -> Result<Vec<Uuid>>;
    async fn list_txs(&self) -> Result<Vec<Transaction>>;
    async fn update_status(
        &self,
        tx_id: Uuid,
        status: TransactionStatus,
    ) -> Result<Option<TransactionStatus>>;
    async fn update_memo(&self, tx_id: Uuid, new_memo: Option<String>) -> Result<Option<String>>;
    async fn link_txs(
        &self,
        tx_id_1: Uuid,
        tx_id_2: Uuid,
        reason: TransactionLinkReason,
    ) -> Result<()>;
    async fn delete_repo(&self) -> Result<()>;
}

///////////////////////////////////////////// Mint Melt Repository

#[derive(Debug)]
pub struct MintRecord {
    pub summary: bcr_wallet_core::types::MintSummary,
    pub premint: cdk00::PreMintSecrets,
    pub content: String,
    pub commitment: bitcoin::secp256k1::schnorr::Signature,
    pub ephemeral_secret: secp256k1::SecretKey,
}

#[cfg_attr(any(test, feature = "test-utils"), mockall::automock)]
#[async_trait]
pub trait MintMeltRepository: SendSync {
    // melt
    async fn store_melt(
        &self,
        qid: String,
        premints: Option<cdk00::PreMintSecrets>,
    ) -> Result<String>;
    async fn load_melt(&self, qid: String) -> Result<cdk00::PreMintSecrets>;
    async fn list_melts(&self) -> Result<Vec<String>>;
    async fn delete_melt(&self, qid: String) -> Result<()>;
    // mint
    async fn store_mint(
        &self,
        quote_id: Uuid,
        amount: bitcoin::Amount,
        address: bitcoin::Address<bitcoin::address::NetworkUnchecked>,
        expiry: u64,
        premints: cdk00::PreMintSecrets,
        content: String,
        commitment: bitcoin::secp256k1::schnorr::Signature,
        ephemeral_secret: secp256k1::SecretKey,
    ) -> Result<Uuid>;
    async fn load_mint(&self, qid: Uuid) -> Result<MintRecord>;
    async fn list_mints(&self) -> Result<Vec<Uuid>>;
    async fn delete_mint(&self, qid: Uuid) -> Result<()>;
    // melt commitment
    async fn store_melt_commitment(&self, record: MeltCommitmentRecord) -> Result<()>;
    async fn load_melt_commitment(&self, quote_id: Uuid) -> Result<MeltCommitmentRecord>;
    async fn delete_melt_commitment(&self, quote_id: Uuid) -> Result<()>;
    async fn list_melt_commitments(&self) -> Result<Vec<MeltCommitmentRecord>>;
    async fn delete_repo(&self) -> Result<()>;
}

//////////////////////////////////////////// Nostr
#[derive(Debug, Clone)]
pub struct NostrEventOffset {
    pub event_id: String,
    pub time: Timestamp,
    pub success: bool,
}

#[derive(Clone, Debug)]
pub struct NostrQueuedMessage {
    pub id: String,
    /// `Some(target)` for private messages, `None` for public broadcast messages.
    pub recipient: Option<String>,
    pub payload: String,
}

#[cfg_attr(any(test, feature = "test-utils"), mockall::automock)]
#[async_trait]
pub trait NostrRepository: SendSync {
    async fn current_offset(&self) -> Result<Timestamp>;
    async fn is_processed(&self, event_id: &str) -> Result<bool>;
    async fn add_event(&self, data: NostrEventOffset) -> Result<()>;
    async fn add_retry_message(&self, message: NostrQueuedMessage, max_retries: i32) -> Result<()>;
    async fn get_retry_messages(&self, limit: u64) -> Result<Vec<NostrQueuedMessage>>;
    async fn fail_retry(&self, id: &str) -> Result<()>;
    async fn succeed_retry(&self, id: &str) -> Result<()>;
    async fn delete_repo(&self) -> Result<()>;
}

//////////////////////////////////////////// Contact
#[cfg_attr(any(test, feature = "test-utils"), mockall::automock)]
#[async_trait]
pub trait ContactStoreApi: SendSync {
    async fn add_contact(&self, contact: Contact) -> Result<Uuid>;
    async fn edit_contact(&self, id: Uuid, contact: Contact) -> Result<()>;
    async fn edit_contact_relays(&self, id: Uuid, relays: Vec<RelayUrl>) -> Result<()>;
    async fn delete_contact(&self, id: Uuid) -> Result<()>;
    async fn get_contact(&self, id: Uuid) -> Result<Option<Contact>>;
    async fn get_contacts_by_node_id(&self, node_id: NodeId) -> Result<Vec<Contact>>;
    async fn list_contacts(&self, search_term: Option<String>) -> Result<Vec<Contact>>;
    async fn delete_repo(&self) -> Result<()>;
}

//////////////////////////////////////////// Pending Incoming and Outgoing Payment Requests
#[cfg_attr(any(test, feature = "test-utils"), mockall::automock)]
#[async_trait]
pub trait PaymentRequestStoreApi: SendSync {
    async fn add_payment_request(&self, payment_request: PaymentRequest) -> Result<()>;
    async fn get_payment_request(&self, id: Uuid) -> Result<Option<PaymentRequest>>;
    async fn list_payment_requests(
        &self,
        direction: PaymentRequestDirection,
        states: &[PaymentRequestState],
    ) -> Result<Vec<PaymentRequest>>;
    async fn set_payment_request_state(&self, id: Uuid, state: PaymentRequestState) -> Result<()>;
    // delete repo
    async fn delete_repo(&self) -> Result<()>;
}
