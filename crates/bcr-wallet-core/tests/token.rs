use bcr_wallet_core::wallet::*;
use std::str::FromStr;

#[test]
fn test_tokenv4_from_tokenv3() {
    let token_v3_str = "cashuAeyJ0b2tlbiI6W3sibWludCI6Imh0dHBzOi8vODMzMy5zcGFjZTozMzM4IiwicHJvb2ZzIjpbeyJhbW91bnQiOjIsImlkIjoiMDA5YTFmMjkzMjUzZTQxZSIsInNlY3JldCI6IjQwNzkxNWJjMjEyYmU2MWE3N2UzZTZkMmFlYjRjNzI3OTgwYmRhNTFjZDA2YTZhZmMyOWUyODYxNzY4YTc4MzciLCJDIjoiMDJiYzkwOTc5OTdkODFhZmIyY2M3MzQ2YjVlNDM0NWE5MzQ2YmQyYTUwNmViNzk1ODU5OGE3MmYwY2Y4NTE2M2VhIn0seyJhbW91bnQiOjgsImlkIjoiMDA5YTFmMjkzMjUzZTQxZSIsInNlY3JldCI6ImZlMTUxMDkzMTRlNjFkNzc1NmIwZjhlZTBmMjNhNjI0YWNhYTNmNGUwNDJmNjE0MzNjNzI4YzcwNTdiOTMxYmUiLCJDIjoiMDI5ZThlNTA1MGI4OTBhN2Q2YzA5NjhkYjE2YmMxZDVkNWZhMDQwZWExZGUyODRmNmVjNjlkNjEyOTlmNjcxMDU5In1dfV0sInVuaXQiOiJzYXQiLCJtZW1vIjoiVGhhbmsgeW91LiJ9";
    let token_v3 = Token::from_str(token_v3_str).expect("TokenV3 should be created from string");
    if let Token::CashuV3(token_v3) = token_v3 {
        let token_v4 =
            cashu::TokenV4::try_from(token_v3).expect("TokenV3 should be converted to TokenV4");
        let token_v4_expected = "cashuBpGFtd2h0dHBzOi8vODMzMy5zcGFjZTozMzM4YXVjc2F0YWRqVGhhbmsgeW91LmF0gaJhaUgAmh8pMlPkHmFwgqRhYQJhc3hANDA3OTE1YmMyMTJiZTYxYTc3ZTNlNmQyYWViNGM3Mjc5ODBiZGE1MWNkMDZhNmFmYzI5ZTI4NjE3NjhhNzgzN2FjWCECvJCXmX2Br7LMc0a15DRak0a9KlBut5WFmKcvDPhRY-phZPakYWEIYXN4QGZlMTUxMDkzMTRlNjFkNzc1NmIwZjhlZTBmMjNhNjI0YWNhYTNmNGUwNDJmNjE0MzNjNzI4YzcwNTdiOTMxYmVhY1ghAp6OUFC4kKfWwJaNsWvB1dX6BA6h3ihPbsadYSmfZxBZYWT2";
        assert_eq!(token_v4.to_string(), token_v4_expected);
    } else {
        panic!("TokenV3 expected");
    }
}

#[test]
fn test_token_str_round_trip() {
    let token_str = "bitcrAeyJ0b2tlbiI6W3sibWludCI6Imh0dHBzOi8vODMzMy5zcGFjZTozMzM4IiwicHJvb2ZzIjpbeyJhbW91bnQiOjIsImlkIjoiMDA5YTFmMjkzMjUzZTQxZSIsInNlY3JldCI6IjQwNzkxNWJjMjEyYmU2MWE3N2UzZTZkMmFlYjRjNzI3OTgwYmRhNTFjZDA2YTZhZmMyOWUyODYxNzY4YTc4MzciLCJDIjoiMDJiYzkwOTc5OTdkODFhZmIyY2M3MzQ2YjVlNDM0NWE5MzQ2YmQyYTUwNmViNzk1ODU5OGE3MmYwY2Y4NTE2M2VhIn0seyJhbW91bnQiOjgsImlkIjoiMDA5YTFmMjkzMjUzZTQxZSIsInNlY3JldCI6ImZlMTUxMDkzMTRlNjFkNzc1NmIwZjhlZTBmMjNhNjI0YWNhYTNmNGUwNDJmNjE0MzNjNzI4YzcwNTdiOTMxYmUiLCJDIjoiMDI5ZThlNTA1MGI4OTBhN2Q2YzA5NjhkYjE2YmMxZDVkNWZhMDQwZWExZGUyODRmNmVjNjlkNjEyOTlmNjcxMDU5In1dfV0sInVuaXQiOiJzYXQiLCJtZW1vIjoiVGhhbmsgeW91LiJ9";

    let token = Token::from_str(token_str).unwrap();
    let v4: cashu::TokenV4 = TryFrom::try_from(token.clone()).unwrap();
    assert_eq!(v4.token.len(), 1);
    assert_eq!(v4.token[0].keyset_id.to_string(), "009a1f293253e41e");

    token.to_string().strip_prefix("bitcrA").unwrap();

    if let Token::BitcrV3(token) = token {
        assert_eq!(token.token[0].mint.to_string(), "https://8333.space:3338");

        assert_eq!(
            token.token[0].proofs[0].clone().keyset_id,
            cashu::Id::from_str("009a1f293253e41e").unwrap()
        );
        assert_eq!(token.unit.clone().unwrap(), cashu::CurrencyUnit::Sat);

        let encoded = &token.to_string();

        let token_data = cashu::TokenV3::from_str(encoded).unwrap();

        assert_eq!(token_data, token);
    } else {
        panic!("Token is not V3");
    }
}

