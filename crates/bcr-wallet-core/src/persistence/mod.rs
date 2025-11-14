// ----- standard library imports
// ----- extra library imports
// ----- local modules
#[cfg(not(target_arch = "wasm32"))]
pub mod inmemory;
#[cfg(target_arch = "wasm32")]
pub mod rexie;
// ----- local imports
use crate::TStamp;

// ----- end imports

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
struct Commitment {
    inputs: Vec<cashu::PublicKey>,
    outputs: Vec<cashu::BlindedMessage>,
    expiration: TStamp,
    commitment: secp256k1::schnorr::Signature,
}
