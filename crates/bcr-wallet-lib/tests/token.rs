use bcr_wallet_lib::wallet::*;
use cashu::nut02 as cdk02;
use std::str::FromStr;

#[test]
fn test_token_str_round_trip_1() {
    let token_str = "cashuBpGF0gaJhaUgArSaMTR9YJmFwgaNhYQFhc3hAOWE2ZGJiODQ3YmQyMzJiYTc2ZGIwZGYxOTcyMTZiMjlkM2I4Y2MxNDU1M2NkMjc4MjdmYzFjYzk0MmZlZGI0ZWFjWCEDhhhUP_trhpXfStS6vN6So0qWvc2X3O4NfM-Y1HISZ5JhZGlUaGFuayB5b3VhbXVodHRwOi8vbG9jYWxob3N0OjMzMzhhdWNzYXQ=";

    let token = Token::from_str(token_str).unwrap();
    assert!(matches!(token, Token::CashuV4(_)));
    let Token::CashuV4(inner) = token.clone() else {
        panic!("Expected CashuV4 token");
    };
    assert_eq!(inner.token.len(), 1);
    assert_eq!(inner.token[0].keyset_id.to_string(), "00ad268c4d1f5826");
    let _ = token.to_string().strip_prefix("cashuB").expect("prefix");
    assert_eq!(inner.mint_url.to_string(), "http://localhost:3338");
    //
    assert_eq!(
        inner.token[0].keyset_id,
        cdk02::ShortKeysetId::from_str("00ad268c4d1f5826").unwrap()
    );
    assert_eq!(inner.unit.clone(), cashu::CurrencyUnit::Sat);

    let encoded = &inner.to_string();

    let token_data = CashuTokenV4::from_str(encoded).unwrap();
    assert_eq!(token_data, inner);
}

