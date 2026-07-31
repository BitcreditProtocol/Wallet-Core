use std::{fmt::Display, str::FromStr};

use serde::{Deserialize, Serialize};

use crate::ValidationError;

const MAX_NAME_LEN: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, PartialOrd, Ord, Hash)]
#[serde(try_from = "String", into = "String")]
pub struct Name(String);

impl Name {
    pub fn new(n: impl Into<String>) -> Result<Self, ValidationError> {
        let s = ammonia::clean(&n.into());

        if s.trim().is_empty() {
            return Err(ValidationError::EmptyName);
        }

        if s.trim().chars().count() > MAX_NAME_LEN {
            return Err(ValidationError::InvalidName);
        }

        Ok(Self(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for Name {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for Name {
    type Err = ValidationError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Name::new(s)
    }
}

impl TryFrom<String> for Name {
    type Error = ValidationError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<Name> for String {
    fn from(value: Name) -> Self {
        value.0
    }
}

impl borsh::BorshSerialize for Name {
    fn serialize<W: std::io::Write>(&self, writer: &mut W) -> std::io::Result<()> {
        borsh::BorshSerialize::serialize(&self.0, writer)
    }
}

impl borsh::BorshDeserialize for Name {
    fn deserialize_reader<R: std::io::Read>(reader: &mut R) -> std::io::Result<Self> {
        let name_str: String = borsh::BorshDeserialize::deserialize_reader(reader)?;
        Name::new(&name_str).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_name() {
        let n = Name::new("minka").expect("works");
        let n_owned = Name::new(String::from("minka")).expect("works");
        assert_eq!(n, n_owned);
        assert_eq!(
            Name::new("Min<script>window.alert('HELLO');</script>ka")
                .expect("works")
                .as_str(),
            "Minka"
        );

        assert!(matches!(
            Name::new(
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            ),
            Err(ValidationError::InvalidName)
        ));
        assert!(matches!(Name::new(""), Err(ValidationError::EmptyName)));
        assert!(matches!(
            Name::new("            "),
            Err(ValidationError::EmptyName)
        ));
    }

    #[test]
    fn test_name_utf8() {
        // Test Arabic name (multi-byte UTF-8 characters)
        let arabic_name = "محمد أحمد";
        let n = Name::new(arabic_name).expect("Arabic name should work");
        assert_eq!(n.as_str(), arabic_name);

        // Test Chinese name
        let chinese_name = "李明";
        let n = Name::new(chinese_name).expect("Chinese name should work");
        assert_eq!(n.as_str(), chinese_name);

        // Test name with emojis
        let emoji_name = "John 👨‍💼";
        let n = Name::new(emoji_name).expect("Name with emoji should work");
        assert_eq!(n.as_str(), emoji_name);

        // Create a string with exactly 200 Arabic characters (which would be more than 200 bytes)
        let long_arabic = "أ".repeat(200);
        assert!(
            Name::new(&long_arabic).is_ok(),
            "200 Arabic chars should be OK"
        );

        // Create a string with 201 Arabic characters (should fail)
        let too_long_arabic = "أ".repeat(201);
        assert!(
            matches!(
                Name::new(&too_long_arabic),
                Err(ValidationError::InvalidName)
            ),
            "201 Arabic chars should fail"
        );
    }
}
