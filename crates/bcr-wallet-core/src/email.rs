use serde::{Deserialize, Serialize};
use std::{fmt::Display, str::FromStr};

use crate::ValidationError;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, PartialOrd, Ord, Hash)]
#[serde(try_from = "String", into = "String")]
pub struct Email(String);

impl Email {
    pub fn new(n: impl Into<String>) -> Result<Self, ValidationError> {
        let s = n.into();
        if s.trim().is_empty() {
            return Err(ValidationError::EmptyEmail);
        }

        if !email_address::EmailAddress::is_valid(&s) {
            return Err(ValidationError::InvalidEmail);
        }

        Ok(Self(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for Email {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for Email {
    type Err = ValidationError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Email::new(s)
    }
}

impl TryFrom<String> for Email {
    type Error = ValidationError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<Email> for String {
    fn from(value: Email) -> Self {
        value.0
    }
}

impl borsh::BorshSerialize for Email {
    fn serialize<W: std::io::Write>(&self, writer: &mut W) -> std::io::Result<()> {
        borsh::BorshSerialize::serialize(&self.0, writer)
    }
}

impl borsh::BorshDeserialize for Email {
    fn deserialize_reader<R: std::io::Read>(reader: &mut R) -> std::io::Result<Self> {
        let email_str: String = borsh::BorshDeserialize::deserialize_reader(reader)?;
        Email::new(&email_str).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_email() {
        let n = Email::new("test@example.com").expect("works");
        let n_owned = Email::new(String::from("test@example.com")).expect("works");
        assert_eq!(n, n_owned);

        assert!(matches!(
            Email::new("totally@$$$12312@sdfds.com"),
            Err(ValidationError::InvalidEmail)
        ));
        assert!(matches!(
            Email::new("12312"),
            Err(ValidationError::InvalidEmail)
        ));
        assert!(matches!(Email::new(""), Err(ValidationError::EmptyEmail)));
        assert!(matches!(
            Email::new("            "),
            Err(ValidationError::EmptyEmail)
        ));
    }
}
