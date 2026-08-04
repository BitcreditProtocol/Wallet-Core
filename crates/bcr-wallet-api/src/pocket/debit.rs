use crate::{
    ClowderMintConnector,
    error::{Error, Result},
    pocket::*,
    wallet::types::SwapConfig,
};
use async_trait::async_trait;
use bcr_common::{
    cashu::{
        self, Amount, CurrencyUnit, KeySet, KeySetInfo, Proof, ProofsMethods, amount::SplitTarget,
        nut00 as cdk00, nut01 as cdk01,
    },
    core::swap::wallet::{PaymentPlan, prepare_payment},
    wire::{common as wire_common, melt as wire_melt, mint as wire_mint, swap as wire_swap},
};
use bcr_wallet_core::types::{
    ForeignMintProof, ForeignMintProofReason, MeltSummary, MintSummary, Seed, SendSummary,
    TransactionFees,
};
use bcr_wallet_persistence::{MeltCommitmentRecord, MintMeltRepository, PocketRepository};
use bitcoin::secp256k1;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use uuid::Uuid;

#[async_trait]
pub trait DebitPocketApi: super::PocketApi {
    /// Reclaim the proofs for the given ys
    /// returns the amount reclaimed
    async fn reclaim_proofs(
        &self,
        ys: &[cashu::PublicKey],
        keysets_info: &HashMap<cashu::Id, KeySetInfo>,
        client: Arc<dyn ClowderMintConnector>,
        swap_config: SwapConfig,
    ) -> Result<Amount>;
    /// Attempt to recover proofs, which are pending, but not part of
    /// a pending transaction
    async fn recover_pending_stale_proofs(
        &self,
        pending_txs_ys: &[cashu::PublicKey],
        keysets_info: &HashMap<cashu::Id, KeySetInfo>,
        client: Arc<dyn ClowderMintConnector>,
        swap_config: SwapConfig,
    ) -> Result<Amount>;
    /// Checks and cleans up spent proofs
    async fn clean_up_spent_proofs(&self, client: Arc<dyn ClowderMintConnector>) -> Result<usize>;
    async fn fetch_foreign_mint_proofs(&self) -> Result<Vec<ForeignMintProof>>;
    async fn delete_foreign_mint_proofs(
        &self,
        clowder_id: secp256k1::PublicKey,
        ys: Vec<cdk01::PublicKey>,
    );
    async fn prepare_onchain_melt(
        &self,
        address: String,
        amount: u64,
        network_fee: u64,
        melt_fee: u64,
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
        client: Arc<dyn ClowderMintConnector>,
    ) -> Result<HashMap<Uuid, CheckPendingMintResult>>;
    async fn protest_mint(
        &self,
        qid: Uuid,
        client: Arc<dyn ClowderMintConnector>,
    ) -> Result<ProtestResult>;
    async fn check_pending_commitments(&self, tstamp: u64) -> Result<()>;
    async fn protest_swap(
        &self,
        commitment_sig: bitcoin::secp256k1::schnorr::Signature,
        keysets_info: &HashMap<cashu::Id, KeySetInfo>,
        alpha_client: Arc<dyn ClowderMintConnector>,
        swap_config: SwapConfig,
    ) -> Result<ProtestResult>;
    async fn protest_melt(&self, quote_id: Uuid) -> Result<MeltProtestResult>;
    async fn list_melt_commitments(&self) -> Result<Vec<(Uuid, u64)>>;
}

#[derive(Debug, Clone)]
pub struct ProtestResult {
    pub status: wire_common::ProtestStatus,
    pub result: Option<(cashu::Amount, Vec<cashu::PublicKey>)>,
}

#[derive(Debug, Clone)]
pub struct MeltProtestResult {
    pub base: ProtestResult,
    pub txid: Option<bitcoin::Txid>,
}

#[derive(Debug, Clone)]
pub struct CheckPendingMintResult {
    pub amount: cashu::Amount,
    pub fee: cashu::Amount,
    pub ys: Vec<cashu::PublicKey>,
}

struct MeltReference {
    rid: Uuid,
    quote_id: Uuid,
}

///////////////////////////////////////////// debit pocket
pub struct Pocket {
    pub unit: cashu::CurrencyUnit,
    pub pdb: Arc<dyn PocketRepository>,
    pub mdb: Arc<dyn MintMeltRepository>,
    seed: Seed,
    beta: Arc<dyn super::BetaProvider>,

    current_send: Mutex<Option<SendReference>>,
    current_melt: Mutex<Option<MeltReference>>,
}

impl Pocket {
    pub fn new(
        unit: CurrencyUnit,
        pdb: Arc<dyn PocketRepository>,
        mdb: Arc<dyn MintMeltRepository>,
        seed: Seed,
        beta: Arc<dyn super::BetaProvider>,
    ) -> Self {
        Self {
            unit,
            pdb,
            mdb,
            seed,
            beta,
            current_send: Mutex::new(None),
            current_melt: Mutex::new(None),
        }
    }

    fn validate_keysets<'a>(
        &self,
        keysets_info: &'a HashMap<cashu::Id, KeySetInfo>,
        inputs: &[cdk00::Proof],
    ) -> Result<HashMap<cashu::Id, &'a KeySetInfo>> {
        let infos = collect_keyset_infos_from_proofs(inputs.iter(), keysets_info)?;
        for info in infos.values() {
            if info.unit != self.unit {
                return Err(Error::InvalidCurrencyUnit(info.unit.clone().to_string()));
            }
            if !info.active {
                return Err(Error::InactiveKeyset(info.id));
            }
        }
        Ok(infos)
    }

    async fn digest_proofs(
        &self,
        client: Arc<dyn ClowderMintConnector>,
        keysets_info: &HashMap<cashu::Id, KeySetInfo>,
        inputs: HashMap<cdk01::PublicKey, cdk00::Proof>,
        swap_config: SwapConfig,
    ) -> Result<(Amount, Vec<cdk01::PublicKey>)> {
        if inputs.is_empty() {
            tracing::warn!("DbPocket::digest_proofs: empty inputs");
            return Ok((Amount::ZERO, Vec::new()));
        }
        // prepare data
        let (ys, swap_proofs): (Vec<_>, Vec<_>) = inputs.into_iter().unzip();

        // create swap plan
        let swap_plan = prepare_swap(&swap_proofs, keysets_info)?;
        tracing::debug!("Digest proofs - swap plan: {swap_plan:?}");

        // collect keysets first as we don't want any failure once the swap request
        // has been made
        let kids: HashSet<cashu::Id> = swap_proofs.iter().map(|p| p.keyset_id).collect();
        let mut keysets: HashMap<cashu::Id, KeySet> = HashMap::new();
        for kid in kids.iter() {
            let keyset = client.get_mint_keyset(*kid).await?;
            keysets.insert(*kid, keyset);
        }

        // prepare the premints
        let mut premints: HashMap<cashu::Id, cdk00::PreMintSecrets> = HashMap::new();
        for (kid, amount) in swap_plan {
            let counter = self.pdb.counter(kid).await?;
            let premint = cdk00::PreMintSecrets::from_seed(
                kid,
                counter,
                &self.seed,
                amount,
                &SplitTarget::None,
                &bcr_wallet_core::util::to_fee_and_amounts(&keysets[&kid]),
            )?;
            let increment = premint.len() as u32;
            premints.insert(kid, premint);
            self.pdb.increment_counter(kid, counter, increment).await?;
        }

        // swap
        let cashed_in = swap(
            self.unit.clone(),
            swap_proofs,
            premints,
            keysets,
            client,
            self.pdb.as_ref(),
            swap_config,
            self.beta.as_ref(),
        )
        .await?;
        Ok((cashed_in, ys))
    }

    async fn compute_send_costs(
        &self,
        target_amount: Amount,
        keysets_info: &HashMap<cashu::Id, KeySetInfo>,
    ) -> Result<(SendSummary, SendReference)> {
        let unspent_proofs = self.pdb.list_unspent().await?;
        let mut proofs: Vec<Proof> = unspent_proofs.values().cloned().collect();
        // sort by amount as required by `prepare_payment`
        proofs.sort_by_key(|proof| proof.amount);

        let infos = collect_keyset_infos_from_proofs(unspent_proofs.values(), keysets_info)?;
        let kinfos: HashMap<cashu::Id, KeySetInfo> =
            infos.iter().map(|(k, v)| (*k, (*v).clone())).collect();

        let payment_plan = prepare_payment(&proofs, target_amount, &kinfos)?;
        let (pocket_summary, send_ref) = match payment_plan {
            PaymentPlan::Ready { inputs, .. } => {
                let mut pocket_summary = SendSummary::new();
                pocket_summary.amount = target_amount;
                pocket_summary.unit = self.unit.clone();

                let send_ref = SendReference {
                    rid: pocket_summary.request_id,
                    target_amount,
                    plan: SendPlan::Ready {
                        proofs: inputs
                            .iter()
                            .map(|proof| proof.y())
                            .collect::<std::result::Result<Vec<cashu::PublicKey>, _>>()?,
                    },
                };
                (pocket_summary, send_ref)
            }
            PaymentPlan::NeedSwap {
                inputs,
                target,
                estimated_fee,
            } => {
                let mut pocket_summary = SendSummary::new();
                pocket_summary.amount = target_amount;
                pocket_summary.unit = self.unit.clone();
                pocket_summary.fees = TransactionFees {
                    swap: estimated_fee,
                    ..Default::default()
                };
                let SplitTarget::Value(target_amount) = target else {
                    return Err(Error::InvalidSplitTarget);
                };
                let send_ref = SendReference {
                    rid: pocket_summary.request_id,
                    target_amount,
                    plan: SendPlan::NeedSwap {
                        inputs: inputs
                            .iter()
                            .map(|proof| proof.y())
                            .collect::<std::result::Result<Vec<cashu::PublicKey>, _>>()?,
                        target: target_amount,
                        estimated_fee,
                    },
                };
                (pocket_summary, send_ref)
            }
        };

        Ok((pocket_summary, send_ref))
    }

    /// Construct proofs from blind signatures, persist into the wallet, and return the result.
    async fn finalize_mint_proofs(
        &self,
        signatures: Vec<cdk00::BlindSignature>,
        premint: &cdk00::PreMintSecrets,
        client: Arc<dyn ClowderMintConnector>,
    ) -> Result<(cashu::Amount, Vec<cashu::PublicKey>)> {
        let active_keyset = client.get_mint_keyset(premint.keyset_id).await?;

        let proofs = cashu::dhke::construct_proofs(
            signatures,
            premint.rs(),
            premint.secrets(),
            &active_keyset.keys,
        )?;

        let mut total_cashed_in = Amount::ZERO;
        let mut ys = Vec::with_capacity(proofs.len());
        for proof in proofs.into_iter() {
            let amount = proof.amount;
            let y = proof.y()?;
            let kid = proof.keyset_id;
            let response = self.pdb.store_new(proof).await;
            if let Err(e) = response {
                tracing::error!("failed at storing new proof: {kid}, {amount}, {e}");
                continue;
            }
            ys.push(y);
            total_cashed_in += amount;
        }

        Ok((total_cashed_in, ys))
    }

    async fn check_pending_mint(
        &self,
        qid: Uuid,
        client: Arc<dyn ClowderMintConnector>,
    ) -> Result<Option<CheckPendingMintResult>> {
        let record = self.mdb.load_mint(qid).await?;
        let mint_amount = Amount::from(record.summary.amount.to_sat());
        let (mint_summary, premint) = (record.summary, record.premint);

        tracing::info!("Mint {qid} - attempting to mint..");
        let mint_req = wire_mint::OnchainMintRequest {
            quote: mint_summary.quote_id,
            alpha_id: self.beta.alpha_id(),
        };
        match client.post_mint_onchain(mint_req).await {
            Ok(mint_response) => {
                let (amount, ys) = self
                    .finalize_mint_proofs(mint_response.signatures, &premint, client)
                    .await?;

                self.mdb.delete_mint(qid).await?;
                let fee = if mint_amount > amount {
                    mint_amount - amount
                } else {
                    Amount::ZERO
                };
                tracing::info!("Minted {qid} successfully for {mint_amount} with fee {fee}");
                Ok(Some(CheckPendingMintResult {
                    amount: mint_amount,
                    fee,
                    ys,
                }))
            }
            Err(e) => {
                tracing::error!("Couldn't mint quote {qid}: {e}");
                Err(Error::MintingError(qid.to_string()))
            }
        }
    }
}

#[async_trait]
impl super::PocketApi for Pocket {
    fn unit(&self) -> CurrencyUnit {
        self.unit.clone()
    }

    fn set_beta_provider(&mut self, beta_provider: Arc<dyn BetaProvider>) {
        self.beta = beta_provider;
    }