#[test]
fn incorrect_tokens() {
    let incorrect_prefix = "casshuAeyJ0b2tlbiI6W3sibWludCI6Imh0dHBzOi8vODMzMy5zcGFjZTozMzM4IiwicHJvb2ZzIjpbeyJhbW91bnQiOjIsImlkIjoiMDA5YTFmMjkzMjUzZTQxZSIsInNlY3JldCI6IjQwNzkxNWJjMjEyYmU2MWE3N2UzZTZkMmFlYjRjNzI3OTgwYmRhNTFjZDA2YTZhZmMyOWUyODYxNzY4YTc4MzciLCJDIjoiMDJiYzkwOTc5OTdkODFhZmIyY2M3MzQ2YjVlNDM0NWE5MzQ2YmQyYTUwNmViNzk1ODU5OGE3MmYwY2Y4NTE2M2VhIn0seyJhbW91bnQiOjgsImlkIjoiMDA5YTFmMjkzMjUzZTQxZSIsInNlY3JldCI6ImZlMTUxMDkzMTRlNjFkNzc1NmIwZjhlZTBmMjNhNjI0YWNhYTNmNGUwNDJmNjE0MzNjNzI4YzcwNTdiOTMxYmUiLCJDIjoiMDI5ZThlNTA1MGI4OTBhN2Q2YzA5NjhkYjE2YmMxZDVkNWZhMDQwZWExZGUyODRmNmVjNjlkNjEyOTlmNjcxMDU5In1dfV0sInVuaXQiOiJzYXQiLCJtZW1vIjoiVGhhbmsgeW91LiJ9";

    let incorrect_prefix_token = Token::from_str(incorrect_prefix);

    assert!(incorrect_prefix_token.is_err());

    let no_prefix = "eyJ0b2tlbiI6W3sibWludCI6Imh0dHBzOi8vODMzMy5zcGFjZTozMzM4IiwicHJvb2ZzIjpbeyJhbW91bnQiOjIsImlkIjoiMDA5YTFmMjkzMjUzZTQxZSIsInNlY3JldCI6IjQwNzkxNWJjMjEyYmU2MWE3N2UzZTZkMmFlYjRjNzI3OTgwYmRhNTFjZDA2YTZhZmMyOWUyODYxNzY4YTc4MzciLCJDIjoiMDJiYzkwOTc5OTdkODFhZmIyY2M3MzQ2YjVlNDM0NWE5MzQ2YmQyYTUwNmViNzk1ODU5OGE3MmYwY2Y4NTE2M2VhIn0seyJhbW91bnQiOjgsImlkIjoiMDA5YTFmMjkzMjUzZTQxZSIsInNlY3JldCI6ImZlMTUxMDkzMTRlNjFkNzc1NmIwZjhlZTBmMjNhNjI0YWNhYTNmNGUwNDJmNjE0MzNjNzI4YzcwNTdiOTMxYmUiLCJDIjoiMDI5ZThlNTA1MGI4OTBhN2Q2YzA5NjhkYjE2YmMxZDVkNWZhMDQwZWExZGUyODRmNmVjNjlkNjEyOTlmNjcxMDU5In1dfV0sInVuaXQiOiJzYXQiLCJtZW1vIjoiVGhhbmsgeW91LiJ9";

    let no_prefix_token = Token::from_str(no_prefix);

    assert!(no_prefix_token.is_err());

    let correct_token = "cashuAeyJ0b2tlbiI6W3sibWludCI6Imh0dHBzOi8vODMzMy5zcGFjZTozMzM4IiwicHJvb2ZzIjpbeyJhbW91bnQiOjIsImlkIjoiMDA5YTFmMjkzMjUzZTQxZSIsInNlY3JldCI6IjQwNzkxNWJjMjEyYmU2MWE3N2UzZTZkMmFlYjRjNzI3OTgwYmRhNTFjZDA2YTZhZmMyOWUyODYxNzY4YTc4MzciLCJDIjoiMDJiYzkwOTc5OTdkODFhZmIyY2M3MzQ2YjVlNDM0NWE5MzQ2YmQyYTUwNmViNzk1ODU5OGE3MmYwY2Y4NTE2M2VhIn0seyJhbW91bnQiOjgsImlkIjoiMDA5YTFmMjkzMjUzZTQxZSIsInNlY3JldCI6ImZlMTUxMDkzMTRlNjFkNzc1NmIwZjhlZTBmMjNhNjI0YWNhYTNmNGUwNDJmNjE0MzNjNzI4YzcwNTdiOTMxYmUiLCJDIjoiMDI5ZThlNTA1MGI4OTBhN2Q2YzA5NjhkYjE2YmMxZDVkNWZhMDQwZWExZGUyODRmNmVjNjlkNjEyOTlmNjcxMDU5In1dfV0sInVuaXQiOiJzYXQiLCJtZW1vIjoiVGhhbmsgeW91LiJ9";

    let correct_token = Token::from_str(correct_token);

    assert!(correct_token.is_ok());
}