#[test]
fn test_token_str_round_trip_2() {
    let token_str = "bitcrBpGFtdWh0dHA6Ly9sb2NhbGhvc3Q6MzMzOGF1ZWNyc2F0YXSBomFpSACtJoxNH1gmYXCBo2FhAWFzeEA5YTZkYmI4NDdiZDIzMmJhNzZkYjBkZjE5NzIxNmIyOWQzYjhjYzE0NTUzY2QyNzgyN2ZjMWNjOTQyZmVkYjRlYWNYIQOGGFQ_-2uGld9K1Lq83pKjSpa9zZfc7g18z5jUchJnkmFkaVRoYW5rIHlvdQ";

    let token = Token::from_str(token_str).unwrap();
    assert!(matches!(token, Token::BitcrV4(_)));
    let Token::BitcrV4(inner) = token.clone() else {
        panic!("Expected BitcrV4 token");
    };
    assert_eq!(inner.token.len(), 1);
    assert_eq!(inner.token[0].keyset_id.to_string(), "00ad268c4d1f5826");

    token.to_string().strip_prefix("bitcrB").unwrap();
    assert_eq!(inner.mint_url.to_string(), "http://localhost:3338");
    //
    assert_eq!(
        inner.token[0].keyset_id,
        cdk02::ShortKeysetId::from_str("00ad268c4d1f5826").unwrap()
    );
    assert_eq!(
        inner.unit.clone(),
        cashu::CurrencyUnit::Custom(String::from("CRSAT"))
    ); // this should be Custom("crsat")

    let encoded = &inner.to_string();

    let token_data = BitcrTokenV4::from_str(encoded).unwrap();
    assert_eq!(token_data, inner);
}
#[test]
fn incorrect_tokens() {
    let incorrect_prefix = "casshuAeyJ0b2tlbiI6W3sibWludCI6Imh0dHBzOi8vODMzMy5zcGFjZTozMzM4IiwicHJvb2ZzIjpbeyJhbW91bnQiOjIsImlkIjoiMDA5YTFmMjkzMjUzZTQxZSIsInNlY3JldCI6IjQwNzkxNWJjMjEyYmU2MWE3N2UzZTZkMmFlYjRjNzI3OTgwYmRhNTFjZDA2YTZhZmMyOWUyODYxNzY4YTc4MzciLCJDIjoiMDJiYzkwOTc5OTdkODFhZmIyY2M3MzQ2YjVlNDM0NWE5MzQ2YmQyYTUwNmViNzk1ODU5OGE3MmYwY2Y4NTE2M2VhIn0seyJhbW91bnQiOjgsImlkIjoiMDA5YTFmMjkzMjUzZTQxZSIsInNlY3JldCI6ImZlMTUxMDkzMTRlNjFkNzc1NmIwZjhlZTBmMjNhNjI0YWNhYTNmNGUwNDJmNjE0MzNjNzI4YzcwNTdiOTMxYmUiLCJDIjoiMDI5ZThlNTA1MGI4OTBhN2Q2YzA5NjhkYjE2YmMxZDVkNWZhMDQwZWExZGUyODRmNmVjNjlkNjEyOTlmNjcxMDU5In1dfV0sInVuaXQiOiJzYXQiLCJtZW1vIjoiVGhhbmsgeW91LiJ9";

    let incorrect_prefix_token = Token::from_str(incorrect_prefix);

    assert!(incorrect_prefix_token.is_err());

    let no_prefix = "eyJ0b2tlbiI6W3sibWludCI6Imh0dHBzOi8vODMzMy5zcGFjZTozMzM4IiwicHJvb2ZzIjpbeyJhbW91bnQiOjIsImlkIjoiMDA5YTFmMjkzMjUzZTQxZSIsInNlY3JldCI6IjQwNzkxNWJjMjEyYmU2MWE3N2UzZTZkMmFlYjRjNzI3OTgwYmRhNTFjZDA2YTZhZmMyOWUyODYxNzY4YTc4MzciLCJDIjoiMDJiYzkwOTc5OTdkODFhZmIyY2M3MzQ2YjVlNDM0NWE5MzQ2YmQyYTUwNmViNzk1ODU5OGE3MmYwY2Y4NTE2M2VhIn0seyJhbW91bnQiOjgsImlkIjoiMDA5YTFmMjkzMjUzZTQxZSIsInNlY3JldCI6ImZlMTUxMDkzMTRlNjFkNzc1NmIwZjhlZTBmMjNhNjI0YWNhYTNmNGUwNDJmNjE0MzNjNzI4YzcwNTdiOTMxYmUiLCJDIjoiMDI5ZThlNTA1MGI4OTBhN2Q2YzA5NjhkYjE2YmMxZDVkNWZhMDQwZWExZGUyODRmNmVjNjlkNjEyOTlmNjcxMDU5In1dfV0sInVuaXQiOiJzYXQiLCJtZW1vIjoiVGhhbmsgeW91LiJ9";

    let no_prefix_token = Token::from_str(no_prefix);

    assert!(no_prefix_token.is_err());

    let correct_token = "cashuBo2F0gqJhaUgA_9SLj17PgGFwgaNhYQFhc3hAYWNjMTI0MzVlN2I4NDg0YzNjZjE4NTAxNDkyMThhZjkwZjcxNmE1MmJmNGE1ZWQzNDdlNDhlY2MxM2Y3NzM4OGFjWCECRFODGd5IXVW-07KaZCvuWHk3WrnnpiDhHki6SCQh88-iYWlIAK0mjE0fWCZhcIKjYWECYXN4QDEzMjNkM2Q0NzA3YTU4YWQyZTIzYWRhNGU5ZjFmNDlmNWE1YjRhYzdiNzA4ZWIwZDYxZjczOGY0ODMwN2U4ZWVhY1ghAjRWqhENhLSsdHrr2Cw7AFrKUL9Ffr1XN6RBT6w659lNo2FhAWFzeEA1NmJjYmNiYjdjYzY0MDZiM2ZhNWQ1N2QyMTc0ZjRlZmY4YjQ0MDJiMTc2OTI2ZDNhNTdkM2MzZGNiYjU5ZDU3YWNYIQJzEpxXGeWZN5qXSmJjY8MzxWyvwObQGr5G1YCCgHicY2FtdWh0dHA6Ly9sb2NhbGhvc3Q6MzMzOGF1Y3NhdA==";

    let correct_token = Token::from_str(correct_token);

    assert!(correct_token.is_ok());
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
