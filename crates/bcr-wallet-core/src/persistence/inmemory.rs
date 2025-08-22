// ----- standard library imports
use std::{
    collections::HashMap,
    str::FromStr,
    sync::{Arc, Mutex},
};
// ----- extra library imports
use anyhow::Error as AnyError;
use async_trait::async_trait;
use cashu::{nut00 as cdk00, nut01 as cdk01, nut07 as cdk07};
use cdk::wallet::types::{Transaction, TransactionId};
// ----- local imports
use crate::{
    error::{Error, Result},
    pocket::{PocketRepository, debit::MintMeltRepository},
    purse::PurseRepository,
    types::WalletConfig,
    wallet::TransactionRepository,
};

// ----- end imports

///////////////////////////////////////////// InMemoryPocketRepository
#[derive(Default)]
pub struct InMemoryPocketRepository {
    unspent: Arc<Mutex<HashMap<cdk01::PublicKey, cdk00::Proof>>>,
    pending: Arc<Mutex<HashMap<cdk01::PublicKey, cdk00::Proof>>>,

    counter: Arc<Mutex<HashMap<cashu::Id, u32>>>,
}

#[async_trait]
impl PocketRepository for InMemoryPocketRepository {
    async fn store_new(&self, proof: cdk00::Proof) -> Result<cdk01::PublicKey> {
        let mut unspent = self.unspent.lock().unwrap();
        let y = proof.y()?;
        unspent.insert(y, proof);
        Ok(y)
    }
    async fn store_pendingspent(&self, proof: cdk00::Proof) -> Result<cdk01::PublicKey> {
        let mut pending = self.pending.lock().unwrap();
        let y = proof.y()?;
        pending.insert(y, proof);
        Ok(y)
    }
    async fn load_proof(&self, y: cdk01::PublicKey) -> Result<(cdk00::Proof, cdk07::State)> {
        let unspent = self.unspent.lock().unwrap();
        if let Some(proof) = unspent.get(&y) {
            return Ok((proof.clone(), cdk07::State::Unspent));
        }
        let pending = self.pending.lock().unwrap();
        if let Some(proof) = pending.get(&y) {
            return Ok((proof.clone(), cdk07::State::PendingSpent));
        }
        Err(Error::ProofNotFound(y))
    }
    async fn delete_proof(&self, y: cdk01::PublicKey) -> Result<()> {
        let mut unspent = self.unspent.lock().unwrap();
        if unspent.remove(&y).is_some() {
            return Ok(());
        }
        let mut pending = self.pending.lock().unwrap();
        pending.remove(&y);
        Ok(())
    }
    async fn list_unspent(&self) -> Result<HashMap<cdk01::PublicKey, cdk00::Proof>> {
        let unspent = self.unspent.lock().unwrap();
        Ok(unspent.clone())
    }
    async fn list_pending(&self) -> Result<HashMap<cdk01::PublicKey, cdk00::Proof>> {
        let pending = self.pending.lock().unwrap();
        Ok(pending.clone())
    }
    async fn list_reserved(&self) -> Result<HashMap<cdk01::PublicKey, cdk00::Proof>> {
        Ok(Default::default())
    }
    async fn list_all(&self) -> Result<Vec<cdk01::PublicKey>> {
        let unspent = self.unspent.lock().unwrap();
        let pending = self.pending.lock().unwrap();
        let keys = unspent.keys().chain(pending.keys()).cloned().collect();
        Ok(keys)
    }

    async fn mark_as_pendingspent(&self, y: cdk01::PublicKey) -> Result<cdk00::Proof> {
        let mut unspent = self.unspent.lock().unwrap();
        let Some(proof) = unspent.remove(&y) else {
            return Err(Error::ProofNotFound(y));
        };
        let mut pending = self.pending.lock().unwrap();
        pending.insert(y, proof.clone());
        Ok(proof)
    }

