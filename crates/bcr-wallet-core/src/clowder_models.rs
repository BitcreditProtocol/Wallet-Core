use serde::{Deserialize, Serialize};

#[allow(unused)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectedMintsResponse {
    pub mint_urls: Vec<cashu::MintUrl>,
    pub clowder_urls: Vec<reqwest::Url>,
    pub node_ids: Vec<secp256k1::PublicKey>,
}
