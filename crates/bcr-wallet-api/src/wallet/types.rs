use crate::config::SameMintSafeMode;
use bcr_common::cashu::{self, Amount, CurrencyUnit};
use bitcoin::secp256k1;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub enum SafeMode {
    Disabled,
    Enabled {
        expire: chrono::TimeDelta,
        alpha_pk: secp256k1::PublicKey,
    },
}

impl SafeMode {
    pub fn new(safe_mode: SameMintSafeMode, alpha_pk: secp256k1::PublicKey) -> Self {
        match safe_mode {
            SameMintSafeMode::Disabled => SafeMode::Disabled,
            SameMintSafeMode::Enabled { expiration } => SafeMode::Enabled {
                expire: expiration,
                alpha_pk,
            },
        }
    }
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
