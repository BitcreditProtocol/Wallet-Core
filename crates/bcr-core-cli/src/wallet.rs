use crate::test_utils::generate_random_keypair;
use crate::test_utils::{generate_blinds, get_amounts, random_ebill_request};
use bcr_wdc_webapi::quotes::{BillInfo, SignedEnquireRequest};
use bcr_wdc_webapi::test_utils;
use bitcoin::secp256k1::{Keypair, PublicKey, SecretKey};
use cashu::nuts::nut02 as cdk02;
use cashu::MintBolt11Request;
use cashu::{Amount, Proof};
use std::collections::HashMap;
use tracing::info;
use uuid::Uuid;

pub struct Wallet {
    keypair: Keypair,
    proofs: Vec<Proof>,

    secrets: HashMap<
        (cdk02::Id, cashu::Amount),
        (
            cashu::BlindedMessage,
            cashu::secret::Secret,
            cashu::SecretKey,
        ),
    >,
}

impl Wallet {
    pub fn new() -> Self {
        let keypair = generate_random_keypair();
        Self {
            keypair,
            proofs: vec![],
            secrets: HashMap::new(),
        }
    }

    pub fn public_key(&self) -> PublicKey {
        self.keypair.public_key()
    }

    pub fn create_random_ebill(
        &self,
        public_key: cashu::PublicKey,
        amount: u64,
    ) -> (SignedEnquireRequest, bitcoin::secp256k1::schnorr::Signature) {
        let (request, signature) = random_ebill_request(public_key, amount.into());
        let signed_request = SignedEnquireRequest { request, signature };

        (signed_request, signature)
    }

    pub fn create_blinds(
        &self,
        id: cdk02::Id,
        amount: u64,
    ) -> Vec<(
        cashu::BlindedMessage,
        cashu::secret::Secret,
        cashu::SecretKey,
    )> {
        let amounts = get_amounts(amount)
            .iter()
            .map(|a| Amount::from(*a))
            .collect::<Vec<_>>();
        generate_blinds(id, &amounts)
    }

    pub fn create_mint_request(
        &mut self,
        quote_id: Uuid,
        keyset_id: cdk02::Id,
        amount: u64,
    ) -> (
        MintBolt11Request<Uuid>,
        Vec<cashu::SecretKey>,
        Vec<cashu::secret::Secret>,
    ) {
        let blinds = self.create_blinds(keyset_id, amount);
        let blinded_messages = blinds.iter().map(|b| b.0.clone()).collect::<Vec<_>>();

        info!("Signing NUT20 mint request");
        let mut req = MintBolt11Request {
            quote: quote_id,
            outputs: blinded_messages,
            signature: None,
        };
        req.sign(self.keypair.secret_key().into()).unwrap();

        for blind in &blinds {
            // self.secrets.insert((keyset_id, blind.0.amount), blind);
        }

        let secrets = blinds.iter().map(|b| b.1.clone()).collect::<Vec<_>>();
        let rs = blinds.iter().map(|b| b.2.clone()).collect::<Vec<_>>();

        (req, rs, secrets)
    }
}
