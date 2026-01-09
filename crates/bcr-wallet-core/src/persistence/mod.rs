pub mod redb;
use crate::TStamp;

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
struct Commitment {
    inputs: Vec<cashu::PublicKey>,
    outputs: Vec<cashu::BlindedMessage>,
    expiration: TStamp,
    commitment: secp256k1::schnorr::Signature,
}
