use crate::{
    error::Result,
    redb::{
        migration::{WalletStorageNamespace, migrate_table},
        mintmelt::{
            MeltCommitmentEntry, MeltEntry, MintEntry, encode_melt_commitment_entry,
            encode_melt_entry, encode_mint_entry, premint_entries,
        },
    },
};
use bcr_common::{
    cashu::{self, Amount, nut00 as cdk00, nut01 as cdk01},
    ecash,
};
use bitcoin::address::NetworkUnchecked;
use uuid::Uuid;

///////////////////////////////////////////////////////////////// MIGRATION 0008
const MIGRATION_0008_MINT_MELT_ENVELOPE: &str = "0008_mint_melt_envelope";

pub(super) fn migration_name_for_wallet(wallet_id: &str) -> String {
    format!("{}_{}", MIGRATION_0008_MINT_MELT_ENVELOPE, wallet_id)
}

pub(super) fn migration_0008_mint_melt_envelope(
    txn: &redb::WriteTransaction,
    namespace: &WalletStorageNamespace,
) -> Result<()> {
    tracing::info!("Migrating mint and melt tables..");
    migrate_table(txn, &namespace.melt_table, &namespace.keys, |bytes| {
        let legacy: LegacyMeltEntry = ciborium::from_reader(bytes)?;
        encode_melt_entry(&legacy.into(), &namespace.keys)
    })?;
    migrate_table(txn, &namespace.mint_table, &namespace.keys, |bytes| {
        let legacy: LegacyMintEntry = ciborium::from_reader(bytes)?;
        encode_mint_entry(&legacy.try_into()?, &namespace.keys)
    })?;
    migrate_table(
        txn,
        &namespace.melt_commitment_table,
        &namespace.keys,
        |bytes| {
            let legacy: LegacyMeltCommitmentEntry = ciborium::from_reader(bytes)?;
            encode_melt_commitment_entry(&legacy.try_into()?, &namespace.keys)
        },
    )?;
    tracing::info!("Migrated mint and melt tables.");
    Ok(())
}

///////////////////////////////////////////// Legacy entries
type LegacyPremint = (
    cdk00::BlindedMessage,
    cashu::secret::Secret,
    cdk01::SecretKey,
    Amount,
);

fn legacy_premints(premints: Vec<LegacyPremint>) -> Vec<cdk00::PreMint> {
    premints
        .into_iter()
        .map(|(blinded_message, secret, r, amount)| cdk00::PreMint {
            blinded_message,
            secret,
            r,
            amount,
        })
        .collect()
}

fn secret_bytes(bytes: Vec<u8>) -> Result<[u8; 32]> {
    bytes.try_into().map_err(|b: Vec<u8>| {
        crate::error::Error::Custom(format!("invalid ephemeral secret length {}", b.len()))
    })
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Default)]
struct LegacyMeltEntry {
    quote_id: String,
    premints: Vec<LegacyPremint>,
    kid: Option<ecash::Id>,
}

