pub mod tests {
    use bcr_wallet_core::{types::Seed, util};
    use bitcoin::secp256k1;
    use std::str::FromStr;

    #[cfg(feature = "redb")]
    pub fn in_memory_pocket_db(
        wallet_id: &str,
        unit: bcr_common::cashu::CurrencyUnit,
    ) -> crate::redb::pocket::PocketDB {
        let db = std::sync::Arc::new(
            ::redb::Builder::new()
                .create_with_backend(::redb::backends::InMemoryBackend::new())
                .expect("can create in-memory redb"),
        );
        let keypair = secp256k1::Keypair::new_global(&mut secp256k1::rand::thread_rng());
        crate::redb::pocket::PocketDB::new(db, wallet_id, &unit, keypair)
            .expect("can create PocketDB")
    }

    pub fn zero_seed() -> Seed {
        [0u8; 64]
    }

    pub fn valid_payment_address_testnet() -> bitcoin::Address<bitcoin::address::NetworkUnchecked> {
        bitcoin::Address::from_str("tb1qteyk7pfvvql2r2zrsu4h4xpvju0nz7ykvguyk0").unwrap()
    }

    pub fn wallet_id() -> String {
        let seed = zero_seed();
        util::build_wallet_id(&seed, bitcoin::Network::Testnet)
    }

    pub fn test_pub_key() -> secp256k1::PublicKey {
        secp256k1::PublicKey::from_str(
            "03f9f94d1fdc2090d46f3524807e3f58618c36988e69577d70d5d4d1e9e9645a4f",
        )
        .expect("valid key")
    }

    pub fn test_other_pub_key() -> secp256k1::PublicKey {
        secp256k1::PublicKey::from_str(
            "02295fb5f4eeb2f21e01eaf3a2d9a3be10f39db870d28f02146130317973a40ac0",
        )
        .expect("valid key")
    }
}
