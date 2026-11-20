use crate::{
    error::Result,
    redb::{
        migration::{WalletStorageNamespace, migrate_table},
        mintmelt::{MeltCommitmentEntry, MeltEntry, MintEntry, encode, premint_entries},
        nostr::{
            NostrEventOffsetEntry, NostrQueuedMessageEntry, encode_event_offset,
            encode_queued_message,
        },
        payment_request::{
            PaymentRequestEntry, PaymentRequestEntryDirection, PaymentRequestEntryHistoryEntry,
            PaymentRequestEntryOrigin, PaymentRequestEntryState, encode_payment_request,
        },
    },
};
use bcr_common::{
    cashu::{self, Amount, CurrencyUnit, nut00 as cdk00, nut01 as cdk01},
    core::NodeId,
    ecash,
};
use bitcoin::address::NetworkUnchecked;
use nostr::types::Timestamp;
use uuid::Uuid;

///////////////////////////////////////////////////////////////// MIGRATION 0006
const MIGRATION_0006_RECORD_ENVELOPES: &str = "0006_record_envelopes";

pub(super) fn migration_name_for_wallet(wallet_id: &str) -> String {
    format!("{}_{}", MIGRATION_0006_RECORD_ENVELOPES, wallet_id)
}

pub(super) fn migration_0006_record_envelopes(
    txn: &redb::WriteTransaction,
    namespace: &WalletStorageNamespace,
) -> Result<()> {
    tracing::info!("Migrating nostr, payment request and mint/melt tables..");
    migrate_table(
        txn,
        &namespace.offset_entry_table,
        &namespace.keys,
        |bytes| {
            let legacy: LegacyOffsetEntry = ciborium::from_reader(bytes)?;
            encode_event_offset(&legacy.into())
        },
    )?;
    for table_name in [
        &namespace.queued_message_table,
        &namespace.dead_letter_table,
    ] {
        migrate_table(txn, table_name, &namespace.keys, |bytes| {
            let legacy: LegacyQueuedMessageEntry = ciborium::from_reader(bytes)?;
            encode_queued_message(&legacy.into(), &namespace.keys)
        })?;
    }
    migrate_table(
        txn,
        &namespace.payment_request_table,
        &namespace.keys,
        |bytes| {
            let legacy: LegacyPaymentRequestEntry = ciborium::from_reader(bytes)?;
            encode_payment_request(&legacy.into())
        },
    )?;
    migrate_table(txn, &namespace.melt_table, &namespace.keys, |bytes| {
        let legacy: LegacyMeltEntry = ciborium::from_reader(bytes)?;
        encode(&MeltEntry::from(legacy), &namespace.keys)
    })?;
    migrate_table(txn, &namespace.mint_table, &namespace.keys, |bytes| {
        let legacy: LegacyMintEntry = ciborium::from_reader(bytes)?;
        encode(&MintEntry::try_from(legacy)?, &namespace.keys)
    })?;
    migrate_table(
        txn,
        &namespace.melt_commitment_table,
        &namespace.keys,
        |bytes| {
            let legacy: LegacyMeltCommitmentEntry = ciborium::from_reader(bytes)?;
            encode(&MeltCommitmentEntry::try_from(legacy)?, &namespace.keys)
        },
    )?;
    tracing::info!("Migrated nostr, payment request and mint/melt tables.");
    Ok(())
}

///////////////////////////////////////////// Legacy entries
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
struct LegacyOffsetEntry {
    event_id: String,
    time: Timestamp,
    success: bool,
}