#[test]
fn test_token_v4_str_round_trip() {
    let token_str = "bitcrBpGF0gaJhaUgArSaMTR9YJmFwgaNhYQFhc3hAOWE2ZGJiODQ3YmQyMzJiYTc2ZGIwZGYxOTcyMTZiMjlkM2I4Y2MxNDU1M2NkMjc4MjdmYzFjYzk0MmZlZGI0ZWFjWCEDhhhUP_trhpXfStS6vN6So0qWvc2X3O4NfM-Y1HISZ5JhZGlUaGFuayB5b3VhbXVodHRwOi8vbG9jYWxob3N0OjMzMzhhdWNzYXQ=";
    let token = Token::from_str(token_str).unwrap();
    token.to_string().strip_prefix("bitcrB").unwrap();
    if let Token::BitcrV4(token) = token {
        assert_eq!(
            token.mint_url,
            cashu::MintUrl::from_str("http://localhost:3338").unwrap()
        );
        assert_eq!(
            token.token[0].keyset_id,
            cashu::Id::from_str("00ad268c4d1f5826").unwrap()
        );

        let encoded = &token.to_string();

        let token_data = cashu::TokenV4::from_str(encoded).unwrap();

        assert_eq!(token_data, token);
    } else {
        panic!("Unexpected token type");
    }
}

#[test]
fn test_token_value() {
    let token_str = "bitcrBo2FtdWh0dHA6Ly9sb2NhbGhvc3Q6NDM0M2F1ZWNyc2F0YXSBomFpSABp3j5af6uYYXCHpGFhGEBhc3hAODcyYmIxNzY0ODA3NDY2YWUxMDY2MGQxMjA5ODUxYzQ2MGJmZjJmNDZiY2YyZmJmM2QzY2NjY2QyYzllMzNiMGFjWCECgISwm2AJEFh3vxZKCNjnxx3pZ8BBav7a5AXLtMVQVjRhZPakYWEYgGFzeEBhY2QzYzI5YjlhZjEwYmM4MTdiOWUxNGFhMjllZjIxODkzYmZjZWMwMzFmYWQyM2IxOWExMDhjMzFhZmQyODMyYWNYIQIMmOnUpdbYTBtRceuCXy_qajysL6sG9CsvtRSBukjWO2Fk9qRhYRkCAGFzeEA4ZmU1NDNmOTMxYjA4MzhhOTA3NmMyMjljNzg1OWU3MTc0MTUzMGVmMGFiZWMyMzlkOWE0ZWNjOGEyMGNlYzRmYWNYIQPqj23wVNNNx42KP28By2a5i6N5TMkVU8lixcZ3aeiA7WFk9qRhYQRhc3hAMzk4YjYzMmU4MTZmNzQ4Njc1N2E3NTk5Mzc2YjlhYmFkMGFmNGQwMTVkYTQ0Mjk5Zjg2OGYxNWM4ODdmNDNjYmFjWCEDo8X2Y4JoRJ1hGSXDSVgQH-YXpFw_NYXtPIUv5xJcX-9hZPakYWEIYXN4QGJjNjM4NTYxN2Q2NjJkN2Q5NWIxNDBlMTU4Y2MzMTYwZjAzMmQxMWJiZGEzZWY3MDRhYzcyOTliM2EzYjQyOThhY1ghA_UAeY1dWx5QHqsvepcUK68xfHZJIbuRCaM45uN4t9vsYWT2pGFhGQEAYXN4QDFlNGQ1ZGI1MTc2MzU2YWEwZTI2MzJmZDlkYTUxMjYzYmY1M2EyMjFkNmNhZmE5Y2U4YTExMjg4MGNhMWQwZmZhY1ghAm3brXrx4F8HY8-YeC-msEuI9vfSzBKayKzab58A6xYwYWT2pGFhAWFzeEAwNzcyNTMyYTJkMjZkNDcyOTZjNzQ3NzMxN2NhZjQzOTdjZjA4MmM0ZjkwMzE4YWJjMDljZGRmZTEyMzFiYThlYWNYIQPeNBo_DX-qSXr52rqbwhGKWx9VNpaddKwORBP9-43JzmFk9g==";

    let token = Token::from_str(token_str).unwrap();
    token.to_string().strip_prefix("bitcrB").unwrap();
    if let Token::BitcrV4(token) = token {
        assert_eq!(token.value().unwrap(), cashu::Amount::from(973));
        assert_eq!(token.unit.to_string(), "crsat");
    }
}
