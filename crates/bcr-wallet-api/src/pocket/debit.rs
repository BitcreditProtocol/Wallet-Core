use crate::{
    ClowderMintConnector,
    error::{Error, Result},
    pocket::*,
    wallet::types::SwapConfig,
};
use async_trait::async_trait;
use bcr_common::{
    cashu::{
        self, Amount, CurrencyUnit, Proof, ProofsMethods, amount::SplitTarget, nut00 as cdk00,
        nut01 as cdk01,
    },
    core::swap::wallet::{PaymentPlan, prepare_payment},
    ecash::{self, KeySet, KeySetInfo},
    wire::{common as wire_common, melt as wire_melt, mint as wire_mint, swap as wire_swap},
};
use bcr_wallet_core::types::{
    ForeignMintProof, ForeignMintProofReason, MeltSummary, MintSummary, Seed, SendSummary,
    TransactionFees,
};
use bcr_wallet_persistence::{MeltCommitmentRecord, MintMeltRepository, PocketRepository};
use bitcoin::secp256k1;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
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
        keysets_info: &HashMap<ecash::Id, KeySetInfo>,
        client: Arc<dyn ClowderMintConnector>,
        swap_config: SwapConfig,
    ) -> Result<Amount>;
    /// Attempt to recover proofs, which are pending, but not part of
    /// a pending transaction
    async fn recover_pending_stale_proofs(
        &self,
        pending_txs_ys: &[cashu::PublicKey],
        keysets_info: &HashMap<ecash::Id, KeySetInfo>,
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
        keysets_info: &HashMap<ecash::Id, KeySetInfo>,
        client: Arc<dyn ClowderMintConnector>,
        swap_config: SwapConfig,
    ) -> Result<MeltSummary>;
    async fn pay_onchain_melt(
        &self,
        rid: Uuid,
        client: Arc<dyn ClowderMintConnector>,
    ) -> Result<(
        bitcoin::Txid,
        HashMap<cashu::PublicKey, cashu::Proof>,
        super::Reservation,
    )>;
    async fn mint_onchain(
        &self,
        amount: bitcoin::Amount,
        keysets_info: &HashMap<ecash::Id, KeySetInfo>,
        client: Arc<dyn ClowderMintConnector>,
    ) -> Result<MintSummary>;
    async fn check_pending_mints(
        &self,
        client: Arc<dyn ClowderMintConnector>,
    ) -> Result<BTreeMap<Uuid, CheckPendingMintResult>>;
    async fn protest_mint(
        &self,
        qid: Uuid,
        client: Arc<dyn ClowderMintConnector>,
    ) -> Result<ProtestResult>;
    async fn check_pending_commitments(
        &self,
        tstamp: u64,
        keysets_info: &HashMap<ecash::Id, KeySetInfo>,
        client: Arc<dyn ClowderMintConnector>,
        swap_config: SwapConfig,
    ) -> Result<()>;
    async fn protest_swap(
        &self,
        commitment_sig: bitcoin::secp256k1::schnorr::Signature,
        keysets_info: &HashMap<ecash::Id, KeySetInfo>,
        alpha_client: Arc<dyn ClowderMintConnector>,
        swap_config: SwapConfig,
    ) -> Result<ProtestResult>;
    async fn protest_melt(&self, quote_id: Uuid) -> Result<MeltProtestResult>;
    async fn list_melt_commitments(&self) -> Result<Vec<(Uuid, u64)>>;
    /// HTLC-locks foreign-mint proofs for an intermint exchange at the mint of
    /// `swap_config.alpha_pk`, resuming a commitment stored over the same inputs.
    /// The locked proofs belong to the exchange and are never stored as the wallet's.
    async fn htlc_lock(
        &self,
        tstamp: u64,
        alpha_client: Arc<dyn ClowderMintConnector>,
        proofs: Vec<cashu::Proof>,
        key_locks: Vec<secp256k1::PublicKey>,
        swap_config: SwapConfig,
        beta_provider: RandomBetaProvider,
    ) -> Result<HtlcLock>;
}

