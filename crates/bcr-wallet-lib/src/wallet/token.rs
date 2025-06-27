// ----- standard library imports
use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;
// ----- extra library imports
use bitcoin::base64::engine::{GeneralPurpose, general_purpose};
use bitcoin::base64::{Engine as _, alphabet};
use cashu::{
    Amount, CurrencyUnit, MintUrl, Proof, Proofs,
    nut00::{Error, ProofsMethods, token::TokenV4Token},
};
use serde::{Deserialize, Serialize};
// ----- local modules

// ----- end imports

pub type CashuTokenV4 = cashu::nut00::TokenV4;

#[doc(hidden)]
#[macro_export]
macro_rules! ensure_cdk {
    ($cond:expr, $err:expr) => {
        if !$cond {
            return Err($err);
        }
    };
}

/// Token Enum
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Token {
    BitcrV4(BitcrTokenV4),
    CashuV4(CashuTokenV4),
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let token = match self {
            Self::BitcrV4(token) => token.to_string(),
            Self::CashuV4(token) => token.to_string(),
        };

        write!(f, "{token}")
    }
}

impl Token {
    /// Create new bitcrV4 [`Token`]
    pub fn new_bitcr(
        mint_url: MintUrl,
        proofs: Proofs,
        memo: Option<String>,
        unit: CurrencyUnit,
    ) -> Self {
        let proofs = proofs_to_tokenv4(proofs);

        Self::BitcrV4(BitcrTokenV4 {
            mint_url,
            unit,
            memo,
            token: proofs,
        })
    }

    /// Create new cashuV4 [`Token`]
    pub fn new_cashu(
        mint_url: MintUrl,
        proofs: Proofs,
        memo: Option<String>,
        unit: CurrencyUnit,
    ) -> Self {
        let proofs = proofs_to_tokenv4(proofs);

        Self::CashuV4(CashuTokenV4 {
            mint_url,
            unit,
            memo,
            token: proofs,
        })
    }
    /// Proofs in [`Token`]
    pub fn proofs(&self) -> Proofs {
        match self {
            Self::BitcrV4(token) => token.proofs(),
            Self::CashuV4(token) => token.proofs(),
        }
    }

    /// Total value of [`Token`]
    pub fn value(&self) -> Result<Amount, Error> {
        match self {
            Self::BitcrV4(token) => token.value(),
            Self::CashuV4(token) => token.value(),
        }
    }

    /// [`Token`] memo
    pub fn memo(&self) -> &Option<String> {
        match self {
            Self::BitcrV4(token) => token.memo(),
            Self::CashuV4(token) => token.memo(),
        }
    }

    /// Unit
    pub fn unit(&self) -> Option<CurrencyUnit> {
        match self {
            Self::BitcrV4(token) => Some(token.unit().clone()),
            Self::CashuV4(token) => Some(token.unit().clone()),
        }
    }

    /// Mint url
    pub fn mint_url(&self) -> MintUrl {
        match self {
            Self::BitcrV4(token) => token.mint_url.clone(),
            Self::CashuV4(token) => token.mint_url.clone(),
        }
    }

    /// Serialize the token to raw binary
    pub fn to_raw_bytes(&self) -> Result<Vec<u8>, Error> {
        match self {
            Self::BitcrV4(_) => Err(Error::UnsupportedToken),
            Self::CashuV4(token) => token.to_raw_bytes(),
        }
    }
}

impl FromStr for Token {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match (CashuTokenV4::from_str(s), BitcrTokenV4::from_str(s)) {
            (Ok(token), Err(_)) => Ok(Token::CashuV4(token)),
            (Err(_), Ok(token)) => Ok(Token::BitcrV4(token)),
            _ => Err(Error::UnsupportedToken),
        }
    }
}

impl TryFrom<&Vec<u8>> for Token {
    type Error = Error;

