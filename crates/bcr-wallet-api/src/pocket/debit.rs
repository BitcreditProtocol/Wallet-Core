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
use bcr_wallet_core::types::{MeltSummary, MintSummary, Seed, SendSummary};
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
        keysets_info: &[KeySetInfo],
        client: Arc<dyn ClowderMintConnector>,
        swap_config: SwapConfig,
    ) -> Result<Amount>;
    /// Attempt to recover proofs, which are pending, but not part of
    /// a pending transaction
    async fn recover_pending_stale_proofs(
        &self,
        pending_txs_ys: &[cashu::PublicKey],
        keysets_info: &[KeySetInfo],
        client: Arc<dyn ClowderMintConnector>,
        swap_config: SwapConfig,
    ) -> Result<Amount>;
    async fn prepare_onchain_melt(
        &self,
        address: String,
        amount: u64,
        keysets_info: &[KeySetInfo],
        client: Arc<dyn ClowderMintConnector>,
        swap_config: SwapConfig,
    ) -> Result<MeltSummary>;
    async fn pay_onchain_melt(
        &self,
        rid: Uuid,
        client: Arc<dyn ClowderMintConnector>,
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
    async fn protest_mint(
        &self,
        qid: Uuid,
        keysets_info: &[KeySetInfo],
        client: Arc<dyn ClowderMintConnector>,
        swap_config: SwapConfig,
        clowder_id: bitcoin::secp256k1::PublicKey,
    ) -> Result<ProtestResult>;
    async fn check_pending_commitments(&self, tstamp: u64) -> Result<()>;
    async fn protest_swap(
        &self,
        commitment_sig: bitcoin::secp256k1::schnorr::Signature,
        keysets_info: &[KeySetInfo],
        alpha_client: Arc<dyn ClowderMintConnector>,
        beta_client: Arc<dyn ClowderMintConnector>,
        alpha_id: bitcoin::secp256k1::PublicKey,
        swap_config: SwapConfig,
    ) -> Result<ProtestResult>;
    async fn protest_melt(
        &self,
        quote_id: Uuid,
        beta_client: Arc<dyn ClowderMintConnector>,
        alpha_id: bitcoin::secp256k1::PublicKey,
    ) -> Result<MeltProtestResult>;
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
    pub txid: Option<wire_melt::MeltTx>,
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

    current_send: Mutex<Option<SendReference>>,
    current_melt: Mutex<Option<MeltReference>>,
}

impl Pocket {
    pub fn new(
        unit: CurrencyUnit,
        pdb: Arc<dyn PocketRepository>,
        mdb: Arc<dyn MintMeltRepository>,
        seed: Seed,
    ) -> Self {
        Self {
            unit,
            pdb,
            mdb,
            seed,
            current_send: Mutex::new(None),
            current_melt: Mutex::new(None),
        }
    }