    async fn balance(
        &self,
        keysets_info: &HashMap<cashu::Id, KeySetInfo>,
    ) -> Result<PocketBalance> {
        let proofs: Vec<Proof> = self.pdb.list_unspent().await?.into_values().collect();
        let mut debit = Amount::ZERO;
        let mut credit = Amount::ZERO;

        let infos = collect_keyset_infos_from_proofs(proofs.iter(), keysets_info)?;
        let start_of_today = chrono::Utc::now()
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .expect("valid date")
            .and_utc()
            .timestamp() as u64;

        for proof in proofs {
            let info = infos
                .get(&proof.keyset_id)
                .ok_or(Error::UnknownKeysetId(proof.keyset_id))?;

            // no final expiry -> debit
            // final expiry before today -> debit
            // final expiry today, or after -> credit
            let is_credit = match info.final_expiry {
                Some(expiry) => expiry >= start_of_today,
                None => false,
            };

            if is_credit {
                credit += proof.amount;
            } else {
                debit += proof.amount;
            }
        }

        Ok(PocketBalance { debit, credit })
    }

    async fn receive_proofs(
        &self,
        client: Arc<dyn ClowderMintConnector>,
        keysets_info: &HashMap<cashu::Id, KeySetInfo>,
        inputs: Vec<cdk00::Proof>,
        swap_config: SwapConfig,
    ) -> Result<(Amount, Vec<cdk01::PublicKey>)> {
        self.validate_keysets(keysets_info, &inputs)?;
        // storing proofs in pending state
        let mut proofs: HashMap<cdk01::PublicKey, cdk00::Proof> =
            HashMap::with_capacity(inputs.len());
        for input in inputs.into_iter() {
            let y = input.y()?;
            proofs.insert(y, input);
        }
        self.digest_proofs(client, keysets_info, proofs, swap_config)
            .await
    }

    async fn prepare_send(
        &self,
        target: Amount,
        keysets_info: &HashMap<cashu::Id, KeySetInfo>,
    ) -> Result<SendSummary> {
        let (summary, send_ref) = self.compute_send_costs(target, keysets_info).await?;
        *self.current_send.lock().unwrap() = Some(send_ref);
        Ok(summary)
    }

    async fn send_proofs(
        &self,
        rid: Uuid,
        keysets_info: &HashMap<cashu::Id, KeySetInfo>,
        client: Arc<dyn ClowderMintConnector>,
        swap_config: SwapConfig,
    ) -> Result<HashMap<cdk01::PublicKey, cdk00::Proof>> {
        let send_ref = {
            let mut locked = self.current_send.lock().unwrap();
            if locked.is_none() {
                return Err(Error::NoPrepareRef(rid));
            }
            if locked.as_ref().unwrap().rid != rid {
                return Err(Error::NoPrepareRef(rid));
            }
            locked.take().unwrap()
        };
        let sending_proofs = send_proofs(
            send_ref.plan,
            keysets_info,
            send_ref.target_amount,
            &self.seed,
            self.pdb.as_ref(),
            &client,
            swap_config,
            self.beta.as_ref(),
        )
        .await?;

        Ok(sending_proofs)
    }

    async fn restore_local_proofs(
        &self,
        keysets_info: &HashMap<cashu::Id, KeySetInfo>,
        client: Arc<dyn ClowderMintConnector>,
    ) -> Result<usize> {
        let mut total_recovered = 0;
        for kid in keysets_info.keys() {
            total_recovered +=
                restore::restore_keysetid(&self.seed, *kid, &client, self.pdb.as_ref()).await?;
        }
        Ok(total_recovered)
    }

    async fn delete_proofs(&self) -> Result<HashMap<cashu::Id, Vec<cdk00::Proof>>> {
        let proofs = self.pdb.list_all().await?;

        let mut proofs_by_keyset = HashMap::<cashu::Id, Vec<cdk00::Proof>>::new();

        for y in proofs.iter() {
            if let Some((proof, state)) = self.pdb.delete_proof(*y).await? {
                // delete all, but return only unspent proofs
                if matches!(state, cdk07::State::Unspent) {
                    proofs_by_keyset
                        .entry(proof.keyset_id)
                        .or_default()
                        .push(proof);
                }
            }
        }

        Ok(proofs_by_keyset)
    }

    async fn return_proofs_to_send_for_offline_payment(
        &self,
        rid: Uuid,
    ) -> Result<(Amount, HashMap<cdk01::PublicKey, cdk00::Proof>)> {
        let send_ref = {
            let mut locked = self.current_send.lock().unwrap();
            if locked.is_none() {
                return Err(Error::NoPrepareRef(rid));
            }
            if locked.as_ref().unwrap().rid != rid {
                return Err(Error::NoPrepareRef(rid));
            }
            locked.take().unwrap()
        };
        let proofs_to_send =
            return_proofs_to_send_for_offline_payment(send_ref.plan, self.pdb.as_ref()).await?;
        Ok(proofs_to_send)
    }

    async fn swap_to_unlocked_substitute_proofs(
        &self,
        proofs: Vec<cdk00::Proof>,
        keysets_info: &HashMap<cashu::Id, KeySetInfo>,
        keysets: HashMap<cashu::Id, KeySet>,
        substitute_client: Arc<dyn ClowderMintConnector>,
        substitute_clowder_id: secp256k1::PublicKey,
        beta_provider: RandomBetaProvider,
        send_amount: Amount,
        swap_config: SwapConfig,
    ) -> Result<Vec<cashu::Proof>> {
        let total_amount = proofs.total_amount()?;
        let change_amount = total_amount - send_amount;

        let swap_plan: Vec<_> = prepare_swap(&proofs, keysets_info)?.into_iter().collect();
        tracing::debug!(
            "Swapping to unlocked substitute proofs {swap_plan:?} - {change_amount} will be used for fees and stored temporarily as foreign mint proofs."
        );

        // prepare the premints
        let mut premints: HashMap<cashu::Id, cdk00::PreMintSecrets> = HashMap::new();
        let mut remaining_payment = send_amount;
        // collect payments by kid, so we can reconstruct it after the swap
        let mut payment_targets_by_kid: HashMap<cashu::Id, Amount> = HashMap::new();

        for (kid, amount) in swap_plan {
            let keyset_payment_target = std::cmp::min(amount, remaining_payment);

            let target = if keyset_payment_target > Amount::ZERO {
                // used for our payment - needs to add up to our payment amount
                payment_targets_by_kid.insert(kid, keyset_payment_target);
                remaining_payment -= keyset_payment_target;
                SplitTarget::Value(keyset_payment_target)
            } else {
                // change - doesn't matter how we get it
                SplitTarget::default()
            };

            // no counter etc., since we're not persisting them anyway
            let premint = cashu::PreMintSecrets::random(
                kid,
                amount,
                &target,
                &bcr_wallet_core::util::to_fee_and_amounts(&keysets[&kid]),
            )?;
            premints.insert(kid, premint);
        }

        if remaining_payment != Amount::ZERO {
            return Err(Error::Swap(format!(
                "swap plan cannot fund payment target {send_amount}, missing {remaining_payment}"
            )));
        }

        let blinds: Vec<cdk00::BlindedMessage> = premints
            .values()
            .flat_map(|premint| premint.blinded_messages())
            .collect();

        let attestation = beta_provider.attest(&proofs).await?;
        let signatures = super::committed_swap(
            substitute_client.as_ref(),
            None,
            proofs,
            blinds,
            &swap_config,
            premints.iter().map(|(k, v)| (*k, v.clone())).collect(),
            attestation,
        )
        .await?;

        let mut on_target: Vec<cdk00::Proof> = Vec::new();
        let mut change_proofs: Vec<cdk00::Proof> = Vec::new();

        let mut sigs_by_kid: HashMap<cashu::Id, Vec<cdk00::BlindSignature>> = HashMap::new();
        for signature in signatures {
            sigs_by_kid
                .entry(signature.keyset_id)
                .and_modify(|v| v.push(signature.clone()))
                .or_insert_with(|| vec![signature]);
        }

        let mut selected_amount = Amount::ZERO;
        for (kid, sigs) in sigs_by_kid.into_iter() {
            let premint = premints.remove(&kid).expect("premint should be here");
            let keyset = keysets.get(&kid).expect("keyset should be here");
            let mut proofs = unblind_proofs(keyset, sigs, premint);

            // get payment amount for this keyset
            let keyset_target_amount = payment_targets_by_kid.remove(&kid).unwrap_or(Amount::ZERO);
            let mut selected_amount_per_keyset = Amount::ZERO;

            proofs.sort_by_key(|proof| std::cmp::Reverse(proof.amount));
            for proof in proofs {
                let amount = proof.amount;
                if selected_amount_per_keyset + amount <= keyset_target_amount {
                    selected_amount_per_keyset += amount;
                    selected_amount += amount;
                    on_target.push(proof);
                } else {
                    change_proofs.push(proof);
                }
            }

            if selected_amount_per_keyset != keyset_target_amount {
                return Err(Error::Swap(format!(
                    "did not select exact payment proofs for keyset {kid}: {selected_amount_per_keyset} / {keyset_target_amount}"
                )));
            }
        }

        if !change_proofs.is_empty() {
            let stored_change_amount = change_proofs.total_amount()?;

            tracing::debug!(
                "Storing {} unlocked change proofs for {stored_change_amount} for substitute {}",
                change_proofs.len(),
                substitute_client.mint_url()
            );
            for change_proof in change_proofs {
                let fmp = ForeignMintProof {
                    clowder_id: substitute_clowder_id,
                    proof: change_proof,
                    reason: ForeignMintProofReason::MintOffline,
                };
                if let Err(e) = self.pdb.store_foreign_mint_proof(fmp).await {
                    tracing::error!(
                        "Could not persist foreign mint proof for clowder_id {substitute_clowder_id}: {e}"
                    );
                }
            }
        }

        if selected_amount != send_amount {
            return Err(Error::Swap(format!(
                "did not select exact payment proofs for total amount: {selected_amount} / {send_amount}"
            )));
        }

        Ok(on_target)
    }

    async fn dev_mode_detailed_balance(
        &self,
        keysets_info: &HashMap<cashu::Id, KeySetInfo>,
    ) -> Result<HashMap<cashu::Id, (Option<u64>, Amount)>> {
        let proofs: Vec<Proof> = self.pdb.list_unspent().await?.into_values().collect();
        let infos = collect_keyset_infos_from_proofs(proofs.iter(), keysets_info)?;

        let mut balances: HashMap<cashu::Id, (Option<u64>, Amount)> = HashMap::new();

        for proof in proofs {
            let kid = proof.keyset_id;
            let info = infos.get(&kid).ok_or(Error::UnknownKeysetId(kid))?;

            let entry = balances
                .entry(kid)
                .or_insert((info.final_expiry, Amount::ZERO));

            entry.1 += proof.amount;
        }

        Ok(balances)
    }

    async fn delete(&self) -> Result<()> {
        if let Err(e) = self.mdb.delete_repo().await {
            tracing::error!("Error deleting mint melt DB for pocket {e}")
        }

        if let Err(e) = self.pdb.delete_repo().await {
            tracing::error!("Error deleting proof DB for wallet {e}")
        }

        Ok(())
    }
}

#[async_trait]
impl DebitPocketApi for Pocket {
    async fn reclaim_proofs(
        &self,
        ys: &[cdk01::PublicKey],
        keysets_info: &HashMap<cashu::Id, KeySetInfo>,
        client: Arc<dyn ClowderMintConnector>,
        swap_config: SwapConfig,
    ) -> Result<Amount> {
        let pendings = self.pdb.load_proofs(ys).await?;
        let pendings_len = pendings.len();
        let (reclaimed, _) = self
            .digest_proofs(client, keysets_info, pendings, swap_config)
            .await?;
        tracing::debug!(
            "DbPocket::reclaim_proofs: pendings: {pendings_len} reclaimed: {reclaimed}"
        );
        Ok(reclaimed)
    }

    async fn recover_pending_stale_proofs(
        &self,
        pending_txs_ys: &[cashu::PublicKey],
        keysets_info: &HashMap<cashu::Id, KeySetInfo>,
        client: Arc<dyn ClowderMintConnector>,
        swap_config: SwapConfig,
    ) -> Result<Amount> {
        // remove pending transaction ys from pending proofs
        let mut pendings = self.pdb.list_pending().await?;
        let remove_set: HashSet<&cashu::PublicKey> = pending_txs_ys.iter().collect();
        pendings.retain(|k, _| !remove_set.contains(k));

        let req = cdk07::CheckStateRequest {
            ys: pendings.keys().cloned().collect(),
        };
        let states = client.post_check_state(req).await?;
        let mut to_digest = HashMap::new();
        for state in states.iter() {
            match state.state {
                cdk07::State::Spent => {
                    tracing::warn!(
                        "Pending Stale Proof returned as SPENT from Mint - not recovering and setting to SPENT"
                    );
                    if let Err(e) = self.pdb.mark_pending_as_spent(state.y).await {
                        tracing::error!(
                            "Error setting stale proof {} from Pending/PendingSpent to Spent: {e}",
                            state.y
                        )
                    }
                }
                cdk07::State::Unspent => {
                    // collect for digesting later
                    if let Some(proof) = pendings.get(&state.y) {
                        to_digest.insert(state.y, proof.to_owned());
                    }
                }
                cdk07::State::Pending => {
                    tracing::warn!(
                        "Pending Stale Proof returned as PENDING from Mint - not recovering"
                    );
                }
                cdk07::State::Reserved => {
                    tracing::warn!(
                        "Pending Stale Proof returned as RESERVED from Mint - not recovering"
                    );
                }
                cdk07::State::PendingSpent => {
                    tracing::warn!(
                        "Pending Stale Proof returned as PENDINGSPENT from Mint - not recovering"
                    );
                }
            }
        }
        if to_digest.is_empty() {
            return Ok(Amount::ZERO);
        }
        // attempt to recover the proofs collected for digesting
        let to_digest_ys: Vec<cashu::PublicKey> = to_digest.keys().cloned().collect();
        let (recovered, _) = self
            .digest_proofs(client, keysets_info, to_digest, swap_config)
            .await?;
        // if recovery successful, set previous proofs to spent
        for y in to_digest_ys.into_iter() {
            if let Err(e) = self.pdb.mark_pending_as_spent(y).await {
                tracing::error!(
                    "Error setting recovered stale proof {} from Pending/PendingSpent to Spent: {e}",
                    y
                )
            }
        }

        Ok(recovered)
    }

