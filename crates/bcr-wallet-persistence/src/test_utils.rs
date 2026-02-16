pub mod tests {
    use bcr_wallet_core::util;
    use bitcoin::secp256k1;
    use std::str::FromStr;

    pub fn valid_payment_address_testnet() -> bitcoin::Address<bitcoin::address::NetworkUnchecked> {
        bitcoin::Address::from_str("tb1qteyk7pfvvql2r2zrsu4h4xpvju0nz7ykvguyk0").unwrap()
    }

    pub fn wallet_id() -> String {
        let seed = [0u8; 64];
        util::build_wallet_id(&seed)
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
