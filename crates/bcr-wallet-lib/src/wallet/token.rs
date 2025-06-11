// ----- standard library imports
use std::fmt;
use std::str::FromStr;
// ----- extra library imports
use bitcoin::base64::Engine;
use bitcoin::base64::alphabet;
use bitcoin::base64::engine::{GeneralPurpose, general_purpose};
use cashu::nut00::token::TokenV4Token;
use cashu::{CurrencyUnit, MintUrl, Proof, TokenV3, TokenV4};
use serde::{Deserialize, Serialize};
use thiserror::Error;
// ----- local modules
// ----- end imports

#[derive(Debug, Error)]
pub enum TokenError {
    #[error("Token is not supported")]
    UnsupportedToken,
    #[error("Base 64 decoding failed: {0}")]
    Base64Error(bitcoin::base64::DecodeError),
    #[error("UTF-8 decoding failed: {0}")]
    Utf8Error(std::string::FromUtf8Error),
    #[error("JSON serialization failed: {0}")]
    JsonError(serde_json::Error),
    #[error("CBOR serialization failed: {0}")]
    CborError(ciborium::de::Error<std::io::Error>),
    #[error("Nut00 error: {0}")]
    Nut00Error(cashu::nut00::Error),
    #[error("Cashu error: {0}")]
    CashuError(String),
}

pub trait PrefixCodec {
    const PREFIX: &'static str;

    fn strip_prefix(s: &str) -> Result<&str, TokenError> {
        s.strip_prefix(Self::PREFIX)
            .ok_or(TokenError::UnsupportedToken)
    }

    fn with_prefix(inner: &str) -> String {
        format!("{}{}", Self::PREFIX, inner)
    }
}

pub struct CashuA;
pub struct CashuB;
pub struct BitcrA;
pub struct BitcrB;

impl PrefixCodec for CashuA {
    const PREFIX: &'static str = "cashuA";
}
impl PrefixCodec for CashuB {
    const PREFIX: &'static str = "cashuB";
}
impl PrefixCodec for BitcrA {
    const PREFIX: &'static str = "bitcrA";
}
impl PrefixCodec for BitcrB {
    const PREFIX: &'static str = "bitcrB";
}

fn decode_base64_token(s: &str) -> Result<Vec<u8>, TokenError> {
    let decode_config = general_purpose::GeneralPurposeConfig::new()
        .with_decode_padding_mode(bitcoin::base64::engine::DecodePaddingMode::Indifferent);
    GeneralPurpose::new(&alphabet::URL_SAFE, decode_config)
        .decode(s)
        .map_err(TokenError::Base64Error)
}

fn parse_token_v3_with_prefix<C: PrefixCodec>(s: &str) -> Result<TokenV3, TokenError> {
    let s = C::strip_prefix(s)?;
    let decoded = decode_base64_token(s)?;
    let decoded_str = String::from_utf8(decoded).map_err(TokenError::Utf8Error)?;
    let token: TokenV3 = serde_json::from_str(&decoded_str).map_err(TokenError::JsonError)?;
    Ok(token)
}

