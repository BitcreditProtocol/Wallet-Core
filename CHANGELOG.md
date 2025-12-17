# 0.7.0

* Remove WASM
* Replace rexie (IndexedDB) with redb for persistence
* Add CLI client
* Add Pay by Token
* Fixed Nostr payment
* Add jobs for migrate_rabid and redeeming
* Remove Settings DB and replace with AppStateConfig
* Add an endpoint `wallet_list_txs` that returns all transactions for a wallet, sorted by timestamp descending
* Use mint_url, mnemonic, network from config and fail if wallet doesn't match
* Remove `get_wallets_names` endpoint

# 0.1.0

* Initial version