    async fn clean_up_spent_proofs(&self, client: Arc<dyn ClowderMintConnector>) -> Result<usize> {
        let mut cleaned_up = 0;
        let spent_proofs = self.pdb.list_spent().await?;
        let req = cdk07::CheckStateRequest {
            ys: spent_proofs.keys().cloned().collect(),
        };
        let states = client.post_check_state(req).await?;
        for state in states.iter() {
            match state.state {
                cdk07::State::Spent => {
                    // is spent - delete proof locally
                    if let Err(e) = self.pdb.delete_proof(state.y).await {
                        tracing::error!("Error deleting spent proof {}: {e}", state.y)
                    } else {
                        cleaned_up += 1;
                    }
                }
                _ => {
                    // other states - just log
                    tracing::warn!(
                        "Proof {} saved as SPENT, but got {} from Mint",
                        state.y,
                        state.state
                    );
                }
            }
        }
        Ok(cleaned_up)
    }

    async fn fetch_foreign_mint_proofs(&self) -> Result<Vec<ForeignMintProof>> {
        let foreign_mint_proofs = self.pdb.load_foreign_mint_proofs().await?;
        Ok(foreign_mint_proofs)
    }

    async fn delete_foreign_mint_proofs(
        &self,
        clowder_id: secp256k1::PublicKey,
        ys: Vec<cdk01::PublicKey>,
    ) {
        if let Err(e) = self.pdb.delete_foreign_mint_proofs(clowder_id, ys).await {
            tracing::error!("Could not delete foreign mint proof for {clowder_id}: {e}");
        }
    }

    async fn prepare_onchain_melt(
        &self,
        address: String,
        amount: u64,
        network_fee: u64,
        melt_fee: u64,
        keysets_info: &HashMap<cashu::Id, KeySetInfo>,
        client: Arc<dyn ClowderMintConnector>,
        swap_config: SwapConfig,
    ) -> Result<MeltSummary> {
        let parsed_address: bitcoin::Address<bitcoin::address::NetworkUnchecked> = address
            .parse()
            .map_err(|e| Error::MintingError(format!("invalid address: {e}")))?;

        // inputs need to cover amount + network_fee + melt_fee
        let full_amount = amount + network_fee + melt_fee;
        let (send_summary, send_ref) = self
            .compute_send_costs(Amount::from(full_amount), keysets_info)
            .await?;

        let sending_proofs = send_proofs(
            send_ref.plan,
            keysets_info,
            send_ref.target_amount,
            &self.seed,
            self.pdb.as_ref(),
            &client,
            swap_config.clone(),
            self.beta.as_ref(),
        )
        .await?;
        let sent_ys: Vec<cdk01::PublicKey> = sending_proofs.keys().cloned().collect();

        let quote_record_amount = async {
            let proofs: Vec<cashu::Proof> = sending_proofs.values().cloned().collect();
            let attestation = self.beta.attest(&proofs).await?;
            let quote_result = client
                .post_melt_quote_onchain(
                    proofs,
                    bitcoin::Amount::from_sat(amount),
                    bitcoin::Amount::from_sat(network_fee),
                    parsed_address,
                    swap_config.alpha_pk,
                    attestation,
                )
                .await?;
            let quote_id = quote_result.quote_id;
            let expiry = quote_result.expiry;
            let record = MeltCommitmentRecord {
                quote_id,
                expiry,
                commitment: quote_result.commitment,
                ephemeral_secret: quote_result.ephemeral_secret,
                body_content: quote_result.body_content,
            };
            self.mdb.store_melt_commitment(record).await?;
            Ok::<_, Error>((quote_id, expiry, quote_result.amount))
        }
        .await;

        let (quote_id, expiry, _) = match quote_record_amount {
            Ok(r) => r,
            Err(e) => {
                for y in &sent_ys {
                    if let Err(revert_err) = self.pdb.revert_pendingspent_to_unspent(*y).await {
                        tracing::error!(
                            "failed to revert proof {y} to unspent after melt prepare failure: {revert_err}"
                        );
                    }
                }
                return Err(e);
            }
        };

        let mut summary = MeltSummary::new();
        summary.amount = Amount::from(amount);
        summary.expiry = expiry;
        summary.fees = TransactionFees {
            network: cashu::Amount::from(network_fee),
            melt: cashu::Amount::from(melt_fee),
            swap: send_summary.fees.swap,
        };
        let melt_ref = MeltReference {
            rid: summary.request_id,
            quote_id,
        };
        self.current_melt.lock().unwrap().replace(melt_ref);
        Ok(summary)
    }

    async fn pay_onchain_melt(
        &self,
        rid: Uuid,
        client: Arc<dyn ClowderMintConnector>,
    ) -> Result<(bitcoin::Txid, HashMap<cdk01::PublicKey, cdk00::Proof>)> {
        let melt_ref = self.current_melt.lock().unwrap().take();
        let melt_ref = melt_ref.ok_or(Error::NoPrepareRef(rid))?;
        if melt_ref.rid != rid {
            return Err(Error::NoPrepareRef(rid));
        }

        let record = self.mdb.load_melt_commitment(melt_ref.quote_id).await?;
        let body: wire_melt::MeltQuoteOnchainResponseBody =
            bcr_common::core::signature::deserialize_borsh_msg(&record.body_content)?;
        let input_ys: Vec<cashu::PublicKey> = body.inputs.inputs.iter().map(|fp| fp.y).collect();
        let sending_proofs = self.pdb.load_proofs(&input_ys).await?;

        let inputs: Vec<cdk00::Proof> = sending_proofs.values().cloned().collect();
        let request = wire_melt::MeltOnchainRequest {
            quote: melt_ref.quote_id,
            inputs,
        };
        let response = client.post_melt_onchain(request).await?;

        self.mdb.delete_melt_commitment(melt_ref.quote_id).await?;
        Ok((response.txid, sending_proofs))
    }

    async fn mint_onchain(
        &self,
        amount: bitcoin::Amount,
        keysets_info: &HashMap<cashu::Id, KeySetInfo>,
        client: Arc<dyn ClowderMintConnector>,
    ) -> Result<MintSummary> {
        // find debit keyset
        let active_info = keysets_info
            .values()
            .find(|info| info.unit == self.unit && info.active && info.final_expiry.is_none());
        let Some(active_info) = active_info else {
            return Err(Error::NoActiveKeyset);
        };
        let kid = active_info.id;
        let keyset = client.get_mint_keyset(kid).await?;
        let counter = self.pdb.counter(kid).await?;
        let premint = cdk00::PreMintSecrets::from_seed(
            kid,
            counter,
            &self.seed,
            cashu::Amount::from(amount.to_sat()),
            &SplitTarget::None,
            &bcr_wallet_core::util::to_fee_and_amounts(&keyset),
        )?;
        self.pdb
            .increment_counter(kid, counter, premint.len() as u32)
            .await?;

        let blinded_messages = premint.blinded_messages();

        let ephemeral_keypair =
            secp256k1::Keypair::new_global(&mut bitcoin::secp256k1::rand::thread_rng());
        let ephemeral_secret = secp256k1::SecretKey::from_keypair(&ephemeral_keypair);
        let wallet_key =
            cashu::PublicKey::from(secp256k1::PublicKey::from_keypair(&ephemeral_keypair));

        let request = wire_mint::OnchainMintQuoteRequest {
            blinded_messages: blinded_messages.clone(),
            wallet_key,
        };

        // Request mint quote
        let response = client.post_mint_quote_onchain(request).await?;

        bcr_common::core::signature::schnorr_verify_b64(
            &response.content,
            &response.commitment,
            &self.beta.alpha_id().x_only_public_key().0,
        )?;

        let body: wire_mint::OnchainMintQuoteResponseBody =
            bcr_common::core::signature::deserialize_borsh_msg(&response.content)?;

        if body.blinded_messages != blinded_messages {
            return Err(Error::MintingError(
                "blinded messages mismatch in mint quote response".to_string(),
            ));
        }

        let address: bitcoin::Address<bitcoin::address::NetworkUnchecked> = body
            .address
            .parse()
            .map_err(|e| Error::MintingError(format!("invalid address: {e}")))?;

        let mint_summary = MintSummary {
            quote_id: body.quote,
            amount: body.payment_amount,
            address: address.clone(),
            expiry: body.expiry,
        };

        self.mdb
            .store_mint(
                mint_summary.quote_id,
                mint_summary.amount,
                mint_summary.address.clone(),
                mint_summary.expiry,
                premint,
                response.content,
                response.commitment,
                ephemeral_secret,
            )
            .await?;
        Ok(mint_summary)
    }

    async fn check_pending_mints(
        &self,
        client: Arc<dyn ClowderMintConnector>,
    ) -> Result<HashMap<Uuid, CheckPendingMintResult>> {
        let mint_ids = self.mdb.list_mints().await?;
        let mut res = HashMap::with_capacity(mint_ids.len());

        tracing::debug!("check pending mints for {} mints", mint_ids.len());
        for qid in mint_ids {
            match self.check_pending_mint(qid, client.clone()).await {
                Ok(Some(mint_res)) => {
                    res.insert(qid, mint_res);
                }
                Ok(None) => {} // nop
                Err(e) => {
                    tracing::error!("Error while checking pending mint for {qid}: {e}");
                }
            };
        }
        Ok(res)
    }

    async fn check_pending_commitments(&self, tstamp: u64) -> Result<()> {
        let commitments = self.pdb.list_commitments().await?;
        tracing::debug!(
            "check pending commitments for {} entries",
            commitments.len()
        );
        for record in commitments {
            if record.expiry < tstamp {
                tracing::warn!(
                    "Swap commitment {} expired at {} (now: {tstamp}) - deleting record.",
                    record.commitment,
                    record.expiry,
                );
                self.pdb.delete_commitment(record.commitment).await?;
            }
        }
        Ok(())
    }

    async fn protest_mint(
        &self,
        qid: Uuid,
        client: Arc<dyn ClowderMintConnector>,
    ) -> Result<ProtestResult> {
        let record = self.mdb.load_mint(qid).await?;

        let ephemeral_keypair =
            secp256k1::Keypair::from_secret_key(secp256k1::SECP256K1, &record.ephemeral_secret);
        let wallet_signature = super::sign_content_b64(&record.content, &ephemeral_keypair)?;

        let request = wire_mint::MintProtestRequest {
            alpha_id: self.beta.alpha_id(),
            quote_id: record.summary.quote_id,
            content: record.content,
            commitment: record.commitment,
            wallet_signature,
        };

        let response = client.post_protest_mint(request).await?;

        match response.status {
            wire_common::ProtestStatus::Resolved => {
                let signatures = response.signatures.ok_or(Error::MintingError(
                    "protest resolved but no signatures returned".to_string(),
                ))?;

                let (amount, ys) = self
                    .finalize_mint_proofs(signatures, &record.premint, client)
                    .await?;

                self.mdb.delete_mint(qid).await?;

                tracing::info!("Protest resolved for {qid}, minted {amount}");
                Ok(ProtestResult {
                    status: wire_common::ProtestStatus::Resolved,
                    result: Some((amount, ys)),
                })
            }
            wire_common::ProtestStatus::Rabid => {
                tracing::warn!("Protest for {qid} returned rabid");
                Ok(ProtestResult {
                    status: wire_common::ProtestStatus::Rabid,
                    result: None,
                })
            }
            wire_common::ProtestStatus::Offline => {
                tracing::warn!("Protest for {qid} returned offline");
                Ok(ProtestResult {
                    status: wire_common::ProtestStatus::Offline,
                    result: None,
                })
            }
        }
    }

