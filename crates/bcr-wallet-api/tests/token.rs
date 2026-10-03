use bcr_common::{
    cashu::{self, CurrencyUnit},
    core_tests,
    wallet::Token,
};
use bcr_wallet_api::{MAX_TOKEN_SIZE_BYTES, error::Error, is_valid_token};
use std::str::FromStr;

#[test]
fn test_is_valid_token_errors_on_oversized_token() {
    let mint_url = url::Url::from_str("https://mint.example").unwrap();
    let amounts = vec![cashu::Amount::from(1); 2000];
    let (_info, keyset) = core_tests::generate_random_ecash_keyset();
    let proofs = core_tests::generate_random_ecash_proofs(&keyset, &amounts);

    let token = Token::new_bitcr(
        bcr_wallet_core::util::to_mint_url(&mint_url),
        proofs.into_iter().map(|p| p.into()).collect(),
        Some("oversized token".to_string()),
        CurrencyUnit::Sat,
    );
    let token_str = token.to_string();
    assert!(token_str.len() > MAX_TOKEN_SIZE_BYTES);

    let err = is_valid_token(&token_str).unwrap_err();

    match err {
        Error::TokenTooLarge(_, _) => {}
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn test_is_valid_token_errors_on_oversized_raw_input_that_reencodes_small() {
    let mint_url = url::Url::from_str("https://mint.example").unwrap();
    let (_info, keyset) = core_tests::generate_random_ecash_keyset();
    let proofs = core_tests::generate_random_ecash_proofs(&keyset, &[cashu::Amount::from(1)]);

    let token = Token::new_bitcr(
        bcr_wallet_core::util::to_mint_url(&mint_url),
        proofs.into_iter().map(|p| p.into()).collect(),
        Some("small token".to_string()),
        CurrencyUnit::Sat,
    );
    let token_str = token.to_string();
    assert!(token_str.len() < MAX_TOKEN_SIZE_BYTES);

    let base = token_str.trim_end_matches('=');
    let padding = "A".repeat(MAX_TOKEN_SIZE_BYTES);
    let oversized = format!("{base}{padding}");
    assert!(oversized.len() > MAX_TOKEN_SIZE_BYTES);
    assert_eq!(Token::from_str(&oversized).unwrap().to_string(), token_str);

    let err = is_valid_token(&oversized).unwrap_err();

    match err {
        Error::TokenTooLarge(_, _) => {}
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn test_is_valid_token_errors_on_duplicate_proofs() {
    let mint_url = url::Url::from_str("https://mint.example").unwrap();
    let (_info, keyset) = core_tests::generate_random_ecash_keyset();
    let proofs = core_tests::generate_random_ecash_proofs(&keyset, &[cashu::Amount::from(8)]);
    let proof = proofs[0].clone();

    let token = Token::new_bitcr(
        bcr_wallet_core::util::to_mint_url(&mint_url),
        vec![proof.clone().into(), proof.into()],
        Some("duplicate token".to_string()),
        CurrencyUnit::Sat,
    );

    let err = is_valid_token(&token.to_string()).unwrap_err();

    match err {
        Error::Token(bcr_common::wallet::Error::DuplicateProofs) => {}
        other => panic!("unexpected error: {other:?}"),
    }
}
