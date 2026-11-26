use crate::{
    PushClaim,
    error::Result,
    redb::{
        migration::WalletStorageNamespace,
        nostr::{NostrEventOffsetEntry, decode_event_offset, encode_push_claim},
    },
};
use redb::{ReadableTable, TableDefinition, TableError};

///////////////////////////////////////////////////////////////// MIGRATION 0007
const MIGRATION_0007_PUSH_CLAIMS: &str = "0007_push_claims";

pub(super) fn migration_name_for_wallet(wallet_id: &str) -> String {
    format!("{}_{}", MIGRATION_0007_PUSH_CLAIMS, wallet_id)
}

pub(super) fn migration_0007_push_claims(
    txn: &redb::WriteTransaction,
    namespace: &WalletStorageNamespace,
) -> Result<()> {
    tracing::info!("Creating push claim table..");
    let offsets: TableDefinition<&[u8], Vec<u8>> =
        TableDefinition::new(&namespace.offset_entry_table);
    let claims: TableDefinition<&[u8], Vec<u8>> = TableDefinition::new(&namespace.push_claim_table);

    let processed: Vec<NostrEventOffsetEntry> = match txn.open_table(offsets) {
        Ok(table) => table
            .iter()?
            .map(|row| {
                let (_, v) = row?;
                decode_event_offset(v.value().as_slice())
            })
            .collect::<Result<_>>()?,
        Err(TableError::TableDoesNotExist(_)) => Vec::new(),
        Err(e) => return Err(e.into()),
    };

    let mut table = txn.open_table(claims)?;
    for offset in processed {
        let claim = PushClaim {
            event_id: offset.event_id,
            received_at: None,
            claimed_at: Some(offset.time),
            completed_at: Some(offset.time),
            ..Default::default()
        };
        table.insert(claim.event_id.as_bytes(), encode_push_claim(&claim)?)?;
    }
    tracing::info!("Created push claim table.");
    Ok(())
}