    async fn protest_swap(
        &self,
        commitment_sig: bitcoin::secp256k1::schnorr::Signature,
        keysets_info: &HashMap<cashu::Id, KeySetInfo>,
        alpha_client: Arc<dyn ClowderMintConnector>,
        swap_config: SwapConfig,
    ) -> Result<ProtestResult> {
        let record = self.pdb.load_commitment(commitment_sig).await?;
        let loaded_proofs = self.pdb.load_proofs(&record.inputs).await?;
        let ephemeral_keypair =
            secp256k1::Keypair::from_secret_key(secp256k1::SECP256K1, &record.ephemeral_secret);
        let wallet_signature = super::sign_content_b64(&record.body_content, &ephemeral_keypair)?;

        let request = wire_swap::SwapProtestRequest {
            alpha_id: self.beta.alpha_id(),
            proofs: loaded_proofs.into_values().collect(),
            content: record.body_content,
            commitment: record.commitment,
            wallet_signature,
            blind_signatures: None,
        };

        let response = self.beta.random_client().post_protest_swap(request).await?;

        match response.status {
            wire_common::ProtestStatus::Resolved => {
                let signatures = response.signatures.ok_or(Error::MintingError(
                    "swap protest resolved but no signatures returned".to_string(),
                ))?;

                let mut sigs_by_kid: HashMap<cashu::Id, Vec<cdk00::BlindSignature>> =
                    HashMap::new();
                for signature in signatures {
                    sigs_by_kid
                        .entry(signature.keyset_id)
                        .or_default()
                        .push(signature);
                }

                let mut keysets: HashMap<cashu::Id, KeySet> = HashMap::new();
                for kid in sigs_by_kid.keys() {
                    let keyset = alpha_client.get_mint_keyset(*kid).await?;
                    keysets.insert(*kid, keyset);
                }

                // Unblind using the ORIGINAL premint secrets stored with the commitment
                let mut unblinded: Vec<Proof> = Vec::new();
                for (kid, ps) in record.premints {
                    let keyset = keysets.get(&kid).expect("keyset should be here");
                    let sigs = sigs_by_kid.get(&kid).expect("signatures should be here");
                    let unblinded_proofs = super::unblind_proofs(keyset, sigs.to_owned(), ps);
                    unblinded.extend(unblinded_proofs);
                }

                let mut proofs: HashMap<cdk01::PublicKey, cdk00::Proof> =
                    HashMap::with_capacity(unblinded.len());
                for proof in unblinded {
                    let y = proof.y()?;
                    proofs.insert(y, proof);
                }

                let (amount, ys) = self
                    .digest_proofs(alpha_client, keysets_info, proofs, swap_config)
                    .await?;

                self.pdb.delete_commitment(commitment_sig).await?;

                tracing::info!("Swap protest resolved for {commitment_sig}, received {amount}");
                Ok(ProtestResult {
                    status: wire_common::ProtestStatus::Resolved,
                    result: Some((amount, ys)),
                })
            }
            wire_common::ProtestStatus::Rabid => {
                tracing::warn!("Swap protest for {commitment_sig} returned rabid");
                Ok(ProtestResult {
                    status: wire_common::ProtestStatus::Rabid,
                    result: None,
                })
            }
            wire_common::ProtestStatus::Offline => {
                tracing::warn!("Swap protest for {commitment_sig} returned offline");
                Ok(ProtestResult {
                    status: wire_common::ProtestStatus::Offline,
                    result: None,
                })
            }
        }
    }

    async fn protest_melt(&self, quote_id: Uuid) -> Result<MeltProtestResult> {
        let record = self.mdb.load_melt_commitment(quote_id).await?;
        let ephemeral_keypair =
            secp256k1::Keypair::from_secret_key(secp256k1::SECP256K1, &record.ephemeral_secret);
        let wallet_signature = super::sign_content_b64(&record.body_content, &ephemeral_keypair)?;

        let request = wire_melt::MeltProtestRequest {
            alpha_id: self.beta.alpha_id(),
            quote_id,
            content: record.body_content.clone(),
            commitment: record.commitment,
            wallet_signature,
        };

        let response = self.beta.random_client().post_protest_melt(request).await?;

        match response.status {
            wire_common::ProtestStatus::Resolved => {
                let body: wire_melt::MeltQuoteOnchainResponseBody =
                    bcr_common::core::signature::deserialize_borsh_msg(&record.body_content)?;
                let ys: Vec<cashu::PublicKey> = body.inputs.inputs.iter().map(|fp| fp.y).collect();
                self.mdb.delete_melt_commitment(quote_id).await?;
                tracing::info!("Melt protest resolved for {quote_id}");
                Ok(MeltProtestResult {
                    base: ProtestResult {
                        status: wire_common::ProtestStatus::Resolved,
                        result: Some((cashu::Amount::from(body.amount.to_sat()), ys)),
                    },
                    txid: response.txid,
                })
            }
            wire_common::ProtestStatus::Rabid => {
                tracing::warn!("Melt protest for {quote_id} returned rabid");
                Ok(MeltProtestResult {
                    base: ProtestResult {
                        status: wire_common::ProtestStatus::Rabid,
                        result: None,
                    },
                    txid: None,
                })
            }
            wire_common::ProtestStatus::Offline => {
                tracing::warn!("Melt protest for {quote_id} returned offline");
                Ok(MeltProtestResult {
                    base: ProtestResult {
                        status: wire_common::ProtestStatus::Offline,
                        result: None,
                    },
                    txid: None,
                })
            }
        }
    }

