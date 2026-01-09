# 0.7.2

* Add endpoints to refresh transactions and reclaim unspent funds
    * `wallet_refresh_tx(wallet_id, tx_id)` - refreshes a single transaction
    * `wallet_refresh_txs(wallet_id)` - refreshes all pending transactions of the given wallet
    * `wallet_reclaim_tx(wallet_id, tx_id)` - reclaims the funds from the given transaction
* Add `id` to Transaction Response
* Rename `CashedIn` to `Settled` (breaking DB Change)

# 0.7.1

* Remove `bcr-wallet-lib` in favor of `bcr-common::wallet` for `Token`
* Don't persist mnemonic anymore (breaking DB change)
* Improve locking performance

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