    fn try_from(bytes: &Vec<u8>) -> Result<Self, Self::Error> {
        if let Ok(token) = CashuTokenV4::try_from(bytes) {
            return Ok(Token::CashuV4(token));
        }
        if let Ok(token) = BitcrTokenV4::try_from(bytes) {
            return Ok(Token::BitcrV4(token));
        }
        Err(Error::UnsupportedToken)
    }
}

/// Token V4
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BitcrTokenV4 {
    /// Mint Url
    #[serde(rename = "m")]
    pub mint_url: MintUrl,
    /// Token Unit
    #[serde(rename = "u")]
    pub unit: CurrencyUnit,
    /// Memo for token
    #[serde(rename = "d", skip_serializing_if = "Option::is_none")]
    pub memo: Option<String>,
    /// Proofs grouped by keyset_id
    #[serde(rename = "t")]
    pub token: Vec<TokenV4Token>,
}

impl BitcrTokenV4 {
    /// Proofs from token
    pub fn proofs(&self) -> Proofs {
        self.token
            .iter()
            .flat_map(|token| token.proofs.iter().map(|p| p.into_proof(&token.keyset_id)))
            .collect()
    }

    /// Value - errors if duplicate proofs are found
    #[inline]
    pub fn value(&self) -> Result<Amount, Error> {
        let proofs = self.proofs();
        let unique_count = proofs
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len();

        // Check if there are any duplicate proofs
        if unique_count != proofs.len() {
            return Err(Error::DuplicateProofs);
        }

        proofs.total_amount()
    }

    /// Memo
    #[inline]
    pub fn memo(&self) -> &Option<String> {
        &self.memo
    }

    /// Unit
    #[inline]
    pub fn unit(&self) -> &CurrencyUnit {
        &self.unit
    }

    /// Serialize the token to raw binary
    pub fn to_raw_bytes(&self) -> Result<Vec<u8>, Error> {
        let mut prefix = b"brawB".to_vec();
        let mut data = Vec::new();
        ciborium::into_writer(self, &mut data).map_err(Error::CiboriumSerError)?;
        prefix.extend(data);
        Ok(prefix)
    }
}

impl fmt::Display for BitcrTokenV4 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use serde::ser::Error;
        let mut data = Vec::new();
        ciborium::into_writer(self, &mut data).map_err(|e| fmt::Error::custom(e.to_string()))?;
        let encoded = general_purpose::URL_SAFE.encode(data);
        write!(f, "bitcrB{encoded}")
    }
}

impl FromStr for BitcrTokenV4 {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.strip_prefix("bitcrB").ok_or(Error::UnsupportedToken)?;

        let decode_config = general_purpose::GeneralPurposeConfig::new()
            .with_decode_padding_mode(bitcoin::base64::engine::DecodePaddingMode::Indifferent);
        let decoded = GeneralPurpose::new(&alphabet::URL_SAFE, decode_config).decode(s)?;
        let token: BitcrTokenV4 = ciborium::from_reader(&decoded[..])?;
        Ok(token)
    }
}

impl TryFrom<&Vec<u8>> for BitcrTokenV4 {
    type Error = Error;

    fn try_from(bytes: &Vec<u8>) -> Result<Self, Self::Error> {
        ensure_cdk!(bytes.len() >= 5, Error::UnsupportedToken);

        let prefix = String::from_utf8(bytes[..5].to_vec())?;
        ensure_cdk!(prefix.as_str() == "brawB", Error::UnsupportedToken);

        Ok(ciborium::from_reader(&bytes[5..])?)
    }
}

fn proofs_to_tokenv4(proofs: Proofs) -> Vec<TokenV4Token> {
    proofs
        .into_iter()
        .fold(HashMap::new(), |mut acc, val| {
            acc.entry(val.keyset_id)
                .and_modify(|p: &mut Vec<Proof>| p.push(val.clone()))
                .or_insert(vec![val.clone()]);
            acc
        })
        .into_iter()
        .map(|(id, proofs)| TokenV4Token::new(id, proofs))
        .collect()
}
