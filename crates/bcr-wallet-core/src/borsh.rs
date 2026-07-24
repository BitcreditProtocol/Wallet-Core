use bcr_common::cashu;
use borsh::{
    BorshDeserialize, BorshSerialize,
    io::{Error as BorshError, ErrorKind, Read, Write},
};
use std::{collections::HashMap, str::FromStr};

pub type Result<T> = core::result::Result<T, BorshError>;

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
struct MintKeysetInfo {
    unit: String,
    active: bool,
    input_fee_ppk: u64,
    final_expiry: Option<u64>,
}

pub fn serialize_mint_keyset_infos(
    infos: &HashMap<cashu::Id, cashu::KeySetInfo>,
    writer: &mut impl Write,
) -> Result<()> {
    let mut stored: Vec<(String, MintKeysetInfo)> = infos
        .iter()
        .map(|(id, info)| {
            (
                id.to_string(),
                MintKeysetInfo {
                    unit: info.unit.to_string(),
                    active: info.active,
                    input_fee_ppk: info.input_fee_ppk,
                    final_expiry: info.final_expiry,
                },
            )
        })
        .collect();
    stored.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));
    BorshSerialize::serialize(&stored, writer)
}

pub fn deserialize_mint_keyset_infos(
    reader: &mut impl Read,
) -> Result<HashMap<cashu::Id, cashu::KeySetInfo>> {
    let stored: Vec<(String, MintKeysetInfo)> = BorshDeserialize::deserialize_reader(reader)?;

    let mut infos = HashMap::with_capacity(stored.len());

    for (id, info) in stored {
        let id =
            cashu::Id::from_str(&id).map_err(|e| BorshError::new(ErrorKind::InvalidData, e))?;

        let unit = cashu::CurrencyUnit::from_str(&info.unit)
            .map_err(|e| BorshError::new(ErrorKind::InvalidData, e))?;

        let keyset_info = cashu::KeySetInfo {
            id,
            unit,
            active: info.active,
            input_fee_ppk: info.input_fee_ppk,
            final_expiry: info.final_expiry,
        };

        if infos.insert(id, keyset_info).is_some() {
            return Err(BorshError::new(
                ErrorKind::InvalidData,
                "duplicate keyset ID",
            ));
        }
    }

    Ok(infos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    const KEYSET_ID_1: &str = "009a1f293253e41e";
    const KEYSET_ID_2: &str = "00b4cd27d8861a44";

    fn keyset_info(
        id: &str,
        unit: &str,
        active: bool,
        input_fee_ppk: u64,
        final_expiry: Option<u64>,
    ) -> cashu::KeySetInfo {
        let id = cashu::Id::from_str(id).expect("valid test keyset ID");
        let unit = cashu::CurrencyUnit::from_str(unit).expect("valid test currency unit");

        cashu::KeySetInfo {
            id,
            unit,
            active,
            input_fee_ppk,
            final_expiry,
        }
    }

    fn serialize(infos: &HashMap<cashu::Id, cashu::KeySetInfo>) -> Vec<u8> {
        let mut bytes = Vec::new();
        serialize_mint_keyset_infos(infos, &mut bytes).expect("serialization should succeed");
        bytes
    }

    fn deserialize(bytes: &[u8]) -> Result<HashMap<cashu::Id, cashu::KeySetInfo>> {
        let mut reader = Cursor::new(bytes);
        deserialize_mint_keyset_infos(&mut reader)
    }

    fn assert_keyset_info_eq(actual: &cashu::KeySetInfo, expected: &cashu::KeySetInfo) {
        assert_eq!(actual.id.to_string(), expected.id.to_string());
        assert_eq!(actual.unit.to_string(), expected.unit.to_string());
        assert_eq!(actual.active, expected.active);
        assert_eq!(actual.input_fee_ppk, expected.input_fee_ppk);
        assert_eq!(actual.final_expiry, expected.final_expiry);
    }

    #[test]
    fn round_trip_empty_map() {
        let original = HashMap::new();
        let bytes = serialize(&original);
        let deserialized = deserialize(&bytes).expect("deserialization should succeed");
        assert!(deserialized.is_empty());
    }

    #[test]
    fn round_trip_single_keyset() {
        let info = keyset_info(KEYSET_ID_1, "sat", true, 100, Some(1_900_000_000));
        let mut original = HashMap::new();
        original.insert(info.id, info);
        let bytes = serialize(&original);
        let deserialized = deserialize(&bytes).expect("deserialization should succeed");
        assert_eq!(deserialized.len(), 1);
        let id = cashu::Id::from_str(KEYSET_ID_1).unwrap();
        let actual = deserialized.get(&id).expect("keyset should exist");
        let expected = original.get(&id).expect("original keyset should exist");
        assert_keyset_info_eq(actual, expected);
    }

    #[test]
    fn round_trip_multiple_keysets() {
        let first = keyset_info(KEYSET_ID_1, "sat", true, 0, None);
        let second = keyset_info(KEYSET_ID_2, "msat", false, 250, Some(2_000_000_000));
        let mut original = HashMap::new();
        original.insert(first.id, first);
        original.insert(second.id, second);
        let bytes = serialize(&original);
        let deserialized = deserialize(&bytes).expect("deserialization should succeed");
        assert_eq!(deserialized.len(), original.len());
        for (id, expected) in &original {
            let actual = deserialized
                .get(id)
                .expect("deserialized keyset should exist");

            assert_keyset_info_eq(actual, expected);
        }
    }

    #[test]
    fn serialization_is_deterministic_regardless_of_insertion_order() {
        let first = keyset_info(KEYSET_ID_1, "sat", true, 10, None);
        let second = keyset_info(KEYSET_ID_2, "msat", false, 20, Some(123));
        let mut map_a = HashMap::new();
        map_a.insert(first.id, first.clone());
        map_a.insert(second.id, second.clone());
        let mut map_b = HashMap::new();
        map_b.insert(second.id, second);
        map_b.insert(first.id, first);
        assert_eq!(serialize(&map_a), serialize(&map_b));
    }

    #[test]
    fn serialized_keyset_id_is_taken_from_map_key() {
        let map_id = cashu::Id::from_str(KEYSET_ID_1).unwrap();
        let info = keyset_info(KEYSET_ID_2, "sat", true, 100, None);
        let mut original = HashMap::new();
        original.insert(map_id, info);
        let bytes = serialize(&original);
        let deserialized = deserialize(&bytes).expect("deserialization should succeed");
        let actual = deserialized
            .get(&map_id)
            .expect("map key should be preserved");
        assert_eq!(actual.id.to_string(), KEYSET_ID_1);
        assert!(!deserialized.contains_key(&cashu::Id::from_str(KEYSET_ID_2).unwrap()));
    }

    #[test]
    fn rejects_invalid_keyset_id() {
        let stored = vec![(
            "not-a-keyset-id".to_owned(),
            MintKeysetInfo {
                unit: "sat".to_owned(),
                active: true,
                input_fee_ppk: 0,
                final_expiry: None,
            },
        )];
        let bytes = borsh::to_vec(&stored).expect("test data should serialize");
        let error = deserialize(&bytes).expect_err("invalid ID must be rejected");
        assert_eq!(error.kind(), ErrorKind::InvalidData);
    }
}