    async fn counter(&self, kid: cashu::Id) -> Result<u32> {
        let counter = self.counter.lock().unwrap();
        let val = counter.get(&kid).cloned().unwrap_or_default();
        Ok(val)
    }
    async fn increment_counter(&self, kid: cashu::Id, old: u32, increment: u32) -> Result<()> {
        let mut counter = self.counter.lock().unwrap();
        let val = counter.get(&kid).cloned().unwrap_or_default();
        if val != old {
            return Err(Error::Any(AnyError::msg(
                "InMemoryPocketRepository::increment_counter old counter mismatch",
            )));
        }

        let new_val = val + increment;
        counter.insert(kid, new_val);
        Ok(())
    }
}

///////////////////////////////////////////// InMemoryPurseRepository
#[derive(Default)]
pub struct InMemoryPurseRepository {
    wallets: Arc<Mutex<HashMap<String, WalletConfig>>>,
}

#[async_trait]
impl PurseRepository for InMemoryPurseRepository {
    async fn store(&self, wallet: WalletConfig) -> Result<()> {
        let mut wallets = self.wallets.lock().unwrap();
        wallets.insert(wallet.wallet_id.clone(), wallet);
        Ok(())
    }
    async fn load(&self, wallet_id: &str) -> Result<WalletConfig> {
        let wallets = self.wallets.lock().unwrap();
        wallets
            .get(wallet_id)
            .cloned()
            .ok_or_else(|| Error::WalletIdNotFound(wallet_id.to_string()))
    }
    async fn delete(&self, wallet_id: &str) -> Result<()> {
        let mut wallets = self.wallets.lock().unwrap();
        wallets.remove(wallet_id);
        Ok(())
    }
    async fn list_ids(&self) -> Result<Vec<String>> {
        let wallets = self.wallets.lock().unwrap();
        Ok(wallets.keys().cloned().collect())
    }
}

///////////////////////////////////////////// InMemoryTransactionRepository
#[derive(Default)]
pub struct InMemoryTransactionRepository {
    transactions: Arc<Mutex<HashMap<String, Transaction>>>,
}

#[async_trait]
impl TransactionRepository for InMemoryTransactionRepository {
    async fn store_tx(&self, tx: Transaction) -> Result<TransactionId> {
        let mut transactions = self.transactions.lock().unwrap();
        let tx_id = tx.id();
        transactions.insert(tx_id.to_string(), tx);
        Ok(tx_id)
    }
    async fn load_tx(&self, tx_id: TransactionId) -> Result<Transaction> {
        let transactions = self.transactions.lock().unwrap();
        transactions
            .get(&tx_id.to_string())
            .cloned()
            .ok_or_else(|| Error::TransactionNotFound(tx_id))
    }
    #[allow(dead_code)]
    async fn delete_tx(&self, tx_id: TransactionId) -> Result<()> {
        let mut transactions = self.transactions.lock().unwrap();
        transactions.remove(&tx_id.to_string());
        Ok(())
    }
    async fn list_tx_ids(&self) -> Result<Vec<TransactionId>> {
        let transactions = self.transactions.lock().unwrap();
        let tx_ids = transactions
            .keys()
            .map(|id| TransactionId::from_str(id).unwrap())
            .collect();
        Ok(tx_ids)
    }
}

///////////////////////////////////////////// InMemoryMeltRepository
#[derive(Default)]
pub struct InMemoryMintMeltRepository {
    melts: Arc<Mutex<HashMap<String, Option<cdk00::PreMintSecrets>>>>,
}
#[async_trait]
impl MintMeltRepository for InMemoryMintMeltRepository {
    async fn store_melt(
        &self,
        qid: String,
        premints: Option<cdk00::PreMintSecrets>,
    ) -> Result<String> {
        self.melts.lock().unwrap().insert(qid.clone(), premints);
        Ok(qid)
    }
    async fn load_melt(&self, qid: String) -> Result<cdk00::PreMintSecrets> {
        let melts = self.melts.lock().unwrap();
        let value = melts
            .get(&qid)
            .cloned()
            .ok_or_else(|| Error::MeltNotFound(qid.clone()))?;
        value.ok_or(Error::MeltNotFound(qid))
    }
    async fn list_melts(&self) -> Result<Vec<String>> {
        let melts = self.melts.lock().unwrap();
        Ok(melts.keys().cloned().collect())
    }
    async fn delete_melt(&self, qid: String) -> Result<()> {
        let mut melts = self.melts.lock().unwrap();
        melts.remove(&qid);
        Ok(())
    }
}
