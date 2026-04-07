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
        nut00 as cdk00, nut01 as cdk01, nut05 as cdk05,
    },
    wire::{
        common as wire_common,
        melt::{self as wire_melt, MeltTx},
        mint as wire_mint,
        swap as wire_swap,
    },
};
use bcr_wallet_core::types::{MeltSummary, MintSummary, Seed, SendSummary};
use bcr_wallet_persistence::{MintMeltRepository, PocketRepository};
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
    ) -> Result<(MeltTx, HashMap<cashu::PublicKey, cashu::Proof>)>;
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
    ) -> Result<(
        wire_mint::ProtestStatus,
        Option<(cashu::Amount, Vec<cashu::PublicKey>)>,
    )>;
    async fn check_pending_commitments(&self, tstamp: u64) -> Result<()>;
    async fn protest_swap(
        &self,
        commitment_sig: bitcoin::secp256k1::schnorr::Signature,
        keysets_info: &[KeySetInfo],
        alpha_client: Arc<dyn ClowderMintConnector>,
        beta_client: Arc<dyn ClowderMintConnector>,
        alpha_id: bitcoin::secp256k1::PublicKey,
        swap_config: SwapConfig,
    ) -> Result<(
        wire_common::ProtestStatus,
        Option<(cashu::Amount, Vec<cashu::PublicKey>)>,
    )>;
}

struct MeltReference {
    rid: Uuid,
    send_proofs: Vec<cdk01::PublicKey>,
    swap_proof: Option<(Amount, cdk01::PublicKey)>,
    reserved_fees: Amount,
    mint_quote: String,
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

    fn find_active_keysetid(&self, keysets_info: &[KeySetInfo]) -> Result<cashu::KeySetInfo> {
        let active_info = keysets_info
            .iter()
            .find(|info| info.unit == self.unit && info.active && info.input_fee_ppk == 0);
        let Some(active_info) = active_info else {
            return Err(Error::NoActiveKeyset);
        };
        Ok(active_info.clone())
    }

    async fn find_active_keyset(
        &self,
        keysets_info: &[KeySetInfo],
        client: &Arc<dyn ClowderMintConnector>,
    ) -> Result<(KeySetInfo, KeySet)> {
        let active_info = self.find_active_keysetid(keysets_info)?;
        let active_keyset = client.get_mint_keyset(active_info.id).await?;
        Ok((active_info, active_keyset))
    }

