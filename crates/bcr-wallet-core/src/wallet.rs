// ----- standard library imports
// ----- extra library imports
use async_trait::async_trait;
use bcr_wallet_lib::wallet::Token;
use cashu::KeySetInfo;
use cdk::wallet::MintConnector;
// ----- local imports
use crate::error::{Error, Result};

// ----- end imports

/// trait that represents a single compartment in our wallet where we store proofs/tokens of the
/// same currency emitted by the same mint
#[async_trait(?Send)]
pub trait Pocket {
    async fn balance(&self) -> Result<cashu::Amount>;
    fn is_mine(&self, token: &Token) -> bool;
    async fn receive(
        &self,
        client: &dyn MintConnector,
        keysets_info: &[KeySetInfo],
        token: Token,
    ) -> Result<cashu::Amount>;
}

#[async_trait(?Send)]
pub trait CreditPocket: Pocket {}

#[async_trait(?Send)]
pub trait DebitPocket: Pocket {}

pub struct Wallet<Conn> {
    pub client: Conn,
    pub url: cashu::MintUrl,
    pub debit: Box<dyn DebitPocket>,
    pub credit: Box<dyn CreditPocket>,
    #[allow(dead_code)]
    pub mnemonic: bip39::Mnemonic,
    pub name: String,
}

#[derive(Debug, Clone, Default)]
pub struct WalletBalance {
    pub debit: cashu::Amount,
    pub credit: cashu::Amount,
}

impl<Conn> Wallet<Conn>
where
    Conn: MintConnector,
{
    pub async fn balance(&self) -> Result<WalletBalance> {
        let debit = self.debit.balance().await?;
        let credit = self.credit.balance().await?;
        Ok(WalletBalance { debit, credit })
    }

    pub async fn receive(&self, token: Token) -> Result<cashu::Amount> {
        let keysets_info = self.client.get_mint_keysets().await?.keysets;
        if self.credit.is_mine(&token) {
            tracing::debug!("import credit token");
            self.credit
                .receive(&self.client, &keysets_info, token)
                .await
        } else if self.debit.is_mine(&token) {
            tracing::debug!("import debit token");
            self.debit.receive(&self.client, &keysets_info, token).await
        } else {
            let teaser = token.to_string().chars().take(20).collect::<String>();
            return Err(Error::InvalidToken(teaser));
        }
    }
}