fn parse_token_v4_with_prefix<C: PrefixCodec>(s: &str) -> Result<TokenV4, TokenError> {
    let s = C::strip_prefix(s)?;
    let decoded = decode_base64_token(s)?;
    let token: TokenV4 = ciborium::from_reader(&decoded[..]).map_err(TokenError::CborError)?;
    Ok(token)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Token {
    CashuV4(TokenV4),
    BitcrV4(TokenV4),
}

impl FromStr for Token {
    type Err = TokenError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.starts_with(CashuA::PREFIX) {
            let v3 = parse_token_v3_with_prefix::<CashuA>(s)?;
            let v4 = TokenV4::try_from(v3).map_err(TokenError::Nut00Error)?;
            Ok(Token::CashuV4(v4))
        } else if s.starts_with(CashuB::PREFIX) {
            let v4 = parse_token_v4_with_prefix::<CashuB>(s)?;
            Ok(Token::CashuV4(v4))
        } else if s.starts_with(BitcrA::PREFIX) {
            let v3 = parse_token_v3_with_prefix::<BitcrA>(s)?;
            let v4 = TokenV4::try_from(v3).map_err(TokenError::Nut00Error)?;
            Ok(Token::BitcrV4(v4))
        } else if s.starts_with(BitcrB::PREFIX) {
            let v4 = parse_token_v4_with_prefix::<BitcrB>(s)?;
            Ok(Token::BitcrV4(v4))
        } else {
            Err(TokenError::UnsupportedToken)
        }
    }
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Token::CashuV4(v4) => v4.fmt(f),
            Token::BitcrV4(v4) => {
                let mut data = Vec::new();
                ciborium::into_writer(v4, &mut data).map_err(|_| fmt::Error)?;
                let encoded = general_purpose::URL_SAFE.encode(data);
                write!(f, "{}{encoded}", BitcrB::PREFIX)
            }
        }
    }
}

impl TryFrom<Token> for TokenV4 {
    type Error = TokenError;
    fn try_from(token: Token) -> Result<Self, Self::Error> {
        match token {
            Token::CashuV4(v4) => Ok(v4),
            Token::BitcrV4(v4) => Ok(v4),
        }
    }
}

pub struct ProofDetail {
    pub proof: Proof,
    pub mint_url: MintUrl,
}

pub trait TokenOperations {
    fn unit(&self) -> CurrencyUnit;
    fn memo(&self) -> Option<String>;
    fn proofs(&self) -> Vec<Proof>;
    fn mint_url(&self) -> MintUrl;
}

impl TokenOperations for Token {
    fn mint_url(&self) -> MintUrl {
        match self {
            Token::CashuV4(v4) => v4.mint_url.clone(),
            Token::BitcrV4(v4) => v4.mint_url.clone(),
        }
    }
    fn unit(&self) -> CurrencyUnit {
        match self {
            Token::CashuV4(v4) => v4.unit.clone(),
            Token::BitcrV4(v4) => v4.unit.clone(),
        }
    }
    fn memo(&self) -> Option<String> {
        match self {
            Token::CashuV4(v4) => v4.memo.clone(),
            Token::BitcrV4(v4) => v4.memo.clone(),
        }
    }
    fn proofs(&self) -> Vec<Proof> {
        match self {
            Token::CashuV4(v4) => v4.proofs(),
            Token::BitcrV4(v4) => v4.proofs(),
        }
    }
}

fn create_v4_token(
    mint_url: MintUrl,
    unit: CurrencyUnit,
    memo: Option<String>,
    proofs: Vec<Proof>,
) -> TokenV4 {
    let v4tokens = proofs
        .into_iter()
        .fold(std::collections::HashMap::new(), |mut acc, val| {
            acc.entry(val.keyset_id)
                .and_modify(|p: &mut Vec<Proof>| p.push(val.clone()))
                .or_insert(vec![val]);
            acc
        })
        .into_iter()
        .map(|(id, proofs)| TokenV4Token::new(id, proofs))
        .collect();

    cashu::TokenV4 {
        mint_url: mint_url,
        unit: unit,
        token: v4tokens,
        memo,
    }
}

impl Token {
    pub fn new_debit(
        mint_url: MintUrl,
        unit: CurrencyUnit,
        memo: Option<String>,
        proofs: Vec<Proof>,
    ) -> Token {
        Token::CashuV4(create_v4_token(mint_url, unit, memo, proofs))
    }

    pub fn new_credit(
        mint_url: MintUrl,
        unit: CurrencyUnit,
        memo: Option<String>,
        proofs: Vec<Proof>,
    ) -> Token {
        Token::BitcrV4(create_v4_token(mint_url, unit, memo, proofs))
    }
}
