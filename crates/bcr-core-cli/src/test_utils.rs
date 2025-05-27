use bcr_wdc_webapi::bill::BillAnonParticipant;
// ----- standard library imports
// ----- extra library imports
use bcr_wdc_webapi::{bill::BillParticipant, quotes::BillInfo};
use rand::Rng;
// ----- local modules
use bitcoin::hashes::{sha256::Hash as Sha256, Hash};
use bitcoin::secp256k1::Keypair;
use cashu::dhke as cdk_dhke;
use cashu::nuts::nut00 as cdk00;
use cashu::nuts::nut01 as cdk01;
use cashu::nuts::nut02 as cdk02;
use cashu::secret as cdk_secret;
use cashu::Amount;
use thiserror::Error;

// ----- end imports

pub fn generate_blind(
    kid: cdk02::Id,
    amount: cashu::Amount,
) -> (cdk00::BlindedMessage, cdk_secret::Secret, cdk01::SecretKey) {
    let secret = cdk_secret::Secret::new(rand::random::<u64>().to_string());
    let (b_, r) =
        cdk_dhke::blind_message(secret.as_bytes(), None).expect("cdk_dhke::blind_message");
    (cdk00::BlindedMessage::new(amount, kid, b_), secret, r)
}

pub fn random_ebill_request(
    public_key: cashu::PublicKey,
    amount: cashu::Amount,
) -> (
    bcr_wdc_webapi::quotes::EnquireRequest,
    bitcoin::secp256k1::schnorr::Signature,
) {
    let bill_id = bcr_wdc_webapi::test_utils::random_bill_id();
    let (_, drawee) = bcr_wdc_webapi::test_utils::random_identity_public_data();
    let (_, drawer) = bcr_wdc_webapi::test_utils::random_identity_public_data();
    let (_, payee) = bcr_wdc_webapi::test_utils::random_identity_public_data();

    let endorsees_size = rand::thread_rng().gen_range(0..3);
    let mut endorsees: Vec<BillParticipant> = Vec::with_capacity(endorsees_size);

    let (endorser_kp, endorser) = bcr_wdc_webapi::test_utils::random_identity_public_data();
    endorsees.push(BillParticipant::Ident(endorser));

    let bill = BillInfo {
        id: bill_id,
        maturity_date: random_date(),
        drawee,
        drawer,
        payee: BillParticipant::Ident(payee),
        endorsees,
        sum: amount.into(),
    };

    let request = bcr_wdc_webapi::quotes::EnquireRequest {
        content: bill,
        public_key: public_key,
    };
    let signature = schnorr_sign_borsh_msg_with_key(&request, &endorser_kp)
        .expect("schnorr_sign_borsh_msg_with_key");

    (request, signature)
}

pub fn generate_random_keypair() -> Keypair {
    let mut rng = rand::thread_rng();
    bitcoin::secp256k1::Keypair::new(bitcoin::secp256k1::global::SECP256K1, &mut rng)
}

pub type SchnorrSignBorshResult<T> = std::result::Result<T, SchnorrBorshMsgError>;
#[derive(Debug, Error)]
pub enum SchnorrBorshMsgError {
    #[error("Borsh error {0}")]
    Borsh(borsh::io::Error),
    #[error("Secp256k1 error {0}")]
    Secp256k1(bitcoin::secp256k1::Error),
}

pub fn schnorr_sign_borsh_msg_with_key<Message>(
    msg: &Message,
    keys: &bitcoin::secp256k1::Keypair,
) -> SchnorrSignBorshResult<bitcoin::secp256k1::schnorr::Signature>
where
    Message: borsh::BorshSerialize,
{
    let serialized = borsh::to_vec(&msg).map_err(SchnorrBorshMsgError::Borsh)?;
    let sha = Sha256::hash(&serialized);
    let secp_msg = bitcoin::secp256k1::Message::from_digest(*sha.as_ref());

    Ok(bitcoin::secp256k1::global::SECP256K1.sign_schnorr(&secp_msg, keys))
}

fn random_date() -> String {
    let start = chrono::Utc::now() + chrono::Duration::days(365);
    let days = rand::thread_rng().gen_range(0..365);
    (start + chrono::Duration::days(days)).to_rfc3339()
}

pub fn get_amounts(mut targ: u64) -> Vec<u64> {
    // TODO see if there is an existing cashu implementation
    let mut coins = Vec::new();
    let mut bit_position = 0;
    while targ > 0 {
        if (targ & 1) == 1 {
            coins.push(1 << bit_position);
        }
        targ >>= 1;
        bit_position += 1;
    }
    coins
}

pub fn generate_blinds(
    keyset_id: cdk02::Id,
    amounts: &[Amount],
) -> Vec<(
    cashu::BlindedMessage,
    cashu::secret::Secret,
    cashu::SecretKey,
)> {
    let mut blinds = Vec::new();
    for amount in amounts {
        let blind = generate_blind(keyset_id, *amount);
        blinds.push(blind);
    }
    blinds
}

#[cfg(test)]
mod tests {
    use crate::test_utils::get_amounts;
    #[test]
    fn test_get_amounts() {
        let amounts = get_amounts(1000);
        let sum = amounts.iter().sum::<u64>();

        assert_eq!(amounts, vec![8, 32, 64, 128, 256, 512]);
        assert_eq!(sum, 1000);
    }
}
