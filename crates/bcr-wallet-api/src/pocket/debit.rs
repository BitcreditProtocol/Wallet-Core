use crate::{
    ClowderMintConnector,
    error::{Error, Result},
    pocket::*,
    wallet::types::SafeMode,
};
use async_trait::async_trait;
use bcr_common::{
    cashu::{
        self, Amount, CurrencyUnit, KeySet, KeySetInfo, Proof, ProofsMethods, amount::SplitTarget,
        nut00 as cdk00, nut01 as cdk01, nut05 as cdk05,
    },
    wire::{
        melt::{self as wire_melt, MeltTx},
        mint as wire_mint,
    },
};
use bcr_wallet_core::{
    types::{MeltSummary, MintSummary, Seed, SendSummary},
    util::keypair_from_seed,
};
use bcr_wallet_persistence::{MintMeltRepository, PocketRepository};
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
    ) -> Result<(MeltTx, HashMap<cashu::PublicKey, cashu::Proof>)>;
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
        safe_mode: SafeMode,
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
            safe_mode,
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

    async fn check_pending_mint(
        &self,
        qid: Uuid,
        keysets_info: &[KeySetInfo],
        client: Arc<dyn ClowderMintConnector>,
        tstamp: u64,
        safe_mode: SafeMode,
    ) -> Result<Option<(cashu::Amount, Vec<cashu::PublicKey>)>> {
        let mint_summary = self.mdb.load_mint(qid).await?;
        let mint_state = client.get_mint_quote_onchain(qid.to_string()).await?;

        match mint_state.state {
            Some(mint_quote_state) => {
                match mint_quote_state {
                    cashu::MintQuoteState::Unpaid => {
                        tracing::info!("Mint {qid} not paid yet - skipping");
                        if mint_state.expiry < tstamp {
                            tracing::info!("Mint request with id {qid} expired - deleting.");
                            self.mdb.delete_mint(qid).await?;
                        }
                        Ok(None)
                    }
                    cashu::MintQuoteState::Paid => {
                        tracing::info!("Mint {qid} paid - attempting to mint..");
                        let (active_keyset_info, active_keyset) =
                            self.find_active_keyset(keysets_info, &client).await?;
                        let kid = active_keyset.id;

                        let counter = self.pdb.counter(kid).await?;
                        let premint = cashu::PreMintSecrets::from_seed(
                            kid,
                            counter,
                            &self.seed,
                            cashu::Amount::from(mint_summary.amount.to_sat()),
                            &SplitTarget::None,
                        )?;
                        let increment = premint.len() as u32;
                        self.pdb.increment_counter(kid, counter, increment).await?;

                        let mint_req = cashu::MintRequest {
                            quote: mint_summary.quote_id.to_string(),
                            outputs: premint.blinded_messages(),
                            signature: None,
                        };
                        match client.post_mint_onchain(mint_req).await {
                            Ok(mint_response) => {
                                // create proofs
                                let blinded_signatures = mint_response.signatures;
                                let inputs = cashu::dhke::construct_proofs(
                                    blinded_signatures,
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
                                let safe_mode_clone = safe_mode.clone();

                                // swap proofs
                                let (amount, ys) = self
                                    .digest_proofs(
                                        client,
                                        (active_keyset_info, active_keyset),
                                        proofs,
                                        safe_mode_clone,
                                    )
                                    .await?;

                                // delete local record
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
                    cashu::MintQuoteState::Issued => {
                        tracing::warn!("Mint {qid} already issued - deleting");
                        self.mdb.delete_mint(qid).await?;
                        Ok(None)
                    }
                }
            }
            None => {
                tracing::warn!("Mint {qid} has no state set - skipping");
                Ok(None)
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
        safe_mode: SafeMode,
    ) -> Result<(Amount, Vec<cdk01::PublicKey>)> {
        // storing proofs in pending state
        let mut proofs: HashMap<cdk01::PublicKey, cdk00::Proof> =
            HashMap::with_capacity(inputs.len());
        for input in inputs.into_iter() {
            let y = input.y()?;
            proofs.insert(y, input);
        }
        let active_keys = self.find_active_keyset(keysets_info, &client).await?;
        self.digest_proofs(client, active_keys, proofs, safe_mode)
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
        safe_mode: SafeMode,
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
            safe_mode,
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

        // swap
        let request = cdk03::SwapRequest::new(proofs, blinds);
        let response = client.post_swap(request).await?;

        // We only take the send_splits signatures, they add up to our send_amount
        let mut send_signatures = response.signatures.clone();
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
        safe_mode: SafeMode,
    ) -> Result<Amount> {
        let pendings = self.pdb.load_proofs(ys).await?;
        let pendings_len = pendings.len();
        let active_keys = self.find_active_keyset(keysets_info, &client).await?;
        let (reclaimed, _) = self
            .digest_proofs(client, active_keys, pendings, safe_mode)
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
        let secret_key = keypair_from_seed(self.seed).secret_key();
        let signature = cdk01::SecretKey::from(secret_key).sign(&[])?; // not used currently - sign empty message
        let request = wire_melt::MeltQuoteOnchainRequest {
            request: invoice,
            unit: self.unit.clone(),
            options: None,
            signature,
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
        safe_mode: SafeMode,
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
            safe_mode,
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
            let change = unblind_proofs(&keyset, response.change.unwrap_or(Vec::new()), premints);
            for proof in change {
                self.pdb.store_new(proof).await?;
            }
        }
        Ok((tx_id, sending_proofs))
    }

    async fn mint_onchain(
        &self,
        amount: bitcoin::Amount,
        client: Arc<dyn ClowderMintConnector>,
    ) -> Result<MintSummary> {
        let request = wire_mint::MintQuoteOnchainRequest {
            amount,
            unit: self.unit.clone(),
        };

        // Request mint quote
        let response = client.post_mint_quote_onchain(request).await?;
        let mint_summary = MintSummary {
            quote_id: response.quote,
            amount: response.amount,
            address: response.address,
            expiry: response.expiry,
        };

        // Store mint quote
        self.mdb
            .store_mint(
                mint_summary.quote_id,
                mint_summary.amount,
                mint_summary.address.clone(),
                mint_summary.expiry,
            )
            .await?;
        Ok(mint_summary)
    }

    async fn check_pending_mints(
        &self,
        keysets_info: &[KeySetInfo],
        client: Arc<dyn ClowderMintConnector>,
        tstamp: u64,
        safe_mode: SafeMode,
    ) -> Result<HashMap<Uuid, (cashu::Amount, Vec<cashu::PublicKey>)>> {
        let mint_ids = self.mdb.list_mints().await?;
        let mut res = HashMap::with_capacity(mint_ids.len());

        tracing::debug!("check pending mints for {} mints", mint_ids.len());
        for qid in mint_ids {
            match self
                .check_pending_mint(qid, keysets_info, client.clone(), tstamp, safe_mode.clone())
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
        connector
            .expect_post_swap()
            .times(1)
            .returning(move |request| {
                let amounts = request
                    .outputs()
                    .iter()
                    .map(|b| b.amount)
                    .collect::<Vec<_>>();
                let signatures = core_tests::generate_ecash_signatures(&keyset, &amounts);
                let response = cdk03::SwapResponse { signatures };
                Ok(response)
            });
        pdb.expect_store_new().times(2).returning(|p| {
            let y = p.y().expect("Hash to curve should not fail");
            Ok(y)
        });
        let pocket = pocket(Arc::new(pdb), Arc::new(mdb));
        let (cashed, _) = pocket
            .receive_proofs(Arc::new(connector), &k_infos, proofs, SafeMode::Disabled)
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
        connector
            .expect_post_swap()
            .times(1)
            .returning(move |request| {
                let amounts = request
                    .outputs()
                    .iter()
                    .map(|b| b.amount)
                    .collect::<Vec<_>>();
                let signatures = core_tests::generate_ecash_signatures(&keyset, &amounts);
                let response = cdk03::SwapResponse { signatures };
                Ok(response)
            });
        pdb.expect_store_new().times(2).returning(|p| {
            let y = p.y().expect("Hash to curve should not fail");
            Ok(y)
        });

        let pocket = pocket(Arc::new(pdb), Arc::new(mdb));
        let reclaimed = pocket
            .reclaim_proofs(&ys, &k_infos, Arc::new(connector), SafeMode::Disabled)
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
                    txid: None,
                    fee_reserve: bitcoin::Amount::ZERO,
                    amount,
                    state: cashu::MeltQuoteState::Pending,
                    expiry: chrono::Utc::now().timestamp() as u64,
                    unit: Some(CurrencyUnit::Sat),
                    change: None,
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
        let amount = bitcoin::Amount::from_sat(24);

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
                Ok(wire_melt::MeltQuoteOnchainResponse {
                    quote: uuid,
                    txid: Some(melt_tx_clone.clone()),
                    fee_reserve: bitcoin::Amount::ZERO,
                    amount,
                    state: cashu::MeltQuoteState::Paid,
                    expiry: chrono::Utc::now().timestamp() as u64,
                    unit: Some(CurrencyUnit::Sat),
                    change: None,
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
            .pay_onchain_melt(uuid, &k_infos, Arc::new(connector), SafeMode::Disabled)
            .await
            .expect("pay melt works");
        assert_eq!(res.0.alpha_txid, melt_tx.alpha_txid);
        assert_eq!(res.0.beta_txid, melt_tx.beta_txid);
    }

    #[tokio::test]
    async fn mint_onchain() {
        let amount = bitcoin::Amount::from_sat(24);

        let mut mdb = MockMintMeltRepository::new();
        let pdb = MockPocketRepository::new();
        let mut connector = MockMintConnector::new();

        mdb.expect_store_mint()
            .times(1)
            .returning(|_, _, _, _| Ok(Uuid::new_v4()));

        connector
            .expect_post_mint_quote_onchain()
            .times(1)
            .returning(move |_| {
                Ok(wire_mint::MintQuoteOnchainResponse {
                    quote: Uuid::new_v4(),
                    address: bitcoin::Address::from_str(
                        "tb1qteyk7pfvvql2r2zrsu4h4xpvju0nz7ykvguyk0",
                    )
                    .unwrap(),
                    amount,
                    expiry: chrono::Utc::now().timestamp() as u64,
                    state: Some(cashu::MintQuoteState::Unpaid),
                })
            });

        let pocket = pocket(Arc::new(pdb), Arc::new(mdb));

        let summary = pocket
            .mint_onchain(amount, Arc::new(connector))
            .await
            .expect("mint onchain works");
        assert_eq!(summary.amount, amount);
    }

    #[tokio::test]
    async fn check_pending_mints() {
        let uuid = Uuid::new_v4();
        let amount = bitcoin::Amount::from_sat(24);
        let (info, _) = core_tests::generate_random_ecash_keyset();
        let k_infos = vec![KeySetInfo::from(info)];

        let mut mdb = MockMintMeltRepository::new();
        let pdb = MockPocketRepository::new();
        let mut connector = MockMintConnector::new();

        mdb.expect_list_mints()
            .times(1)
            .returning(move || Ok(vec![uuid]));

        mdb.expect_load_mint().times(1).returning(move |_| {
            Ok(MintSummary {
                quote_id: uuid,
                amount,
                address: bitcoin::Address::from_str("tb1qteyk7pfvvql2r2zrsu4h4xpvju0nz7ykvguyk0")
                    .unwrap(),
                expiry: chrono::Utc::now().timestamp() as u64,
            })
        });

        connector
            .expect_get_mint_quote_onchain()
            .times(1)
            .returning(move |_| {
                Ok(wire_mint::MintQuoteOnchainResponse {
                    quote: uuid,
                    address: bitcoin::Address::from_str(
                        "tb1qteyk7pfvvql2r2zrsu4h4xpvju0nz7ykvguyk0",
                    )
                    .unwrap(),
                    amount,
                    expiry: chrono::Utc::now().timestamp() as u64,
                    state: Some(cashu::MintQuoteState::Unpaid),
                })
            });

        let pocket = pocket(Arc::new(pdb), Arc::new(mdb));

        let res = pocket
            .check_pending_mints(
                &k_infos,
                Arc::new(connector),
                chrono::Utc::now().timestamp() as u64,
                SafeMode::Disabled,
            )
            .await
            .expect("check pending mint works");
        assert_eq!(res.len(), 0);
    }
}
