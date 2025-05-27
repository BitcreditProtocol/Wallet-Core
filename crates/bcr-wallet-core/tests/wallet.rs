use std::str::FromStr;

use bcr_wallet_core::db::{MemoryDatabase, WalletDatabase};
use bcr_wallet_core::wallet::{DebitWallet, Wallet, new_debit};
use cashu::MintUrl;
use wasm_bindgen_test::wasm_bindgen_test;

#[wasm_bindgen_test(async)]
async fn test_debit() {
    let db = MemoryDatabase::default();
    let url = MintUrl::from_str("http://example.com".into()).unwrap();
    let wallet = new_debit()
        .set_unit(cashu::CurrencyUnit::Sat)
        .set_mint_url(url)
        .set_database(db)
        .set_seed([0; 32])
        .build();
    let proofs = wallet.db.get_proofs().await;
    assert!(proofs.len() == 0)
}