    async fn list_melt_commitments(&self) -> Result<Vec<(Uuid, u64)>> {
        let commitments = self.mdb.list_melt_commitments().await?;
        Ok(commitments
            .into_iter()
            .map(|r| (r.quote_id, r.expiry))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;
    use crate::{
        external::mint::{MeltQuoteResult, MockClowderMintConnector},
        pocket::{
            PocketApi,
            debit::DebitPocketApi,
            test_utils::tests::{mock_commitment_result, test_kinfos},
        },
    };
    use bcr_common::{core_tests, wire::mint::OnchainMintResponse};
    use bcr_wallet_persistence::{
        MockMintMeltRepository, MockPocketRepository,
        test_utils::tests::valid_payment_address_testnet,
    };
    use mockall::predicate::*;

    use crate::pocket::test_utils::tests::{
        setup_attestation_mock, setup_commitment_mocks, test_beta_provider, test_swap_config,
    };

    fn pocket(pdb: Arc<dyn PocketRepository>, mdb: Arc<dyn MintMeltRepository>) -> super::Pocket {
        let unit = CurrencyUnit::Sat;
        let seed = bip39::Mnemonic::generate(12).unwrap().to_seed("");
        super::Pocket::new(unit, pdb, mdb, seed, Arc::new(test_beta_provider()))
    }

    fn pocket_with_beta(
        pdb: Arc<dyn PocketRepository>,
        mdb: Arc<dyn MintMeltRepository>,
        betas: Vec<Arc<dyn crate::ClowderMintConnector>>,
        alpha_id: bitcoin::secp256k1::PublicKey,
    ) -> super::Pocket {
        let provider = crate::pocket::RandomBetaProvider::new(betas, alpha_id).unwrap();
        let unit = CurrencyUnit::Sat;
        let seed = bip39::Mnemonic::generate(12).unwrap().to_seed("");
        super::Pocket::new(unit, pdb, mdb, seed, Arc::new(provider))
    }

    #[tokio::test]
    async fn debit_balance() {
        let (info, keyset) = core_tests::generate_random_ecash_keyset();
        let k_infos = test_kinfos(info);
        let amounts = [Amount::from(8u64), Amount::from(16u64)];
        let proofs = core_tests::generate_random_ecash_proofs(&keyset, &amounts);
        let mut pdb = MockPocketRepository::new();
        let mdb = MockMintMeltRepository::new();

        let proofs_clone = proofs.clone();
        pdb.expect_list_unspent().times(1).returning(move || {
            let mut map = HashMap::new();
            map.insert(proofs_clone[0].y().unwrap(), proofs_clone[0].clone());
            map.insert(proofs_clone[1].y().unwrap(), proofs_clone[1].clone());
            Ok(map)
        });

        let pocket = pocket(Arc::new(pdb), Arc::new(mdb));
        let balance = pocket.balance(&k_infos).await.expect("balance works");
        assert_eq!(balance.credit, Amount::ZERO);
        assert_eq!(balance.debit, Amount::from(24u64))
    }

    #[tokio::test]
    async fn credit_balance_keyset_expiring_in_future() {
        let (info, keyset) = core_tests::generate_random_ecash_keyset();
        let mut k_info = KeySetInfo::from(info);
        k_info.final_expiry =
            Some((chrono::Utc::now() + chrono::TimeDelta::days(1)).timestamp() as u64);

        let mut k_infos = HashMap::new();
        k_infos.insert(k_info.id, k_info);
        let amounts = [Amount::from(8u64), Amount::from(16u64)];
        let proofs = core_tests::generate_random_ecash_proofs(&keyset, &amounts);
        let mut pdb = MockPocketRepository::new();
        let mdb = MockMintMeltRepository::new();

        let proofs_clone = proofs.clone();
        pdb.expect_list_unspent().times(1).returning(move || {
            let mut map = HashMap::new();
            map.insert(proofs_clone[0].y().unwrap(), proofs_clone[0].clone());
            map.insert(proofs_clone[1].y().unwrap(), proofs_clone[1].clone());
            Ok(map)
        });

        let pocket = pocket(Arc::new(pdb), Arc::new(mdb));
        let balance = pocket.balance(&k_infos).await.expect("balance works");

        assert_eq!(balance.debit, Amount::ZERO);
        assert_eq!(balance.credit, Amount::from(24u64));
    }

    #[tokio::test]
    async fn credit_balance_keyset_expiring_earlier_today() {
        let (info, keyset) = core_tests::generate_random_ecash_keyset();
        let mut k_info = KeySetInfo::from(info);

        let earlier_today = chrono::Utc::now()
            .date_naive()
            .and_hms_opt(0, 0, 1)
            .unwrap()
            .and_utc()
            .timestamp() as u64;

        k_info.final_expiry = Some(earlier_today);

        let mut k_infos = HashMap::new();
        k_infos.insert(k_info.id, k_info);
        let amounts = [Amount::from(8u64), Amount::from(16u64)];
        let proofs = core_tests::generate_random_ecash_proofs(&keyset, &amounts);
        let mut pdb = MockPocketRepository::new();
        let mdb = MockMintMeltRepository::new();

        let proofs_clone = proofs.clone();
        pdb.expect_list_unspent().times(1).returning(move || {
            let mut map = HashMap::new();
            map.insert(proofs_clone[0].y().unwrap(), proofs_clone[0].clone());
            map.insert(proofs_clone[1].y().unwrap(), proofs_clone[1].clone());
            Ok(map)
        });

        let pocket = pocket(Arc::new(pdb), Arc::new(mdb));
        let balance = pocket.balance(&k_infos).await.expect("balance works");

        assert_eq!(balance.debit, Amount::ZERO);
        assert_eq!(balance.credit, Amount::from(24u64));
    }

    #[tokio::test]
    async fn mixed_credit_and_debit_balance() {
        let (info_debit, keyset_debit) = core_tests::generate_random_ecash_keyset();
        let (info_credit, keyset_credit) = core_tests::generate_random_ecash_keyset();

        let mut ks_debit = KeySetInfo::from(info_debit);
        // yesterday → debit
        ks_debit.final_expiry =
            Some((chrono::Utc::now() - chrono::TimeDelta::days(1)).timestamp() as u64);

        let mut ks_credit = KeySetInfo::from(info_credit);
        // tomorrow → credit
        ks_credit.final_expiry =
            Some((chrono::Utc::now() + chrono::TimeDelta::days(1)).timestamp() as u64);

        let mut k_infos = HashMap::new();
        k_infos.insert(ks_debit.id, ks_debit);
        k_infos.insert(ks_credit.id, ks_credit);

        let debit_amount = Amount::from(8u64);
        let credit_amount = Amount::from(16u64);

        let proofs_debit = core_tests::generate_random_ecash_proofs(&keyset_debit, &[debit_amount]);
        let proofs_credit =
            core_tests::generate_random_ecash_proofs(&keyset_credit, &[credit_amount]);

        let mut pdb = MockPocketRepository::new();
        let mdb = MockMintMeltRepository::new();

        let p_debit = proofs_debit[0].clone();
        let p_credit = proofs_credit[0].clone();

        pdb.expect_list_unspent().times(1).returning(move || {
            let mut map = HashMap::new();
            map.insert(p_debit.y().unwrap(), p_debit.clone());
            map.insert(p_credit.y().unwrap(), p_credit.clone());
            Ok(map)
        });

        let pocket = pocket(Arc::new(pdb), Arc::new(mdb));
        let balance = pocket.balance(&k_infos).await.expect("balance works");

        assert_eq!(balance.debit, debit_amount);
        assert_eq!(balance.credit, credit_amount);
    }

    #[tokio::test]
    async fn debit_receive_proofs() {
        let (info, keyset) = core_tests::generate_random_ecash_keyset();
        let kid = info.id;
        let k_infos = test_kinfos(info);
        let amounts = [Amount::from(8u64), Amount::from(16u64)];
        let proofs = core_tests::generate_random_ecash_proofs(&keyset, &amounts);

        let mdb = MockMintMeltRepository::new();
        let mut pdb = MockPocketRepository::new();
        let mut connector = MockClowderMintConnector::new();
        let cloned_keyset = keyset.clone();
        connector
            .expect_get_mint_keyset()
            .times(1)
            .with(eq(kid))
            .returning(move |_| Ok(bcr_wallet_core::util::to_keyset(&cloned_keyset, None)));
        pdb.expect_counter()
            .times(1)
            .with(eq(kid))
            .returning(|_| Ok(0));
        pdb.expect_increment_counter()
            .times(1)
            .with(eq(kid), eq(0), eq(2))
            .returning(|_, _, _| Ok(()));
        setup_commitment_mocks(&mut connector, &mut pdb);
        connector
            .expect_post_swap_committed()
            .times(1)
            .returning(move |_, outp, _| {
                let amounts = outp.iter().map(|b| b.amount).collect::<Vec<_>>();
                let signatures = core_tests::generate_ecash_signatures(&keyset, &amounts);
                Ok(signatures)
            });
        pdb.expect_store_new().times(2).returning(|p| {
            let y = p.y().expect("Hash to curve should not fail");
            Ok(y)
        });
        let pocket = pocket(Arc::new(pdb), Arc::new(mdb));
        let (cashed, _) = pocket
            .receive_proofs(Arc::new(connector), &k_infos, proofs, test_swap_config())
            .await
            .unwrap();
        assert_eq!(cashed, Amount::from(24u64));
    }

    #[tokio::test]
    async fn debit_reclaim_proofs() {
        let (info, keyset) = core_tests::generate_random_ecash_keyset();
        let kid = info.id;
        let k_infos = test_kinfos(info);
        let amounts = [Amount::from(8u64), Amount::from(16u64)];
        let proofs = core_tests::generate_random_ecash_proofs(&keyset, &amounts);

        let ys: Vec<cdk01::PublicKey> = proofs.iter().map(|p| p.y().expect("valid y")).collect();

        let mdb = MockMintMeltRepository::new();
        let mut pdb = MockPocketRepository::new();
        let mut connector = MockClowderMintConnector::new();
        let cloned_keyset = keyset.clone();

        connector
            .expect_get_mint_keyset()
            .times(1)
            .with(eq(kid))
            .returning(move |_| Ok(bcr_wallet_core::util::to_keyset(&cloned_keyset, None)));
        let proofs_clone = proofs.clone();
        let ys_clone = ys.clone();
        pdb.expect_load_proofs()
            .times(1)
            .with(eq(ys_clone))
            .returning(move |_| {
                let mut map = HashMap::new();
                map.insert(proofs_clone[0].y().unwrap(), proofs_clone[0].clone());
                map.insert(proofs_clone[1].y().unwrap(), proofs_clone[1].clone());
                Ok(map)
            });
        pdb.expect_counter()
            .times(1)
            .with(eq(kid))
            .returning(|_| Ok(0));
        pdb.expect_increment_counter()
            .times(1)
            .with(eq(kid), eq(0), eq(2))
            .returning(|_, _, _| Ok(()));
        setup_commitment_mocks(&mut connector, &mut pdb);
        connector
            .expect_post_swap_committed()
            .times(1)
            .returning(move |_, outp, _| {
                let amounts = outp.iter().map(|b| b.amount).collect::<Vec<_>>();
                let signatures = core_tests::generate_ecash_signatures(&keyset, &amounts);
                Ok(signatures)
            });
        pdb.expect_store_new().times(2).returning(|p| {
            let y = p.y().expect("Hash to curve should not fail");
            Ok(y)
        });

        let pocket = pocket(Arc::new(pdb), Arc::new(mdb));
        let reclaimed = pocket
            .reclaim_proofs(&ys, &k_infos, Arc::new(connector), test_swap_config())
            .await
            .expect("reclaim works");
        assert_eq!(reclaimed, Amount::from(24u64));
    }

    #[tokio::test]
    async fn debit_recover_pending_stale_proofs() {
        let (info, keyset) = core_tests::generate_random_ecash_keyset();
        let kid = info.id;
        let k_infos = test_kinfos(info);
        let amounts = [Amount::from(8u64), Amount::from(16u64)];
        let proofs = core_tests::generate_random_ecash_proofs(&keyset, &amounts);

        // we pretend that the second proof belongs to a pending transaction we don't want to recover
        let pending_tx_y = proofs[1].clone().y().unwrap();

        let mdb = MockMintMeltRepository::new();
        let mut pdb = MockPocketRepository::new();
        let mut connector = MockClowderMintConnector::new();
        let cloned_keyset = keyset.clone();

        connector
            .expect_get_mint_keyset()
            .times(1)
            .with(eq(kid))
            .returning(move |_| Ok(bcr_wallet_core::util::to_keyset(&cloned_keyset, None)));
        connector
            .expect_post_check_state()
            .times(1)
            .returning(move |request| {
                let states = request
                    .ys
                    .iter()
                    .map(|y| cdk07::ProofState {
                        y: *y,
                        state: cdk07::State::Unspent,
                        witness: None,
                    })
                    .collect();
                Ok(states)
            });
        let proofs_clone = proofs.clone();
        pdb.expect_list_pending().times(1).returning(move || {
            let mut map = HashMap::new();
            map.insert(proofs_clone[0].y().unwrap(), proofs_clone[0].clone());
            map.insert(proofs_clone[1].y().unwrap(), proofs_clone[1].clone());
            Ok(map)
        });
        pdb.expect_counter()
            .times(1)
            .with(eq(kid))
            .returning(|_| Ok(0));
        pdb.expect_increment_counter()
            .times(1)
            .with(eq(kid), eq(0), eq(1))
            .returning(|_, _, _| Ok(()));
        let proofs_clone_mark = proofs.clone();
        pdb.expect_mark_pending_as_spent()
            .times(1)
            .returning(move |_| Ok(proofs_clone_mark[0].clone()));
        setup_commitment_mocks(&mut connector, &mut pdb);
        connector
            .expect_post_swap_committed()
            .times(1)
            .returning(move |_, outp, _| {
                let amounts = outp.iter().map(|b| b.amount).collect::<Vec<_>>();
                let signatures = core_tests::generate_ecash_signatures(&keyset, &amounts);
                Ok(signatures)
            });
        pdb.expect_store_new().times(1).returning(|p| {
            let y = p.y().expect("Hash to curve should not fail");
            Ok(y)
        });

        let pocket = pocket(Arc::new(pdb), Arc::new(mdb));
        let recovered = pocket
            .recover_pending_stale_proofs(
                &[pending_tx_y],
                &k_infos,
                Arc::new(connector),
                test_swap_config(),
            )
            .await
            .expect("recover pending stale proofs works");
        assert_eq!(recovered, Amount::from(8u64));
    }

    #[tokio::test]
    async fn pay_onchain_melt() {
        let quote_id = Uuid::new_v4();
        let rid = Uuid::new_v4();
        let tx_id = bitcoin::Txid::from_str(
            "c66bdb3be47c2252cf60bf98da828c595592b91637e4bab88471a7eb76e81562",
        )
        .unwrap();

        let mut mdb = MockMintMeltRepository::new();
        let mut pdb = MockPocketRepository::new();
        let mut connector = MockClowderMintConnector::new();

        // Mock load_melt_commitment
        let ephemeral = secp256k1::Keypair::new_global(&mut secp256k1::rand::thread_rng());
        let commitment_sig = cashu::SecretKey::generate().sign(&[0u8; 32]).unwrap();
        let wallet_key = cashu::PublicKey::from(secp256k1::PublicKey::from_keypair(&ephemeral));
        let body = wire_melt::MeltQuoteOnchainResponseBody {
            quote: quote_id,
            inputs: bcr_common::wire::attestation::AttestedFingerprints {
                inputs: vec![],
                attestation: crate::pocket::test_utils::tests::mock_attestation(),
            },
            address: bitcoin::Address::from_str("tb1qteyk7pfvvql2r2zrsu4h4xpvju0nz7ykvguyk0")
                .expect("valid address"),
            amount: bitcoin::Amount::from_sat(100),
            network_fee: bitcoin::Amount::from_sat(10),
            melt_fee: bitcoin::Amount::from_sat(1),
            expiry: 999999,
            wallet_key,
        };
        use bitcoin::base64::{Engine, engine::general_purpose::STANDARD};
        let body_content = STANDARD.encode(borsh::to_vec(&body).unwrap());
        mdb.expect_load_melt_commitment()
            .times(1)
            .returning(move |_| {
                Ok(bcr_wallet_persistence::MeltCommitmentRecord {
                    quote_id,
                    expiry: 999999,
                    commitment: commitment_sig,
                    ephemeral_secret: secp256k1::SecretKey::from_keypair(&ephemeral),
                    body_content: body_content.clone(),
                })
            });

        pdb.expect_load_proofs()
            .times(1)
            .returning(|_| Ok(HashMap::new()));

        connector
            .expect_post_melt_onchain()
            .times(1)
            .returning(move |_| Ok(wire_melt::MeltOnchainResponse { txid: tx_id }));

        mdb.expect_delete_melt_commitment()
            .times(1)
            .returning(|_| Ok(()));

        let pocket = pocket(Arc::new(pdb), Arc::new(mdb));
        let melt_ref = MeltReference { rid, quote_id };
        pocket.current_melt.lock().unwrap().replace(melt_ref);

        let res = pocket
            .pay_onchain_melt(rid, Arc::new(connector))
            .await
            .expect("pay melt works");
        assert_eq!(res.0, tx_id);
    }

    fn mock_melt_commitment_body(quote_id: Uuid, amount: u64) -> String {
        let ephemeral = secp256k1::Keypair::new_global(&mut secp256k1::rand::thread_rng());
        let wallet_key = cashu::PublicKey::from(secp256k1::PublicKey::from_keypair(&ephemeral));
        let body = wire_melt::MeltQuoteOnchainResponseBody {
            quote: quote_id,
            inputs: bcr_common::wire::attestation::AttestedFingerprints {
                inputs: vec![],
                attestation: crate::pocket::test_utils::tests::mock_attestation(),
            },
            address: bitcoin::Address::from_str("tb1qteyk7pfvvql2r2zrsu4h4xpvju0nz7ykvguyk0")
                .expect("valid address"),
            amount: bitcoin::Amount::from_sat(amount),
            network_fee: bitcoin::Amount::from_sat(3),
            melt_fee: bitcoin::Amount::from_sat(1),
            expiry: 999999,
            wallet_key,
        };
        use bitcoin::base64::{Engine, engine::general_purpose::STANDARD};
        STANDARD.encode(borsh::to_vec(&body).unwrap())
    }

    #[tokio::test]
    async fn protest_melt_resolved() {
        let quote_id = Uuid::new_v4();
        let tx_id = bitcoin::Txid::from_str(
            "c66bdb3be47c2252cf60bf98da828c595592b91637e4bab88471a7eb76e81562",
        )
        .unwrap();

        let mut mdb = MockMintMeltRepository::new();
        let pdb = MockPocketRepository::new();

        let ephemeral = secp256k1::Keypair::new_global(&mut secp256k1::rand::thread_rng());
        let commitment_sig = cashu::SecretKey::generate().sign(&[0u8; 32]).unwrap();
        let body_content = mock_melt_commitment_body(quote_id, 100);
        mdb.expect_load_melt_commitment()
            .times(1)
            .returning(move |_| {
                Ok(bcr_wallet_persistence::MeltCommitmentRecord {
                    quote_id,
                    expiry: 999999,
                    commitment: commitment_sig,
                    ephemeral_secret: secp256k1::SecretKey::from_keypair(&ephemeral),
                    body_content: body_content.clone(),
                })
            });

        mdb.expect_delete_melt_commitment()
            .times(1)
            .returning(|_| Ok(()));

        let mut beta_mock = MockClowderMintConnector::new();
        beta_mock
            .expect_post_protest_melt()
            .times(1)
            .returning(move |_| {
                Ok(wire_melt::MeltProtestResponse {
                    status: wire_common::ProtestStatus::Resolved,
                    txid: Some(tx_id),
                })
            });
        let alpha_id = bitcoin::secp256k1::PublicKey::from_keypair(
            &bitcoin::secp256k1::Keypair::new_global(&mut secp256k1::rand::thread_rng()),
        );
        let pocket = pocket_with_beta(
            Arc::new(pdb),
            Arc::new(mdb),
            vec![Arc::new(beta_mock) as Arc<dyn crate::ClowderMintConnector>],
            alpha_id,
        );
        let result = pocket
            .protest_melt(quote_id)
            .await
            .expect("protest_melt resolved works");

        assert!(matches!(
            result.base.status,
            wire_common::ProtestStatus::Resolved
        ));
        assert_eq!(result.txid, Some(tx_id));
        assert!(result.base.result.is_some());
    }

    #[tokio::test]
    async fn protest_melt_rabid() {
        let quote_id = Uuid::new_v4();

        let mut mdb = MockMintMeltRepository::new();
        let pdb = MockPocketRepository::new();

        let ephemeral = secp256k1::Keypair::new_global(&mut secp256k1::rand::thread_rng());
        let commitment_sig = cashu::SecretKey::generate().sign(&[0u8; 32]).unwrap();
        let body_content = mock_melt_commitment_body(quote_id, 100);
        mdb.expect_load_melt_commitment()
            .times(1)
            .returning(move |_| {
                Ok(bcr_wallet_persistence::MeltCommitmentRecord {
                    quote_id,
                    expiry: 999999,
                    commitment: commitment_sig,
                    ephemeral_secret: secp256k1::SecretKey::from_keypair(&ephemeral),
                    body_content: body_content.clone(),
                })
            });

        let mut beta_mock = MockClowderMintConnector::new();
        beta_mock
            .expect_post_protest_melt()
            .times(1)
            .returning(|_| {
                Ok(wire_melt::MeltProtestResponse {
                    status: wire_common::ProtestStatus::Rabid,
                    txid: None,
                })
            });
        let alpha_id = bitcoin::secp256k1::PublicKey::from_keypair(
            &bitcoin::secp256k1::Keypair::new_global(&mut secp256k1::rand::thread_rng()),
        );
        let pocket = pocket_with_beta(
            Arc::new(pdb),
            Arc::new(mdb),
            vec![Arc::new(beta_mock) as Arc<dyn crate::ClowderMintConnector>],
            alpha_id,
        );
        let result = pocket
            .protest_melt(quote_id)
            .await
            .expect("protest_melt rabid works");

        assert!(matches!(
            result.base.status,
            wire_common::ProtestStatus::Rabid
        ));
        assert!(result.txid.is_none());
        assert!(result.base.result.is_none());
    }

    #[tokio::test]
    async fn mint_onchain() {
        let (info, keyset) = core_tests::generate_random_ecash_keyset();
        let kid = info.id;
        let k_infos = test_kinfos(info);
        let amount = bitcoin::Amount::from_sat(24);

        let mut mdb = MockMintMeltRepository::new();
        let mut pdb = MockPocketRepository::new();
        let mut connector = MockClowderMintConnector::new();

        pdb.expect_counter()
            .times(1)
            .with(eq(kid))
            .returning(|_| Ok(0));
        pdb.expect_increment_counter()
            .times(1)
            .returning(|_, _, _| Ok(()));

        mdb.expect_store_mint()
            .times(1)
            .returning(|_, _, _, _, _, _, _, _| Ok(Uuid::new_v4()));

        let clowder_keypair = {
            let secret_bytes: [u8; 32] = rand::random();
            bitcoin::secp256k1::Keypair::from_seckey_slice(
                bitcoin::secp256k1::SECP256K1,
                &secret_bytes,
            )
            .unwrap()
        };
        let clowder_id = bitcoin::secp256k1::PublicKey::from_keypair(&clowder_keypair);

        connector
            .expect_post_mint_quote_onchain()
            .times(1)
            .returning(move |req| {
                let body = wire_mint::OnchainMintQuoteResponseBody {
                    quote: Uuid::new_v4(),
                    address: "tb1qteyk7pfvvql2r2zrsu4h4xpvju0nz7ykvguyk0".to_string(),
                    payment_amount: amount,
                    expiry: chrono::Utc::now().timestamp() as u64,
                    blinded_messages: req.blinded_messages,
                    wallet_key: req.wallet_key,
                };
                let (content, commitment) =
                    bcr_common::core::signature::serialize_n_schnorr_sign_borsh_msg(
                        &body,
                        &clowder_keypair,
                    )
                    .unwrap();
                Ok(wire_mint::OnchainMintQuoteResponse {
                    content,
                    commitment,
                })
            });

        let keyset_clone = keyset.clone();
        connector
            .expect_get_mint_keyset()
            .times(1)
            .returning(move |_| Ok(bcr_wallet_core::util::to_keyset(&keyset_clone, None)));

        let mut beta_mock = MockClowderMintConnector::new();
        setup_attestation_mock(&mut beta_mock);
        let pocket = pocket_with_beta(
            Arc::new(pdb),
            Arc::new(mdb),
            vec![Arc::new(beta_mock) as Arc<dyn crate::ClowderMintConnector>],
            clowder_id,
        );

        let summary = pocket
            .mint_onchain(amount, &k_infos, Arc::new(connector))
            .await
            .expect("mint onchain works");
        assert_eq!(summary.amount, amount);
    }

    #[tokio::test]
    async fn check_pending_mints() {
        let uuid = Uuid::new_v4();
        let amount = bitcoin::Amount::from_sat(24);
        let (_, keyset) = core_tests::generate_random_ecash_keyset();

        let mut mdb = MockMintMeltRepository::new();
        let pdb = MockPocketRepository::new();
        let mut connector = MockClowderMintConnector::new();

        mdb.expect_list_mints()
            .times(1)
            .returning(move || Ok(vec![uuid]));

        let keyset_clone = keyset.clone();
        let dummy_secret = secp256k1::SecretKey::from_slice(&[1u8; 32]).unwrap();
        mdb.expect_load_mint().times(1).returning(move |_| {
            let premint = cdk00::PreMintSecrets::random(
                bcr_wallet_core::util::to_keyset(&keyset_clone, None).id,
                Amount::from(amount.to_sat()),
                &SplitTarget::None,
                &bcr_wallet_core::util::to_fee_and_amounts(&bcr_wallet_core::util::to_keyset(
                    &keyset_clone,
                    None,
                )),
            )
            .unwrap();
            let dummy_sig = bitcoin::secp256k1::schnorr::Signature::from_slice(&[0xab; 64])
                .expect("valid sig bytes");
            Ok(bcr_wallet_persistence::MintRecord {
                summary: MintSummary {
                    quote_id: uuid,
                    amount,
                    address: bitcoin::Address::from_str(
                        "tb1qteyk7pfvvql2r2zrsu4h4xpvju0nz7ykvguyk0",
                    )
                    .unwrap(),
                    expiry: chrono::Utc::now().timestamp() as u64,
                },
                premint,
                content: "dGVzdA==".to_string(),
                commitment: dummy_sig,
                ephemeral_secret: dummy_secret,
            })
        });

        let keyset_clone = keyset.clone();
        connector
            .expect_get_mint_keyset()
            .times(1)
            .returning(move |_| Ok(bcr_wallet_core::util::to_keyset(&keyset_clone, None)));

        connector
            .expect_post_mint_onchain()
            .times(1)
            .returning(move |_| Ok(OnchainMintResponse { signatures: vec![] }));

        let pocket = pocket(Arc::new(pdb), Arc::new(mdb));

        let res = pocket
            .check_pending_mints(Arc::new(connector))
            .await
            .expect("check pending mint works");
        assert_eq!(res.len(), 0);
    }

    #[tokio::test]
    async fn protest_mint_resolved() {
        let uuid = Uuid::new_v4();
        let amount = bitcoin::Amount::from_sat(24);
        let (info, mintkeyset) = core_tests::generate_random_ecash_keyset();
        let kid = info.id;
        let premint = cdk00::PreMintSecrets::random(
            kid,
            Amount::from(amount.to_sat()),
            &SplitTarget::None,
            &bcr_wallet_core::util::to_fee_and_amounts(&bcr_wallet_core::util::to_keyset(
                &mintkeyset,
                None,
            )),
        )
        .unwrap();

        let blind_sigs: Vec<cdk00::BlindSignature> = premint
            .blinded_messages()
            .iter()
            .map(|bm| {
                bcr_common::core::signature::sign_ecash(&mintkeyset, bm)
                    .expect("signing should work")
            })
            .collect();

        let premint_clone = premint.clone();
        let mut mdb = MockMintMeltRepository::new();
        let mut pdb = MockPocketRepository::new();
        let mut connector = MockClowderMintConnector::new();

        let dummy_sig = bitcoin::secp256k1::schnorr::Signature::from_slice(&[0xab; 64])
            .expect("valid sig bytes");
        let dummy_secret = secp256k1::SecretKey::from_slice(&[1u8; 32]).unwrap();
        mdb.expect_load_mint().times(1).returning(move |_| {
            Ok(bcr_wallet_persistence::MintRecord {
                summary: MintSummary {
                    quote_id: uuid,
                    amount,
                    address: bitcoin::Address::from_str(
                        "tb1qteyk7pfvvql2r2zrsu4h4xpvju0nz7ykvguyk0",
                    )
                    .unwrap(),
                    expiry: chrono::Utc::now().timestamp() as u64,
                },
                premint: premint_clone.clone(),
                content: "dGVzdA==".to_string(),
                commitment: dummy_sig,
                ephemeral_secret: dummy_secret,
            })
        });

        connector
            .expect_post_protest_mint()
            .times(1)
            .returning(move |_| {
                Ok(wire_mint::MintProtestResponse {
                    status: wire_common::ProtestStatus::Resolved,
                    signatures: Some(blind_sigs.clone()),
                })
            });

        let keyset_clone = mintkeyset.clone();
        connector
            .expect_get_mint_keyset()
            .times(1)
            .with(eq(kid))
            .returning(move |_| Ok(bcr_wallet_core::util::to_keyset(&keyset_clone, None)));

        pdb.expect_store_new().returning(|p| {
            let y = p.y().expect("Hash to curve should not fail");
            Ok(y)
        });

        mdb.expect_delete_mint().times(1).returning(move |_| Ok(()));

        let pocket = pocket(Arc::new(pdb), Arc::new(mdb));
        let ProtestResult { status, result } = pocket
            .protest_mint(uuid, Arc::new(connector))
            .await
            .expect("protest_mint resolved works");

        assert!(matches!(status, wire_common::ProtestStatus::Resolved));
        let (minted_amount, ys) = result.expect("resolved should return proofs");
        assert_eq!(minted_amount, Amount::from(amount.to_sat()));
        assert!(!ys.is_empty());
    }

    #[tokio::test]
    async fn protest_mint_rabid() {
        let uuid = Uuid::new_v4();
        let amount = bitcoin::Amount::from_sat(24);
        let (info, mintkeyset) = core_tests::generate_random_ecash_keyset();
        let kid = info.id;

        let premint = cdk00::PreMintSecrets::random(
            kid,
            Amount::from(amount.to_sat()),
            &SplitTarget::None,
            &bcr_wallet_core::util::to_fee_and_amounts(&bcr_wallet_core::util::to_keyset(
                &mintkeyset,
                None,
            )),
        )
        .unwrap();

        let mut mdb = MockMintMeltRepository::new();
        let pdb = MockPocketRepository::new();
        let mut connector = MockClowderMintConnector::new();

        let dummy_sig = bitcoin::secp256k1::schnorr::Signature::from_slice(&[0xab; 64])
            .expect("valid sig bytes");
        let dummy_secret = secp256k1::SecretKey::from_slice(&[1u8; 32]).unwrap();
        mdb.expect_load_mint().times(1).returning(move |_| {
            Ok(bcr_wallet_persistence::MintRecord {
                summary: MintSummary {
                    quote_id: uuid,
                    amount,
                    address: bitcoin::Address::from_str(
                        "tb1qteyk7pfvvql2r2zrsu4h4xpvju0nz7ykvguyk0",
                    )
                    .unwrap(),
                    expiry: chrono::Utc::now().timestamp() as u64,
                },
                premint: premint.clone(),
                content: "dGVzdA==".to_string(),
                commitment: dummy_sig,
                ephemeral_secret: dummy_secret,
            })
        });

        connector
            .expect_post_protest_mint()
            .times(1)
            .returning(move |_| {
                Ok(wire_mint::MintProtestResponse {
                    status: wire_common::ProtestStatus::Rabid,
                    signatures: None,
                })
            });

        let pocket = pocket(Arc::new(pdb), Arc::new(mdb));
        let ProtestResult { status, result } = pocket
            .protest_mint(uuid, Arc::new(connector))
            .await
            .expect("protest_mint rabid works");

        assert!(matches!(status, wire_common::ProtestStatus::Rabid));
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn protest_swap_resolved() {
        let amount = Amount::from(24u64);
        let (info, mintkeyset) = core_tests::generate_random_ecash_keyset();
        let kid = info.id;
        let k_infos = test_kinfos(info);

        // Generate input proofs that were committed
        let input_amounts = [Amount::from(16u64), Amount::from(8u64)];
        let input_proofs = core_tests::generate_random_ecash_proofs(&mintkeyset, &input_amounts);
        let input_ys: Vec<cashu::PublicKey> = input_proofs
            .iter()
            .map(|p| p.y().expect("y works"))
            .collect();
        let input_proofs_map: HashMap<cashu::PublicKey, cdk00::Proof> = input_proofs
            .iter()
            .map(|p| (p.y().unwrap(), p.clone()))
            .collect();

        // Generate premint secrets and sign them — these are the ORIGINAL blinding factors
        let premint = cdk00::PreMintSecrets::random(
            kid,
            amount,
            &SplitTarget::None,
            &bcr_wallet_core::util::to_fee_and_amounts(&bcr_wallet_core::util::to_keyset(
                &mintkeyset,
                None,
            )),
        )
        .unwrap();
        let blind_sigs: Vec<cdk00::BlindSignature> = premint
            .blinded_messages()
            .iter()
            .map(|bm| {
                bcr_common::core::signature::sign_ecash(&mintkeyset, bm)
                    .expect("signing should work")
            })
            .collect();
        let stored_premints = HashMap::from([(kid, premint)]);

        // Create ephemeral keypair for the commitment record
        let ephemeral_keypair = secp256k1::Keypair::new_global(&mut secp256k1::rand::thread_rng());
        let ephemeral_secret = secp256k1::SecretKey::from_keypair(&ephemeral_keypair);
        let wallet_key =
            cashu::PublicKey::from(secp256k1::PublicKey::from_keypair(&ephemeral_keypair));

        let commitment_sig = bitcoin::secp256k1::schnorr::Signature::from_slice(&[0xab; 64])
            .expect("valid sig bytes");

        let mdb = MockMintMeltRepository::new();
        let mut pdb = MockPocketRepository::new();
        let mut beta_connector = MockClowderMintConnector::new();
        let mut alpha_connector = MockClowderMintConnector::new();

        let record_inputs = input_ys.clone();
        let record_secret = ephemeral_secret;
        let record_commitment = commitment_sig;
        let record_wallet_key = wallet_key;
        let record_premints = stored_premints.clone();
        pdb.expect_load_commitment().times(1).returning(move |_| {
            Ok(bcr_wallet_persistence::SwapCommitmentRecord {
                inputs: record_inputs.clone(),
                outputs: vec![],
                expiry: 1000,
                commitment: record_commitment,
                ephemeral_secret: record_secret,
                body_content: "dGVzdA==".to_string(),
                wallet_key: record_wallet_key,
                premints: record_premints.clone(),
            })
        });

        let proofs_map = input_proofs_map.clone();
        pdb.expect_load_proofs()
            .times(1)
            .returning(move |_| Ok(proofs_map.clone()));

        // Beta handles the protest request
        beta_connector
            .expect_post_protest_swap()
            .times(1)
            .returning(move |_| {
                Ok(wire_swap::SwapProtestResponse {
                    status: wire_common::ProtestStatus::Resolved,
                    signatures: Some(blind_sigs.clone()),
                })
            });
        setup_attestation_mock(&mut beta_connector);

        // Alpha handles keyset lookup (for unblinding + digest_proofs)
        let keyset_clone = mintkeyset.clone();
        alpha_connector
            .expect_get_mint_keyset()
            .returning(move |_| Ok(bcr_wallet_core::util::to_keyset(&keyset_clone, None)));

        // Mocks for digest_proofs swap (runs against alpha)
        pdb.expect_counter().with(eq(kid)).returning(|_| Ok(0));
        pdb.expect_increment_counter().returning(|_, _, _| Ok(()));
        setup_commitment_mocks(&mut alpha_connector, &mut pdb);
        let swap_keyset = mintkeyset.clone();
        alpha_connector
            .expect_post_swap_committed()
            .times(1)
            .returning(move |_, outp, _| {
                let amounts: Vec<_> = outp.iter().map(|b| b.amount).collect();
                let signatures = core_tests::generate_ecash_signatures(&swap_keyset, &amounts);
                Ok(signatures)
            });

        pdb.expect_store_new().returning(|p| {
            let y = p.y().expect("Hash to curve should not fail");
            Ok(y)
        });

        pdb.expect_delete_commitment()
            .times(1)
            .returning(move |_| Ok(()));

        let alpha_id = bitcoin::secp256k1::PublicKey::from_keypair(
            &bitcoin::secp256k1::Keypair::new_global(&mut secp256k1::rand::thread_rng()),
        );
        let pocket = pocket_with_beta(
            Arc::new(pdb),
            Arc::new(mdb),
            vec![Arc::new(beta_connector) as Arc<dyn crate::ClowderMintConnector>],
            alpha_id,
        );
        let ProtestResult { status, result } = pocket
            .protest_swap(
                commitment_sig,
                &k_infos,
                Arc::new(alpha_connector),
                test_swap_config(),
            )
            .await
            .expect("protest_swap resolved works");

        assert!(matches!(status, wire_common::ProtestStatus::Resolved));
        let (swapped_amount, ys) = result.expect("resolved should return proofs");
        assert_eq!(swapped_amount, amount);
        assert!(!ys.is_empty());
    }

    #[tokio::test]
    async fn protest_swap_rabid() {
        let (info, mintkeyset) = core_tests::generate_random_ecash_keyset();
        let k_infos = test_kinfos(info);

        let input_amounts = [Amount::from(16u64), Amount::from(8u64)];
        let input_proofs = core_tests::generate_random_ecash_proofs(&mintkeyset, &input_amounts);
        let input_ys: Vec<cashu::PublicKey> = input_proofs
            .iter()
            .map(|p| p.y().expect("y works"))
            .collect();
        let input_proofs_map: HashMap<cashu::PublicKey, cdk00::Proof> = input_proofs
            .iter()
            .map(|p| (p.y().unwrap(), p.clone()))
            .collect();

        let ephemeral_keypair = secp256k1::Keypair::new_global(&mut secp256k1::rand::thread_rng());
        let ephemeral_secret = secp256k1::SecretKey::from_keypair(&ephemeral_keypair);
        let wallet_key =
            cashu::PublicKey::from(secp256k1::PublicKey::from_keypair(&ephemeral_keypair));

        let commitment_sig = bitcoin::secp256k1::schnorr::Signature::from_slice(&[0xab; 64])
            .expect("valid sig bytes");

        let mdb = MockMintMeltRepository::new();
        let mut pdb = MockPocketRepository::new();
        let mut beta_connector = MockClowderMintConnector::new();
        let alpha_connector = MockClowderMintConnector::new();

        let record_inputs = input_ys.clone();
        let record_secret = ephemeral_secret;
        let record_commitment = commitment_sig;
        let record_wallet_key = wallet_key;
        pdb.expect_load_commitment().times(1).returning(move |_| {
            Ok(bcr_wallet_persistence::SwapCommitmentRecord {
                inputs: record_inputs.clone(),
                outputs: vec![],
                expiry: 1000,
                commitment: record_commitment,
                ephemeral_secret: record_secret,
                body_content: "dGVzdA==".to_string(),
                wallet_key: record_wallet_key,
                premints: HashMap::new(),
            })
        });

        let proofs_map = input_proofs_map.clone();
        pdb.expect_load_proofs()
            .times(1)
            .returning(move |_| Ok(proofs_map.clone()));

        beta_connector
            .expect_post_protest_swap()
            .times(1)
            .returning(|_| {
                Ok(wire_swap::SwapProtestResponse {
                    status: wire_common::ProtestStatus::Rabid,
                    signatures: None,
                })
            });

        let alpha_id = bitcoin::secp256k1::PublicKey::from_keypair(
            &bitcoin::secp256k1::Keypair::new_global(&mut secp256k1::rand::thread_rng()),
        );
        let pocket = pocket_with_beta(
            Arc::new(pdb),
            Arc::new(mdb),
            vec![Arc::new(beta_connector) as Arc<dyn crate::ClowderMintConnector>],
            alpha_id,
        );
        let ProtestResult { status, result } = pocket
            .protest_swap(
                commitment_sig,
                &k_infos,
                Arc::new(alpha_connector),
                test_swap_config(),
            )
            .await
            .expect("protest_swap rabid works");

        assert!(matches!(status, wire_common::ProtestStatus::Rabid));
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn compute_send_costs_ready() {
        let (info, keyset) = core_tests::generate_random_ecash_keyset();
        let k_infos = test_kinfos(info);
        let target = Amount::from(24u64);
        let amounts = [Amount::from(8u64), Amount::from(16u64)];
        let proofs = core_tests::generate_random_ecash_proofs(&keyset, &amounts);

        let mut pdb = MockPocketRepository::new();
        let mdb = MockMintMeltRepository::new();

        let proofs_clone = proofs.clone();
        pdb.expect_list_unspent().times(1).returning(move || {
            let mut map = HashMap::new();
            for proof in &proofs_clone {
                map.insert(proof.y().unwrap(), proof.clone());
            }
            Ok(map)
        });

        let pocket = pocket(Arc::new(pdb), Arc::new(mdb));
        let (summary, send_ref) = pocket
            .compute_send_costs(target, &k_infos)
            .await
            .expect("compute send costs works");

        assert_eq!(summary.amount, target);
        assert_eq!(summary.unit, CurrencyUnit::Sat);
        assert_eq!(send_ref.rid, summary.request_id);
        assert_eq!(send_ref.target_amount, target);

        match send_ref.plan {
            SendPlan::Ready { proofs: selected } => {
                assert_eq!(selected.len(), 2);
                let expected: Vec<_> = proofs.iter().map(|p| p.y().unwrap()).collect();
                for y in expected {
                    assert!(selected.contains(&y));
                }
            }
            SendPlan::NeedSwap { .. } => panic!("expected ready send plan"),
        }
    }

    #[tokio::test]
    async fn compute_send_costs_need_swap_after_collecting_input() {
        let (info, keyset) = core_tests::generate_random_ecash_keyset();
        let k_infos = test_kinfos(info);

        // smallest first approach
        // 8 + 16 + 32 = 48, swap with 1 fee, 41 payment, 6 change
        let target = Amount::from(41u64);
        let amounts = [Amount::from(8u64), Amount::from(16u64), Amount::from(32u64)];
        let proofs = core_tests::generate_random_ecash_proofs(&keyset, &amounts);

        let mut pdb = MockPocketRepository::new();
        let mdb = MockMintMeltRepository::new();

        let proofs_clone = proofs.clone();
        pdb.expect_list_unspent().times(1).returning(move || {
            let mut map = HashMap::new();
            for proof in &proofs_clone {
                map.insert(proof.y().unwrap(), proof.clone());
            }
            Ok(map)
        });

        let pocket = pocket(Arc::new(pdb), Arc::new(mdb));
        let (summary, send_ref) = pocket
            .compute_send_costs(target, &k_infos)
            .await
            .expect("compute send costs works");

        assert_eq!(summary.amount, target);
        assert_eq!(summary.unit, CurrencyUnit::Sat);
        assert_eq!(send_ref.rid, summary.request_id);
        assert_eq!(send_ref.target_amount, target);

        match send_ref.plan {
            SendPlan::NeedSwap {
                inputs,
                target,
                estimated_fee,
            } => {
                assert_eq!(inputs.len(), amounts.len());
                assert_eq!(target, Amount::from(41u64));
                assert_eq!(summary.fees.swap, estimated_fee);
            }
            SendPlan::Ready { .. } => panic!("expected swap send plan"),
        }
    }

    #[tokio::test]
    async fn compute_send_costs_need_swap_small_over() {
        let (info, keyset) = core_tests::generate_random_ecash_keyset();
        let k_infos = test_kinfos(info);

        // swap 16+8
        let target = Amount::from(23u64);
        let amounts = [Amount::from(8u64), Amount::from(16u64)];
        let proofs = core_tests::generate_random_ecash_proofs(&keyset, &amounts);

        let mut pdb = MockPocketRepository::new();
        let mdb = MockMintMeltRepository::new();

        let proofs_clone = proofs.clone();

        pdb.expect_list_unspent().times(1).returning(move || {
            let mut map = HashMap::new();
            for proof in &proofs_clone {
                map.insert(proof.y().unwrap(), proof.clone());
            }
            Ok(map)
        });

        let pocket = pocket(Arc::new(pdb), Arc::new(mdb));
        let (summary, send_ref) = pocket
            .compute_send_costs(target, &k_infos)
            .await
            .expect("compute send costs works");

        assert_eq!(summary.amount, target);
        assert_eq!(summary.unit, CurrencyUnit::Sat);
        assert_eq!(send_ref.rid, summary.request_id);
        assert_eq!(send_ref.target_amount, target);

        match send_ref.plan {
            SendPlan::NeedSwap {
                inputs,
                target,
                estimated_fee,
            } => {
                assert_eq!(inputs.len(), amounts.len());
                assert_eq!(target, Amount::from(23u64));
                assert_eq!(summary.fees.swap, estimated_fee);
            }
            SendPlan::Ready { .. } => panic!("expected swap send plan"),
        }
    }

    #[tokio::test]
    async fn compute_send_costs_errors_without_funds() {
        let (info, _keyset) = core_tests::generate_random_ecash_keyset();
        let k_infos = test_kinfos(info);

        let mut pdb = MockPocketRepository::new();
        let mdb = MockMintMeltRepository::new();

        pdb.expect_list_unspent()
            .times(1)
            .returning(|| Ok(HashMap::new()));

        let pocket = pocket(Arc::new(pdb), Arc::new(mdb));
        let result = pocket
            .compute_send_costs(Amount::from(1u64), &k_infos)
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn check_pending_mint_success() {
        let qid = Uuid::new_v4();
        let amount = bitcoin::Amount::from_sat(24);

        let (info, mintkeyset) = core_tests::generate_random_ecash_keyset();
        let kid = info.id;

        let premint = cdk00::PreMintSecrets::random(
            kid,
            Amount::from(amount.to_sat()),
            &SplitTarget::None,
            &bcr_wallet_core::util::to_fee_and_amounts(&bcr_wallet_core::util::to_keyset(
                &mintkeyset,
                None,
            )),
        )
        .unwrap();

        let blind_sigs: Vec<cdk00::BlindSignature> = premint
            .blinded_messages()
            .iter()
            .map(|bm| bcr_common::core::signature::sign_ecash(&mintkeyset, bm).unwrap())
            .collect();

        let mut mdb = MockMintMeltRepository::new();
        let mut pdb = MockPocketRepository::new();
        let mut connector = MockClowderMintConnector::new();

        let premint_clone = premint.clone();
        let dummy_sig = bitcoin::secp256k1::schnorr::Signature::from_slice(&[0xab; 64]).unwrap();
        let dummy_secret = secp256k1::SecretKey::from_slice(&[1u8; 32]).unwrap();

        mdb.expect_load_mint().times(1).returning(move |_| {
            Ok(bcr_wallet_persistence::MintRecord {
                summary: MintSummary {
                    quote_id: qid,
                    amount,
                    address: valid_payment_address_testnet(),
                    expiry: chrono::Utc::now().timestamp() as u64,
                },
                premint: premint_clone.clone(),
                content: "dGVzdA==".to_string(),
                commitment: dummy_sig,
                ephemeral_secret: dummy_secret,
            })
        });

        connector
            .expect_post_mint_onchain()
            .times(1)
            .returning(move |_| {
                Ok(OnchainMintResponse {
                    signatures: blind_sigs.clone(),
                })
            });

        let keyset_clone = mintkeyset.clone();
        connector
            .expect_get_mint_keyset()
            .times(1)
            .with(eq(kid))
            .returning(move |_| Ok(bcr_wallet_core::util::to_keyset(&keyset_clone, None)));

        pdb.expect_store_new().returning(|p| {
            let y = p.y().unwrap();
            Ok(y)
        });

        mdb.expect_delete_mint().times(1).returning(|_| Ok(()));

        let pocket = pocket(Arc::new(pdb), Arc::new(mdb));

        let result = pocket
            .check_pending_mint(qid, Arc::new(connector))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(result.amount, Amount::from(amount.to_sat()));
        assert_eq!(result.fee, Amount::ZERO);
        assert!(!result.ys.is_empty());
    }

    #[tokio::test]
    async fn check_pending_mint_returns_error_when_minting_fails() {
        let qid = Uuid::new_v4();
        let amount = bitcoin::Amount::from_sat(24);

        let (info, keyset) = core_tests::generate_random_ecash_keyset();
        let kid = info.id;

        let premint = cdk00::PreMintSecrets::random(
            kid,
            Amount::from(amount.to_sat()),
            &SplitTarget::None,
            &bcr_wallet_core::util::to_fee_and_amounts(&bcr_wallet_core::util::to_keyset(
                &keyset, None,
            )),
        )
        .unwrap();

        let mut mdb = MockMintMeltRepository::new();
        let pdb = MockPocketRepository::new();
        let mut connector = MockClowderMintConnector::new();

        let dummy_sig = bitcoin::secp256k1::schnorr::Signature::from_slice(&[0xab; 64]).unwrap();
        let dummy_secret = secp256k1::SecretKey::from_slice(&[1u8; 32]).unwrap();

        mdb.expect_load_mint().times(1).returning(move |_| {
            Ok(bcr_wallet_persistence::MintRecord {
                summary: MintSummary {
                    quote_id: qid,
                    amount,
                    address: valid_payment_address_testnet(),
                    expiry: chrono::Utc::now().timestamp() as u64,
                },
                premint: premint.clone(),
                content: "dGVzdA==".to_string(),
                commitment: dummy_sig,
                ephemeral_secret: dummy_secret,
            })
        });

        connector
            .expect_post_mint_onchain()
            .times(1)
            .returning(|_| Err(Error::MintingError("not paid yet".to_string())));

        mdb.expect_delete_mint().times(0);

        let pocket = pocket(Arc::new(pdb), Arc::new(mdb));

        let result = pocket.check_pending_mint(qid, Arc::new(connector)).await;

        assert!(matches!(result, Err(Error::MintingError(_))));
    }

    #[tokio::test]
    async fn prepare_onchain_melt_ready_success() {
        let (info, keyset) = core_tests::generate_random_ecash_keyset();
        let k_infos = test_kinfos(info);

        let amount = 24;
        let network_fee = 3;
        let melt_fee = 1;
        let quote_id = Uuid::new_v4();
        let expiry = 999999;
        let offered_amount = bitcoin::Amount::from_sat(amount);

        let proofs = core_tests::generate_random_ecash_proofs(
            &keyset,
            &[Amount::from(4), Amount::from(8), Amount::from(16)],
        );

        let proofs_by_y: HashMap<_, _> = proofs
            .iter()
            .cloned()
            .map(|p| (p.y().unwrap(), p))
            .collect();

        let mut pdb = MockPocketRepository::new();
        let mut mdb = MockMintMeltRepository::new();
        let mut connector = MockClowderMintConnector::new();

        let unspent = proofs_by_y.clone();
        pdb.expect_list_unspent()
            .times(1)
            .returning(move || Ok(unspent.clone()));

        let pending = proofs_by_y.clone();
        pdb.expect_mark_as_pendingspent()
            .times(3)
            .returning(move |y| Ok(pending.get(&y).unwrap().clone()));

        connector
            .expect_post_melt_quote_onchain()
            .times(1)
            .returning(move |_, _, _, _, _, __| {
                Ok(MeltQuoteResult {
                    quote_id,
                    expiry,
                    amount: offered_amount,
                    commitment: cashu::SecretKey::generate().sign(&[0; 32]).unwrap(),
                    ephemeral_secret: secp256k1::SecretKey::from_keypair(
                        &secp256k1::Keypair::new_global(&mut secp256k1::rand::thread_rng()),
                    ),
                    body_content: mock_melt_commitment_body(quote_id, offered_amount.to_sat()),
                })
            });

        mdb.expect_store_melt_commitment()
            .times(1)
            .returning(|_| Ok(()));

        let pocket = pocket(Arc::new(pdb), Arc::new(mdb));

        let summary = pocket
            .prepare_onchain_melt(
                valid_payment_address_testnet().assume_checked().to_string(),
                amount,
                network_fee,
                melt_fee,
                &k_infos,
                Arc::new(connector),
                test_swap_config(),
            )
            .await
            .unwrap();

        assert_eq!(summary.amount, Amount::from(amount));
        assert_eq!(summary.expiry, expiry);
        assert_eq!(summary.fees.network, Amount::from(network_fee));
        assert_eq!(summary.fees.melt, Amount::from(melt_fee));
        assert_eq!(summary.fees.swap, Amount::ZERO);

        let current_melt = pocket.current_melt.lock().unwrap();
        let melt_ref = current_melt.as_ref().unwrap();
        assert_eq!(melt_ref.rid, summary.request_id);
        assert_eq!(melt_ref.quote_id, quote_id);
    }

    #[tokio::test]
    async fn prepare_onchain_melt_rejects_invalid_address() {
        let (info, _keyset) = core_tests::generate_random_ecash_keyset();
        let k_infos = test_kinfos(info);

        let pdb = MockPocketRepository::new();
        let mdb = MockMintMeltRepository::new();
        let connector = MockClowderMintConnector::new();

        let pocket = pocket(Arc::new(pdb), Arc::new(mdb));

        let result = pocket
            .prepare_onchain_melt(
                "invalid-bitcoin-address".to_string(),
                24,
                2,
                1,
                &k_infos,
                Arc::new(connector),
                test_swap_config(),
            )
            .await;

        assert!(matches!(result, Err(Error::MintingError(_))));
    }

    #[tokio::test]
    async fn clean_up_spent_proofs_all_spent_deleted() {
        let (_info, keyset) = core_tests::generate_random_ecash_keyset();
        let proofs =
            core_tests::generate_random_ecash_proofs(&keyset, &[Amount::from(8), Amount::from(16)]);

        let spent_map: HashMap<_, _> = proofs.iter().map(|p| (p.y().unwrap(), p.clone())).collect();

        let ys: Vec<_> = spent_map.keys().cloned().collect();

        let mut pdb = MockPocketRepository::new();
        let mdb = MockMintMeltRepository::new();
        let mut connector = MockClowderMintConnector::new();

        let spent_clone = spent_map.clone();
        pdb.expect_list_spent()
            .times(1)
            .returning(move || Ok(spent_clone.clone()));

        connector
            .expect_post_check_state()
            .times(1)
            .returning(move |_| {
                Ok(ys
                    .iter()
                    .map(|y| cdk07::ProofState {
                        y: *y,
                        state: cdk07::State::Spent,
                        witness: None,
                    })
                    .collect())
            });

        pdb.expect_delete_proof().times(2).returning(|_| Ok(None));

        let pocket = pocket(Arc::new(pdb), Arc::new(mdb));

        let cleaned = pocket
            .clean_up_spent_proofs(Arc::new(connector))
            .await
            .unwrap();

        assert_eq!(cleaned, 2);
    }

    #[tokio::test]
    async fn swap_to_unlocked_substitute_proofs_returns_payment_and_stores_change() {
        let (info, mint_keyset) = core_tests::generate_random_ecash_keyset();
        let kid = info.id;

        let keysets_info = test_kinfos(info);
        let keyset = bcr_wallet_core::util::to_keyset(&mint_keyset, None);
        let keysets = HashMap::from([(kid, keyset)]);

        // 24 total, 16 payment, 8 change
        let input_proofs = core_tests::generate_random_ecash_proofs(
            &mint_keyset,
            &[Amount::from(8u64), Amount::from(16u64)],
        );

        let send_amount = Amount::from(16u64);
        let expected_change_amount = Amount::from(8u64);

        let substitute_keypair = secp256k1::Keypair::new_global(&mut secp256k1::rand::thread_rng());
        let substitute_clowder_id = secp256k1::PublicKey::from_keypair(&substitute_keypair);
        let mdb = MockMintMeltRepository::new();
        let mut pdb = MockPocketRepository::new();
        let mut substitute_client = MockClowderMintConnector::new();

        substitute_client
            .expect_post_swap_commitment()
            .times(1)
            .returning(|_, _, _, _, _| Ok(mock_commitment_result()));

        let signing_keyset = mint_keyset.clone();
        substitute_client
            .expect_post_swap_committed()
            .times(1)
            .returning(move |_inputs, outputs, _commitment| {
                let amounts = outputs
                    .iter()
                    .map(|output| output.amount)
                    .collect::<Vec<_>>();

                Ok(core_tests::generate_ecash_signatures(
                    &signing_keyset,
                    &amounts,
                ))
            });

        // collect stored changed proofs
        let stored_foreign_proofs = Arc::new(Mutex::new(Vec::<ForeignMintProof>::new()));
        let stored_foreign_proofs_clone = stored_foreign_proofs.clone();
        let expected_clowder_id = substitute_clowder_id;

        pdb.expect_store_foreign_mint_proof()
            .times(1)
            .returning(move |foreign_proof| {
                assert_eq!(foreign_proof.clowder_id, expected_clowder_id);
                assert!(matches!(
                    foreign_proof.reason,
                    ForeignMintProofReason::MintOffline
                ));
                assert!(foreign_proof.proof.witness.is_none());

                let y = foreign_proof
                    .proof
                    .y()
                    .expect("stored change proof has valid y");

                stored_foreign_proofs_clone
                    .lock()
                    .expect("stored proof mutex")
                    .push(foreign_proof);

                Ok(y)
            });

        let swap_config = test_swap_config();
        let mut beta_connector = MockClowderMintConnector::new();
        setup_attestation_mock(&mut beta_connector);
        let beta_provider = RandomBetaProvider::new(
            vec![Arc::new(beta_connector) as Arc<dyn crate::ClowderMintConnector>],
            swap_config.alpha_pk,
        )
        .expect("can create beta provider");

        let pocket = pocket(Arc::new(pdb), Arc::new(mdb));
        let payment_proofs = pocket
            .swap_to_unlocked_substitute_proofs(
                input_proofs,
                &keysets_info,
                keysets,
                Arc::new(substitute_client),
                substitute_clowder_id,
                beta_provider,
                send_amount,
                swap_config,
            )
            .await
            .expect("swap to unlocked substitute proofs works");

        assert_eq!(payment_proofs.total_amount().unwrap(), send_amount);
        assert!(
            payment_proofs
                .iter()
                .all(|proof| { proof.witness.is_none() && proof.p2pk_e.is_none() })
        );

        let stored_foreign_proofs = stored_foreign_proofs.lock().expect("stored proof mutex");
        assert_eq!(stored_foreign_proofs.len(), 1);
        assert_eq!(
            stored_foreign_proofs
                .iter()
                .map(|entry| entry.proof.clone())
                .collect::<Vec<_>>()
                .total_amount()
                .unwrap(),
            expected_change_amount
        );

        // payment_proofs + stored_foreign_proofs = total amount
        assert_eq!(
            payment_proofs.total_amount().unwrap()
                + stored_foreign_proofs
                    .iter()
                    .map(|fmp| fmp.proof.clone())
                    .collect::<Vec<_>>()
                    .total_amount()
                    .unwrap(),
            Amount::from(24u64)
        );
    }
}
