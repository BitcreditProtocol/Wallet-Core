pub mod error;
#[cfg(feature = "redb")]
pub mod redb;
#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;

use crate::error::Result;
use async_trait::async_trait;
use bcr_common::cashu::{self, nut00 as cdk00, nut01 as cdk01, nut07 as cdk07};
use bcr_common::cdk::wallet::types::{Transaction, TransactionId};
use bcr_wallet_core::{SendSync, types::WalletConfig};
use bitcoin::secp256k1;
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
    async fn delete_proof(&self, y: cdk01::PublicKey) -> Result<Option<cdk00::Proof>>;
    async fn list_unspent(&self) -> Result<HashMap<cdk01::PublicKey, cdk00::Proof>>;
    async fn list_pending(&self) -> Result<HashMap<cdk01::PublicKey, cdk00::Proof>>;
    async fn list_reserved(&self) -> Result<HashMap<cdk01::PublicKey, cdk00::Proof>>;
    async fn list_all(&self) -> Result<Vec<cdk01::PublicKey>>;
    async fn mark_as_pendingspent(&self, y: cdk01::PublicKey) -> Result<cdk00::Proof>;

    async fn counter(&self, kid: cashu::Id) -> Result<u32>;
    async fn increment_counter(&self, kid: cashu::Id, old: u32, increment: u32) -> Result<()>;

    async fn store_commitment(&self, record: SwapCommitmentRecord) -> Result<()>;

    async fn load_commitment(
        &self,
        commitment: secp256k1::schnorr::Signature,
    ) -> Result<SwapCommitmentRecord>;

    async fn delete_commitment(
        &self,
        commitment: secp256k1::schnorr::Signature,
    ) -> Result<()>;

    async fn list_commitments(&self) -> Result<Vec<SwapCommitmentRecord>>;
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
    async fn store_tx(&self, tx: Transaction) -> Result<TransactionId>;
    async fn load_tx(&self, tx_id: TransactionId) -> Result<Transaction>;
    #[allow(dead_code)]
    async fn delete_tx(&self, tx_id: TransactionId) -> Result<()>;
    async fn list_tx_ids(&self) -> Result<Vec<TransactionId>>;
    async fn list_txs(&self) -> Result<Vec<Transaction>>;
    async fn update_metadata(
        &self,
        tx_id: TransactionId,
        key: String,
        value: String,
    ) -> Result<Option<String>>;
}

///////////////////////////////////////////// Mint Melt Repository

#[derive(Debug)]
pub struct MintRecord {
    pub summary: bcr_wallet_core::types::MintSummary,
    pub premint: cdk00::PreMintSecrets,
    pub content: String,
    pub commitment: bitcoin::secp256k1::schnorr::Signature,
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
    ) -> Result<Uuid>;
    async fn load_mint(&self, qid: Uuid) -> Result<MintRecord>;
    async fn list_mints(&self) -> Result<Vec<Uuid>>;
    async fn delete_mint(&self, qid: Uuid) -> Result<()>;
}
