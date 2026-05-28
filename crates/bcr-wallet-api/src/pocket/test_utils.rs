pub mod tests {
    use crate::Result;
    use crate::external::mint::ClowderMintConnector;
    use crate::pocket::{
        PocketApi,
        debit::{DebitPocketApi, MeltProtestResult, ProtestResult},
    };
    use crate::types::{MeltSummary, MintSummary, SendSummary};
    use crate::wallet::types::SwapConfig;
    use async_trait::async_trait;
    use bcr_common::cdk_common::mint::MintKeySetInfo;
    use bcr_common::wire::melt as wire_melt;
    use std::collections::HashMap;
    use std::sync::Arc;
    use uuid::Uuid;

    use bcr_common::cashu::{self, Amount, CurrencyUnit, KeySetInfo};
    use bitcoin::secp256k1;

    pub fn test_kinfos(info: MintKeySetInfo) -> HashMap<cashu::Id, KeySetInfo> {
        let mut map = HashMap::new();
        map.insert(info.id, KeySetInfo::from(info));
        map
    }

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

    pub fn mock_attestation() -> bcr_common::wire::attestation::IssuanceAttestation {
        let keypair = secp256k1::Keypair::new_global(&mut secp256k1::rand::thread_rng());
        let msg = secp256k1::Message::from_digest([7u8; 32]);
        let signature = secp256k1::global::SECP256K1.sign_schnorr(&msg, &keypair);
        bcr_common::wire::attestation::IssuanceAttestation {
            beta_id: keypair.public_key(),
            fp_digest: [1u8; 32],
            coords_mac: [2u8; 32],
            signature,
        }
    }

    pub fn setup_commitment_mocks(
        connector: &mut crate::external::mint::MockClowderMintConnector,
        db: &mut bcr_wallet_persistence::MockPocketRepository,
    ) {
        connector
            .expect_post_swap_commitment()
            .times(1)
            .returning(|_, _, _, _| Ok(mock_commitment_result()));
        db.expect_store_commitment().times(1).returning(|_| Ok(()));
        db.expect_delete_commitment().times(1).returning(|_| Ok(()));
    }

    pub fn setup_attestation_mock(connector: &mut crate::external::mint::MockClowderMintConnector) {
        connector
            .expect_post_attest_issuance()
            .returning(|_| Ok(mock_attestation()));
    }

    pub fn test_beta_provider() -> crate::pocket::RandomBetaProvider {
        let mut beta_mock = crate::external::mint::MockClowderMintConnector::new();
        setup_attestation_mock(&mut beta_mock);
        let alpha_id = bitcoin::secp256k1::PublicKey::from_keypair(
            &bitcoin::secp256k1::Keypair::new_global(&mut secp256k1::rand::thread_rng()),
        );
        crate::pocket::RandomBetaProvider::new(
            vec![Arc::new(beta_mock) as Arc<dyn crate::ClowderMintConnector>],
            alpha_id,
        )
        .unwrap()
    }

    mockall::mock! {
        pub DebitPocket {}

        #[async_trait]
        impl PocketApi for DebitPocket {
            fn unit(&self) -> CurrencyUnit;
            async fn balance(&self,
                keysets_info: &HashMap<cashu::Id, KeySetInfo>,
            ) -> Result<crate::pocket::PocketBalance>;
            async fn receive_proofs(
                &self,
                client: Arc<dyn ClowderMintConnector>,
                keysets_info: &HashMap<cashu::Id, KeySetInfo>,
                proofs: Vec<cashu::Proof>,
                swap_config: SwapConfig,
            ) -> Result<(Amount, Vec<cashu::PublicKey>)>;
            async fn prepare_send(&self, amount: Amount,
                keysets_info: &HashMap<cashu::Id, KeySetInfo>,
            ) -> Result<SendSummary>;
            async fn send_proofs(
                &self,
                rid: Uuid,
                keysets_info: &HashMap<cashu::Id, KeySetInfo>,
                client: Arc<dyn ClowderMintConnector>,
                swap_config: SwapConfig,
            ) -> Result<HashMap<cashu::PublicKey, cashu::Proof>>;
            async fn restore_local_proofs(
                &self,
                keysets_info: &HashMap<cashu::Id, KeySetInfo>,
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
                keysets_info: &HashMap<cashu::Id, KeySetInfo>,
                client: Arc<dyn ClowderMintConnector>,
                send_amount: Amount,
                swap_config: SwapConfig,
            ) -> Result<Vec<cashu::Proof>>;
            async fn dev_mode_detailed_balance(
                &self,
                keysets_info: &HashMap<cashu::Id, KeySetInfo>,
            ) -> Result<HashMap<cashu::Id, (Option<u64>, Amount)>>;
            async fn delete(&self) -> Result<()>;
        }

        #[async_trait]
        impl DebitPocketApi for DebitPocket {
            async fn reclaim_proofs(
                &self,
                ys: &[cashu::PublicKey],
                keysets_info: &HashMap<cashu::Id, KeySetInfo>,
                client: Arc<dyn ClowderMintConnector>,
                swap_config: SwapConfig,
            ) -> Result<Amount>;
            async fn recover_pending_stale_proofs(
                &self,
                pending_txs_ys: &[cashu::PublicKey],
                keysets_info: &HashMap<cashu::Id, KeySetInfo>,
                client: Arc<dyn ClowderMintConnector>,
                swap_config: SwapConfig,
            ) -> Result<Amount>;
            async fn clean_up_spent_proofs(&self, client: Arc<dyn ClowderMintConnector>) -> Result<usize>;
            async fn prepare_onchain_melt(
                &self,
                address: String,
                amount: u64,
                keysets_info: &HashMap<cashu::Id, KeySetInfo>,
                client: Arc<dyn ClowderMintConnector>,
                swap_config: SwapConfig,
            ) -> Result<MeltSummary>;
            async fn pay_onchain_melt(
                &self,
                rid: Uuid,
                client: Arc<dyn ClowderMintConnector>,
            ) -> Result<(bitcoin::Txid, HashMap<cashu::PublicKey, cashu::Proof>)>;
            async fn mint_onchain(
                &self,
                amount: bitcoin::Amount,
                keysets_info: &HashMap<cashu::Id, KeySetInfo>,
                client: Arc<dyn ClowderMintConnector>,
            ) -> Result<MintSummary>;
            async fn check_pending_mints(
                &self,
                keysets_info: &HashMap<cashu::Id, KeySetInfo>,
                client: Arc<dyn ClowderMintConnector>,
                tstamp: u64,
                swap_config: SwapConfig,
            ) -> Result<HashMap<Uuid, crate::pocket::debit::CheckPendingMintResult>>;
            async fn check_pending_commitments(&self, tstamp: u64) -> Result<()>;
            async fn protest_mint(
                &self,
                qid: Uuid,
                keysets_info: &HashMap<cashu::Id, KeySetInfo>,
                client: Arc<dyn ClowderMintConnector>,
                swap_config: SwapConfig,
            ) -> Result<ProtestResult>;
            async fn protest_swap(
                &self,
                commitment_sig: bitcoin::secp256k1::schnorr::Signature,
                keysets_info: &HashMap<cashu::Id, KeySetInfo>,
                alpha_client: Arc<dyn ClowderMintConnector>,
                swap_config: SwapConfig,
            ) -> Result<ProtestResult>;
            async fn protest_melt(
                &self,
                quote_id: Uuid,
            ) -> Result<MeltProtestResult>;
            async fn list_melt_commitments(&self) -> Result<Vec<(Uuid, u64)>>;
        }
    }
}
