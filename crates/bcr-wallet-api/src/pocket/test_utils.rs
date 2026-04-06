pub mod tests {
    use crate::RedemptionSummary;
    use crate::Result;
    use crate::external::mint::ClowderMintConnector;
    use crate::pocket::{PocketApi, credit::CreditPocketApi, debit::DebitPocketApi};
    use crate::types::{MeltSummary, MintSummary, SendSummary};
    use crate::wallet::types::SwapConfig;
    use async_trait::async_trait;
    use bcr_common::wire::{melt as wire_melt, mint as wire_mint};
    use std::collections::HashMap;
    use std::sync::Arc;
    use uuid::Uuid;

    use bcr_common::cashu::{self, Amount, CurrencyUnit, KeySetInfo};
    use bitcoin::secp256k1;

    pub fn test_swap_config() -> SwapConfig {
        let keypair = secp256k1::Keypair::new_global(&mut secp256k1::rand::thread_rng());
        SwapConfig {
            expiry: chrono::TimeDelta::seconds(600),
            alpha_pk: secp256k1::PublicKey::from_keypair(&keypair),
        }
    }

    pub fn mock_commitment_result() -> crate::external::mint::SwapCommitmentResult {
        let ephemeral = secp256k1::Keypair::new_global(&mut secp256k1::rand::thread_rng());
        let key = cashu::SecretKey::generate();
        crate::external::mint::SwapCommitmentResult {
            inputs_ys: vec![],
            outputs: vec![],
            expiry: 1000,
            commitment: key.sign(&[0u8; 32]).unwrap(),
            ephemeral_secret: secp256k1::SecretKey::from_keypair(&ephemeral),
            body_content: "test".to_string(),
            wallet_key: cashu::PublicKey::from(secp256k1::PublicKey::from_keypair(&ephemeral)),
        }
    }

    pub fn setup_commitment_mocks(
        connector: &mut crate::external::test_utils::tests::MockMintConnector,
        db: &mut bcr_wallet_persistence::MockPocketRepository,
    ) {
        connector
            .expect_post_swap_commitment()
            .times(1)
            .returning(|_, _, _, _| Ok(mock_commitment_result()));
        db.expect_store_commitment()
            .times(1)
            .returning(|_| Ok(()));
        db.expect_delete_commitment()
            .times(1)
            .returning(|_| Ok(()));
    }

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
                swap_config: SwapConfig,
            ) -> Result<(Amount, Vec<cashu::PublicKey>)>;
            async fn prepare_send(&self, amount: Amount, infos: &[KeySetInfo]) -> Result<SendSummary>;
            async fn send_proofs(
                &self,
                rid: Uuid,
                keysets_info: &[KeySetInfo],
                client: Arc<dyn ClowderMintConnector>,
                swap_config: SwapConfig,
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
                swap_config: SwapConfig,
            ) -> Result<Vec<cashu::Proof>>;
        }

        #[async_trait]
        impl DebitPocketApi for DebitPocket {
            async fn reclaim_proofs(
                &self,
                ys: &[cashu::PublicKey],
                keysets_info: &[KeySetInfo],
                client: Arc<dyn ClowderMintConnector>,
                swap_config: SwapConfig,
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
                swap_config: SwapConfig,
            ) -> Result<(wire_melt::MeltTx, HashMap<cashu::PublicKey, cashu::Proof>)>;
            async fn mint_onchain(
                &self,
                amount: bitcoin::Amount,
                keysets_info: &[KeySetInfo],
                client: Arc<dyn ClowderMintConnector>,
                clowder_id: bitcoin::secp256k1::PublicKey,
            ) -> Result<MintSummary>;
            async fn check_pending_mints(
                &self,
                keysets_info: &[KeySetInfo],
                client: Arc<dyn ClowderMintConnector>,
                tstamp: u64,
                swap_config: SwapConfig,
                clowder_id: bitcoin::secp256k1::PublicKey,
            ) -> Result<HashMap<Uuid, (cashu::Amount, Vec<cashu::PublicKey>)>>;
            async fn check_pending_commitments(&self, tstamp: u64) -> Result<()>;
            async fn protest_mint(
                &self,
                qid: Uuid,
                keysets_info: &[KeySetInfo],
                client: Arc<dyn ClowderMintConnector>,
                swap_config: SwapConfig,
                clowder_id: bitcoin::secp256k1::PublicKey,
            ) -> Result<(wire_mint::ProtestStatus, Option<(cashu::Amount, Vec<cashu::PublicKey>)>)>;
            async fn protest_swap(
                &self,
                commitment_sig: bitcoin::secp256k1::schnorr::Signature,
                keysets_info: &[KeySetInfo],
                beta_client: Arc<dyn ClowderMintConnector>,
                alpha_id: bitcoin::secp256k1::PublicKey,
                swap_config: SwapConfig,
            ) -> Result<(bcr_common::wire::common::ProtestStatus, Option<(cashu::Amount, Vec<cashu::PublicKey>)>)>;
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
                swap_config: SwapConfig,
            ) -> Result<(Amount, Vec<cashu::PublicKey>)>;
            async fn prepare_send(&self, amount: Amount, infos: &[KeySetInfo]) -> Result<SendSummary>;
            async fn send_proofs(
                &self,
                rid: Uuid,
                keysets_info: &[KeySetInfo],
                client: Arc<dyn ClowderMintConnector>,
                swap_config: SwapConfig,
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
                swap_config: SwapConfig,
            ) -> Result<Vec<cashu::Proof>>;
        }

        #[async_trait]
        impl CreditPocketApi for CreditPocket {
            async fn reclaim_proofs(
                &self,
                ys: &[cashu::PublicKey],
                keysets_info: &[KeySetInfo],
                client: Arc<dyn ClowderMintConnector>,
                swap_config: SwapConfig,
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