    fn validate_keysets<'inf>(
        &self,
        keysets_info: &'inf [KeySetInfo],
        inputs: &[cdk00::Proof],
    ) -> Result<HashMap<cashu::Id, &'inf KeySetInfo>> {
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

    fn find_debit_keysetid(&self, keysets_info: &[KeySetInfo]) -> Result<cashu::KeySetInfo> {
        let active_info = keysets_info
            .iter()
            .find(|info| info.unit == self.unit && info.active && info.final_expiry.is_none());
        let Some(active_info) = active_info else {
            return Err(Error::NoActiveKeyset);
        };
        Ok(active_info.clone())
    }

    async fn digest_proofs(
        &self,
        client: Arc<dyn ClowderMintConnector>,
        keysets_info: &[KeySetInfo],
        inputs: HashMap<cdk01::PublicKey, cdk00::Proof>,
        swap_config: SwapConfig,
    ) -> Result<(Amount, Vec<cdk01::PublicKey>)> {
        if inputs.is_empty() {
            tracing::warn!("DbPocket::digest_proofs: empty inputs");
            return Ok((Amount::ZERO, Vec::new()));
        }
        // prepare data
        let kinfos: HashMap<cashu::Id, KeySetInfo> =
            keysets_info.iter().cloned().map(|k| (k.id, k)).collect();
        let (ys, swap_proofs): (Vec<_>, Vec<_>) = inputs.into_iter().unzip();

        // create swap plan
        let swap_plan = prepare_swap(&swap_proofs, &kinfos)?;
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
        )
        .await?;
        Ok((cashed_in, ys))
    }

    async fn compute_send_costs(
        &self,
        target_amount: Amount,
        keysets_info: &[KeySetInfo],
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
            PaymentPlan::NeedSplit {
                proof,
                target,
                estimated_fee,
            } => {
                let mut pocket_summary = SendSummary::new();
                pocket_summary.amount = target_amount;
                pocket_summary.unit = self.unit.clone();
                pocket_summary.swap_fees = estimated_fee;
                let SplitTarget::Value(split_amount) = target else {
                    return Err(Error::InvalidSplitTarget);
                };
                let send_ref = SendReference {
                    rid: pocket_summary.request_id,
                    target_amount,
                    plan: SendPlan::NeedSplit {
                        proof: proof.y()?,
                        split_amount,
                        estimated_fee,
                    },
                };
                (pocket_summary, send_ref)
            }
        };

        Ok((pocket_summary, send_ref))
    }

    /// Construct proofs from blind signatures, swap them into the wallet, and return the result.
    async fn finalize_mint_proofs(
        &self,
        signatures: Vec<cdk00::BlindSignature>,
        premint: &cdk00::PreMintSecrets,
        keysets_info: &[KeySetInfo],
        client: Arc<dyn ClowderMintConnector>,
        swap_config: SwapConfig,
    ) -> Result<(cashu::Amount, Vec<cashu::PublicKey>)> {
        let active_keyset = client.get_mint_keyset(premint.keyset_id).await?;

        let inputs = cashu::dhke::construct_proofs(
            signatures,
            premint.rs(),
            premint.secrets(),
            &active_keyset.keys,
        )?;

        let mut proofs: HashMap<cdk01::PublicKey, cdk00::Proof> =
            HashMap::with_capacity(inputs.len());
        for input in inputs.into_iter() {
            let y = input.y()?;
            proofs.insert(y, input);
        }

        self.digest_proofs(client, keysets_info, proofs, swap_config)
            .await
    }

    async fn check_pending_mint(
        &self,
        qid: Uuid,
        keysets_info: &[KeySetInfo],
        client: Arc<dyn ClowderMintConnector>,
        _tstamp: u64,
        swap_config: SwapConfig,
        clowder_id: bitcoin::secp256k1::PublicKey,
    ) -> Result<Option<(cashu::Amount, Vec<cashu::PublicKey>)>> {
        let record = self.mdb.load_mint(qid).await?;
        let (mint_summary, premint) = (record.summary, record.premint);

        tracing::info!("Mint {qid} - attempting to mint..");
        let mint_req = wire_mint::OnchainMintRequest {
            quote: mint_summary.quote_id,
            alpha_id: clowder_id,
        };
        match client.post_mint_onchain(mint_req).await {
            Ok(mint_response) => {
                let (amount, ys) = self
                    .finalize_mint_proofs(
                        mint_response.signatures,
                        &premint,
                        keysets_info,
                        client,
                        swap_config,
                    )
                    .await?;

                self.mdb.delete_mint(qid).await?;

                tracing::info!("Minted {qid} successfully for {amount}");
                Ok(Some((amount, ys)))
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

    async fn balance(&self, keysets_info: &[KeySetInfo]) -> Result<PocketBalance> {
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
        keysets_info: &[KeySetInfo],
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
        keysets_info: &[KeySetInfo],
    ) -> Result<SendSummary> {
        let (summary, send_ref) = self.compute_send_costs(target, keysets_info).await?;
        *self.current_send.lock().unwrap() = Some(send_ref);
        Ok(summary)
    }

    async fn send_proofs(
        &self,
        rid: Uuid,
        keysets_info: &[KeySetInfo],
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
        )
        .await?;

        Ok(sending_proofs)
    }

    async fn cleanup_local_proofs(
        &self,
        client: Arc<dyn ClowderMintConnector>,
    ) -> Result<Vec<cdk01::PublicKey>> {
        let cleaned_ys = cleanup_local_proofs(self.pdb.as_ref(), client).await?;
        Ok(cleaned_ys)
    }

    async fn restore_local_proofs(
        &self,
        keysets_info: &[KeySetInfo],
        client: Arc<dyn ClowderMintConnector>,
    ) -> Result<usize> {
        let kids = keysets_info.iter().filter_map(|info| {
            if info.unit == self.unit {
                Some(info.id)
            } else {
                None
            }
        });
        let mut total_recovered = 0;
        for kid in kids.into_iter() {
            total_recovered +=
                restore::restore_keysetid(&self.seed, kid, &client, self.pdb.as_ref()).await?;
        }
        Ok(total_recovered)
    }

    async fn delete_proofs(&self) -> Result<HashMap<cashu::Id, Vec<cdk00::Proof>>> {
        let proofs = self.pdb.list_all().await?;

        let mut proofs_by_keyset = HashMap::<cashu::Id, Vec<cdk00::Proof>>::new();

        for y in proofs.iter() {
            if let Some(proof) = self.pdb.delete_proof(*y).await? {
                proofs_by_keyset
                    .entry(proof.keyset_id)
                    .or_default()
                    .push(proof);
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
        keysets_info: &[KeySetInfo],
        client: Arc<dyn ClowderMintConnector>,
        send_amount: Amount,
        swap_config: SwapConfig,
    ) -> Result<Vec<cashu::Proof>> {
        let mut swapped_proofs = Vec::new();
        let total_amount = proofs.total_amount()?;
        let change_amount = total_amount - send_amount;
        tracing::debug!("Swapping to unlocked substitute proofs - {change_amount} will be lost.");
        // handle keyset
        let active_info = keysets_info
            .iter()
            .find(|info| info.unit == self.unit && info.active);
        let Some(active_info) = active_info else {
            return Err(Error::NoActiveKeyset);
        };
        let active_keyset = client.get_mint_keyset(active_info.id).await?;
        // calculate splits
        let send_splits = send_amount.split();
        let send_splits_len = send_splits.len();
        let change_splits = change_amount.split();
        let mut splits: Vec<Amount> = Vec::with_capacity(send_splits.len() + change_splits.len());
        splits.extend(send_splits);
        splits.extend(change_splits);
        // TODO: How to add Fees here?
        // no counter etc., since we're not persisting them anyway
        let premint_secrets = cashu::PreMintSecrets::random(
            active_info.id,
            total_amount,
            &SplitTarget::Values(splits),
        )?;
        let mut premints = HashMap::from([(active_info.id, premint_secrets)]);
        let keysets = HashMap::from([(active_info.id, active_keyset)]);
        let blinds: Vec<cdk00::BlindedMessage> = premints
            .values()
            .flat_map(|premint| premint.blinded_messages())
            .collect();

        let all_signatures = super::committed_swap(
            client.as_ref(),
            None,
            proofs,
            blinds,
            &swap_config,
            HashMap::new(),
        )
        .await?;

        // We only take the send_splits signatures, they add up to our send_amount
        let mut send_signatures = all_signatures;
        send_signatures.truncate(send_splits_len);

        let mut signatures: HashMap<cashu::Id, Vec<cdk00::BlindSignature>> = HashMap::new();
        for signature in send_signatures {
            signatures
                .entry(signature.keyset_id)
                .and_modify(|v| v.push(signature.clone()))
                .or_insert_with(|| vec![signature]);
        }

        let mut current_amount = Amount::ZERO;
        // only collect sending proofs, so we can take all - change is discarded for now
        for (kid, signatures) in signatures.into_iter() {
            let premint = premints.remove(&kid).expect("premint should be here");
            let keyset = keysets.get(&kid).expect("keyset should be here");
            let unblinded_proofs = unblind_proofs(keyset, signatures, premint);
            for proof in unblinded_proofs.into_iter() {
                current_amount += proof.amount;
                swapped_proofs.push(proof);
            }
        }

        if current_amount != send_amount {
            tracing::warn!(
                "Mismatch between target {send_amount} and amount from proofs {current_amount}"
            );
        }

        Ok(swapped_proofs)
    }

    async fn dev_mode_detailed_balance(
        &self,
        keysets_info: &[KeySetInfo],
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
}

#[async_trait]
impl DebitPocketApi for Pocket {
    async fn reclaim_proofs(
        &self,
        ys: &[cdk01::PublicKey],
        keysets_info: &[KeySetInfo],
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
        keysets_info: &[KeySetInfo],
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

    async fn prepare_onchain_melt(
        &self,
        address: String,
        amount: u64,
        keysets_info: &[KeySetInfo],
        client: Arc<dyn ClowderMintConnector>,
        swap_config: SwapConfig,
    ) -> Result<MeltSummary> {
        let parsed_address: bitcoin::Address<bitcoin::address::NetworkUnchecked> = address
            .parse()
            .map_err(|e| Error::MintingError(format!("invalid address: {e}")))?;
        let btc_amount = bitcoin::Amount::from_sat(amount);

        let (_, send_ref) = self
            .compute_send_costs(Amount::from(amount), keysets_info)
            .await?;

        let sending_proofs = send_proofs(
            send_ref.plan,
            keysets_info,
            send_ref.target_amount,
            &self.seed,
            self.pdb.as_ref(),
            &client,
            swap_config.clone(),
        )
        .await?;
        let sent_ys: Vec<cdk01::PublicKey> = sending_proofs.keys().cloned().collect();

        let quote_and_record = async {
            let proofs: Vec<cashu::Proof> = sending_proofs.values().cloned().collect();
            let quote_result = client
                .post_melt_quote_onchain(proofs, parsed_address, btc_amount, swap_config.alpha_pk)
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
            Ok::<_, Error>((quote_id, expiry))
        }
        .await;

        let (quote_id, expiry) = match quote_and_record {
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
    ) -> Result<(wire_melt::MeltTx, HashMap<cdk01::PublicKey, cdk00::Proof>)> {
        let melt_ref = self.current_melt.lock().unwrap().take();
        let melt_ref = melt_ref.ok_or(Error::NoPrepareRef(rid))?;
        if melt_ref.rid != rid {
            return Err(Error::NoPrepareRef(rid));
        }

        let record = self.mdb.load_melt_commitment(melt_ref.quote_id).await?;
        let body: wire_melt::MeltQuoteOnchainResponseBody =
            bcr_common::core::signature::deserialize_borsh_msg(&record.body_content)?;
        let input_ys: Vec<cashu::PublicKey> = body.inputs.iter().map(|fp| fp.y).collect();
        let sending_proofs = self.pdb.load_proofs(&input_ys).await?;

        let request = wire_melt::MeltOnchainRequest {
            quote: melt_ref.quote_id,
            inputs: sending_proofs.values().cloned().collect(),
        };
        let response = client.post_melt_onchain(request).await?;

        self.mdb.delete_melt_commitment(melt_ref.quote_id).await?;
        Ok((response.txid, sending_proofs))
    }

    async fn mint_onchain(
        &self,
        amount: bitcoin::Amount,
        keysets_info: &[KeySetInfo],
        client: Arc<dyn ClowderMintConnector>,
        clowder_id: bitcoin::secp256k1::PublicKey,
    ) -> Result<MintSummary> {
        let active_info = self.find_debit_keysetid(keysets_info)?;
        let kid = active_info.id;
        let counter = self.pdb.counter(kid).await?;
        let premint = cdk00::PreMintSecrets::from_seed(
            kid,
            counter,
            &self.seed,
            cashu::Amount::from(amount.to_sat()),
            &SplitTarget::None,
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
            &clowder_id.x_only_public_key().0,
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
        keysets_info: &[KeySetInfo],
        client: Arc<dyn ClowderMintConnector>,
        tstamp: u64,
        swap_config: SwapConfig,
        clowder_id: bitcoin::secp256k1::PublicKey,
    ) -> Result<HashMap<Uuid, (cashu::Amount, Vec<cashu::PublicKey>)>> {
        let mint_ids = self.mdb.list_mints().await?;
        let mut res = HashMap::with_capacity(mint_ids.len());

        tracing::debug!("check pending mints for {} mints", mint_ids.len());
        for qid in mint_ids {
            match self
                .check_pending_mint(
                    qid,
                    keysets_info,
                    client.clone(),
                    tstamp,
                    swap_config.clone(),
                    clowder_id,
                )
                .await
            {
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
        keysets_info: &[KeySetInfo],
        client: Arc<dyn ClowderMintConnector>,
        swap_config: SwapConfig,
        clowder_id: bitcoin::secp256k1::PublicKey,
    ) -> Result<ProtestResult> {
        let record = self.mdb.load_mint(qid).await?;

        let ephemeral_keypair =
            secp256k1::Keypair::from_secret_key(secp256k1::SECP256K1, &record.ephemeral_secret);
        let wallet_signature = super::sign_content_b64(&record.content, &ephemeral_keypair)?;

        let request = wire_mint::MintProtestRequest {
            alpha_id: clowder_id,
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
                    .finalize_mint_proofs(
                        signatures,
                        &record.premint,
                        keysets_info,
                        client,
                        swap_config,
                    )
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
        }
    }

    async fn protest_swap(
        &self,
        commitment_sig: bitcoin::secp256k1::schnorr::Signature,
        keysets_info: &[KeySetInfo],
        alpha_client: Arc<dyn ClowderMintConnector>,
        beta_client: Arc<dyn ClowderMintConnector>,
        alpha_id: bitcoin::secp256k1::PublicKey,
        swap_config: SwapConfig,
    ) -> Result<ProtestResult> {
        let record = self.pdb.load_commitment(commitment_sig).await?;
        let loaded_proofs = self.pdb.load_proofs(&record.inputs).await?;
        let ephemeral_keypair =
            secp256k1::Keypair::from_secret_key(secp256k1::SECP256K1, &record.ephemeral_secret);
        let wallet_signature = super::sign_content_b64(&record.body_content, &ephemeral_keypair)?;

        let request = wire_swap::SwapProtestRequest {
            alpha_id,
            proofs: loaded_proofs.into_values().collect(),
            content: record.body_content,
            commitment: record.commitment,
            wallet_signature,
            blind_signatures: None,
        };

        let response = beta_client.post_protest_swap(request).await?;

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
                        .and_modify(|v| v.push(signature.clone()))
                        .or_insert_with(|| vec![signature]);
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
        }
    }

    async fn protest_melt(
        &self,
        quote_id: Uuid,
        beta_client: Arc<dyn ClowderMintConnector>,
        alpha_id: bitcoin::secp256k1::PublicKey,
    ) -> Result<MeltProtestResult> {
        let record = self.mdb.load_melt_commitment(quote_id).await?;
        let ephemeral_keypair =
            secp256k1::Keypair::from_secret_key(secp256k1::SECP256K1, &record.ephemeral_secret);
        let wallet_signature = super::sign_content_b64(&record.body_content, &ephemeral_keypair)?;

        let request = wire_melt::MeltProtestRequest {
            alpha_id,
            quote_id,
            content: record.body_content.clone(),
            commitment: record.commitment,
            wallet_signature,
        };

        let response = beta_client.post_protest_melt(request).await?;

        match response.status {
            wire_common::ProtestStatus::Resolved => {
                let body: wire_melt::MeltQuoteOnchainResponseBody =
                    bcr_common::core::signature::deserialize_borsh_msg(&record.body_content)?;
                let ys: Vec<cashu::PublicKey> = body.inputs.iter().map(|fp| fp.y).collect();
                self.mdb.delete_melt_commitment(quote_id).await?;
                tracing::info!("Melt protest resolved for {quote_id}");
                Ok(MeltProtestResult {
                    base: ProtestResult {
                        status: wire_common::ProtestStatus::Resolved,
                        result: Some((body.total, ys)),
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
        external::test_utils::tests::MockMintConnector,
        pocket::{PocketApi, debit::DebitPocketApi},
    };
    use bcr_common::{core_tests, wire::mint::MintResponse};
    use bcr_wallet_persistence::{MockMintMeltRepository, MockPocketRepository};
    use mockall::predicate::*;

    use crate::pocket::test_utils::tests::{setup_commitment_mocks, test_swap_config};

    fn pocket(pdb: Arc<dyn PocketRepository>, mdb: Arc<dyn MintMeltRepository>) -> super::Pocket {
        let unit = CurrencyUnit::Sat;
        let mnemonic = bip39::Mnemonic::generate(12).unwrap();
        let seed = mnemonic.to_seed("");
        super::Pocket::new(unit, pdb, mdb, seed)
    }

    #[tokio::test]
    async fn debit_balance() {
        let (info, keyset) = core_tests::generate_random_ecash_keyset();
        let k_infos = vec![KeySetInfo::from(info)];
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

        let k_infos = vec![k_info];
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

        let k_infos = vec![k_info];
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

        let k_infos = vec![ks_debit, ks_credit];

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
        let k_infos = vec![KeySetInfo::from(info)];
        let amounts = [Amount::from(8u64), Amount::from(16u64)];
        let proofs = core_tests::generate_random_ecash_proofs(&keyset, &amounts);

        let mdb = MockMintMeltRepository::new();
        let mut pdb = MockPocketRepository::new();
        let mut connector = MockMintConnector::new();
        let cloned_keyset = keyset.clone();
        connector
            .expect_get_mint_keyset()
            .times(1)
            .with(eq(kid))
            .returning(move |_| Ok(KeySet::from(cloned_keyset.clone())));
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
            .returning(move |request| {
                let amounts = request.outputs.iter().map(|b| b.amount).collect::<Vec<_>>();
                let signatures = core_tests::generate_ecash_signatures(&keyset, &amounts);
                Ok(bcr_common::wire::swap::SwapResponse { signatures })
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
        let k_infos = vec![KeySetInfo::from(info)];
        let amounts = [Amount::from(8u64), Amount::from(16u64)];
        let proofs = core_tests::generate_random_ecash_proofs(&keyset, &amounts);

        let ys: Vec<cdk01::PublicKey> = proofs.iter().map(|p| p.y().expect("valid y")).collect();

        let mdb = MockMintMeltRepository::new();
        let mut pdb = MockPocketRepository::new();
        let mut connector = MockMintConnector::new();
        let cloned_keyset = keyset.clone();

        connector
            .expect_get_mint_keyset()
            .times(1)
            .with(eq(kid))
            .returning(move |_| Ok(KeySet::from(cloned_keyset.clone())));
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
            .returning(move |request| {
                let amounts = request.outputs.iter().map(|b| b.amount).collect::<Vec<_>>();
                let signatures = core_tests::generate_ecash_signatures(&keyset, &amounts);
                Ok(bcr_common::wire::swap::SwapResponse { signatures })
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
        let k_infos = vec![KeySetInfo::from(info)];
        let amounts = [Amount::from(8u64), Amount::from(16u64)];
        let proofs = core_tests::generate_random_ecash_proofs(&keyset, &amounts);

        // we pretend that the second proof belongs to a pending transaction we don't want to recover
        let pending_tx_y = proofs[1].clone().y().unwrap();

        let mdb = MockMintMeltRepository::new();
        let mut pdb = MockPocketRepository::new();
        let mut connector = MockMintConnector::new();
        let cloned_keyset = keyset.clone();

        connector
            .expect_get_mint_keyset()
            .times(1)
            .with(eq(kid))
            .returning(move |_| Ok(KeySet::from(cloned_keyset.clone())));
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
            .returning(move |request| {
                let amounts = request.outputs.iter().map(|b| b.amount).collect::<Vec<_>>();
                let signatures = core_tests::generate_ecash_signatures(&keyset, &amounts);
                Ok(bcr_common::wire::swap::SwapResponse { signatures })
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
        let melt_tx = wire_melt::MeltTx {
            alpha_txid: Some(tx_id),
            beta_txid: None,
        };

        let mut mdb = MockMintMeltRepository::new();
        let mut pdb = MockPocketRepository::new();
        let mut connector = MockMintConnector::new();

        // Mock load_melt_commitment
        let ephemeral = secp256k1::Keypair::new_global(&mut secp256k1::rand::thread_rng());
        let commitment_sig = cashu::SecretKey::generate().sign(&[0u8; 32]).unwrap();
        let wallet_key = cashu::PublicKey::from(secp256k1::PublicKey::from_keypair(&ephemeral));
        let body = wire_melt::MeltQuoteOnchainResponseBody {
            quote: quote_id,
            inputs: vec![],
            address: bitcoin::Address::from_str("tb1qteyk7pfvvql2r2zrsu4h4xpvju0nz7ykvguyk0")
                .expect("valid address"),
            amount: bitcoin::Amount::from_sat(100),
            total: cashu::Amount::from(100u64),
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
            .returning(move |_| {
                Ok(wire_melt::MeltOnchainResponse {
                    txid: melt_tx.clone(),
                })
            });

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
        assert_eq!(res.0.alpha_txid, Some(tx_id));
    }

    fn mock_melt_commitment_body(quote_id: Uuid, amount: u64) -> String {
        let ephemeral = secp256k1::Keypair::new_global(&mut secp256k1::rand::thread_rng());
        let wallet_key = cashu::PublicKey::from(secp256k1::PublicKey::from_keypair(&ephemeral));
        let body = wire_melt::MeltQuoteOnchainResponseBody {
            quote: quote_id,
            inputs: vec![],
            address: bitcoin::Address::from_str("tb1qteyk7pfvvql2r2zrsu4h4xpvju0nz7ykvguyk0")
                .expect("valid address"),
            amount: bitcoin::Amount::from_sat(amount),
            total: cashu::Amount::from(amount),
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
        let melt_tx = wire_melt::MeltTx {
            alpha_txid: Some(tx_id),
            beta_txid: None,
        };

        let mut mdb = MockMintMeltRepository::new();
        let pdb = MockPocketRepository::new();
        let mut connector = MockMintConnector::new();

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

        connector
            .expect_post_protest_melt()
            .times(1)
            .returning(move |_| {
                Ok(wire_melt::MeltProtestResponse {
                    status: wire_common::ProtestStatus::Resolved,
                    txid: Some(melt_tx.clone()),
                })
            });

        mdb.expect_delete_melt_commitment()
            .times(1)
            .returning(|_| Ok(()));

        let alpha_id = bitcoin::secp256k1::PublicKey::from_keypair(
            &bitcoin::secp256k1::Keypair::new_global(&mut secp256k1::rand::thread_rng()),
        );
        let pocket = pocket(Arc::new(pdb), Arc::new(mdb));
        let result = pocket
            .protest_melt(quote_id, Arc::new(connector), alpha_id)
            .await
            .expect("protest_melt resolved works");

        assert!(matches!(
            result.base.status,
            wire_common::ProtestStatus::Resolved
        ));
        assert_eq!(result.txid.and_then(|t| t.alpha_txid), Some(tx_id));
        assert!(result.base.result.is_some());
    }

    #[tokio::test]
    async fn protest_melt_rabid() {
        let quote_id = Uuid::new_v4();

        let mut mdb = MockMintMeltRepository::new();
        let pdb = MockPocketRepository::new();
        let mut connector = MockMintConnector::new();

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

        connector
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
        let pocket = pocket(Arc::new(pdb), Arc::new(mdb));
        let result = pocket
            .protest_melt(quote_id, Arc::new(connector), alpha_id)
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
        let (info, _keyset) = core_tests::generate_random_ecash_keyset();
        let kid = info.id;
        let k_infos = vec![KeySetInfo::from(info)];
        let amount = bitcoin::Amount::from_sat(24);

        let mut mdb = MockMintMeltRepository::new();
        let mut pdb = MockPocketRepository::new();
        let mut connector = MockMintConnector::new();

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
        let clowder_pk = bitcoin::secp256k1::PublicKey::from_keypair(&clowder_keypair);

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

        let pocket = pocket(Arc::new(pdb), Arc::new(mdb));

        let summary = pocket
            .mint_onchain(amount, &k_infos, Arc::new(connector), clowder_pk)
            .await
            .expect("mint onchain works");
        assert_eq!(summary.amount, amount);
    }

    #[tokio::test]
    async fn check_pending_mints() {
        let uuid = Uuid::new_v4();
        let amount = bitcoin::Amount::from_sat(24);
        let (info, keyset) = core_tests::generate_random_ecash_keyset();
        let k_infos = vec![KeySetInfo::from(info)];

        let mut mdb = MockMintMeltRepository::new();
        let pdb = MockPocketRepository::new();
        let mut connector = MockMintConnector::new();

        mdb.expect_list_mints()
            .times(1)
            .returning(move || Ok(vec![uuid]));

        let keyset_clone = keyset.clone();
        let dummy_secret = secp256k1::SecretKey::from_slice(&[1u8; 32]).unwrap();
        mdb.expect_load_mint().times(1).returning(move |_| {
            let premint = cdk00::PreMintSecrets::random(
                cashu::KeySet::from(keyset_clone.clone()).id,
                Amount::from(amount.to_sat()),
                &SplitTarget::None,
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
            .returning(move |_| Ok(KeySet::from(keyset_clone.clone())));

        connector
            .expect_post_mint_onchain()
            .times(1)
            .returning(move |_| Ok(MintResponse { signatures: vec![] }));

        let clowder_keypair = {
            let secret_bytes: [u8; 32] = rand::random();
            bitcoin::secp256k1::Keypair::from_seckey_slice(
                bitcoin::secp256k1::SECP256K1,
                &secret_bytes,
            )
            .unwrap()
        };

        let pocket = pocket(Arc::new(pdb), Arc::new(mdb));

        let clowder_id = bitcoin::secp256k1::PublicKey::from_keypair(&clowder_keypair);
        let res = pocket
            .check_pending_mints(
                &k_infos,
                Arc::new(connector),
                chrono::Utc::now().timestamp() as u64,
                test_swap_config(),
                clowder_id,
            )
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
        let k_infos = vec![KeySetInfo::from(info)];
        let premint =
            cdk00::PreMintSecrets::random(kid, Amount::from(amount.to_sat()), &SplitTarget::None)
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
        let mut connector = MockMintConnector::new();

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
            .times(2)
            .with(eq(kid))
            .returning(move |_| Ok(KeySet::from(keyset_clone.clone())));

        pdb.expect_counter()
            .times(1)
            .with(eq(kid))
            .returning(|_| Ok(0));
        pdb.expect_increment_counter()
            .times(1)
            .returning(|_, _, _| Ok(()));

        setup_commitment_mocks(&mut connector, &mut pdb);
        let swap_keyset = mintkeyset.clone();
        connector
            .expect_post_swap_committed()
            .times(1)
            .returning(move |request| {
                let amounts: Vec<_> = request.outputs.iter().map(|b| b.amount).collect();
                let signatures = core_tests::generate_ecash_signatures(&swap_keyset, &amounts);
                Ok(bcr_common::wire::swap::SwapResponse { signatures })
            });

        pdb.expect_store_new().returning(|p| {
            let y = p.y().expect("Hash to curve should not fail");
            Ok(y)
        });

        mdb.expect_delete_mint().times(1).returning(move |_| Ok(()));

        let clowder_keypair = {
            let secret_bytes: [u8; 32] = rand::random();
            bitcoin::secp256k1::Keypair::from_seckey_slice(
                bitcoin::secp256k1::SECP256K1,
                &secret_bytes,
            )
            .unwrap()
        };
        let clowder_id = bitcoin::secp256k1::PublicKey::from_keypair(&clowder_keypair);

        let pocket = pocket(Arc::new(pdb), Arc::new(mdb));
        let ProtestResult { status, result } = pocket
            .protest_mint(
                uuid,
                &k_infos,
                Arc::new(connector),
                test_swap_config(),
                clowder_id,
            )
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
        let (info, _mintkeyset) = core_tests::generate_random_ecash_keyset();
        let kid = info.id;
        let k_infos = vec![KeySetInfo::from(info)];

        let premint =
            cdk00::PreMintSecrets::random(kid, Amount::from(amount.to_sat()), &SplitTarget::None)
                .unwrap();

        let mut mdb = MockMintMeltRepository::new();
        let pdb = MockPocketRepository::new();
        let mut connector = MockMintConnector::new();

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

        let clowder_keypair = {
            let secret_bytes: [u8; 32] = rand::random();
            bitcoin::secp256k1::Keypair::from_seckey_slice(
                bitcoin::secp256k1::SECP256K1,
                &secret_bytes,
            )
            .unwrap()
        };
        let clowder_id = bitcoin::secp256k1::PublicKey::from_keypair(&clowder_keypair);

        let pocket = pocket(Arc::new(pdb), Arc::new(mdb));
        let ProtestResult { status, result } = pocket
            .protest_mint(
                uuid,
                &k_infos,
                Arc::new(connector),
                test_swap_config(),
                clowder_id,
            )
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
        let k_infos = vec![KeySetInfo::from(info)];

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
        let premint = cdk00::PreMintSecrets::random(kid, amount, &SplitTarget::None).unwrap();
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
        let mut beta_connector = MockMintConnector::new();
        let mut alpha_connector = MockMintConnector::new();

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

        // Alpha handles keyset lookup (for unblinding + digest_proofs)
        let keyset_clone = mintkeyset.clone();
        alpha_connector
            .expect_get_mint_keyset()
            .returning(move |_| Ok(KeySet::from(keyset_clone.clone())));

        // Mocks for digest_proofs swap (runs against alpha)
        pdb.expect_counter().with(eq(kid)).returning(|_| Ok(0));
        pdb.expect_increment_counter().returning(|_, _, _| Ok(()));
        setup_commitment_mocks(&mut alpha_connector, &mut pdb);
        let swap_keyset = mintkeyset.clone();
        alpha_connector
            .expect_post_swap_committed()
            .times(1)
            .returning(move |request| {
                let amounts: Vec<_> = request.outputs.iter().map(|b| b.amount).collect();
                let signatures = core_tests::generate_ecash_signatures(&swap_keyset, &amounts);
                Ok(bcr_common::wire::swap::SwapResponse { signatures })
            });

        pdb.expect_store_new().returning(|p| {
            let y = p.y().expect("Hash to curve should not fail");
            Ok(y)
        });

        pdb.expect_delete_commitment()
            .times(1)
            .returning(move |_| Ok(()));

        let clowder_keypair = {
            let secret_bytes: [u8; 32] = rand::random();
            secp256k1::Keypair::from_seckey_slice(secp256k1::SECP256K1, &secret_bytes).unwrap()
        };
        let clowder_id = secp256k1::PublicKey::from_keypair(&clowder_keypair);

        let pocket = pocket(Arc::new(pdb), Arc::new(mdb));
        let ProtestResult { status, result } = pocket
            .protest_swap(
                commitment_sig,
                &k_infos,
                Arc::new(alpha_connector),
                Arc::new(beta_connector),
                clowder_id,
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
        let k_infos = vec![KeySetInfo::from(info)];

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
        let mut beta_connector = MockMintConnector::new();
        let alpha_connector = MockMintConnector::new();

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
            .returning(move |_| {
                Ok(wire_swap::SwapProtestResponse {
                    status: wire_common::ProtestStatus::Rabid,
                    signatures: None,
                })
            });

        let clowder_keypair = {
            let secret_bytes: [u8; 32] = rand::random();
            secp256k1::Keypair::from_seckey_slice(secp256k1::SECP256K1, &secret_bytes).unwrap()
        };
        let clowder_id = secp256k1::PublicKey::from_keypair(&clowder_keypair);

        let pocket = pocket(Arc::new(pdb), Arc::new(mdb));
        let ProtestResult { status, result } = pocket
            .protest_swap(
                commitment_sig,
                &k_infos,
                Arc::new(alpha_connector),
                Arc::new(beta_connector),
                clowder_id,
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
        let k_infos = vec![KeySetInfo::from(info)];
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
            SendPlan::NeedSplit { .. } => panic!("expected ready send plan"),
        }
    }

    #[tokio::test]
    async fn compute_send_costs_need_split_after_collecting_input() {
        let (info, keyset) = core_tests::generate_random_ecash_keyset();
        let k_infos = vec![KeySetInfo::from(info)];

        // The split candidate (16) is checked against full target (41), but not usable
        // Fall back to gt_p => none
        // Fall back to last() => 32, can satisfy split_target
        // => split proof is 32
        let target = Amount::from(41u64);
        let amounts = [Amount::from(8u64), Amount::from(16u64), Amount::from(32u64)];
        let proofs = core_tests::generate_random_ecash_proofs(&keyset, &amounts);

        let mut pdb = MockPocketRepository::new();
        let mdb = MockMintMeltRepository::new();

        let proofs_clone = proofs.clone();
        let split_proof_y = proofs[2].y().unwrap();

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
            SendPlan::NeedSplit {
                proof,
                split_amount,
                estimated_fee,
            } => {
                assert_eq!(proof, split_proof_y);
                assert_eq!(split_amount, Amount::from(1u64));
                assert_eq!(summary.swap_fees, estimated_fee);
            }
            SendPlan::Ready { .. } => panic!("expected split send plan"),
        }
    }

    #[tokio::test]
    async fn compute_send_costs_need_split_from_gtp_candidate() {
        let (info, keyset) = core_tests::generate_random_ecash_keyset();
        let k_infos = vec![KeySetInfo::from(info)];

        // gt_p points to 64, because it is the first proof > target.
        // => split proof is 64
        let target = Amount::from(40u64);
        let amounts = [Amount::from(8u64), Amount::from(16u64), Amount::from(64u64)];
        let proofs = core_tests::generate_random_ecash_proofs(&keyset, &amounts);

        let mut pdb = MockPocketRepository::new();
        let mdb = MockMintMeltRepository::new();

        let proofs_clone = proofs.clone();
        let split_proof_y = proofs[2].y().unwrap();

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
            SendPlan::NeedSplit {
                proof,
                split_amount,
                estimated_fee,
            } => {
                assert_eq!(proof, split_proof_y);
                assert_eq!(split_amount, Amount::from(16u64));
                assert_eq!(summary.swap_fees, estimated_fee);
            }
            SendPlan::Ready { .. } => panic!("expected split send plan"),
        }
    }

    #[tokio::test]
    async fn compute_send_costs_errors_without_funds() {
        let (_info, _keyset) = core_tests::generate_random_ecash_keyset();
        let k_infos = vec![KeySetInfo::from(_info)];

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
}
