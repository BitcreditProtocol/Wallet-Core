use bcr_common::{
    cashu::{self, Amount, CurrencyUnit},
    wire::common as wire_common,
};
use bitcoin::secp256k1;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct SwapConfig {
    pub expiry: chrono::TimeDelta,
    pub alpha_pk: secp256k1::PublicKey,
}

pub enum WalletPaymentType {
    Cdk18 {
        transport: cashu::Transport,
        id: Option<String>,
    },
    OnChain,
    Token,
}

pub struct PayReference {
    pub request_id: Uuid,
    pub unit: CurrencyUnit,
    pub fees: Amount,
    pub ptype: WalletPaymentType,
    pub memo: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct WalletBalance {
    pub debit: cashu::Amount,
    pub credit: cashu::Amount,
}

#[derive(Debug, Clone)]
pub struct WalletProtestResult {
    pub status: wire_common::ProtestStatus,
    pub result: Option<(cashu::Amount, Vec<cashu::PublicKey>)>,
}
