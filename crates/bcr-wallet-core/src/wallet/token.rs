// ----- standard library imports
use std::fmt;
use std::str::FromStr;
// ----- extra library imports
use anyhow::Result;
use bitcoin::base64::Engine;
use bitcoin::base64::alphabet;
use bitcoin::base64::engine::{GeneralPurpose, general_purpose};
use cashu::{TokenV3, TokenV4};
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

fn parse_token_v3_with_prefix<C: PrefixCodec>(s: &str) -> Result<TokenV3, TokenError> {
    let s = C::strip_prefix(s)?;

    let decode_config = general_purpose::GeneralPurposeConfig::new()
        .with_decode_padding_mode(bitcoin::base64::engine::DecodePaddingMode::Indifferent);
    let decoded = GeneralPurpose::new(&alphabet::URL_SAFE, decode_config)
        .decode(s)
        .map_err(|e| TokenError::Base64Error(e))?;
    let decoded_str = String::from_utf8(decoded).map_err(TokenError::Utf8Error)?;
    let token: TokenV3 = serde_json::from_str(&decoded_str).map_err(TokenError::JsonError)?;
    Ok(token)
}

fn parse_token_v4_with_prefix<C: PrefixCodec>(s: &str) -> Result<TokenV4, TokenError> {
    let s = C::strip_prefix(s)?;

    let decode_config = general_purpose::GeneralPurposeConfig::new()
        .with_decode_padding_mode(bitcoin::base64::engine::DecodePaddingMode::Indifferent);
    let decoded = GeneralPurpose::new(&alphabet::URL_SAFE, decode_config)
        .decode(s)
        .map_err(|e| TokenError::Base64Error(e))?;
    let token: TokenV4 = ciborium::from_reader(&decoded[..]).map_err(TokenError::CborError)?;
    Ok(token)
}

/// Your unified enum, no change here
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Token {
    CashuV3(TokenV3),
    CashuV4(TokenV4),
    BitcrV3(TokenV3),
    BitcrV4(TokenV4),
}

impl FromStr for Token {
    type Err = TokenError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.starts_with(CashuA::PREFIX) {
            let v3 = parse_token_v3_with_prefix::<CashuA>(s)?;
            Ok(Token::CashuV3(v3))
        } else if s.starts_with(CashuB::PREFIX) {
            let v4 = parse_token_v4_with_prefix::<CashuB>(s)?;
            Ok(Token::CashuV4(v4))
        } else if s.starts_with(BitcrA::PREFIX) {
            let v3 = parse_token_v3_with_prefix::<BitcrA>(s)?;
            Ok(Token::BitcrV3(v3))
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
            Token::CashuV3(v3) => v3.fmt(f),
            Token::CashuV4(v4) => v4.fmt(f),
            Token::BitcrV3(v3) => {
                let json_string = serde_json::to_string(v3).map_err(|_| fmt::Error)?;
                let encoded = general_purpose::URL_SAFE.encode(json_string);
                write!(f, "bitcrA{encoded}")
            }
            Token::BitcrV4(v4) => {
                use serde::ser::Error;
                let mut data = Vec::new();
                ciborium::into_writer(v4, &mut data)
                    .map_err(|e| fmt::Error::custom(e.to_string()))?;
                let encoded = general_purpose::URL_SAFE.encode(data);
                write!(f, "bitcrB{encoded}")
            }
        }
    }
}

impl TryFrom<Token> for TokenV4 {
    type Error = TokenError;
    fn try_from(token: Token) -> Result<Self, Self::Error> {
        match token {
            Token::BitcrV3(v3) => TryFrom::try_from(v3).map_err(|_| TokenError::UnsupportedToken),
            Token::CashuV3(v3) => TryFrom::try_from(v3).map_err(|_| TokenError::UnsupportedToken),
            Token::CashuV4(v4) => Ok(v4),
            Token::BitcrV4(v4) => Ok(v4),
        }
    }
}