/// Proofs HTLC-locked for an intermint exchange, with the preimage key whose hash
/// locks them and the wallet key that signs them
#[derive(Debug, Clone)]
pub struct HtlcLock {
    pub proofs: Vec<cashu::Proof>,
    pub preimage: cashu::SecretKey,
    pub wallet_key: cashu::SecretKey,
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
    reservation: super::Reservation,
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
    in_flight: InFlight,
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
            in_flight: InFlight::default(),
        }
    }

    fn validate_keysets<'a>(
        &self,
        keysets_info: &'a HashMap<ecash::Id, KeySetInfo>,
        inputs: &[cdk00::Proof],
    ) -> Result<HashMap<ecash::Id, &'a KeySetInfo>> {
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

    async fn find_matching_commitment(
        &self,
        input_ys: &HashSet<cdk01::PublicKey>,
        substitute_clowder_id: Option<secp256k1::PublicKey>,
        htlc_locked: bool,
    ) -> Result<Option<bcr_wallet_persistence::SwapCommitmentRecord>> {
        let commitments = match substitute_clowder_id {
            None => self.pdb.list_commitments().await?,
            Some(id) => self.pdb.list_substitute_commitments(id).await?,
        };
        Ok(commitments.into_iter().find(|record| {
            is_htlc_locked(record) == htlc_locked
                && record.inputs.len() == input_ys.len()
                && record.inputs.iter().all(|y| input_ys.contains(y))
        }))
    }

    async fn finalize_resumed_signatures(
        &self,
        client: Arc<dyn ClowderMintConnector>,
        premints: &HashMap<ecash::Id, cdk00::PreMintSecrets>,
        signatures: Vec<cdk00::BlindSignature>,
    ) -> Result<Amount> {
        let mut sigs_by_kid: HashMap<ecash::Id, Vec<cdk00::BlindSignature>> = HashMap::new();
        for signature in signatures {
            sigs_by_kid
                .entry(signature.keyset_id.into())
                .or_default()
                .push(signature);
        }
        let mut total = Amount::ZERO;
        for (kid, sigs) in sigs_by_kid {
            let Some(premint) = premints.get(&kid) else {
                tracing::warn!("resumed swap: no stored premint for keyset {kid}");
                continue;
            };
            let keyset = client.get_mint_keyset(kid).await?;
            let proofs = unblind_proofs(&keyset, sigs, premint.clone());
            for proof in proofs {
                let amount = proof.amount;
                self.pdb.store_new(proof).await?;
                total += amount;
            }
        }
        Ok(total)
    }

    /// Unblinds whatever the mint restores of the record's committed outputs
    async fn restore_committed_outputs(
        &self,
        client: Arc<dyn ClowderMintConnector>,
        record: &bcr_wallet_persistence::SwapCommitmentRecord,
    ) -> Result<Vec<cdk00::Proof>> {
        let premints_by_output: HashMap<cashu::PublicKey, (ecash::Id, cdk00::PreMint)> = record
            .premints
            .iter()
            .flat_map(|(kid, premint)| {
                premint
                    .iter()
                    .map(move |pm| (pm.blinded_message.blinded_secret, (*kid, pm.clone())))
            })
            .collect();

        let restored = client
            .post_restore(cashu::RestoreRequest {
                outputs: record.outputs.clone(),
            })
            .await?;

        let mut keysets: HashMap<ecash::Id, KeySet> = HashMap::new();
        let mut proofs = Vec::new();
        for (blinded_message, signature) in restored {
            let Some((kid, premint)) = premints_by_output.get(&blinded_message.blinded_secret)
            else {
                continue;
            };
            if !keysets.contains_key(kid) {
                let keyset = client.get_mint_keyset(*kid).await?;
                keysets.insert(*kid, keyset);
            }
            let keyset = keysets.get(kid).expect("keyset should be here");
            match bcr_common::core::signature::unblind_ecash_signature(
                keyset,
                premint.clone(),
                signature,
            ) {
                Ok(proof) => proofs.push(proof),
                Err(e) => tracing::error!("unblind_ecash_signature failed during restore: {e}"),
            }
        }
        Ok(proofs)
    }

    async fn resume_via_restore(
        &self,
        client: Arc<dyn ClowderMintConnector>,
        record: &bcr_wallet_persistence::SwapCommitmentRecord,
    ) -> Result<Option<(Amount, Vec<cdk01::PublicKey>)>> {
        let proofs = self.restore_committed_outputs(client, record).await?;
        if proofs.is_empty() {
            return Ok(None);
        }
        let mut total = Amount::ZERO;
        let mut stored_ys = Vec::with_capacity(proofs.len());
        for proof in proofs {
            let amount = proof.amount;
            stored_ys.push(self.pdb.store_new(proof).await?);
            total += amount;
        }
        Ok(Some((total, stored_ys)))
    }

    /// Resumes a commitment made with another mint over the same inputs: replays it
    /// while it is live, else restores its outputs. `None` means the expired commitment
    /// was never executed and has been dropped, so a fresh swap may run.
    async fn resume_foreign_swap(
        &self,
        client: Arc<dyn ClowderMintConnector>,
        keysets: &HashMap<ecash::Id, KeySet>,
        inputs: &[cdk00::Proof],
        record: bcr_wallet_persistence::SwapCommitmentRecord,
    ) -> Result<Option<Vec<cdk00::Proof>>> {
        let now = time::OffsetDateTime::now_utc().unix_timestamp() as u64;
        let live = record.expiry >= now;
        if live {
            let mut by_y = HashMap::with_capacity(inputs.len());
            for proof in inputs {
                by_y.insert(proof.y()?, proof.clone());
            }
            let committed_inputs = crate::wallet::util::remove_dleq_from_proofs(
                record
                    .inputs
                    .iter()
                    .filter_map(|y| by_y.remove(y))
                    .collect(),
            );
            match client
                .post_swap_committed(committed_inputs, record.outputs.clone(), record.commitment)
                .await
            {
                Ok(signatures) => {
                    let proofs = unblind_by_keyset(keysets, &record.premints, signatures)?;
                    self.pdb.delete_commitment(record.commitment).await?;
                    return Ok(Some(proofs));
                }
                Err(e) => tracing::warn!(
                    "replaying foreign commitment {} failed: {e} - trying restore",
                    record.commitment
                ),
            }
        }

        let restored = self.restore_committed_outputs(client, &record).await?;
        if !restored.is_empty() {
            self.pdb.delete_commitment(record.commitment).await?;
            return Ok(Some(restored));
        }
        if live {
            return Err(Error::Swap(format!(
                "foreign commitment {} is live but neither replayed nor restored",
                record.commitment
            )));
        }
        self.pdb.delete_commitment(record.commitment).await?;
        Ok(None)
    }

    async fn resume_committed_swap(
        &self,
        client: Arc<dyn ClowderMintConnector>,
        keysets_info: &HashMap<ecash::Id, KeySetInfo>,
        record: bcr_wallet_persistence::SwapCommitmentRecord,
        swap_config: SwapConfig,
    ) -> Result<Option<(Amount, Vec<cdk01::PublicKey>)>> {
        let ys = record.inputs.clone();
        // The record carries the exact (DLEQ-stripped) proofs it committed - a receive's
        // inputs are the sender's proofs and are never stored in the wallet's own proof
        // table, so this is the only place a resume can find them. A record stored before
        // this field existed has none: replaying with a partial or empty input set would
        // only mislead the mint, so go straight to restore/protest instead.
        let committed_proofs = record.input_proofs.clone();
        let have_all_inputs = committed_proofs.len() == ys.len();
        let committed_proofs = crate::wallet::util::remove_dleq_from_proofs(committed_proofs);

        if have_all_inputs {
            match client
                .post_swap_committed(committed_proofs, record.outputs.clone(), record.commitment)
                .await
            {
                Ok(signatures) => {
                    let amount = self
                        .finalize_resumed_signatures(client, &record.premints, signatures)
                        .await?;
                    self.pdb.delete_commitment(record.commitment).await?;
                    return Ok(Some((amount, ys)));
                }
                // The mint rejecting the replay (not found, or a bad-request like an
                // expired commitment) still means it never executed it, so fall through
                // to restore and protest instead of leaving the record stuck forever.
                Err(
                    e @ (Error::Transport(_)
                    | Error::MintClientServiceUnavailable(_)
                    | Error::ReqwestClient(_)),
                ) => return Err(e),
                Err(_) => {}
            }
        }

        if let Some(result) = self.resume_via_restore(client.clone(), &record).await? {
            self.pdb.delete_commitment(record.commitment).await?;
            return Ok(Some(result));
        }

        match self
            .protest_swap(record.commitment, keysets_info, client, swap_config)
            .await?
        {
            ProtestResult {
                status: wire_common::ProtestStatus::Resolved,
                result: Some(result),
            } => Ok(Some(result)),
            ProtestResult {
                status: wire_common::ProtestStatus::Resolved,
                result: None,
            } => Err(Error::MintingError(
                "swap protest resolved but no result returned".to_string(),
            )),
            ProtestResult {
                status: wire_common::ProtestStatus::Rabid,
                ..
            } => {
                self.pdb.delete_commitment(record.commitment).await?;
                Ok(None)
            }
            ProtestResult {
                status: wire_common::ProtestStatus::Offline,
                ..
            } => Err(Error::MintClientServiceUnavailable(format!(
                "mint unreachable while resuming swap commitment {}",
                record.commitment
            ))),
        }
    }

    async fn digest_proofs(
        &self,
        client: Arc<dyn ClowderMintConnector>,
        keysets_info: &HashMap<ecash::Id, KeySetInfo>,
        inputs: HashMap<cdk01::PublicKey, cdk00::Proof>,
        swap_config: SwapConfig,
    ) -> Result<(Amount, Vec<cdk01::PublicKey>)> {
        if inputs.is_empty() {
            tracing::warn!("DbPocket::digest_proofs: empty inputs");
            return Ok((Amount::ZERO, Vec::new()));
        }

        let input_ys: HashSet<cdk01::PublicKey> = inputs.keys().copied().collect();
        if let Some(record) = self
            .find_matching_commitment(&input_ys, None, false)
            .await?
            && let Some(result) = self
                .resume_committed_swap(client.clone(), keysets_info, record, swap_config.clone())
                .await?
        {
            return Ok(result);
        }

        let keysets_info: HashMap<cashu::Id, KeySetInfo> = keysets_info
            .iter()
            .map(|(id, info)| ((*id).into(), info.clone()))
            .collect();
        // prepare data
        let (ys, swap_proofs): (Vec<_>, Vec<_>) = inputs.into_iter().unzip();

        // create swap plan
        let swap_plan: BTreeMap<_, _> = prepare_swap(&swap_proofs, &keysets_info)?
            .into_iter()
            .collect();
        tracing::debug!("Digest proofs - swap plan: {swap_plan:?}");

        // collect keysets first as we don't want any failure once the swap request
        // has been made
        let kids: HashSet<ecash::Id> = swap_proofs.iter().map(|p| p.keyset_id.into()).collect();
        let mut keysets: HashMap<ecash::Id, KeySet> = HashMap::new();
        for kid in kids.iter() {
            let keyset = client.get_mint_keyset(*kid).await?;
            keysets.insert(*kid, keyset);
        }

        // prepare the premints
        let mut premints: BTreeMap<ecash::Id, cdk00::PreMintSecrets> = BTreeMap::new();
        for (kid, amount) in swap_plan {
            let kid: ecash::Id = kid.into();
            let premint = premint_from_counter(
                self.pdb.as_ref(),
                &self.seed,
                kid,
                amount,
                &SplitTarget::None,
                &keysets[&kid],
            )
            .await?;
            premints.insert(kid, premint);
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
        keysets_info: &HashMap<ecash::Id, KeySetInfo>,
    ) -> Result<(SendSummary, SendReference)> {
        let unspent_proofs = self.pdb.list_unspent().await?;
        let mut proofs: Vec<(&cdk01::PublicKey, &Proof)> = unspent_proofs.iter().collect();
        // sort by amount as required by `prepare_payment`, ties spend the earliest expiring keyset first
        proofs.sort_by_key(|(y, proof)| {
            let expiry = keysets_info
                .get(&proof.keyset_id.into())
                .and_then(|info| info.final_expiry);
            (
                proof.amount,
                expiry.unwrap_or(u64::MAX),
                proof.keyset_id,
                **y,
            )
        });
        let proofs: Vec<Proof> = proofs.into_iter().map(|(_, p)| p.clone()).collect();

        let infos = collect_keyset_infos_from_proofs(unspent_proofs.values(), keysets_info)?;
        let kinfos: HashMap<cashu::Id, KeySetInfo> = infos
            .iter()
            .map(|(k, v)| ((*k).into(), (*v).clone()))
            .collect();

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
        let active_keyset = client.get_mint_keyset(premint.keyset_id.into()).await?;

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
            self.pdb.store_new(proof).await?;
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
        keysets_info: &HashMap<ecash::Id, KeySetInfo>,
    ) -> Result<PocketBalance> {
        let proofs: Vec<Proof> = self.pdb.list_unspent().await?.into_values().collect();
        let mut debit = Amount::ZERO;
        let mut credit = Amount::ZERO;

        let infos = collect_keyset_infos_from_proofs(proofs.iter(), keysets_info)?;
        let start_of_today = time::OffsetDateTime::now_utc()
            .date()
            .midnight()
            .assume_utc()
            .unix_timestamp() as u64;

        for proof in proofs {
            let info = infos
                .get(&proof.keyset_id.into())
                .ok_or(Error::UnknownKeysetId(proof.keyset_id.into()))?;

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
        keysets_info: &HashMap<ecash::Id, KeySetInfo>,
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
        keysets_info: &HashMap<ecash::Id, KeySetInfo>,
    ) -> Result<SendSummary> {
        let (summary, send_ref) = self.compute_send_costs(target, keysets_info).await?;
        *self.current_send.lock().unwrap() = Some(send_ref);
        Ok(summary)
    }

    async fn send_proofs(
        &self,
        rid: Uuid,
        keysets_info: &HashMap<ecash::Id, KeySetInfo>,
        client: Arc<dyn ClowderMintConnector>,
        swap_config: SwapConfig,
    ) -> Result<(HashMap<cdk01::PublicKey, cdk00::Proof>, super::Reservation)> {
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
        send_proofs(
            send_ref.plan,
            keysets_info,
            send_ref.target_amount,
            &self.seed,
            self.pdb.as_ref(),
            &client,
            swap_config,
            self.beta.as_ref(),
            &self.in_flight,
        )
        .await
    }

    async fn restore_local_proofs(
        &self,
        keysets_info: &HashMap<ecash::Id, KeySetInfo>,
        client: Arc<dyn ClowderMintConnector>,
    ) -> Result<usize> {
        let mut total_recovered = 0;
        for kid in keysets_info.keys() {
            total_recovered +=
                restore::restore_keysetid(&self.seed, *kid, &client, self.pdb.as_ref()).await?;
        }
        Ok(total_recovered)
    }

    async fn delete_proofs(&self) -> Result<HashMap<ecash::Id, Vec<cdk00::Proof>>> {
        let proofs = self.pdb.list_all().await?;

        let mut proofs_by_keyset = HashMap::<ecash::Id, Vec<cdk00::Proof>>::new();

        for y in proofs.iter() {
            if let Some((proof, state)) = self.pdb.delete_proof(*y).await? {
                // delete all, but return only unspent proofs
                if matches!(state, cdk07::State::Unspent) {
                    proofs_by_keyset
                        .entry(proof.keyset_id.into())
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
    ) -> Result<(
        Amount,
        HashMap<cdk01::PublicKey, cdk00::Proof>,
        super::Reservation,
    )> {
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
        return_proofs_to_send_for_offline_payment(send_ref.plan, self.pdb.as_ref(), &self.in_flight)
            .await
    }

    async fn swap_to_unlocked_substitute_proofs(
        &self,
        proofs: Vec<cdk00::Proof>,
        keysets_info: &HashMap<ecash::Id, KeySetInfo>,
        keysets: HashMap<ecash::Id, KeySet>,
        substitute_client: Arc<dyn ClowderMintConnector>,
        substitute_clowder_id: secp256k1::PublicKey,
        beta_provider: RandomBetaProvider,
        send_amount: Amount,
        swap_config: SwapConfig,
    ) -> Result<Vec<cashu::Proof>> {
        let total_amount = proofs.total_amount()?;
        let change_amount = total_amount - send_amount;

        let keysets_info: HashMap<cashu::Id, KeySetInfo> = keysets_info
            .iter()
            .map(|(id, info)| ((*id).into(), info.clone()))
            .collect();
        let swap_plan: BTreeMap<_, _> = prepare_swap(&proofs, &keysets_info)?.into_iter().collect();
        tracing::debug!(
            "Swapping to unlocked substitute proofs {swap_plan:?} - {change_amount} will be used for fees and stored temporarily as foreign mint proofs."
        );

        // collect payments by kid, so we can reconstruct it after the swap
        let mut remaining_payment = send_amount;
        let mut payment_targets_by_kid: HashMap<ecash::Id, Amount> = HashMap::new();
        let mut split_targets: Vec<(ecash::Id, Amount, SplitTarget)> = Vec::new();

        for (kid, amount) in swap_plan {
            let keyset_payment_target = std::cmp::min(amount, remaining_payment);

            let target = if keyset_payment_target > Amount::ZERO {
                // used for our payment - needs to add up to our payment amount
                payment_targets_by_kid.insert(kid.into(), keyset_payment_target);
                remaining_payment -= keyset_payment_target;
                SplitTarget::Value(keyset_payment_target)
            } else {
                // change - doesn't matter how we get it
                SplitTarget::default()
            };
            split_targets.push((kid.into(), amount, target));
        }

        if remaining_payment != Amount::ZERO {
            return Err(Error::Swap(format!(
                "swap plan cannot fund payment target {send_amount}, missing {remaining_payment}"
            )));
        }

        let mut input_ys = HashSet::with_capacity(proofs.len());
        for proof in &proofs {
            input_ys.insert(proof.y()?);
        }
        let resumed = match self
            .find_matching_commitment(&input_ys, Some(substitute_clowder_id), false)
            .await?
        {
            Some(record) => {
                self.resume_foreign_swap(substitute_client.clone(), &keysets, &proofs, record)
                    .await?
            }
            None => None,
        };

        let swapped = match resumed {
            Some(swapped) => swapped,
            None => {
                let mut premints: BTreeMap<ecash::Id, cdk00::PreMintSecrets> = BTreeMap::new();
                for (kid, amount, target) in split_targets {
                    let premint = premint_from_counter(
                        self.pdb.as_ref(),
                        &self.seed,
                        kid,
                        amount,
                        &target,
                        &keysets[&kid],
                    )
                    .await?;
                    premints.insert(kid, premint);
                }

                let blinds: Vec<cdk00::BlindedMessage> = premints
                    .values()
                    .flat_map(|premint| premint.blinded_messages())
                    .collect();
                let premints: HashMap<ecash::Id, cdk00::PreMintSecrets> =
                    premints.into_iter().collect();

                let attestation = beta_provider.attest(&proofs).await?;
                let signatures = super::committed_swap(
                    substitute_client.as_ref(),
                    Some(self.pdb.as_ref()),
                    proofs,
                    blinds,
                    &swap_config,
                    premints.clone(),
                    attestation,
                    Some(substitute_clowder_id),
                )
                .await?;
                unblind_by_keyset(&keysets, &premints, signatures)?
            }
        };

        let mut on_target: Vec<cdk00::Proof> = Vec::new();
        let mut change_proofs: Vec<cdk00::Proof> = Vec::new();

        let mut proofs_by_kid: HashMap<ecash::Id, Vec<cdk00::Proof>> = HashMap::new();
        for proof in swapped {
            proofs_by_kid
                .entry(proof.keyset_id.into())
                .or_default()
                .push(proof);
        }

        let mut selected_amount = Amount::ZERO;
        for (kid, mut proofs) in proofs_by_kid.into_iter() {
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
        keysets_info: &HashMap<ecash::Id, KeySetInfo>,
    ) -> Result<HashMap<ecash::Id, (Option<u64>, Amount)>> {
        let proofs: Vec<Proof> = self.pdb.list_unspent().await?.into_values().collect();
        let infos = collect_keyset_infos_from_proofs(proofs.iter(), keysets_info)?;

        let mut balances: HashMap<ecash::Id, (Option<u64>, Amount)> = HashMap::new();

        for proof in proofs {
            let kid = proof.keyset_id.into();
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

/// Whether the record's outputs carry a NUT-14 HTLC spending condition, i.e. it locks
/// proofs for an intermint exchange instead of swapping them for the wallet
fn is_htlc_locked(record: &bcr_wallet_persistence::SwapCommitmentRecord) -> bool {
    record
        .premints
        .values()
        .flat_map(|premint| premint.iter())
        .any(|premint| {
            matches!(
                cashu::SpendingConditions::try_from(&premint.secret),
                Ok(cashu::SpendingConditions::HTLCConditions { .. })
            )
        })
}

/// Unblinds swap signatures with the premints and keysets of their keyset ids
fn unblind_by_keyset(
    keysets: &HashMap<ecash::Id, KeySet>,
    premints: &HashMap<ecash::Id, cdk00::PreMintSecrets>,
    signatures: Vec<cdk00::BlindSignature>,
) -> Result<Vec<cdk00::Proof>> {
    let mut sigs_by_kid: HashMap<ecash::Id, Vec<cdk00::BlindSignature>> = HashMap::new();
    for signature in signatures {
        sigs_by_kid
            .entry(signature.keyset_id.into())
            .or_default()
            .push(signature);
    }
    let mut proofs = Vec::new();
    for (kid, sigs) in sigs_by_kid {
        let keyset = keysets.get(&kid).ok_or(Error::UnknownKeysetId(kid))?;
        let premint = premints
            .get(&kid)
            .ok_or_else(|| Error::Swap(format!("no premint for keyset {kid}")))?;
        proofs.extend(unblind_proofs(keyset, sigs, premint.clone()));
    }
    Ok(proofs)
}

#[async_trait]
impl DebitPocketApi for Pocket {
    async fn reclaim_proofs(
        &self,
        ys: &[cdk01::PublicKey],
        keysets_info: &HashMap<ecash::Id, KeySetInfo>,
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
        keysets_info: &HashMap<ecash::Id, KeySetInfo>,
        client: Arc<dyn ClowderMintConnector>,
        swap_config: SwapConfig,
    ) -> Result<Amount> {
        // remove pending transaction ys from pending proofs
        let mut pendings = self.pdb.list_pending().await?;
        let remove_set: HashSet<&cashu::PublicKey> = pending_txs_ys.iter().collect();
        pendings.retain(|k, _| !remove_set.contains(k));
        self.in_flight.exclude(&mut pendings);

        let req = cdk07::CheckStateRequest {
            ys: pendings.keys().cloned().collect(),
        };
        let states = client.post_check_state(req).await?;
        let mut commitments_by_y: Option<
            HashMap<cashu::PublicKey, bcr_wallet_persistence::SwapCommitmentRecord>,
        > = None;
        let mut resumed_commitments = HashSet::new();
        let mut resumed = Amount::ZERO;
        let mut to_digest = HashMap::new();
        for state in states.iter() {
            match state.state {
                cdk07::State::Spent => {
                    let commitments_by_y = match &commitments_by_y {
                        Some(map) => map,
                        None => {
                            let mut map = HashMap::new();
                            for record in self.pdb.list_commitments().await? {
                                for y in &record.inputs {
                                    map.insert(*y, record.clone());
                                }
                            }
                            commitments_by_y.insert(map)
                        }
                    };
                    if let Some(record) = commitments_by_y.get(&state.y).cloned() {
                        if resumed_commitments.insert(record.commitment) {
                            tracing::warn!(
                                "Pending Stale Proof returned as SPENT from Mint - resuming its commitment {} before marking SPENT",
                                record.commitment
                            );
                            match self
                                .resume_committed_swap(
                                    client.clone(),
                                    keysets_info,
                                    record,
                                    swap_config.clone(),
                                )
                                .await
                            {
                                Ok(Some((amount, _))) => resumed += amount,
                                Ok(None) => {}
                                // An unreachable mint must still fail the whole call,
                                // same as resuming it directly would: the caller needs
                                // to see it and retry later, not read a partial batch
                                // as success.
                                Err(e @ Error::MintClientServiceUnavailable(_)) => return Err(e),
                                // Any other resume failure (the protest itself was
                                // rejected, ...) must not abort recovery for every
                                // other stale proof in this batch, nor mark this one
                                // spent before its commitment is actually resolved.
                                Err(e) => {
                                    tracing::error!(
                                        "Failed to resume commitment for stale Spent proof {}: {e} - leaving it pending",
                                        state.y
                                    );
                                    continue;
                                }
                            }
                        }
                    } else {
                        tracing::warn!(
                            "Pending Stale Proof returned as SPENT from Mint - not recovering and setting to SPENT"
                        );
                    }
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
            return Ok(resumed);
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

        Ok(resumed + recovered)
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
        keysets_info: &HashMap<ecash::Id, KeySetInfo>,
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

        let (sending_proofs, reservation) = send_proofs(
            send_ref.plan,
            keysets_info,
            send_ref.target_amount,
            &self.seed,
            self.pdb.as_ref(),
            &client,
            swap_config.clone(),
            self.beta.as_ref(),
            &self.in_flight,
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
            reservation,
        };
        self.current_melt.lock().unwrap().replace(melt_ref);
        Ok(summary)
    }

    async fn pay_onchain_melt(
        &self,
        rid: Uuid,
        client: Arc<dyn ClowderMintConnector>,
    ) -> Result<(
        bitcoin::Txid,
        HashMap<cdk01::PublicKey, cdk00::Proof>,
        super::Reservation,
    )> {
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
        Ok((response.txid, sending_proofs, melt_ref.reservation))
    }

    async fn mint_onchain(
        &self,
        amount: bitcoin::Amount,
        keysets_info: &HashMap<ecash::Id, KeySetInfo>,
        client: Arc<dyn ClowderMintConnector>,
    ) -> Result<MintSummary> {
        // find debit keyset
        let active_info = keysets_info
            .values()
            .filter(|info| info.unit == self.unit && info.active && info.final_expiry.is_none())
            .min_by_key(|info| info.id);
        let Some(active_info) = active_info else {
            return Err(Error::NoActiveKeyset);
        };
        let kid = active_info.id;
        let keyset = client.get_mint_keyset(kid).await?;
        let premint = premint_from_counter(
            self.pdb.as_ref(),
            &self.seed,
            kid,
            cashu::Amount::from(amount.to_sat()),
            &SplitTarget::None,
            &keyset,
        )
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

        let body: wire_mint::OnchainMintQuoteResponseBodyV1 =
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
    ) -> Result<BTreeMap<Uuid, CheckPendingMintResult>> {
        let mint_ids = self.mdb.list_mints().await?;
        let mut res = BTreeMap::new();

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

    async fn check_pending_commitments(
        &self,
        tstamp: u64,
        keysets_info: &HashMap<ecash::Id, KeySetInfo>,
        client: Arc<dyn ClowderMintConnector>,
        swap_config: SwapConfig,
    ) -> Result<()> {
        let commitments = self.pdb.list_commitments().await?;
        tracing::debug!(
            "check pending commitments for {} entries",
            commitments.len()
        );
        for record in commitments {
            if record.expiry < tstamp {
                let commitment_sig = record.commitment;
                tracing::warn!(
                    "Swap commitment {commitment_sig} expired at {} (now: {tstamp}) - attempting recovery before deleting.",
                    record.expiry,
                );
                match self
                    .resume_committed_swap(
                        client.clone(),
                        keysets_info,
                        record,
                        swap_config.clone(),
                    )
                    .await
                {
                    Ok(_) => {
                        tracing::info!(
                            "Expired commitment {commitment_sig} resolved during recovery."
                        );
                    }
                    Err(e) => {
                        tracing::error!(
                            "Failed to recover expired commitment {commitment_sig}: {e} - keeping it for a later pass."
                        );
                    }
                }
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
        keysets_info: &HashMap<ecash::Id, KeySetInfo>,
        alpha_client: Arc<dyn ClowderMintConnector>,
        swap_config: SwapConfig,
    ) -> Result<ProtestResult> {
        let record = self.pdb.load_commitment(commitment_sig).await?;
        if let Some(substitute) = record.substitute_clowder_id {
            return Err(Error::Swap(format!(
                "commitment {commitment_sig} was made with substitute mint {substitute}"
            )));
        }
        let ephemeral_keypair =
            secp256k1::Keypair::from_secret_key(secp256k1::SECP256K1, &record.ephemeral_secret);
        let wallet_signature = super::sign_content_b64(&record.body_content, &ephemeral_keypair)?;

        let request = wire_swap::SwapProtestRequest {
            alpha_id: self.beta.alpha_id(),
            proofs: record.input_proofs.clone(),
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

                let mut sigs_by_kid: HashMap<ecash::Id, Vec<cdk00::BlindSignature>> =
                    HashMap::new();
                for signature in signatures {
                    sigs_by_kid
                        .entry(signature.keyset_id.into())
                        .or_default()
                        .push(signature);
                }

                let mut keysets: HashMap<ecash::Id, KeySet> = HashMap::new();
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

    async fn htlc_lock(
        &self,
        tstamp: u64,
        alpha_client: Arc<dyn ClowderMintConnector>,
        proofs: Vec<cashu::Proof>,
        key_locks: Vec<secp256k1::PublicKey>,
        swap_config: SwapConfig,
        beta_provider: RandomBetaProvider,
    ) -> Result<HtlcLock> {
        let mut ys = Vec::with_capacity(proofs.len());
        for proof in &proofs {
            ys.push(proof.y()?);
        }
        let (preimage, wallet_key) = crate::wallet::util::htlc_lock_keys(&self.seed, &ys);
        let input_ys: HashSet<cdk01::PublicKey> = ys.into_iter().collect();

        let resumed = match self
            .find_matching_commitment(&input_ys, Some(swap_config.alpha_pk), true)
            .await?
        {
            Some(record) => {
                let mut keysets = HashMap::with_capacity(record.premints.len());
                for kid in record.premints.keys() {
                    keysets.insert(*kid, alpha_client.get_mint_keyset(*kid).await?);
                }
                self.resume_foreign_swap(alpha_client.clone(), &keysets, &proofs, record)
                    .await?
            }
            None => None,
        };

        let proofs = match resumed {
            Some(locked) => locked,
            None => {
                crate::wallet::util::htlc_lock(
                    tstamp,
                    alpha_client.as_ref(),
                    self.pdb.as_ref(),
                    proofs,
                    bitcoin::hashes::Hash::hash(&preimage.to_secret_bytes()),
                    key_locks,
                    *wallet_key.public_key(),
                    swap_config,
                    &beta_provider,
                )
                .await?
            }
        };
        Ok(HtlcLock {
            proofs,
            preimage,
            wallet_key,
        })
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

    /// A stored swap commitment with its matching input proofs, keyset and
    /// the mint's would-be signatures over the committed outputs - the setup
    /// a resume (retry or expiry) attempt is tested against.
    struct SwapCommitmentFixture {
        k_infos: HashMap<ecash::Id, KeySetInfo>,
        mintkeyset: ecash::MintKeySet,
        input_ys: Vec<cashu::PublicKey>,
        input_proofs_map: HashMap<cashu::PublicKey, cdk00::Proof>,
        blind_sigs: Vec<cdk00::BlindSignature>,
        record: bcr_wallet_persistence::SwapCommitmentRecord,
    }

    fn swap_commitment_fixture(amount: Amount, sig_byte: u8, expiry: u64) -> SwapCommitmentFixture {
        let (info, mintkeyset) = core_tests::generate_random_ecash_keyset();
        let kid = info.id;
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

        let premint = cdk00::PreMintSecrets::random(
            kid.into(),
            amount,
            &SplitTarget::None,
            &bcr_wallet_core::util::to_fee_and_amounts(&bcr_wallet_core::util::to_keyset(
                &mintkeyset,
                None,
            )),
        )
        .unwrap();
        let outputs = premint.blinded_messages();
        let blind_sigs: Vec<cdk00::BlindSignature> = outputs
            .iter()
            .map(|bm| {
                bcr_common::core::signature::sign_ecash(&mintkeyset, bm)
                    .expect("signing should work")
            })
            .collect();
        let stored_premints = HashMap::from([(kid, premint)]);

        let ephemeral_keypair = secp256k1::Keypair::new_global(&mut secp256k1::rand::thread_rng());
        let ephemeral_secret = secp256k1::SecretKey::from_keypair(&ephemeral_keypair);
        let wallet_key =
            cashu::PublicKey::from(secp256k1::PublicKey::from_keypair(&ephemeral_keypair));
        let commitment_sig = bitcoin::secp256k1::schnorr::Signature::from_slice(&[sig_byte; 64])
            .expect("valid sig bytes");

        let record = bcr_wallet_persistence::SwapCommitmentRecord {
            inputs: input_ys.clone(),
            outputs,
            expiry,
            commitment: commitment_sig,
            ephemeral_secret,
            body_content: "dGVzdA==".to_string(),
            wallet_key,
            premints: stored_premints,
            substitute_clowder_id: None,
            input_proofs: input_proofs.clone(),
        };

        SwapCommitmentFixture {
            k_infos,
            mintkeyset,
            input_ys,
            input_proofs_map,
            blind_sigs,
            record,
        }
    }

    /// digest_proofs feeds the same inputs and keyset info to prepare_swap on every
    /// retry; a nondeterministic split would make a retry's outputs diverge from
    /// the first attempt's, breaking commitment resume.
    #[tokio::test]
    async fn prepare_swap_call_site_is_deterministic() {
        let (info, keyset) = core_tests::generate_random_ecash_keyset();
        let k_infos = test_kinfos(info);
        let keysets_info: HashMap<cashu::Id, KeySetInfo> = k_infos
            .iter()
            .map(|(id, info)| ((*id).into(), info.clone()))
            .collect();
        let amounts = [Amount::from(8u64), Amount::from(16u64), Amount::from(4u64)];
        let proofs = core_tests::generate_random_ecash_proofs(&keyset, &amounts);

        let first: BTreeMap<_, _> = prepare_swap(&proofs, &keysets_info)
            .expect("prepare_swap works")
            .into_iter()
            .collect();
        let second: BTreeMap<_, _> = prepare_swap(&proofs, &keysets_info)
            .expect("prepare_swap works")
            .into_iter()
            .collect();

        assert_eq!(first, second);
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
        k_info.final_expiry = Some(
            (time::OffsetDateTime::now_utc() + time::Duration::days(1)).unix_timestamp() as u64,
        );

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
        let earlier_today = time::OffsetDateTime::now_utc()
            .date()
            .with_hms(0, 0, 1)
            .unwrap()
            .assume_utc()
            .unix_timestamp() as u64;

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
        ks_debit.final_expiry = Some(
            (time::OffsetDateTime::now_utc() - time::Duration::days(1)).unix_timestamp() as u64,
        );

        let mut ks_credit = KeySetInfo::from(info_credit);
        // tomorrow → credit
        ks_credit.final_expiry = Some(
            (time::OffsetDateTime::now_utc() + time::Duration::days(1)).unix_timestamp() as u64,
        );

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
        pdb.expect_reserve_counter()
            .times(1)
            .with(eq(kid), eq(2))
            .returning(|_, _| Ok(0));
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_receive_proofs_never_reuses_counter_indices() {
        let (info, keyset) = core_tests::generate_random_ecash_keyset();
        let kid = info.id;
        let k_infos = test_kinfos(info);

        let pdb: Arc<dyn PocketRepository> = Arc::new(
            bcr_wallet_persistence::test_utils::tests::in_memory_pocket_db(
                &bcr_wallet_persistence::test_utils::tests::wallet_id(),
                CurrencyUnit::Sat,
            ),
        );
        let mdb: Arc<dyn MintMeltRepository> = Arc::new(MockMintMeltRepository::new());

        let seen_outputs: Arc<Mutex<Vec<cdk00::BlindedMessage>>> = Arc::new(Mutex::new(Vec::new()));

        let mut connector = MockClowderMintConnector::new();
        let cloned_keyset = keyset.clone();
        connector
            .expect_get_mint_keyset()
            .times(2)
            .with(eq(kid))
            .returning(move |_| Ok(bcr_wallet_core::util::to_keyset(&cloned_keyset, None)));
        let seen_for_commit = seen_outputs.clone();
        connector
            .expect_post_swap_commitment()
            .times(2)
            .returning(move |_, outputs, _, _, _| {
                let mut seen = seen_for_commit.lock().unwrap();
                for output in &outputs {
                    assert!(
                        !seen.contains(output),
                        "blinded message reused across concurrent swaps"
                    );
                }
                seen.extend(outputs);
                Ok(mock_commitment_result())
            });
        let sign_keyset = keyset.clone();
        connector
            .expect_post_swap_committed()
            .times(2)
            .returning(move |_, outp, _| {
                let amounts = outp.iter().map(|b| b.amount).collect::<Vec<_>>();
                Ok(core_tests::generate_ecash_signatures(
                    &sign_keyset,
                    &amounts,
                ))
            });
        let connector: Arc<dyn ClowderMintConnector> = Arc::new(connector);

        let pocket = Arc::new(pocket(pdb, mdb));

        let mut handles = Vec::new();
        for _ in 0..2 {
            let pocket = pocket.clone();
            let connector = connector.clone();
            let k_infos = k_infos.clone();
            let proofs = core_tests::generate_random_ecash_proofs(
                &keyset,
                &[Amount::from(8u64), Amount::from(16u64)],
            );
            handles.push(tokio::spawn(async move {
                pocket
                    .receive_proofs(connector, &k_infos, proofs, test_swap_config())
                    .await
                    .expect("receive_proofs should succeed")
            }));
        }

        for handle in handles {
            handle.await.expect("task should not panic");
        }

        assert_eq!(
            seen_outputs.lock().unwrap().len(),
            4,
            "both concurrent swaps should have minted their outputs"
        );
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
        pdb.expect_reserve_counter()
            .times(1)
            .with(eq(kid), eq(2))
            .returning(|_, _| Ok(0));
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
        pdb.expect_reserve_counter()
            .times(1)
            .with(eq(kid), eq(1))
            .returning(|_, _| Ok(0));
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
        let melt_ref = MeltReference {
            rid,
            quote_id,
            reservation: Default::default(),
        };
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
        let (info_2, _) = core_tests::generate_random_ecash_keyset();
        let kid = std::cmp::min(info.id, info_2.id);
        let mut k_infos = test_kinfos(info);
        k_infos.extend(test_kinfos(info_2));
        let amount = bitcoin::Amount::from_sat(24);

        let mut mdb = MockMintMeltRepository::new();
        let mut pdb = MockPocketRepository::new();
        let mut connector = MockClowderMintConnector::new();

        pdb.expect_reserve_counter()
            .times(1)
            .with(eq(kid), always())
            .returning(|_, _| Ok(0));

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
                let body = wire_mint::OnchainMintQuoteResponseBodyV1 {
                    quote: Uuid::new_v4(),
                    address: "tb1qteyk7pfvvql2r2zrsu4h4xpvju0nz7ykvguyk0".to_string(),
                    payment_amount: amount,
                    expiry: time::OffsetDateTime::now_utc().unix_timestamp() as u64,
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
                bcr_wallet_core::util::to_keyset(&keyset_clone, None)
                    .id
                    .into(),
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
                    expiry: time::OffsetDateTime::now_utc().unix_timestamp() as u64,
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
            kid.into(),
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
                    expiry: time::OffsetDateTime::now_utc().unix_timestamp() as u64,
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
            kid.into(),
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
                    expiry: time::OffsetDateTime::now_utc().unix_timestamp() as u64,
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

        // Generate premint secrets and sign them — these are the ORIGINAL blinding factors
        let premint = cdk00::PreMintSecrets::random(
            kid.into(),
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
        let record_input_proofs = input_proofs.clone();
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
                substitute_clowder_id: None,
                input_proofs: record_input_proofs.clone(),
            })
        });

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
        pdb.expect_reserve_counter()
            .with(eq(kid), always())
            .returning(|_, _| Ok(0));
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
        let record_input_proofs = input_proofs.clone();
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
                substitute_clowder_id: None,
                input_proofs: record_input_proofs.clone(),
            })
        });

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
    async fn compute_send_costs_breaks_amount_ties_deterministically() {
        let (info, keyset) = core_tests::generate_random_ecash_keyset();
        let k_infos = test_kinfos(info);
        let proofs = core_tests::generate_random_ecash_proofs(&keyset, &[Amount::from(8u64); 8]);
        let expected = proofs.iter().map(|p| p.y().unwrap()).min().unwrap();

        let mut pdb = MockPocketRepository::new();
        pdb.expect_list_unspent()
            .returning(move || Ok(proofs.iter().map(|p| (p.y().unwrap(), p.clone())).collect()));

        let pocket = pocket(Arc::new(pdb), Arc::new(MockMintMeltRepository::new()));
        for _ in 0..8 {
            let (_, send_ref) = pocket
                .compute_send_costs(Amount::from(8u64), &k_infos)
                .await
                .expect("compute send costs works");
            match send_ref.plan {
                SendPlan::Ready { proofs } => assert_eq!(proofs, vec![expected]),
                SendPlan::NeedSwap { .. } => panic!("expected ready send plan"),
            }
        }
    }

    #[tokio::test]
    async fn recover_pending_stale_proofs_skips_reserved_until_released() {
        let (info, keyset) = core_tests::generate_random_ecash_keyset();
        let k_infos = test_kinfos(info);
        let proofs =
            core_tests::generate_random_ecash_proofs(&keyset, &[Amount::from(8), Amount::from(16)]);
        let reserved_y = proofs[0].y().unwrap();
        let stale_y = proofs[1].y().unwrap();

        let mut pdb = MockPocketRepository::new();
        let mut seq = mockall::Sequence::new();
        let reserved = proofs[0].clone();
        pdb.expect_mark_as_pendingspent()
            .times(1)
            .in_sequence(&mut seq)
            .returning(move |_| Ok(vec![reserved.clone()]));
        pdb.expect_mark_as_pendingspent()
            .times(1)
            .in_sequence(&mut seq)
            .returning(move |_| {
                Err(bcr_wallet_persistence::error::Error::InvalidProofState(
                    reserved_y,
                ))
            });
        pdb.expect_list_pending()
            .times(2)
            .returning(move || Ok(proofs.iter().map(|p| (p.y().unwrap(), p.clone())).collect()));
        let pdb = Arc::new(pdb);
        let mut connector = MockClowderMintConnector::new();
        let mut seq = mockall::Sequence::new();
        connector
            .expect_post_check_state()
            .times(1)
            .in_sequence(&mut seq)
            .withf(move |req| req.ys == vec![stale_y])
            .returning(|_| Ok(vec![]));
        connector
            .expect_post_check_state()
            .times(1)
            .in_sequence(&mut seq)
            .withf(move |req| req.ys.len() == 2 && req.ys.contains(&reserved_y))
            .returning(|_| Ok(vec![]));
        let connector: Arc<dyn ClowderMintConnector> = Arc::new(connector);

        let pocket = pocket(pdb.clone(), Arc::new(MockMintMeltRepository::new()));
        let mut reservation = pocket.in_flight.reservation();
        reservation
            .reserve(pdb.as_ref(), vec![reserved_y])
            .await
            .expect("reserve works");
        let mut overlapping = pocket.in_flight.reservation();
        assert!(
            overlapping
                .reserve(pdb.as_ref(), vec![reserved_y])
                .await
                .is_err()
        );
        drop(overlapping);
        pocket
            .recover_pending_stale_proofs(&[], &k_infos, connector.clone(), test_swap_config())
            .await
            .expect("recover works");

        drop(reservation);
        pocket
            .recover_pending_stale_proofs(&[], &k_infos, connector, test_swap_config())
            .await
            .expect("recover works");
    }

    #[tokio::test]
    async fn compute_send_costs_spends_earliest_expiring_keyset_first() {
        let (expiring, expiring_keyset) = core_tests::generate_random_ecash_keyset();
        let (lasting, lasting_keyset) = core_tests::generate_random_ecash_keyset();
        let mut k_infos = test_kinfos(expiring);
        k_infos
            .values_mut()
            .for_each(|info| info.final_expiry = Some(1));
        k_infos.extend(test_kinfos(lasting));
        let mut proofs =
            core_tests::generate_random_ecash_proofs(&lasting_keyset, &[Amount::from(8u64); 4]);
        let expiring_proofs =
            core_tests::generate_random_ecash_proofs(&expiring_keyset, &[Amount::from(8u64)]);
        let expected = expiring_proofs[0].y().unwrap();
        proofs.extend(expiring_proofs);

        let mut pdb = MockPocketRepository::new();
        pdb.expect_list_unspent()
            .returning(move || Ok(proofs.iter().map(|p| (p.y().unwrap(), p.clone())).collect()));

        let pocket = pocket(Arc::new(pdb), Arc::new(MockMintMeltRepository::new()));
        let (_, send_ref) = pocket
            .compute_send_costs(Amount::from(8u64), &k_infos)
            .await
            .expect("compute send costs works");
        match send_ref.plan {
            SendPlan::Ready { proofs } => assert_eq!(proofs, vec![expected]),
            SendPlan::NeedSwap { .. } => panic!("expected ready send plan"),
        }
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
            kid.into(),
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
                    expiry: time::OffsetDateTime::now_utc().unix_timestamp() as u64,
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
            kid.into(),
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
                    expiry: time::OffsetDateTime::now_utc().unix_timestamp() as u64,
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
            .times(1)
            .returning(move |ys| Ok(ys.iter().map(|y| pending[y].clone()).collect()));

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
        pdb.expect_reserve_counter().returning(|_, _| Ok(0));
        pdb.expect_list_substitute_commitments()
            .times(1)
            .returning(|_| Ok(vec![]));
        pdb.expect_store_commitment().times(1).returning(|record| {
            assert!(record.substitute_clowder_id.is_some());
            Ok(())
        });
        pdb.expect_delete_commitment()
            .times(1)
            .returning(|_| Ok(()));
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

    /// The substitute swap's change is persisted as a ForeignMintProof; the
    /// wallet never gets a second chance to re-derive it from the output it
    /// keeps, so a seed-only restore (no persisted premint) must be able to
    /// find it.
    #[tokio::test]
    async fn swap_to_unlocked_substitute_proofs_change_is_seed_derived() {
        let (info, mint_keyset) = core_tests::generate_random_ecash_keyset();
        let kid = info.id;

        let keysets_info = test_kinfos(info);
        let keyset = bcr_wallet_core::util::to_keyset(&mint_keyset, None);
        let keysets = HashMap::from([(kid, keyset.clone())]);

        // 24 total, 16 payment, 8 change
        let input_proofs = core_tests::generate_random_ecash_proofs(
            &mint_keyset,
            &[Amount::from(8u64), Amount::from(16u64)],
        );
        let send_amount = Amount::from(16u64);
        let expected_change_amount = Amount::from(8u64);

        let substitute_keypair = secp256k1::Keypair::new_global(&mut secp256k1::rand::thread_rng());
        let substitute_clowder_id = secp256k1::PublicKey::from_keypair(&substitute_keypair);

        let seed: Seed = [9u8; 64];

        let mdb = MockMintMeltRepository::new();
        let mut pdb = MockPocketRepository::new();
        pdb.expect_reserve_counter().returning(|_, _| Ok(0));
        pdb.expect_list_substitute_commitments()
            .times(1)
            .returning(|_| Ok(vec![]));
        pdb.expect_store_commitment().times(1).returning(|record| {
            assert!(record.substitute_clowder_id.is_some());
            Ok(())
        });
        pdb.expect_delete_commitment()
            .times(1)
            .returning(|_| Ok(()));

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

        let stored_foreign_proofs = Arc::new(Mutex::new(Vec::<ForeignMintProof>::new()));
        let stored_foreign_proofs_clone = stored_foreign_proofs.clone();
        pdb.expect_store_foreign_mint_proof()
            .times(1)
            .returning(move |foreign_proof| {
                let y = foreign_proof.proof.y().expect("valid y");
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

        let pocket = super::Pocket::new(
            CurrencyUnit::Sat,
            Arc::new(pdb),
            Arc::new(mdb),
            seed,
            Arc::new(test_beta_provider()),
        );

        pocket
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

        let stored_change = stored_foreign_proofs
            .lock()
            .expect("stored proof mutex")
            .clone();
        assert_eq!(stored_change.len(), 1);
        assert_eq!(stored_change[0].proof.amount, expected_change_amount);

        // the whole swap (payment and change together) was blinded in one
        // premint_from_counter call at counter 0, over the full amount with
        // the payment as its split target; a seed-only restore over that
        // same range must find the change secret among what it recovers,
        // with no persisted premint to fall back on.
        let expected_full_premint = cdk00::PreMintSecrets::from_seed(
            kid.into(),
            0,
            &seed,
            Amount::from(24u64),
            &SplitTarget::Value(send_amount),
            &bcr_wallet_core::util::to_fee_and_amounts(&keyset),
        )
        .expect("premint derivation works");
        let expected_blinds = expected_full_premint.blinded_messages();
        assert!(
            expected_full_premint
                .secrets()
                .iter()
                .any(|secret| *secret == stored_change[0].proof.secret),
            "the persisted change secret must be seed-derivable from counter 0"
        );

        let mut restore_client = MockClowderMintConnector::new();
        let signing_keyset = mint_keyset.clone();
        let expected_blinds_clone = expected_blinds.clone();
        restore_client
            .expect_post_restore()
            .returning(move |request| {
                // the mint matches a restore request by its blinded point alone, echoes
                // back the request's own (zero-amount) message and signs with the real
                // amount it originally minted under that point.
                let matched: Vec<(cdk00::BlindedMessage, cashu::Amount)> = request
                    .outputs
                    .iter()
                    .filter_map(|o| {
                        expected_blinds_clone
                            .iter()
                            .find(|e| e.blinded_secret == o.blinded_secret)
                            .map(|e| (o.clone(), e.amount))
                    })
                    .collect();
                if matched.is_empty() {
                    return Ok(vec![]);
                }
                let amounts: Vec<_> = matched.iter().map(|(_, amount)| *amount).collect();
                let sigs = core_tests::generate_ecash_signatures(&signing_keyset, &amounts);
                Ok(matched
                    .into_iter()
                    .map(|(output, _)| output)
                    .zip(sigs)
                    .collect())
            });
        let keyset_for_restore = mint_keyset.clone();
        restore_client
            .expect_get_mint_keyset()
            .returning(move |_| Ok(bcr_wallet_core::util::to_keyset(&keyset_for_restore, None)));
        restore_client.expect_post_check_state().returning(|req| {
            Ok(req
                .ys
                .iter()
                .map(|y| cdk07::ProofState {
                    y: *y,
                    state: cdk07::State::Unspent,
                    witness: None,
                })
                .collect())
        });

        let mut restore_pdb = MockPocketRepository::new();
        restore_pdb.expect_reserve_counter().returning(|_, _| Ok(0));
        restore_pdb
            .expect_advance_counter_to()
            .returning(|_, _| Ok(()));
        let recovered_amount = Arc::new(Mutex::new(Amount::ZERO));
        let recovered_amount_clone = recovered_amount.clone();
        restore_pdb.expect_store_new().returning(move |p| {
            *recovered_amount_clone
                .lock()
                .expect("recovered amount mutex") += p.amount;
            Ok(p.y().expect("valid y"))
        });

        let restore_client: Arc<dyn crate::ClowderMintConnector> = Arc::new(restore_client);
        let total_restored = restore::restore_keysetid(&seed, kid, &restore_client, &restore_pdb)
            .await
            .expect("restore works");

        assert!(total_restored > 0, "the seed-only restore must find proofs");
        assert_eq!(
            *recovered_amount.lock().expect("recovered amount mutex"),
            Amount::from(24u64)
        );
    }

    #[tokio::test]
    async fn swap_retry_never_reached_mint_replays_commitment() {
        let amount = Amount::from(24u64);
        let fx = swap_commitment_fixture(amount, 0xcd, 1000);

        let mdb = MockMintMeltRepository::new();
        let mut pdb = MockPocketRepository::new();
        let mut alpha_connector = MockClowderMintConnector::new();

        let input_proofs_map = fx.input_proofs_map.clone();
        pdb.expect_load_proofs()
            .times(1)
            .returning(move |_| Ok(input_proofs_map.clone()));

        let record = fx.record.clone();
        pdb.expect_list_commitments()
            .times(1)
            .returning(move || Ok(vec![record.clone()]));

        let keyset_clone = fx.mintkeyset.clone();
        alpha_connector
            .expect_get_mint_keyset()
            .returning(move |_| Ok(bcr_wallet_core::util::to_keyset(&keyset_clone, None)));

        let blind_sigs = fx.blind_sigs.clone();
        alpha_connector
            .expect_post_swap_committed()
            .times(1)
            .returning(move |_, _, _| Ok(blind_sigs.clone()));

        pdb.expect_store_new().returning(|p| {
            let y = p.y().expect("Hash to curve should not fail");
            Ok(y)
        });

        let commitment_sig = fx.record.commitment;
        pdb.expect_delete_commitment()
            .times(1)
            .withf(move |sig| *sig == commitment_sig)
            .returning(|_| Ok(()));

        let pocket = pocket(Arc::new(pdb), Arc::new(mdb));
        let reclaimed = pocket
            .reclaim_proofs(
                &fx.input_ys,
                &fx.k_infos,
                Arc::new(alpha_connector),
                test_swap_config(),
            )
            .await
            .expect("reclaim should resume via replay");

        assert_eq!(reclaimed, amount);
    }

    #[tokio::test]
    async fn swap_retry_dropped_response_recovers_via_restore() {
        let amount = Amount::from(24u64);
        let fx = swap_commitment_fixture(amount, 0xef, 1000);
        let restored: Vec<(cdk00::BlindedMessage, cdk00::BlindSignature)> = fx
            .record
            .outputs
            .iter()
            .cloned()
            .zip(fx.blind_sigs.iter().cloned())
            .collect();

        let mdb = MockMintMeltRepository::new();
        let mut pdb = MockPocketRepository::new();
        let mut alpha_connector = MockClowderMintConnector::new();

        let input_proofs_map = fx.input_proofs_map.clone();
        pdb.expect_load_proofs()
            .times(1)
            .returning(move |_| Ok(input_proofs_map.clone()));

        let record = fx.record.clone();
        pdb.expect_list_commitments()
            .times(1)
            .returning(move || Ok(vec![record.clone()]));

        let keyset_clone = fx.mintkeyset.clone();
        alpha_connector
            .expect_get_mint_keyset()
            .returning(move |_| Ok(bcr_wallet_core::util::to_keyset(&keyset_clone, None)));

        alpha_connector
            .expect_post_swap_committed()
            .times(1)
            .returning(|_, _, _| Err(Error::MintClientResourceNotFound("gone".to_string())));

        alpha_connector
            .expect_post_restore()
            .times(1)
            .returning(move |_| Ok(restored.clone()));

        pdb.expect_store_new().returning(|p| {
            let y = p.y().expect("Hash to curve should not fail");
            Ok(y)
        });

        let commitment_sig = fx.record.commitment;
        pdb.expect_delete_commitment()
            .times(1)
            .withf(move |sig| *sig == commitment_sig)
            .returning(|_| Ok(()));

        let pocket = pocket(Arc::new(pdb), Arc::new(mdb));
        let reclaimed = pocket
            .reclaim_proofs(
                &fx.input_ys,
                &fx.k_infos,
                Arc::new(alpha_connector),
                test_swap_config(),
            )
            .await
            .expect("reclaim should resume via restore");

        assert_eq!(reclaimed, amount);
    }

    /// Mocks a commitment resume whose replay is rejected, whose restore finds
    /// nothing and whose protest reports the mint Offline; the record must
    /// survive and no input may be marked spent or swapped over again.
    fn offline_resume_pocket(
        fx: &SwapCommitmentFixture,
        mut pdb: MockPocketRepository,
        alpha_connector: &mut MockClowderMintConnector,
    ) -> super::Pocket {
        let mut beta_connector = MockClowderMintConnector::new();

        let record = fx.record.clone();
        pdb.expect_list_commitments()
            .times(1)
            .returning(move || Ok(vec![record.clone()]));
        let record = fx.record.clone();
        pdb.expect_load_commitment()
            .times(1)
            .returning(move |_| Ok(record.clone()));
        pdb.expect_delete_commitment().times(0);
        pdb.expect_store_new().times(0);
        pdb.expect_mark_pending_as_spent().times(0);

        alpha_connector
            .expect_post_swap_committed()
            .times(1)
            .returning(|_, _, _| Err(Error::MintClientResourceNotFound("gone".to_string())));
        alpha_connector
            .expect_post_restore()
            .times(1)
            .returning(|_| Ok(vec![]));
        alpha_connector.expect_get_mint_keyset().times(0);

        beta_connector
            .expect_post_protest_swap()
            .times(1)
            .returning(|_| {
                Ok(wire_swap::SwapProtestResponse {
                    status: wire_common::ProtestStatus::Offline,
                    signatures: None,
                })
            });

        let alpha_id = bitcoin::secp256k1::PublicKey::from_keypair(
            &bitcoin::secp256k1::Keypair::new_global(&mut secp256k1::rand::thread_rng()),
        );
        pocket_with_beta(
            Arc::new(pdb),
            Arc::new(MockMintMeltRepository::new()),
            vec![Arc::new(beta_connector) as Arc<dyn crate::ClowderMintConnector>],
            alpha_id,
        )
    }

    #[tokio::test]
    async fn resume_offline_keeps_inputs_pending() {
        let fx = swap_commitment_fixture(Amount::from(24u64), 0x55, 1000);
        let mut alpha_connector = MockClowderMintConnector::new();

        let mut pdb = MockPocketRepository::new();
        let input_proofs_map = fx.input_proofs_map.clone();
        pdb.expect_load_proofs()
            .times(1)
            .returning(move |_| Ok(input_proofs_map.clone()));

        let pocket = offline_resume_pocket(&fx, pdb, &mut alpha_connector);
        let err = pocket
            .reclaim_proofs(
                &fx.input_ys,
                &fx.k_infos,
                Arc::new(alpha_connector),
                test_swap_config(),
            )
            .await
            .expect_err("an offline mint must not look like a finished zero-amount swap");

        assert!(matches!(err, Error::MintClientServiceUnavailable(_)));
    }

    #[tokio::test]
    async fn recover_pending_stale_proof_resumes_commitment_instead_of_marking_spent() {
        let amount = Amount::from(24u64);
        let fx = swap_commitment_fixture(amount, 0x57, 1000);
        let mut pdb = MockPocketRepository::new();
        let mut alpha_connector = MockClowderMintConnector::new();

        let input_proofs_map = fx.input_proofs_map.clone();
        pdb.expect_list_pending()
            .times(1)
            .returning(move || Ok(input_proofs_map.clone()));
        alpha_connector
            .expect_post_check_state()
            .times(1)
            .returning(|request| {
                Ok(request
                    .ys
                    .iter()
                    .map(|y| cdk07::ProofState {
                        y: *y,
                        state: cdk07::State::Spent,
                        witness: None,
                    })
                    .collect())
            });

        let record = fx.record.clone();
        pdb.expect_list_commitments()
            .times(1)
            .returning(move || Ok(vec![record.clone()]));

        let keyset_clone = fx.mintkeyset.clone();
        alpha_connector
            .expect_get_mint_keyset()
            .returning(move |_| Ok(bcr_wallet_core::util::to_keyset(&keyset_clone, None)));
        let blind_sigs = fx.blind_sigs.clone();
        alpha_connector
            .expect_post_swap_committed()
            .times(1)
            .returning(move |_, _, _| Ok(blind_sigs.clone()));

        pdb.expect_store_new().returning(|p| {
            let y = p.y().expect("Hash to curve should not fail");
            Ok(y)
        });

        let commitment_sig = fx.record.commitment;
        pdb.expect_delete_commitment()
            .times(1)
            .withf(move |sig| *sig == commitment_sig)
            .returning(|_| Ok(()));
        let input_proofs_map = fx.input_proofs_map.clone();
        pdb.expect_mark_pending_as_spent()
            .times(fx.input_ys.len())
            .returning(move |y| Ok(input_proofs_map.get(&y).expect("known stale proof").clone()));

        let pocket = pocket(Arc::new(pdb), Arc::new(MockMintMeltRepository::new()));
        let recovered = pocket
            .recover_pending_stale_proofs(&[], &fx.k_infos, Arc::new(alpha_connector), test_swap_config())
            .await
            .expect("a stale Spent proof with a live commitment must be recovered, not just marked spent");

        assert_eq!(recovered, amount);
    }

    #[tokio::test]
    async fn resume_offline_keeps_inputs_pending_on_stale_spent_recovery() {
        let fx = swap_commitment_fixture(Amount::from(24u64), 0x58, 1000);
        let mut pdb = MockPocketRepository::new();
        let mut alpha_connector = MockClowderMintConnector::new();

        let input_proofs_map = fx.input_proofs_map.clone();
        pdb.expect_list_pending()
            .times(1)
            .returning(move || Ok(input_proofs_map.clone()));
        alpha_connector
            .expect_post_check_state()
            .times(1)
            .returning(|request| {
                Ok(request
                    .ys
                    .iter()
                    .map(|y| cdk07::ProofState {
                        y: *y,
                        state: cdk07::State::Spent,
                        witness: None,
                    })
                    .collect())
            });

        let pocket = offline_resume_pocket(&fx, pdb, &mut alpha_connector);
        let err = pocket
            .recover_pending_stale_proofs(
                &[],
                &fx.k_infos,
                Arc::new(alpha_connector),
                test_swap_config(),
            )
            .await
            .expect_err("an offline mint must not mark a stale Spent proof's inputs spent");

        assert!(matches!(err, Error::MintClientServiceUnavailable(_)));
    }

    #[tokio::test]
    async fn resume_offline_keeps_inputs_pending_on_stale_recovery() {
        let fx = swap_commitment_fixture(Amount::from(24u64), 0x56, 1000);
        let mut pdb = MockPocketRepository::new();
        let mut alpha_connector = MockClowderMintConnector::new();

        let input_proofs_map = fx.input_proofs_map.clone();
        pdb.expect_list_pending()
            .times(1)
            .returning(move || Ok(input_proofs_map.clone()));
        alpha_connector
            .expect_post_check_state()
            .times(1)
            .returning(|request| {
                Ok(request
                    .ys
                    .iter()
                    .map(|y| cdk07::ProofState {
                        y: *y,
                        state: cdk07::State::Unspent,
                        witness: None,
                    })
                    .collect())
            });

        let pocket = offline_resume_pocket(&fx, pdb, &mut alpha_connector);
        let err = pocket
            .recover_pending_stale_proofs(
                &[],
                &fx.k_infos,
                Arc::new(alpha_connector),
                test_swap_config(),
            )
            .await
            .expect_err("an offline mint must not mark the stale inputs spent");

        assert!(matches!(err, Error::MintClientServiceUnavailable(_)));
    }

    #[tokio::test]
    async fn expired_commitment_already_signed_is_recovered_not_deleted() {
        let amount = Amount::from(24u64);
        let fx = swap_commitment_fixture(amount, 0x22, 500);

        let mdb = MockMintMeltRepository::new();
        let mut pdb = MockPocketRepository::new();
        let mut alpha_connector = MockClowderMintConnector::new();

        let record = fx.record.clone();
        pdb.expect_list_commitments()
            .times(1)
            .returning(move || Ok(vec![record.clone()]));

        let keyset_clone = fx.mintkeyset.clone();
        alpha_connector
            .expect_get_mint_keyset()
            .returning(move |_| Ok(bcr_wallet_core::util::to_keyset(&keyset_clone, None)));

        let blind_sigs = fx.blind_sigs.clone();
        alpha_connector
            .expect_post_swap_committed()
            .times(1)
            .returning(move |_, _, _| Ok(blind_sigs.clone()));

        pdb.expect_store_new().returning(|p| {
            let y = p.y().expect("Hash to curve should not fail");
            Ok(y)
        });

        let commitment_sig = fx.record.commitment;
        pdb.expect_delete_commitment()
            .times(1)
            .withf(move |sig| *sig == commitment_sig)
            .returning(|_| Ok(()));

        let pocket = pocket(Arc::new(pdb), Arc::new(mdb));
        pocket
            .check_pending_commitments(
                1000,
                &fx.k_infos,
                Arc::new(alpha_connector),
                test_swap_config(),
            )
            .await
            .expect("check should recover the expired commitment rather than just delete it");
    }

    #[tokio::test]
    async fn expired_commitment_offline_is_kept() {
        let amount = Amount::from(24u64);
        let fx = swap_commitment_fixture(amount, 0x33, 500);

        let mdb = MockMintMeltRepository::new();
        let mut pdb = MockPocketRepository::new();
        let mut alpha_connector = MockClowderMintConnector::new();
        let mut beta_connector = MockClowderMintConnector::new();

        let record = fx.record.clone();
        pdb.expect_list_commitments()
            .times(1)
            .returning(move || Ok(vec![record.clone()]));
        let record = fx.record.clone();
        pdb.expect_load_commitment()
            .times(1)
            .returning(move |_| Ok(record.clone()));

        alpha_connector
            .expect_post_swap_committed()
            .times(1)
            .returning(|_, _, _| Err(Error::MintClientResourceNotFound("gone".to_string())));
        alpha_connector
            .expect_post_restore()
            .times(1)
            .returning(|_| Ok(vec![]));

        beta_connector
            .expect_post_protest_swap()
            .times(1)
            .returning(|_| {
                Ok(wire_swap::SwapProtestResponse {
                    status: wire_common::ProtestStatus::Offline,
                    signatures: None,
                })
            });

        pdb.expect_delete_commitment().times(0);

        let alpha_id = bitcoin::secp256k1::PublicKey::from_keypair(
            &bitcoin::secp256k1::Keypair::new_global(&mut secp256k1::rand::thread_rng()),
        );
        let pocket = pocket_with_beta(
            Arc::new(pdb),
            Arc::new(mdb),
            vec![Arc::new(beta_connector) as Arc<dyn crate::ClowderMintConnector>],
            alpha_id,
        );
        pocket
            .check_pending_commitments(
                1000,
                &fx.k_infos,
                Arc::new(alpha_connector),
                test_swap_config(),
            )
            .await
            .expect("an offline mint must not fail the check, only keep the record pending");
    }

    #[tokio::test]
    async fn expired_commitment_never_executed_is_deleted() {
        let amount = Amount::from(24u64);
        let fx = swap_commitment_fixture(amount, 0x44, 500);

        let mdb = MockMintMeltRepository::new();
        let mut pdb = MockPocketRepository::new();
        let mut alpha_connector = MockClowderMintConnector::new();
        let mut beta_connector = MockClowderMintConnector::new();

        let record = fx.record.clone();
        pdb.expect_list_commitments()
            .times(1)
            .returning(move || Ok(vec![record.clone()]));
        let record = fx.record.clone();
        pdb.expect_load_commitment()
            .times(1)
            .returning(move |_| Ok(record.clone()));

        alpha_connector
            .expect_post_swap_committed()
            .times(1)
            .returning(|_, _, _| {
                Err(Error::MintClientBadRequest(
                    "commitment has expired".to_string(),
                ))
            });
        alpha_connector
            .expect_post_restore()
            .times(1)
            .returning(|_| Ok(vec![]));

        beta_connector
            .expect_post_protest_swap()
            .times(1)
            .returning(|_| {
                Ok(wire_swap::SwapProtestResponse {
                    status: wire_common::ProtestStatus::Rabid,
                    signatures: None,
                })
            });

        let commitment_sig = fx.record.commitment;
        pdb.expect_delete_commitment()
            .times(1)
            .withf(move |sig| *sig == commitment_sig)
            .returning(|_| Ok(()));

        let alpha_id = bitcoin::secp256k1::PublicKey::from_keypair(
            &bitcoin::secp256k1::Keypair::new_global(&mut secp256k1::rand::thread_rng()),
        );
        let pocket = pocket_with_beta(
            Arc::new(pdb),
            Arc::new(mdb),
            vec![Arc::new(beta_connector) as Arc<dyn crate::ClowderMintConnector>],
            alpha_id,
        );
        pocket
            .check_pending_commitments(
                1000,
                &fx.k_infos,
                Arc::new(alpha_connector),
                test_swap_config(),
            )
            .await
            .expect("a commitment the mint never executed must be deleted, not kept forever");
    }

    #[tokio::test]
    async fn expired_receive_commitment_is_recovered_via_protest() {
        // A receive's inputs are the sender's proofs: receive_proofs never stores them
        // locally, so unlike every other swap path, the real repository here never has
        // them under their Ys. If post_swap_committed's response is lost and the user
        // never retries before the commitment expires, check_pending_commitments must
        // still be able to replay/restore/protest it using what the commitment itself
        // persisted, not what the (still empty) proof table has.
        let (info, mint_keyset) = core_tests::generate_random_ecash_keyset();
        let keysets_info = test_kinfos(info);
        let sender_proofs = core_tests::generate_random_ecash_proofs(
            &mint_keyset,
            &[Amount::from(16u64), Amount::from(8u64)],
        );
        let total_amount = Amount::from(24u64);

        let pdb = Arc::new(
            bcr_wallet_persistence::redb::pocket::PocketDB::in_memory("wallet", &CurrencyUnit::Sat)
                .expect("in-memory pocket db"),
        );

        let committed_outputs: Arc<Mutex<Option<Vec<cdk00::BlindedMessage>>>> =
            Arc::new(Mutex::new(None));
        let committed_outputs_clone = committed_outputs.clone();

        let mut receive_client = MockClowderMintConnector::new();
        let keyset_for_receive = mint_keyset.clone();
        receive_client
            .expect_get_mint_keyset()
            .returning(move |_| Ok(bcr_wallet_core::util::to_keyset(&keyset_for_receive, None)));
        receive_client
            .expect_post_swap_commitment()
            .times(1)
            .returning(move |inputs, outputs, _, _, _| {
                let mut result = mock_commitment_result();
                result.inputs_ys = inputs.iter().map(|p| p.y().unwrap()).collect();
                result.outputs = outputs.clone();
                // already expired by the time check_pending_commitments runs below
                result.expiry = 100;
                *committed_outputs_clone.lock().unwrap() = Some(outputs);
                Ok(result)
            });
        receive_client
            .expect_post_swap_committed()
            .times(1)
            .returning(|_, _, _| {
                Err(Error::Transport(
                    bcr_wallet_transport::error::Error::Network("connection dropped".to_string()),
                ))
            });

        let mut beta_connector = MockClowderMintConnector::new();
        setup_attestation_mock(&mut beta_connector);
        let signing_keyset = mint_keyset.clone();
        beta_connector
            .expect_post_protest_swap()
            .times(1)
            .returning(move |_req| {
                let outputs = committed_outputs
                    .lock()
                    .unwrap()
                    .clone()
                    .expect("commitment was stored before the protest");
                let amounts: Vec<_> = outputs.iter().map(|o| o.amount).collect();
                Ok(wire_swap::SwapProtestResponse {
                    status: wire_common::ProtestStatus::Resolved,
                    signatures: Some(core_tests::generate_ecash_signatures(
                        &signing_keyset,
                        &amounts,
                    )),
                })
            });

        let alpha_id = bitcoin::secp256k1::PublicKey::from_keypair(
            &bitcoin::secp256k1::Keypair::new_global(&mut secp256k1::rand::thread_rng()),
        );
        let seed: Seed = [3u8; 64];
        let beta_provider = crate::pocket::RandomBetaProvider::new(
            vec![Arc::new(beta_connector) as Arc<dyn crate::ClowderMintConnector>],
            alpha_id,
        )
        .expect("can create beta provider");
        let pocket = super::Pocket::new(
            CurrencyUnit::Sat,
            pdb.clone(),
            Arc::new(MockMintMeltRepository::new()),
            seed,
            Arc::new(beta_provider),
        );

        pocket
            .receive_proofs(
                Arc::new(receive_client),
                &keysets_info,
                sender_proofs,
                test_swap_config(),
            )
            .await
            .expect_err("a dropped post_swap_committed response must surface as an error");

        let pending = pdb.list_commitments().await.expect("list commitments");
        assert_eq!(
            pending.len(),
            1,
            "the dropped receive left its commitment pending"
        );
        assert!(
            pdb.list_unspent().await.expect("list unspent").is_empty(),
            "nothing is credited yet"
        );

        // A fresh retry client: the mint genuinely never executed the commitment
        // (replay is rejected, restore finds nothing), so recovery must fall through
        // to protest using the commitment's own stored inputs.
        let swap_committed_calls = Arc::new(Mutex::new(0u32));
        let swap_committed_calls_clone = swap_committed_calls.clone();
        let retry_signing_keyset = mint_keyset.clone();
        let mut retry_client = MockClowderMintConnector::new();
        let keyset_for_retry = mint_keyset.clone();
        retry_client
            .expect_get_mint_keyset()
            .returning(move |_| Ok(bcr_wallet_core::util::to_keyset(&keyset_for_retry, None)));
        retry_client
            .expect_post_swap_committed()
            .returning(move |_, outputs, _| {
                let mut calls = swap_committed_calls_clone.lock().unwrap();
                *calls += 1;
                if *calls == 1 {
                    // the resumed replay of the original (expired) commitment
                    Err(Error::MintClientResourceNotFound("gone".to_string()))
                } else {
                    // the fresh swap digest_proofs runs over the protest's unblinded proofs
                    let amounts: Vec<_> = outputs.iter().map(|o| o.amount).collect();
                    Ok(core_tests::generate_ecash_signatures(
                        &retry_signing_keyset,
                        &amounts,
                    ))
                }
            });
        retry_client
            .expect_post_restore()
            .times(1)
            .returning(|_| Ok(vec![]));
        retry_client
            .expect_post_swap_commitment()
            .times(1)
            .returning(|inputs, outputs, _, _, _| {
                let mut result = mock_commitment_result();
                result.inputs_ys = inputs.iter().map(|p| p.y().unwrap()).collect();
                result.outputs = outputs;
                Ok(result)
            });

        pocket
            .check_pending_commitments(
                200,
                &keysets_info,
                Arc::new(retry_client),
                test_swap_config(),
            )
            .await
            .expect("recovery must not fail even though the inputs were never stored locally");

        assert!(
            pdb.list_commitments()
                .await
                .expect("list commitments")
                .is_empty(),
            "the resolved commitment must be deleted"
        );
        let credited: Amount = pdb
            .list_unspent()
            .await
            .expect("list unspent")
            .values()
            .fold(Amount::ZERO, |acc, p| acc + p.amount);
        assert_eq!(
            credited, total_amount,
            "the received amount must be credited after the protest resolves"
        );
    }

    /// A substitute swap whose `post_swap_committed` response was dropped, left
    /// behind in a real repository the way a first attempt leaves it.
    struct DroppedSubstituteSwap {
        pdb: Arc<bcr_wallet_persistence::redb::pocket::PocketDB>,
        seed: Seed,
        mint_keyset: ecash::MintKeySet,
        keysets_info: HashMap<ecash::Id, KeySetInfo>,
        keysets: HashMap<ecash::Id, KeySet>,
        input_proofs: Vec<cdk00::Proof>,
        substitute_clowder_id: secp256k1::PublicKey,
        other_substitute_clowder_id: secp256k1::PublicKey,
        other_commitment: secp256k1::schnorr::Signature,
        outputs: Vec<cdk00::BlindedMessage>,
        commitment: secp256k1::schnorr::Signature,
        send_amount: Amount,
    }

    fn substitute_id() -> secp256k1::PublicKey {
        secp256k1::PublicKey::from_keypair(&secp256k1::Keypair::new_global(
            &mut secp256k1::rand::thread_rng(),
        ))
    }

    fn unused_beta_provider(alpha_id: secp256k1::PublicKey) -> RandomBetaProvider {
        RandomBetaProvider::new(
            vec![Arc::new(MockClowderMintConnector::new()) as Arc<dyn crate::ClowderMintConnector>],
            alpha_id,
        )
        .expect("can create beta provider")
    }

    fn with_dleq(proofs: &[cdk00::Proof]) -> Vec<cdk00::Proof> {
        proofs
            .iter()
            .cloned()
            .map(|mut proof| {
                proof.dleq = Some(cashu::ProofDleq::new(
                    cashu::SecretKey::generate(),
                    cashu::SecretKey::generate(),
                    cashu::SecretKey::generate(),
                ));
                proof
            })
            .collect()
    }

    async fn dropped_substitute_swap(expiry: u64) -> DroppedSubstituteSwap {
        let (info, mint_keyset) = core_tests::generate_random_ecash_keyset();
        let kid = info.id;
        let keysets_info = test_kinfos(info);
        let keysets = HashMap::from([(kid, bcr_wallet_core::util::to_keyset(&mint_keyset, None))]);
        let input_proofs = core_tests::generate_random_ecash_proofs(
            &mint_keyset,
            &[Amount::from(8u64), Amount::from(2u64)],
        );
        let input_ys: Vec<cashu::PublicKey> = input_proofs.iter().map(|p| p.y().unwrap()).collect();
        let substitute_clowder_id = substitute_id();
        let other_substitute_clowder_id = substitute_id();
        let seed: Seed = [7u8; 64];

        let pdb = Arc::new(
            bcr_wallet_persistence::redb::pocket::PocketDB::in_memory("wallet", &CurrencyUnit::Sat)
                .expect("in-memory pocket db"),
        );
        let mut other = swap_commitment_fixture(Amount::from(10u64), 0x52, u64::MAX).record;
        other.inputs = input_ys.clone();
        other.substitute_clowder_id = Some(other_substitute_clowder_id);
        let other_commitment = other.commitment;
        pdb.store_commitment(other).await.unwrap();

        let committed = Arc::new(Mutex::new(None));
        let committed_clone = committed.clone();
        let mut substitute_client = MockClowderMintConnector::new();
        substitute_client
            .expect_post_swap_commitment()
            .times(1)
            .returning(move |inputs, outputs, _, _, _| {
                let mut result = mock_commitment_result();
                result.inputs_ys = inputs.iter().map(|p| p.y().unwrap()).collect();
                result.outputs = outputs.clone();
                result.expiry = expiry;
                *committed_clone.lock().unwrap() = Some((outputs, result.commitment));
                Ok(result)
            });
        substitute_client
            .expect_post_swap_committed()
            .times(1)
            .returning(|_, _, _| {
                Err(Error::MintClientServiceUnavailable(
                    "connection reset".to_string(),
                ))
            });

        let swap_config = test_swap_config();
        let mut beta_connector = MockClowderMintConnector::new();
        setup_attestation_mock(&mut beta_connector);
        let beta_provider = RandomBetaProvider::new(
            vec![Arc::new(beta_connector) as Arc<dyn crate::ClowderMintConnector>],
            substitute_clowder_id,
        )
        .expect("can create beta provider");

        let send_amount = Amount::from(8u64);
        let pocket = super::Pocket::new(
            CurrencyUnit::Sat,
            pdb.clone(),
            Arc::new(MockMintMeltRepository::new()),
            seed,
            Arc::new(test_beta_provider()),
        );
        pocket
            .swap_to_unlocked_substitute_proofs(
                input_proofs.clone(),
                &keysets_info,
                keysets.clone(),
                Arc::new(substitute_client),
                substitute_clowder_id,
                beta_provider,
                send_amount,
                swap_config,
            )
            .await
            .expect_err("a dropped post_swap_committed response fails the swap");

        let (outputs, commitment) = committed.lock().unwrap().take().expect("committed once");
        DroppedSubstituteSwap {
            pdb,
            seed,
            mint_keyset,
            keysets_info,
            keysets,
            input_proofs,
            substitute_clowder_id,
            other_substitute_clowder_id,
            other_commitment,
            outputs,
            commitment,
            send_amount,
        }
    }

    impl DroppedSubstituteSwap {
        fn pocket(&self) -> super::Pocket {
            super::Pocket::new(
                CurrencyUnit::Sat,
                self.pdb.clone(),
                Arc::new(MockMintMeltRepository::new()),
                self.seed,
                Arc::new(test_beta_provider()),
            )
        }

        fn signatures(&self) -> Vec<cdk00::BlindSignature> {
            self.outputs
                .iter()
                .map(|bm| bcr_common::core::signature::sign_ecash(&self.mint_keyset, bm).unwrap())
                .collect()
        }

        async fn retry(
            &self,
            substitute_client: MockClowderMintConnector,
            beta_provider: RandomBetaProvider,
        ) -> Result<Vec<cdk00::Proof>> {
            self.pocket()
                .swap_to_unlocked_substitute_proofs(
                    with_dleq(&self.input_proofs),
                    &self.keysets_info,
                    self.keysets.clone(),
                    Arc::new(substitute_client),
                    self.substitute_clowder_id,
                    beta_provider,
                    self.send_amount,
                    test_swap_config(),
                )
                .await
        }

        async fn substitute_commitments(&self) -> Vec<secp256k1::schnorr::Signature> {
            self.pdb
                .list_substitute_commitments(self.substitute_clowder_id)
                .await
                .unwrap()
                .into_iter()
                .map(|r| r.commitment)
                .collect()
        }

        async fn assert_recovered(&self, on_target: Vec<cdk00::Proof>) {
            assert_eq!(on_target.total_amount().unwrap(), Amount::from(8u64));
            let change = self.pdb.load_foreign_mint_proofs().await.unwrap();
            assert_eq!(change.len(), 1);
            assert_eq!(change[0].clowder_id, self.substitute_clowder_id);
            assert_eq!(change[0].proof.amount, Amount::from(2u64));
            assert!(matches!(
                change[0].reason,
                ForeignMintProofReason::MintOffline
            ));
            assert!(self.substitute_commitments().await.is_empty());
            let other = self
                .pdb
                .list_substitute_commitments(self.other_substitute_clowder_id)
                .await
                .unwrap();
            assert_eq!(other.len(), 1);
            assert_eq!(other[0].commitment, self.other_commitment);
        }

        fn restore_returning(
            &self,
            substitute_client: &mut MockClowderMintConnector,
            signatures: Vec<cdk00::BlindSignature>,
        ) {
            let outputs = self.outputs.clone();
            substitute_client
                .expect_post_restore()
                .times(1)
                .returning(move |request| {
                    assert_eq!(request.outputs, outputs);
                    Ok(outputs.iter().cloned().zip(signatures.clone()).collect())
                });
            let keyset = bcr_wallet_core::util::to_keyset(&self.mint_keyset, None);
            substitute_client
                .expect_get_mint_keyset()
                .returning(move |_| Ok(keyset.clone()));
        }
    }

    #[tokio::test]
    async fn substitute_swap_retry_replays_commitment() {
        let fx = dropped_substitute_swap(u64::MAX).await;

        let stored = fx
            .pdb
            .list_substitute_commitments(fx.substitute_clowder_id)
            .await
            .unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].commitment, fx.commitment);
        assert_eq!(stored[0].outputs, fx.outputs);
        assert_eq!(
            stored[0].substitute_clowder_id,
            Some(fx.substitute_clowder_id)
        );
        assert!(fx.pdb.list_commitments().await.unwrap().is_empty());

        let mut substitute_client = MockClowderMintConnector::new();
        substitute_client.expect_post_swap_commitment().times(0);
        let expected_outputs = fx.outputs.clone();
        let expected_commitment = fx.commitment;
        let expected_inputs = fx.input_proofs.clone();
        let signatures = fx.signatures();
        substitute_client
            .expect_post_swap_committed()
            .times(1)
            .returning(move |inputs, outputs, commitment| {
                assert!(inputs.iter().all(|p| p.dleq.is_none()));
                let ys: Vec<_> = inputs.iter().map(|p| p.y().unwrap()).collect();
                let expected_ys: Vec<_> = expected_inputs.iter().map(|p| p.y().unwrap()).collect();
                assert_eq!(ys, expected_ys);
                assert_eq!(outputs, expected_outputs);
                assert_eq!(commitment, expected_commitment);
                Ok(signatures.clone())
            });

        let on_target = fx
            .retry(
                substitute_client,
                unused_beta_provider(fx.substitute_clowder_id),
            )
            .await
            .expect("the retry replays the stored commitment");
        fx.assert_recovered(on_target).await;
    }

    #[tokio::test]
    async fn substitute_swap_retry_falls_back_to_restore() {
        // live commitment, replay rejected: restore recovers the signed outputs
        let fx = dropped_substitute_swap(u64::MAX).await;
        let mut substitute_client = MockClowderMintConnector::new();
        substitute_client.expect_post_swap_commitment().times(0);
        substitute_client
            .expect_post_swap_committed()
            .times(1)
            .returning(|_, _, _| Err(Error::MintClientBadRequest("already spent".to_string())));
        fx.restore_returning(&mut substitute_client, fx.signatures());
        let on_target = fx
            .retry(
                substitute_client,
                unused_beta_provider(fx.substitute_clowder_id),
            )
            .await
            .expect("restore recovers the replay-rejected commitment");
        fx.assert_recovered(on_target).await;

        // expired commitment: no replay, restore recovers the signed outputs
        let fx = dropped_substitute_swap(1).await;
        let mut substitute_client = MockClowderMintConnector::new();
        substitute_client.expect_post_swap_commitment().times(0);
        substitute_client.expect_post_swap_committed().times(0);
        fx.restore_returning(&mut substitute_client, fx.signatures());
        let on_target = fx
            .retry(
                substitute_client,
                unused_beta_provider(fx.substitute_clowder_id),
            )
            .await
            .expect("restore recovers the expired commitment");
        fx.assert_recovered(on_target).await;

        // live commitment, replay rejected and nothing to restore: kept, retry fails
        let fx = dropped_substitute_swap(u64::MAX).await;
        let mut substitute_client = MockClowderMintConnector::new();
        substitute_client.expect_post_swap_commitment().times(0);
        substitute_client
            .expect_post_swap_committed()
            .times(1)
            .returning(|_, _, _| Err(Error::MintClientBadRequest("already spent".to_string())));
        fx.restore_returning(&mut substitute_client, vec![]);
        fx.retry(
            substitute_client,
            unused_beta_provider(fx.substitute_clowder_id),
        )
        .await
        .expect_err("an unresolved live commitment must not be swapped over");
        assert_eq!(fx.substitute_commitments().await, vec![fx.commitment]);

        // expired commitment the mint never executed: deleted, fresh swap runs
        let fx = dropped_substitute_swap(1).await;
        let mut substitute_client = MockClowderMintConnector::new();
        fx.restore_returning(&mut substitute_client, vec![]);
        substitute_client
            .expect_post_swap_commitment()
            .times(1)
            .returning(|inputs, outputs, _, _, _| {
                let mut result = mock_commitment_result();
                result.inputs_ys = inputs.iter().map(|p| p.y().unwrap()).collect();
                result.outputs = outputs;
                result.expiry = u64::MAX;
                Ok(result)
            });
        let signing_keyset = fx.mint_keyset.clone();
        substitute_client
            .expect_post_swap_committed()
            .times(1)
            .returning(move |_, outputs, _| {
                Ok(outputs
                    .iter()
                    .map(|bm| bcr_common::core::signature::sign_ecash(&signing_keyset, bm).unwrap())
                    .collect())
            });
        let mut beta_connector = MockClowderMintConnector::new();
        setup_attestation_mock(&mut beta_connector);
        let beta_provider = RandomBetaProvider::new(
            vec![Arc::new(beta_connector) as Arc<dyn crate::ClowderMintConnector>],
            fx.substitute_clowder_id,
        )
        .unwrap();
        let on_target = fx
            .retry(substitute_client, beta_provider)
            .await
            .expect("a fresh swap runs once the expired commitment is gone");
        fx.assert_recovered(on_target).await;
    }

    #[tokio::test]
    async fn substitute_commitment_never_resumed_against_own_mint() {
        let fx = dropped_substitute_swap(1).await;
        for proof in &fx.input_proofs {
            fx.pdb.store_pendingspent(proof.clone()).await.unwrap();
        }
        assert!(fx.pdb.list_commitments().await.unwrap().is_empty());

        let mut own_client = MockClowderMintConnector::new();
        own_client.expect_post_swap_committed().times(0);
        own_client.expect_post_restore().times(0);
        own_client.expect_post_check_state().returning(|req| {
            Ok(req
                .ys
                .iter()
                .map(|y| cdk07::ProofState {
                    y: *y,
                    state: cdk07::State::Spent,
                    witness: None,
                })
                .collect())
        });
        let own_client: Arc<dyn crate::ClowderMintConnector> = Arc::new(own_client);

        let betas: Vec<Arc<dyn crate::ClowderMintConnector>> =
            vec![Arc::new(MockClowderMintConnector::new())];
        let pocket = pocket_with_beta(
            fx.pdb.clone(),
            Arc::new(MockMintMeltRepository::new()),
            betas,
            substitute_id(),
        );

        pocket
            .check_pending_commitments(
                u64::MAX,
                &fx.keysets_info,
                own_client.clone(),
                test_swap_config(),
            )
            .await
            .unwrap();
        pocket
            .recover_pending_stale_proofs(
                &[],
                &fx.keysets_info,
                own_client.clone(),
                test_swap_config(),
            )
            .await
            .unwrap();
        pocket
            .protest_swap(
                fx.commitment,
                &fx.keysets_info,
                own_client,
                test_swap_config(),
            )
            .await
            .expect_err("a substitute commitment is never protested at the own mint");

        assert_eq!(fx.substitute_commitments().await, vec![fx.commitment]);
    }
}
