// ----- standard library imports
// ----- extra library imports
use bitcoin::hashes::sha256::Hash as Sha256;
use bitcoin::secp256k1::PublicKey;
use cashu::{Proof, ProofDleq};
use serde::{Deserialize, Serialize};
// ----- local imports
// ----- end imports

// TODO, move to bcr-common

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectedMintsResponse {
    pub mint_urls: Vec<cashu::MintUrl>,
    pub clowder_urls: Vec<reqwest::Url>,
    pub node_ids: Vec<bitcoin::secp256k1::PublicKey>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectedMintResponse {
    pub mint_url: cashu::MintUrl,
    pub clowder_url: reqwest::Url,
    pub node_id: bitcoin::secp256k1::PublicKey,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PathRequest {
    pub origin_mint_url: cashu::MintUrl,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExchangeRequest {
    pub alpha_proofs: Vec<cashu::Proof>,
    pub exchange_path: Vec<bitcoin::secp256k1::PublicKey>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExchangeResponse {
    pub beta_proofs: Vec<cashu::Proof>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublicKeyResponse {
    pub public_key: bitcoin::secp256k1::PublicKey,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProofFingerprint {
    pub amount: cashu::Amount,
    pub keyset_id: cashu::Id,
    pub c: cashu::PublicKey,
    pub y: cashu::PublicKey,
    pub dleq: Option<ProofDleq>,
    pub witness: Option<cashu::Witness>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubstituteExchangeRequest {
    pub proofs: Vec<ProofFingerprint>,
    pub locks: Vec<Sha256>,
    pub wallet_pubkey: PublicKey,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubstituteExchangeResponse {
    pub outputs: Vec<Proof>,
    pub signature: bitcoin::secp256k1::schnorr::Signature,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OfflineResponse {
    pub offline: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum RabidReason {
    Forked(u64, Sha256, Sha256),
    HashSeqDiscrepancy(u64, Sha256, Sha256),
    // TODO needs to be signed by a time service so the timestamp can't be made up
    Offline(u64),
}

// Hash order doesn't matter
impl PartialEq for RabidReason {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (RabidReason::Forked(a1, a2, a3), RabidReason::Forked(b1, b2, b3)) => {
                a1 == b1 && ((a2, a3) == (b2, b3) || (a2, a3) == (b3, b2))
            }
            (
                RabidReason::HashSeqDiscrepancy(a1, a2, a3),
                RabidReason::HashSeqDiscrepancy(b1, b2, b3),
            ) => a1 == b1 && ((a2, a3) == (b2, b3) || (a2, a3) == (b3, b2)),
            (RabidReason::Offline(a), RabidReason::Offline(b)) => a == b,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum AlphaState {
    /// Last seen timestamp
    Online(u64),
    /// Last seen timestamp
    Offline(u64),
    /// Pre Rabid
    Rabid(RabidReason),
    /// Post Rabid
    ConfiscatedRabid(bitcoin::Txid, RabidReason),
}