    async fn digest_proofs(
        &self,
        client: Arc<dyn ClowderMintConnector>,
        (active_info, active_keyset): (cashu::KeySetInfo, cashu::KeySet),
        inputs: HashMap<cdk01::PublicKey, cdk00::Proof>,
        swap_config: SwapConfig,
    ) -> Result<(Amount, Vec<cdk01::PublicKey>)> {
        if inputs.is_empty() {
            tracing::warn!("DbPocket::digest_proofs: empty inputs");
            return Ok((Amount::ZERO, Vec::new()));
        }
        let (ys, proofs): (Vec<cdk01::PublicKey>, Vec<cdk00::Proof>) = inputs.into_iter().unzip();
        let counter = self.pdb.counter(active_info.id).await?;
        let total_amount = proofs.total_amount()?;
        let premint_secrets = cdk00::PreMintSecrets::from_seed(
            active_info.id,
            counter,
            &self.seed,
            total_amount,
            &SplitTarget::None,
        )?;
        self.pdb
            .increment_counter(active_info.id, counter, premint_secrets.len() as u32)
            .await?;
        let premints = HashMap::from([(active_info.id, premint_secrets)]);
        let keysets = HashMap::from([(active_info.id, active_keyset)]);
        let cashed_in = swap(
            self.unit.clone(),
            proofs,
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
        target: Amount,
        keysets_info: &[KeySetInfo],
    ) -> Result<(SendSummary, SendReference)> {
        let proofs = self.pdb.list_unspent().await?;
        let infos = collect_keyset_infos_from_proofs(proofs.values(), keysets_info)?;
        let ys = group_ys_by_keyset_id(proofs.iter());
        let mut kids: Vec<cashu::Id> = Vec::with_capacity(infos.len());
        for (kid, info) in infos.iter() {
            if info.unit == self.unit && info.input_fee_ppk == 0 {
                kids.push(*kid);
            }
        }
        let mut current_amount = Amount::ZERO;
        let mut pocket_summary = SendSummary::new();
        pocket_summary.amount = target;
        pocket_summary.unit = self.unit.clone();
        let mut send_ref = SendReference {
            rid: pocket_summary.request_id,
            ..Default::default()
        };

        for kid in kids {
            let kid_ys = ys.get(&kid).cloned().unwrap_or_default();
            for y in kid_ys {
                let proof = proofs.get(&y).expect("proof should be here");
                if current_amount + proof.amount > target {
                    send_ref.swap_proof = Some((target - current_amount, y));
                    return Ok((pocket_summary, send_ref));
                } else if current_amount + proof.amount == target {
                    send_ref.send_proofs.push(y);
                    return Ok((pocket_summary, send_ref));
                } else {
                    send_ref.send_proofs.push(y);
                    current_amount += proof.amount;
                }
            }
        }
        Err(Error::InsufficientFunds)
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
        let (active_keyset_info, active_keyset) =
            self.find_active_keyset(keysets_info, &client).await?;

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

        self.digest_proofs(
            client,
            (active_keyset_info, active_keyset),
            proofs,
            swap_config,
        )
        .await
    }

    async fn check_pending_mint(
        &self,
        qid: Uuid,
        keysets_info: &[KeySetInfo],
        client: Arc<dyn ClowderMintConnector>,
        tstamp: u64,
        swap_config: SwapConfig,
        clowder_id: bitcoin::secp256k1::PublicKey,
    ) -> Result<Option<(cashu::Amount, Vec<cashu::PublicKey>)>> {
        let record = self.mdb.load_mint(qid).await?;
        let (mint_summary, premint) = (record.summary, record.premint);
        let mint_state = client.get_mint_quote_onchain(qid.to_string()).await?;
        let body: wire_mint::OnchainMintQuoteResponseBody =
            bcr_common::core::signature::deserialize_borsh_msg(&mint_state.content)?;

        if body.expiry < tstamp {
            tracing::info!("Mint request with id {qid} expired - deleting.");
            self.mdb.delete_mint(qid).await?;
            return Ok(None);
        }

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

    async fn balance(&self) -> Result<cashu::Amount> {
        let proofs: Vec<Proof> = self.pdb.list_unspent().await?.into_values().collect();
        let total = proofs.total_amount()?;
        Ok(total)
    }

    async fn receive_proofs(
        &self,
        client: Arc<dyn ClowderMintConnector>,
        keysets_info: &[KeySetInfo],
        inputs: Vec<cdk00::Proof>,
        swap_config: SwapConfig,
    ) -> Result<(Amount, Vec<cdk01::PublicKey>)> {
        // storing proofs in pending state
        let mut proofs: HashMap<cdk01::PublicKey, cdk00::Proof> =
            HashMap::with_capacity(inputs.len());
        for input in inputs.into_iter() {
            let y = input.y()?;
            proofs.insert(y, input);
        }
        let active_keys = self.find_active_keyset(keysets_info, &client).await?;
        self.digest_proofs(client, active_keys, proofs, swap_config)
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
        let info = self.find_active_keysetid(keysets_info)?;
        let sending_proofs = send_proofs(
            send_ref.send_proofs,
            send_ref.swap_proof,
            &self.seed,
            self.pdb.as_ref(),
            &client,
            Some(info.id),
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
        let proofs_to_send = return_proofs_to_send_for_offline_payment(
            send_ref.send_proofs,
            send_ref.swap_proof,
            self.pdb.as_ref(),
        )
        .await?;
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
        tracing::debug!(
            "Swapping to unlocked substitute debit proofs - {change_amount} will be lost."
        );
        // handle keyset
        let (active_info, active_keyset) = self.find_active_keyset(keysets_info, &client).await?;
        // calculate splits
        let send_splits = send_amount.split();
        let send_splits_len = send_splits.len();
        let change_splits = change_amount.split();
        let mut splits: Vec<Amount> = Vec::with_capacity(send_splits.len() + change_splits.len());
        splits.extend(send_splits);
        splits.extend(change_splits);
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
        let active_keys = self.find_active_keyset(keysets_info, &client).await?;
        let (reclaimed, _) = self
            .digest_proofs(client, active_keys, pendings, swap_config)
            .await?;
        tracing::debug!(
            "DbPocket::reclaim_proofs: pendings: {pendings_len} reclaimed: {reclaimed}"
        );
        Ok(reclaimed)
    }

    async fn prepare_onchain_melt(
        &self,
        invoice: wire_melt::OnchainInvoice,
        keysets_info: &[KeySetInfo],
        client: Arc<dyn ClowderMintConnector>,
    ) -> Result<MeltSummary> {
        let request = wire_melt::MeltQuoteOnchainRequest {
            request: invoice,
            unit: self.unit.clone(),
            change: Vec::new(),
        };
        let response = client.post_melt_quote_onchain(request).await?;
        let total_amount = response.amount + response.fee_reserve;
        let (sendsummary, send_ref) = self
            .compute_send_costs(Amount::from(total_amount.to_sat()), keysets_info)
            .await?;

        let mut summary = MeltSummary::new();
        summary.amount = sendsummary.amount;
        summary.fees = sendsummary.send_fees + sendsummary.swap_fees;
        summary.reserved_fees = Amount::from(response.fee_reserve.to_sat());
        summary.expiry = response.expiry;
        let melt_ref = MeltReference {
            rid: summary.request_id,
            mint_quote: response.quote.to_string(),
            send_proofs: send_ref.send_proofs,
            swap_proof: send_ref.swap_proof,
            reserved_fees: Amount::from(response.fee_reserve.to_sat()),
        };
        self.current_melt.lock().unwrap().replace(melt_ref);
        Ok(summary)
    }

    async fn pay_onchain_melt(
        &self,
        rid: Uuid,
        keysets_info: &[KeySetInfo],
        client: Arc<dyn ClowderMintConnector>,
        swap_config: SwapConfig,
    ) -> Result<(MeltTx, HashMap<cdk01::PublicKey, cdk00::Proof>)> {
        let melt_ref = self.current_melt.lock().unwrap().take();
        let melt_ref = melt_ref.ok_or(Error::NoPrepareRef(rid))?;
        if melt_ref.rid != rid {
            return Err(Error::NoPrepareRef(rid));
        }

        let (info, keyset) = self.find_active_keyset(keysets_info, &client).await?;
        let sending_proofs = send_proofs(
            melt_ref.send_proofs,
            melt_ref.swap_proof,
            &self.seed,
            self.pdb.as_ref(),
            &client,
            Some(info.id),
            swap_config,
        )
        .await?;

        let premints = if melt_ref.reserved_fees != Amount::ZERO {
            let counter = self.pdb.counter(info.id).await?;
            let premints = cdk00::PreMintSecrets::from_seed(
                info.id,
                counter,
                &self.seed,
                melt_ref.reserved_fees,
                &SplitTarget::None,
            )?;
            self.pdb
                .increment_counter(info.id, counter, premints.len() as u32)
                .await?;
            Some(premints)
        } else {
            None
        };

        let request = cdk05::MeltRequest::new(
            melt_ref.mint_quote,
            sending_proofs.values().cloned().collect(),
            premints.clone().map(|p| p.blinded_messages()),
        );
        let response = client.post_melt_onchain(request).await?;

        if !matches!(response.state, cdk05::QuoteState::Paid) {
            return Err(Error::MeltUnpaid(response.quote.to_string()));
        }

        let Some(tx_id) = response.txid else {
            tracing::warn!("DbPocket::pay_melt: did not receive btc transaction id");
            return Err(Error::MeltUnpaid(response.quote.to_string()));
        };

        if let Some(premints) = premints {
            let change = unblind_proofs(&keyset, response.change, premints);
            for proof in change {
                self.pdb.store_new(proof).await?;
            }
        }
        Ok((tx_id, sending_proofs))
    }

    async fn mint_onchain(
        &self,
        amount: bitcoin::Amount,
        keysets_info: &[KeySetInfo],
        client: Arc<dyn ClowderMintConnector>,
        clowder_id: bitcoin::secp256k1::PublicKey,
    ) -> Result<MintSummary> {
        let active_info = self.find_active_keysetid(keysets_info)?;
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
        let request = wire_mint::OnchainMintQuoteRequest {
            blinded_messages: blinded_messages.clone(),
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
        tracing::debug!("check pending commitments for {} entries", commitments.len());
        for record in commitments {
            if record.expiry < tstamp {
                tracing::warn!(
                    "Swap commitment {} expired at {} (now: {tstamp}) - deleting record.",
                    record.commitment, record.expiry,
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
    ) -> Result<(
        wire_mint::ProtestStatus,
        Option<(cashu::Amount, Vec<cashu::PublicKey>)>,
    )> {
        let record = self.mdb.load_mint(qid).await?;

        let request = wire_mint::MintProtestRequest {
            alpha_id: clowder_id,
            quote_id: record.summary.quote_id,
            content: record.content,
            commitment: record.commitment,
        };

        let response = client.post_protest_mint(request).await?;

        match response.status {
            wire_mint::ProtestStatus::Resolved => {
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
                Ok((wire_mint::ProtestStatus::Resolved, Some((amount, ys))))
            }
            wire_mint::ProtestStatus::Rabid => {
                tracing::warn!("Protest for {qid} returned rabid");
                Ok((wire_mint::ProtestStatus::Rabid, None))
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
    ) -> Result<(
        wire_common::ProtestStatus,
        Option<(cashu::Amount, Vec<cashu::PublicKey>)>,
    )> {
        let record = self.pdb.load_commitment(commitment_sig).await?;
        let loaded_proofs = self.pdb.load_proofs(&record.inputs).await?;
        let ephemeral_keypair =
            secp256k1::Keypair::from_secret_key(secp256k1::SECP256K1, &record.ephemeral_secret);

        let protest_body = wire_swap::SwapProtestRequestBody {
            alpha_id,
            proofs: loaded_proofs.into_values().collect(),
            content: record.body_content,
            commitment: record.commitment,
            blind_signatures: None,
        };

        let (body, wallet_signature) =
            bcr_common::core::signature::serialize_n_schnorr_sign_borsh_msg(
                &protest_body,
                &ephemeral_keypair,
            )?;

        let request = wire_swap::SwapProtestRequest {
            body,
            wallet_key: record.wallet_key,
            wallet_signature,
        };

        let response = beta_client.post_protest_swap(request).await?;

        match response.status {
            wire_common::ProtestStatus::Resolved => {
                let signatures = response.signatures.ok_or(Error::MintingError(
                    "swap protest resolved but no signatures returned".to_string(),
                ))?;

                // Use alpha client for keyset lookup — signatures were issued by alpha
                let (active_keyset_info, active_keyset) =
                    self.find_active_keyset(keysets_info, &alpha_client).await?;

                // Unblind using the ORIGINAL premint secrets stored with the commitment
                let all_premints = {
                    let mut secrets = Vec::new();
                    let mut kid = active_keyset_info.id;
                    for (k, ps) in record.premints {
                        kid = k;
                        secrets.extend(ps.secrets);
                    }
                    cdk00::PreMintSecrets {
                        secrets,
                        keyset_id: kid,
                    }
                };
                let unblinded =
                    super::unblind_proofs(&active_keyset, signatures, all_premints);

                let mut proofs: HashMap<cdk01::PublicKey, cdk00::Proof> =
                    HashMap::with_capacity(unblinded.len());
                for proof in unblinded {
                    let y = proof.y()?;
                    proofs.insert(y, proof);
                }

                let (amount, ys) = self
                    .digest_proofs(
                        alpha_client,
                        (active_keyset_info, active_keyset),
                        proofs,
                        swap_config,
                    )
                    .await?;

                self.pdb.delete_commitment(commitment_sig).await?;

                tracing::info!(
                    "Swap protest resolved for {commitment_sig}, received {amount}"
                );
                Ok((wire_common::ProtestStatus::Resolved, Some((amount, ys))))
            }
            wire_common::ProtestStatus::Rabid => {
                tracing::warn!("Swap protest for {commitment_sig} returned rabid");
                Ok((wire_common::ProtestStatus::Rabid, None))
            }
        }
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
    use bcr_common::core_tests;
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
                let amounts = request
                    .outputs
                    .iter()
                    .map(|b| b.amount)
                    .collect::<Vec<_>>();
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
                let amounts = request
                    .outputs
                    .iter()
                    .map(|b| b.amount)
                    .collect::<Vec<_>>();
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
    async fn prepare_onchain_melt() {
        let (info, keyset) = core_tests::generate_random_ecash_keyset();
        let k_infos = vec![KeySetInfo::from(info)];
        let amounts = [Amount::from(8u64), Amount::from(16u64)];
        let proofs = core_tests::generate_random_ecash_proofs(&keyset, &amounts);
        let proofs_map: HashMap<cdk01::PublicKey, cdk00::Proof> =
            HashMap::from_iter(proofs.into_iter().map(|p| {
                let y = p.y().expect("Hash to curve should not fail");
                (y, p)
            }));

        let amount = bitcoin::Amount::from_sat(24);

        let mdb = MockMintMeltRepository::new();
        let mut pdb = MockPocketRepository::new();
        let mut connector = MockMintConnector::new();

        pdb.expect_list_unspent()
            .times(1)
            .returning(move || Ok(proofs_map.clone()));

        connector
            .expect_post_melt_quote_onchain()
            .times(1)
            .returning(move |_| {
                Ok(wire_melt::MeltQuoteOnchainResponse {
                    quote: Uuid::new_v4(),
                    fee_reserve: bitcoin::Amount::ZERO,
                    amount,
                    state: cashu::MeltQuoteState::Pending,
                    expiry: chrono::Utc::now().timestamp() as u64,
                    unit: Some(CurrencyUnit::Sat),
                })
            });

        let invoice = wire_melt::OnchainInvoice {
            amount,
            address: bitcoin::Address::from_str("tb1qteyk7pfvvql2r2zrsu4h4xpvju0nz7ykvguyk0")
                .unwrap(),
        };

        let pocket = pocket(Arc::new(pdb), Arc::new(mdb));

        let summary = pocket
            .prepare_onchain_melt(invoice, &k_infos, Arc::new(connector))
            .await
            .expect("prepare melt works");
        assert_eq!(summary.amount, Amount::from(amount.to_sat()));
    }

    #[tokio::test]
    async fn pay_onchain_melt() {
        let uuid = Uuid::new_v4();
        let tx_id = bitcoin::Txid::from_str(
            "c66bdb3be47c2252cf60bf98da828c595592b91637e4bab88471a7eb76e81562",
        )
        .unwrap();
        let melt_tx = MeltTx {
            alpha_txid: Some(tx_id),
            beta_txid: None,
        };
        let (info, keyset) = core_tests::generate_random_ecash_keyset();
        let kid = info.id;
        let k_infos = vec![KeySetInfo::from(info)];

        let mdb = MockMintMeltRepository::new();
        let pdb = MockPocketRepository::new();
        let mut connector = MockMintConnector::new();

        let cloned_keyset = keyset.clone();
        connector
            .expect_get_mint_keyset()
            .times(1)
            .with(eq(kid))
            .returning(move |_| Ok(KeySet::from(cloned_keyset.clone())));

        let melt_tx_clone = melt_tx.clone();
        connector
            .expect_post_melt_onchain()
            .times(1)
            .returning(move |_| {
                Ok(wire_melt::MeltOnchainResponse {
                    quote: uuid,
                    txid: Some(melt_tx_clone.clone()),
                    state: cashu::MeltQuoteState::Paid,
                    change: Vec::new(),
                })
            });

        let pocket = pocket(Arc::new(pdb), Arc::new(mdb));
        let melt_ref = MeltReference {
            rid: uuid,
            mint_quote: uuid.to_string(),
            send_proofs: vec![],
            swap_proof: None,
            reserved_fees: Amount::ZERO,
        };
        pocket.current_melt.lock().unwrap().replace(melt_ref);

        let res = pocket
            .pay_onchain_melt(uuid, &k_infos, Arc::new(connector), test_swap_config())
            .await
            .expect("pay melt works");
        assert_eq!(res.0.alpha_txid, melt_tx.alpha_txid);
        assert_eq!(res.0.beta_txid, melt_tx.beta_txid);
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
            .returning(|_, _, _, _, _, _, _| Ok(Uuid::new_v4()));

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

        mdb.expect_delete_mint().times(1).returning(move |_| Ok(()));

        connector
            .expect_get_mint_quote_onchain()
            .times(1)
            .returning(move |_| {
                let body = wire_mint::OnchainMintQuoteResponseBody {
                    quote: uuid,
                    address: "tb1qteyk7pfvvql2r2zrsu4h4xpvju0nz7ykvguyk0".to_string(),
                    payment_amount: amount,
                    expiry: 0, // expired
                    blinded_messages: vec![],
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
            })
        });

        connector
            .expect_post_protest_mint()
            .times(1)
            .returning(move |_| {
                Ok(wire_mint::MintProtestResponse {
                    status: wire_mint::ProtestStatus::Resolved,
                    signatures: Some(blind_sigs.clone()),
                })
            });

        let keyset_clone = mintkeyset.clone();
        connector
            .expect_get_mint_keyset()
            .times(1)
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
        let (status, result) = pocket
            .protest_mint(
                uuid,
                &k_infos,
                Arc::new(connector),
                test_swap_config(),
                clowder_id,
            )
            .await
            .expect("protest_mint resolved works");

        assert!(matches!(status, wire_mint::ProtestStatus::Resolved));
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
            })
        });

        connector
            .expect_post_protest_mint()
            .times(1)
            .returning(move |_| {
                Ok(wire_mint::MintProtestResponse {
                    status: wire_mint::ProtestStatus::Rabid,
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
        let (status, result) = pocket
            .protest_mint(
                uuid,
                &k_infos,
                Arc::new(connector),
                test_swap_config(),
                clowder_id,
            )
            .await
            .expect("protest_mint rabid works");

        assert!(matches!(status, wire_mint::ProtestStatus::Rabid));
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
        let premint =
            cdk00::PreMintSecrets::random(kid, amount, &SplitTarget::None).unwrap();
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
        let ephemeral_keypair =
            secp256k1::Keypair::new_global(&mut secp256k1::rand::thread_rng());
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
        pdb.expect_load_commitment()
            .times(1)
            .returning(move |_| {
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
        pdb.expect_counter()
            .with(eq(kid))
            .returning(|_| Ok(0));
        pdb.expect_increment_counter()
            .returning(|_, _, _| Ok(()));
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
        let (status, result) = pocket
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

        let ephemeral_keypair =
            secp256k1::Keypair::new_global(&mut secp256k1::rand::thread_rng());
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
        pdb.expect_load_commitment()
            .times(1)
            .returning(move |_| {
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
        let (status, result) = pocket
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
}