impl From<LegacyMeltEntry> for MeltEntry {
    fn from(value: LegacyMeltEntry) -> Self {
        Self {
            quote_id: value.quote_id,
            premints: premint_entries(legacy_premints(value.premints)),
            kid: value.kid,
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
struct LegacyMintEntry {
    quote_id: Uuid,
    amount: bitcoin::Amount,
    address: bitcoin::Address<NetworkUnchecked>,
    expiry: u64,
    premints: Vec<LegacyPremint>,
    kid: ecash::Id,
    content: String,
    commitment: bitcoin::secp256k1::schnorr::Signature,
    ephemeral_secret: Vec<u8>,
}

impl TryFrom<LegacyMintEntry> for MintEntry {
    type Error = crate::error::Error;

    fn try_from(value: LegacyMintEntry) -> Result<Self> {
        Ok(Self {
            quote_id: value.quote_id,
            amount_sat: value.amount.to_sat(),
            address: value.address,
            expiry: value.expiry,
            premints: premint_entries(legacy_premints(value.premints)),
            kid: value.kid,
            content: value.content,
            commitment: value.commitment.serialize(),
            ephemeral_secret: secret_bytes(value.ephemeral_secret)?,
        })
    }
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
struct LegacyMeltCommitmentEntry {
    quote_id: Uuid,
    expiry: u64,
    commitment: bitcoin::secp256k1::schnorr::Signature,
    ephemeral_secret: Vec<u8>,
    body_content: String,
}

impl TryFrom<LegacyMeltCommitmentEntry> for MeltCommitmentEntry {
    type Error = crate::error::Error;

    fn try_from(value: LegacyMeltCommitmentEntry) -> Result<Self> {
        Ok(Self {
            quote_id: value.quote_id,
            expiry: value.expiry,
            commitment: value.commitment.serialize(),
            ephemeral_secret: secret_bytes(value.ephemeral_secret)?,
            body_content: value.body_content,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        MintMeltRepository,
        error::Error,
        redb::{
            migration::{collect_wallet_namespace, migrate_wallet},
            mintmelt::MintMeltDB,
        },
        test_utils::tests::valid_payment_address_testnet,
    };
    use bcr_common::{
        cashu::{CurrencyUnit, amount::SplitTarget},
        core_tests,
    };
    use redb::{
        Builder, Database, ReadableDatabase, ReadableTable, TableDefinition,
        backends::InMemoryBackend,
    };
    use std::sync::Arc;

    const WALLET_ID: &str = "wallet-1";
    const EPHEMERAL_SECRET: [u8; 32] = [0x42; 32];

    fn test_database() -> Arc<Database> {
        Arc::new(
            Builder::new()
                .create_with_backend(InMemoryBackend::new())
                .expect("create in-memory database"),
        )
    }

    fn test_keypair() -> bitcoin::secp256k1::Keypair {
        bitcoin::secp256k1::Keypair::new_global(&mut bitcoin::secp256k1::rand::thread_rng())
    }

    fn test_signature() -> bitcoin::secp256k1::schnorr::Signature {
        bitcoin::secp256k1::schnorr::Signature::from_slice(&[0xab; 64]).unwrap()
    }

    fn random_premints() -> cdk00::PreMintSecrets {
        let (_, mintkeyset) = core_tests::generate_random_ecash_keyset();
        let keyset = bcr_wallet_core::util::to_keyset(&mintkeyset, None);
        cdk00::PreMintSecrets::random(
            keyset.id.into(),
            Amount::from(13u64),
            &SplitTarget::None,
            &bcr_wallet_core::util::to_fee_and_amounts(&keyset),
        )
        .unwrap()
    }

    fn legacy_premint_tuples(premints: &cdk00::PreMintSecrets) -> Vec<LegacyPremint> {
        premints
            .secrets
            .iter()
            .map(|p| {
                (
                    p.blinded_message.clone(),
                    p.secret.clone(),
                    p.r.clone(),
                    p.amount,
                )
            })
            .collect()
    }

    fn insert_legacy<T: serde::Serialize>(
        db: &Arc<Database>,
        table_name: &str,
        key: &[u8],
        value: &T,
    ) {
        let mut bytes = Vec::new();
        ciborium::into_writer(value, &mut bytes).unwrap();
        let table_def: TableDefinition<&[u8], Vec<u8>> = TableDefinition::new(table_name);
        let txn = db.begin_write().unwrap();
        {
            let mut table = txn.open_table(table_def).unwrap();
            table.insert(key, bytes).unwrap();
        }
        txn.commit().unwrap();
    }

    fn raw_values(db: &Arc<Database>, table_name: &str) -> Vec<Vec<u8>> {
        let table_def: TableDefinition<&[u8], Vec<u8>> = TableDefinition::new(table_name);
        let txn = db.begin_read().unwrap();
        let table = txn.open_table(table_def).unwrap();
        table
            .iter()
            .unwrap()
            .map(|item| item.unwrap().1.value())
            .collect()
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    struct Seeded {
        melt_premints: cdk00::PreMintSecrets,
        mint_premints: cdk00::PreMintSecrets,
        mint_id: Uuid,
        commitment_id: Uuid,
    }

    fn seed_legacy(db: &Arc<Database>) -> Seeded {
        let unit = CurrencyUnit::Sat;
        let melt_premints = random_premints();
        let mint_premints = random_premints();
        let mint_id = Uuid::new_v4();
        let commitment_id = Uuid::new_v4();

        insert_legacy(
            db,
            &MintMeltDB::melt_table_name(WALLET_ID, &unit),
            b"melt-with-premints",
            &LegacyMeltEntry {
                quote_id: "melt-with-premints".to_string(),
                premints: legacy_premint_tuples(&melt_premints),
                kid: Some(melt_premints.keyset_id.into()),
            },
        );
        insert_legacy(
            db,
            &MintMeltDB::melt_table_name(WALLET_ID, &unit),
            b"melt-without-premints",
            &LegacyMeltEntry {
                quote_id: "melt-without-premints".to_string(),
                ..Default::default()
            },
        );
        insert_legacy(
            db,
            &MintMeltDB::mint_table_name(WALLET_ID, &unit),
            mint_id.as_bytes(),
            &LegacyMintEntry {
                quote_id: mint_id,
                amount: bitcoin::Amount::from_sat(1234),
                address: valid_payment_address_testnet(),
                expiry: 99,
                premints: legacy_premint_tuples(&mint_premints),
                kid: mint_premints.keyset_id.into(),
                content: "bWludA==".to_string(),
                commitment: test_signature(),
                ephemeral_secret: EPHEMERAL_SECRET.to_vec(),
            },
        );
        insert_legacy(
            db,
            &MintMeltDB::melt_commitment_table_name(WALLET_ID, &unit),
            commitment_id.as_bytes(),
            &LegacyMeltCommitmentEntry {
                quote_id: commitment_id,
                expiry: 77,
                commitment: test_signature(),
                ephemeral_secret: EPHEMERAL_SECRET.to_vec(),
                body_content: "bWVsdA==".to_string(),
            },
        );
        Seeded {
            melt_premints,
            mint_premints,
            mint_id,
            commitment_id,
        }
    }

    #[tokio::test]
    async fn migration_0008_converts_legacy_rows_and_hides_secrets() {
        let db = test_database();
        let keys = test_keypair();
        let seeded = seed_legacy(&db);

        let namespace = collect_wallet_namespace(WALLET_ID.to_string(), CurrencyUnit::Sat, keys);
        migrate_wallet(db.clone(), namespace)
            .await
            .expect("migration succeeds");

        let unit = CurrencyUnit::Sat;
        let mut plaintext_secrets: Vec<Vec<u8>> = vec![EPHEMERAL_SECRET.to_vec()];
        for premints in [&seeded.melt_premints, &seeded.mint_premints] {
            for premint in &premints.secrets {
                plaintext_secrets.push(premint.secret.to_string().into_bytes());
            }
        }
        for table in [
            MintMeltDB::melt_table_name(WALLET_ID, &unit),
            MintMeltDB::mint_table_name(WALLET_ID, &unit),
            MintMeltDB::melt_commitment_table_name(WALLET_ID, &unit),
        ] {
            for raw in raw_values(&db, &table) {
                for secret in &plaintext_secrets {
                    assert!(!contains(&raw, secret));
                }
            }
        }

        let repo = MintMeltDB::new(db.clone(), WALLET_ID, &unit, keys).unwrap();

        let mut melts = repo.list_melts().await.unwrap();
        melts.sort();
        assert_eq!(melts, vec!["melt-with-premints", "melt-without-premints"]);
        assert_eq!(
            repo.load_melt("melt-with-premints".to_string())
                .await
                .unwrap(),
            seeded.melt_premints
        );
        assert!(matches!(
            repo.load_melt("melt-without-premints".to_string()).await,
            Err(Error::MeltNotFound(_))
        ));

        let mint = repo.load_mint(seeded.mint_id).await.unwrap();
        assert_eq!(mint.premint, seeded.mint_premints);
        assert_eq!(mint.summary.amount, bitcoin::Amount::from_sat(1234));
        assert_eq!(mint.summary.address, valid_payment_address_testnet());
        assert_eq!(mint.summary.expiry, 99);
        assert_eq!(mint.content, "bWludA==");
        assert_eq!(mint.commitment, test_signature());
        assert_eq!(mint.ephemeral_secret.secret_bytes(), EPHEMERAL_SECRET);
        assert_eq!(repo.list_mints().await.unwrap(), vec![seeded.mint_id]);

        let commitment = repo
            .load_melt_commitment(seeded.commitment_id)
            .await
            .unwrap();
        assert_eq!(commitment.expiry, 77);
        assert_eq!(commitment.commitment, test_signature());
        assert_eq!(commitment.ephemeral_secret.secret_bytes(), EPHEMERAL_SECRET);
        assert_eq!(commitment.body_content, "bWVsdA==");
        assert_eq!(repo.list_melt_commitments().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn migration_0008_is_a_noop_on_second_open_and_empty_database() {
        let keys = test_keypair();
        let namespace = collect_wallet_namespace(WALLET_ID.to_string(), CurrencyUnit::Sat, keys);
        migrate_wallet(test_database(), namespace.clone())
            .await
            .expect("empty migration succeeds");

        let db = test_database();
        seed_legacy(&db);
        migrate_wallet(db.clone(), namespace.clone())
            .await
            .expect("first migration succeeds");
        let tables = [
            MintMeltDB::melt_table_name(WALLET_ID, &CurrencyUnit::Sat),
            MintMeltDB::mint_table_name(WALLET_ID, &CurrencyUnit::Sat),
            MintMeltDB::melt_commitment_table_name(WALLET_ID, &CurrencyUnit::Sat),
        ];
        let before: Vec<_> = tables.iter().map(|t| raw_values(&db, t)).collect();
        migrate_wallet(db.clone(), namespace)
            .await
            .expect("second migration succeeds");
        let after: Vec<_> = tables.iter().map(|t| raw_values(&db, t)).collect();
        assert_eq!(before, after);
    }

    #[tokio::test]
    async fn migration_0008_quarantines_undecodable_and_invalid_secret_rows() {
        #[derive(serde::Serialize)]
        struct TypeError {
            quote_id: Uuid,
            expiry: String,
            commitment: bitcoin::secp256k1::schnorr::Signature,
            ephemeral_secret: Vec<u8>,
            body_content: String,
        }
        let db = test_database();
        let keys = test_keypair();
        let table = MintMeltDB::melt_commitment_table_name(WALLET_ID, &CurrencyUnit::Sat);
        let (decode_id, secret_id) = (Uuid::new_v4(), Uuid::new_v4());
        insert_legacy(
            &db,
            &table,
            decode_id.as_bytes(),
            &TypeError {
                quote_id: decode_id,
                expiry: "not-a-u64".to_string(),
                commitment: test_signature(),
                ephemeral_secret: EPHEMERAL_SECRET.to_vec(),
                body_content: "bWVsdA==".to_string(),
            },
        );
        let short_secret = [0x37u8; 5];
        insert_legacy(
            &db,
            &table,
            secret_id.as_bytes(),
            &LegacyMeltCommitmentEntry {
                quote_id: secret_id,
                expiry: 77,
                commitment: test_signature(),
                ephemeral_secret: short_secret.to_vec(),
                body_content: "bWVsdA==".to_string(),
            },
        );
        let namespace = collect_wallet_namespace(WALLET_ID.to_string(), CurrencyUnit::Sat, keys);
        migrate_wallet(db.clone(), namespace.clone()).await.unwrap();
        migrate_wallet(db.clone(), namespace).await.unwrap();
        assert!(raw_values(&db, &table).is_empty());
        let quarantined = raw_values(&db, &format!("{table}_quarantine"));
        assert_eq!(quarantined.len(), 2);
        for row in quarantined {
            assert!(!contains(&row, &EPHEMERAL_SECRET));
            assert!(!contains(&row, &short_secret));
        }
    }
}
