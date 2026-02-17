pub mod tests {
    use crate::RedemptionSummary;
    use crate::Result;
    use crate::external::mint::ClowderMintConnector;
    use crate::pocket::{PocketApi, credit::CreditPocketApi, debit::DebitPocketApi};
    use crate::types::{MeltSummary, MintSummary, SendSummary};
    use crate::wallet::types::SafeMode;
    use async_trait::async_trait;
    use bcr_common::wire::melt as wire_melt;
    use std::collections::HashMap;
    use std::sync::Arc;
    use uuid::Uuid;

    use bcr_common::cashu::{self, Amount, CurrencyUnit, KeySetInfo};

    mockall::mock! {
        pub DebitPocket {}

        #[async_trait]
        impl PocketApi for DebitPocket {
            fn unit(&self) -> CurrencyUnit;
            async fn balance(&self) -> Result<Amount>;
            async fn receive_proofs(
                &self,
                client: Arc<dyn ClowderMintConnector>,
                keysets_info: &[KeySetInfo],
                proofs: Vec<cashu::Proof>,
                safe_mode: SafeMode,
            ) -> Result<(Amount, Vec<cashu::PublicKey>)>;
            async fn prepare_send(&self, amount: Amount, infos: &[KeySetInfo]) -> Result<SendSummary>;
            async fn send_proofs(
                &self,
                rid: Uuid,
                keysets_info: &[KeySetInfo],
                client: Arc<dyn ClowderMintConnector>,
                safe_mode: SafeMode,
            ) -> Result<HashMap<cashu::PublicKey, cashu::Proof>>;
            async fn cleanup_local_proofs(
                &self,
                client: Arc<dyn ClowderMintConnector>,
            ) -> Result<Vec<cashu::PublicKey>>;
            async fn restore_local_proofs(
                &self,
                keysets_info: &[KeySetInfo],
                client: Arc<dyn ClowderMintConnector>,
            ) -> Result<usize>;
            async fn delete_proofs(&self) -> Result<HashMap<cashu::Id, Vec<cashu::Proof>>>;
            async fn return_proofs_to_send_for_offline_payment(
                &self,
                rid: Uuid,
            ) -> Result<(Amount, HashMap<cashu::PublicKey, cashu::Proof>)>;
            async fn swap_to_unlocked_substitute_proofs(
                &self,
                proofs: Vec<cashu::Proof>,
                keysets_info: &[KeySetInfo],
                client: Arc<dyn ClowderMintConnector>,
                send_amount: Amount,
            ) -> Result<Vec<cashu::Proof>>;
        }

        #[async_trait]
        impl DebitPocketApi for DebitPocket {
            async fn reclaim_proofs(
                &self,
                ys: &[cashu::PublicKey],
                keysets_info: &[KeySetInfo],
                client: Arc<dyn ClowderMintConnector>,
                safe_mode: SafeMode,
            ) -> Result<Amount>;
            async fn prepare_onchain_melt(
                &self,
                invoice: wire_melt::OnchainInvoice,
                keysets_info: &[KeySetInfo],
                client: Arc<dyn ClowderMintConnector>,
            ) -> Result<MeltSummary>;
            async fn pay_onchain_melt(
                &self,
                rid: Uuid,
                keysets_info: &[KeySetInfo],
                client: Arc<dyn ClowderMintConnector>,
                safe_mode: SafeMode,
            ) -> Result<(bitcoin::Txid, HashMap<cashu::PublicKey, cashu::Proof>)>;
            async fn mint_onchain(
                &self,
                amount: bitcoin::Amount,
                client: Arc<dyn ClowderMintConnector>,
            ) -> Result<MintSummary>;
            async fn check_pending_mints(
                &self,
                keysets_info: &[KeySetInfo],
                client: Arc<dyn ClowderMintConnector>,
                tstamp: u64,
                safe_mode: SafeMode,
            ) -> Result<HashMap<Uuid, (cashu::Amount, Vec<cashu::PublicKey>)>>;
        }
    }

    mockall::mock! {
        pub CreditPocket {}

        #[async_trait]
        impl PocketApi for CreditPocket {
            fn unit(&self) -> CurrencyUnit;
            async fn balance(&self) -> Result<Amount>;
            async fn receive_proofs(
                &self,
                client: Arc<dyn ClowderMintConnector>,
                keysets_info: &[KeySetInfo],
                proofs: Vec<cashu::Proof>,
                safe_mode: SafeMode,
            ) -> Result<(Amount, Vec<cashu::PublicKey>)>;
            async fn prepare_send(&self, amount: Amount, infos: &[KeySetInfo]) -> Result<SendSummary>;
            async fn send_proofs(
                &self,
                rid: Uuid,
                keysets_info: &[KeySetInfo],
                client: Arc<dyn ClowderMintConnector>,
                safe_mode: SafeMode,
            ) -> Result<HashMap<cashu::PublicKey, cashu::Proof>>;
            async fn cleanup_local_proofs(
                &self,
                client: Arc<dyn ClowderMintConnector>,
            ) -> Result<Vec<cashu::PublicKey>>;
            async fn restore_local_proofs(
                &self,
                keysets_info: &[KeySetInfo],
                client: Arc<dyn ClowderMintConnector>,
            ) -> Result<usize>;
            async fn delete_proofs(&self) -> Result<HashMap<cashu::Id, Vec<cashu::Proof>>>;
            async fn return_proofs_to_send_for_offline_payment(
                &self,
                rid: Uuid,
            ) -> Result<(Amount, HashMap<cashu::PublicKey, cashu::Proof>)>;
            async fn swap_to_unlocked_substitute_proofs(
                &self,
                proofs: Vec<cashu::Proof>,
                keysets_info: &[KeySetInfo],
                client: Arc<dyn ClowderMintConnector>,
                send_amount: Amount,
            ) -> Result<Vec<cashu::Proof>>;
        }

        #[async_trait]
        impl CreditPocketApi for CreditPocket {
            async fn reclaim_proofs(
                &self,
                ys: &[cashu::PublicKey],
                keysets_info: &[KeySetInfo],
                client: Arc<dyn ClowderMintConnector>,
                safe_mode: SafeMode,
            ) -> Result<(Amount, Vec<cashu::Proof>)>;
            async fn get_redeemable_proofs(&self, keysets_info: &[KeySetInfo])
                -> Result<Vec<cashu::Proof>>;
            async fn list_redemptions(
                &self,
                keysets_info: &[KeySetInfo],
                payment_window: std::time::Duration,
            ) -> Result<Vec<RedemptionSummary>>;
        }
    }
}
