# Wallet Core

Bitcredit wallet core in Rust

## CLI

1. git clone git@github.com:BitcreditProtocol/Wallet-Core.git
2. install just (https://github.com/casey/just)
3. Use wallet:

Set configs in crates/bcr-wallet-cli/alice.toml

To reset just rm crates/bcr-wallet-cli/alice.db

```
just cli -w alice restore_wallet

just cli -w alice info

just cli -w alice receive 0 $token

just cli -w alice send_payment 0 $token

just cli -w alice request_payment 0 150 sat

just cli -w alice melt 0 1000 $btcaddress

just cli -w alice pay_by_token 0 100 sat

just cli -w alice mint 0 1200

just cli -w alice reclaim 0 $txid
```