impl From<LegacyOffsetEntry> for NostrEventOffsetEntry {
    fn from(value: LegacyOffsetEntry) -> Self {
        Self {
            event_id: value.event_id,
            time: value.time,
            success: value.success,
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
struct LegacyQueuedMessageEntry {
    id: String,
    recipient: Option<String>,
    payload: String,
    #[serde(with = "time::serde::rfc3339")]
    created: time::OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    last_try: time::OffsetDateTime,
    num_retries: i32,
    max_retries: i32,
    #[serde(with = "time::serde::rfc3339")]
    processing_started_at: time::OffsetDateTime,
}

impl From<LegacyQueuedMessageEntry> for NostrQueuedMessageEntry {
    fn from(value: LegacyQueuedMessageEntry) -> Self {
        Self {
            id: value.id,
            recipient: value.recipient,
            payload: value.payload,
            created: value.created,
            last_try: value.last_try,
            num_retries: value.num_retries,
            max_retries: value.max_retries,
            processing_started_at: value.processing_started_at,
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
enum LegacyDirection {
    Incoming,
    Outgoing,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
enum LegacyState {
    Pending,
    Paid { tx_id: Uuid },
    Canceled,
    Rejected,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
enum LegacyOrigin {
    Local,
    Remote { event_id: String },
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
struct LegacyHistoryEntry {
    state: LegacyState,
    applied: bool,
    actor: Option<NodeId>,
    at: u64,
    origin: LegacyOrigin,
    reason: Option<String>,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
struct LegacyPaymentRequestEntry {
    id: Uuid,
    #[serde(default)]
    node_id: Option<NodeId>,
    amount: Amount,
    unit: CurrencyUnit,
    description: Option<String>,
    deadline: Option<u64>,
    created_at: u64,
    state: LegacyState,
    direction: LegacyDirection,
    #[serde(default)]
    history: Vec<LegacyHistoryEntry>,
    #[serde(default)]
    tombstone: bool,
}

impl From<LegacyDirection> for PaymentRequestEntryDirection {
    fn from(value: LegacyDirection) -> Self {
        match value {
            LegacyDirection::Incoming => Self::Incoming,
            LegacyDirection::Outgoing => Self::Outgoing,
        }
    }
}

impl From<LegacyState> for PaymentRequestEntryState {
    fn from(value: LegacyState) -> Self {
        match value {
            LegacyState::Pending => Self::Pending,
            LegacyState::Paid { tx_id } => Self::Paid { tx_id },
            LegacyState::Canceled => Self::Canceled,
            LegacyState::Rejected => Self::Rejected,
        }
    }
}

impl From<LegacyOrigin> for PaymentRequestEntryOrigin {
    fn from(value: LegacyOrigin) -> Self {
        match value {
            LegacyOrigin::Local => Self::Local,
            LegacyOrigin::Remote { event_id } => Self::Remote { event_id },
        }
    }
}

impl From<LegacyHistoryEntry> for PaymentRequestEntryHistoryEntry {
    fn from(value: LegacyHistoryEntry) -> Self {
        Self {
            state: value.state.into(),
            applied: value.applied,
            actor: value.actor,
            at: value.at,
            origin: value.origin.into(),
            reason: value.reason,
        }
    }
}

impl From<LegacyPaymentRequestEntry> for PaymentRequestEntry {
    fn from(value: LegacyPaymentRequestEntry) -> Self {
        Self {
            id: value.id,
            node_id: value.node_id,
            amount: value.amount,
            unit: value.unit,
            description: value.description,
            deadline: value.deadline,
            created_at: value.created_at,
            state: value.state.into(),
            direction: value.direction.into(),
            history: value.history.into_iter().map(Into::into).collect(),
            tombstone: value.tombstone,
        }
    }
}

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
        MintMeltRepository, NostrRepository, PaymentRequestStoreApi,
        error::Error,
        redb::{
            migration::{collect_wallet_namespace, migrate_wallet},
            mintmelt::MintMeltDB,
            nostr::NostrDB,
            payment_request::PaymentRequestDB,
        },
        test_utils::tests::valid_payment_address_testnet,
    };
    use bcr_common::{cashu::amount::SplitTarget, core_tests};
    use bcr_wallet_core::types::{PaymentRequestDirection, PaymentRequestState};
    use redb::{
        Builder, Database, ReadableDatabase, ReadableTable, TableDefinition,
        backends::InMemoryBackend,
    };
    use std::sync::Arc;

    const WALLET_ID: &str = "wallet-1";
    const SECRET_PAYLOAD: &str = "super-secret-queued-payload";
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

    fn insert_raw(db: &Arc<Database>, table_name: &str, key: &[u8], bytes: Vec<u8>) {
        let table_def: TableDefinition<&[u8], Vec<u8>> = TableDefinition::new(table_name);
        let txn = db.begin_write().unwrap();
        {
            let mut table = txn.open_table(table_def).unwrap();
            table.insert(key, bytes).unwrap();
        }
        txn.commit().unwrap();
    }

    fn insert_legacy<T: serde::Serialize>(
        db: &Arc<Database>,
        table_name: &str,
        key: &[u8],
        value: &T,
    ) {
        let mut bytes = Vec::new();
        ciborium::into_writer(value, &mut bytes).unwrap();
        insert_raw(db, table_name, key, bytes);
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

    fn legacy_message(id: &str, retries: i32) -> LegacyQueuedMessageEntry {
        LegacyQueuedMessageEntry {
            id: id.to_string(),
            recipient: Some("recipient".to_string()),
            payload: SECRET_PAYLOAD.to_string(),
            created: time::OffsetDateTime::from_unix_timestamp(1_000).unwrap(),
            last_try: time::OffsetDateTime::from_unix_timestamp(0).unwrap(),
            num_retries: retries,
            max_retries: 5,
            processing_started_at: time::OffsetDateTime::from_unix_timestamp(0).unwrap(),
        }
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

    #[tokio::test]
    async fn migration_0006_converts_nostr_rows_and_hides_payload() {
        let db = test_database();
        let keys = test_keypair();
        let tables = [
            NostrDB::queued_message_table_name(WALLET_ID),
            NostrDB::dead_letter_table_name(WALLET_ID),
        ];
        insert_legacy(
            &db,
            &NostrDB::offset_entry_table_name(WALLET_ID),
            b"event-1",
            &LegacyOffsetEntry {
                event_id: "event-1".to_string(),
                time: Timestamp::from(100),
                success: true,
            },
        );
        insert_legacy(&db, &tables[0], b"queued-1", &legacy_message("queued-1", 1));
        insert_legacy(&db, &tables[1], b"dead-1", &legacy_message("dead-1", 5));

        let namespace = collect_wallet_namespace(WALLET_ID.to_string(), CurrencyUnit::Sat, keys);
        migrate_wallet(db.clone(), namespace).await.unwrap();

        for table in &tables {
            for raw in raw_values(&db, table) {
                assert!(!contains(&raw, SECRET_PAYLOAD.as_bytes()));
            }
        }
        let repo = NostrDB::new(db.clone(), WALLET_ID, keys).unwrap();
        assert!(repo.is_processed("event-1").await.unwrap());
        let messages = repo.get_retry_messages(10).await.unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].id, "queued-1");
        assert_eq!(messages[0].payload, SECRET_PAYLOAD);
    }

    #[tokio::test]
    async fn migration_0006_converts_mint_melt_rows_and_hides_secrets() {
        let db = test_database();
        let keys = test_keypair();
        let unit = CurrencyUnit::Sat;
        let melt_premints = random_premints();
        let mint_premints = random_premints();
        let (mint_id, commitment_id) = (Uuid::new_v4(), Uuid::new_v4());

        insert_legacy(
            &db,
            &MintMeltDB::melt_table_name(WALLET_ID, &unit),
            b"melt-1",
            &LegacyMeltEntry {
                quote_id: "melt-1".to_string(),
                premints: legacy_premint_tuples(&melt_premints),
                kid: Some(melt_premints.keyset_id.into()),
            },
        );
        insert_legacy(
            &db,
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
            &db,
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

        let namespace = collect_wallet_namespace(WALLET_ID.to_string(), unit.clone(), keys);
        migrate_wallet(db.clone(), namespace).await.unwrap();

        let mut plaintext_secrets: Vec<Vec<u8>> = vec![EPHEMERAL_SECRET.to_vec()];
        for premint in melt_premints.secrets.iter().chain(&mint_premints.secrets) {
            plaintext_secrets.push(premint.secret.to_string().into_bytes());
        }
        for table in [
            MintMeltDB::melt_table_name(WALLET_ID, &unit),
            MintMeltDB::mint_table_name(WALLET_ID, &unit),
            MintMeltDB::melt_commitment_table_name(WALLET_ID, &unit),
        ] {
            for raw in raw_values(&db, &table) {
                assert!(plaintext_secrets.iter().all(|s| !contains(&raw, s)));
            }
        }

        let repo = MintMeltDB::new(db.clone(), WALLET_ID, &unit, keys).unwrap();
        assert_eq!(
            repo.load_melt("melt-1".to_string()).await.unwrap(),
            melt_premints
        );
        let mint = repo.load_mint(mint_id).await.unwrap();
        assert_eq!(mint.premint, mint_premints);
        assert_eq!(mint.summary.amount, bitcoin::Amount::from_sat(1234));
        assert_eq!(mint.ephemeral_secret.secret_bytes(), EPHEMERAL_SECRET);
        let commitment = repo.load_melt_commitment(commitment_id).await.unwrap();
        assert_eq!(commitment.expiry, 77);
        assert_eq!(commitment.ephemeral_secret.secret_bytes(), EPHEMERAL_SECRET);
        assert!(matches!(
            repo.load_melt("missing".to_string()).await,
            Err(Error::MeltNotFound(_))
        ));
    }

    #[tokio::test]
    async fn migration_0006_applies_defaults_to_old_payment_requests() {
        #[derive(serde::Serialize)]
        struct OlderEntry {
            id: Uuid,
            amount: Amount,
            unit: CurrencyUnit,
            description: Option<String>,
            deadline: Option<u64>,
            created_at: u64,
            state: LegacyState,
            direction: LegacyDirection,
        }
        let db = test_database();
        let keys = test_keypair();
        let (id, tx_id) = (Uuid::new_v4(), Uuid::new_v4());
        insert_legacy(
            &db,
            &PaymentRequestDB::payment_requests_table_name(WALLET_ID),
            id.as_bytes(),
            &OlderEntry {
                id,
                amount: Amount::from(9u64),
                unit: CurrencyUnit::Sat,
                description: None,
                deadline: None,
                created_at: 4,
                state: LegacyState::Paid { tx_id },
                direction: LegacyDirection::Incoming,
            },
        );

        let namespace = collect_wallet_namespace(WALLET_ID.to_string(), CurrencyUnit::Sat, keys);
        migrate_wallet(db.clone(), namespace).await.unwrap();

        let repo = PaymentRequestDB::new(db, WALLET_ID).unwrap();
        let loaded = repo.get_payment_request(id).await.unwrap().unwrap();
        assert_eq!(loaded.state, PaymentRequestState::Paid { tx_id });
        assert_eq!(loaded.direction, PaymentRequestDirection::Incoming);
        assert_eq!(loaded.node_id, None);
        assert!(loaded.history.is_empty());
        assert!(!loaded.tombstone);
    }

    #[tokio::test]
    async fn migration_0006_quarantines_unconvertible_rows() {
        let db = test_database();
        let keys = test_keypair();
        let short_secret = [0x37u8; 5];
        let commitment_table =
            MintMeltDB::melt_commitment_table_name(WALLET_ID, &CurrencyUnit::Sat);
        let queue_table = NostrDB::queued_message_table_name(WALLET_ID);
        let request_table = PaymentRequestDB::payment_requests_table_name(WALLET_ID);
        let secret_id = Uuid::new_v4();
        insert_raw(&db, &queue_table, b"bad", vec![0xff, 0x01]);
        insert_legacy(&db, &request_table, b"bad", &"not a payment request");
        insert_legacy(
            &db,
            &commitment_table,
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

        for table in [&queue_table, &request_table, &commitment_table] {
            assert!(raw_values(&db, table).is_empty());
            let quarantined = raw_values(&db, &format!("{table}_quarantine"));
            assert_eq!(quarantined.len(), 1);
            assert!(!contains(&quarantined[0], &short_secret));
        }
    }
}
