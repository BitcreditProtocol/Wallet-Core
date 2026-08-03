use bcr_common::{
    cashu::{self, CurrencyUnit},
    core::NodeId,
    wire::common as wire_common,
};
use bcr_wallet_core::types::TransactionFees;
use bitcoin::secp256k1;
use nostr::types::RelayUrl;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct SwapConfig {
    pub expiry: chrono::TimeDelta,
    pub alpha_pk: secp256k1::PublicKey,
}

#[derive(Debug, Clone)]
pub enum WalletPaymentType {
    Cdk18 {
        transport: cashu::Transport,
        id: Option<String>,
    },
    OnChain,
    Token,
    Contact {
        contact_id: Uuid,
        payment_request_id: Option<Uuid>,
    },
    SharedPaymentRequest {
        node_id: NodeId,
    },
}

pub struct PayReference {
    pub request_id: Uuid,
    pub unit: CurrencyUnit,
    pub fees: TransactionFees,
    pub ptype: WalletPaymentType,
    pub memo: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct WalletBalance {
    pub debit: cashu::Amount,
    pub credit: cashu::Amount,
    pub total: cashu::Amount,
}

#[derive(Debug, Clone)]
pub struct WalletDetailedBalanceEntry {
    pub kid: cashu::Id,
    pub final_expiry: Option<u64>,
    pub amount: cashu::Amount,
}

#[derive(Debug, Clone)]
pub struct WalletProtestResult {
    pub status: wire_common::ProtestStatus,
    pub result: Option<(cashu::Amount, Vec<cashu::PublicKey>)>,
}

#[derive(Debug, Clone)]
pub struct WalletInfo {
    pub name: String,
    pub node_id: NodeId,
    pub network: bitcoin::Network,
    pub default_mint_url: url::Url,
    pub nostr_relays: Vec<RelayUrl>,
}
