# 0.9.1

* Add `code` to `WalletError` for ffi
* Fix fee for minting

# 0.9.0

* Updated to newest bcr-common
* Add protest mint flow via `POST /v1/protest/mint` (Resolved/Rabid)
    * Breaking change: `store_mint`/`load_mint` now include `content` and `commitment`; existing pending mints won't deserialize
* Change default mint and relays
* Improve Payment Request Reliability & Performance
    * Remove `initial_delay` and `check_interval` parameters
    * We now use a long-running subscription and listen to it when receiving payments only, which is much more efficient
    * `check_received_payment` returns a `cancel_token` and takes a `result_callback`
        * This way, the caller can control, when to cancel a payment request asynchronously
* Expose `InsufficientFunds` error as `bad request`
* Remove the concept of a `credit` currency (fully backwards breaking)
    * Remove credit pocket
    * Remove unit from API
    * Remove the concept of redemption
* Add job and endpoint for `wallet_recover_pending_stale_proofs`, which recovers proofs which are stale after a failed operation
* Remove cdk MintConnector
* Add `dev_mode` field in config
* Add Endpoint `wallet_dev_mode_get_detailed_balance` that returns a listing of funds for each keyset with the expiry of the keyset
* Return `debit`, `credit` and `total` from balance
* Implement basic Fees and Coin Selection

# 0.8.2

* Check Rabid and Migrate Rabid now also work with ConfiscatedRabid state

# 0.8.1

* Update minting flow - breaking database change for storing premint secrets during minting
* Persist alpha_tx_id and beta_tx_id for melts as per https://github.com/BitcreditProtocol/Clowder/pull/207 (breaking DB change)
    * replace `btc_tx_id` with optional `melt_tx` struct on `Transaction` (breaking API change)

# 0.8.0

* Refactoring (breaking DB and API changes)
    * Split into multiple crates
    * Add DB Tests
    * Move some types and utils to core, or where they belong
    * Restructure purse/wallet/pocket and mint code
        * Mods for wallet, purse and mint
        * Move traits to their impls
        * Split up Wallet for structure
        * Move wallet specific functions directly to wallet, not going through purse
    * Add Purse Tests
    * Rework Errors structure
    * Clean up outer types a bit
* Move wallet-ffi to Wallet-Core repo
* Remove `clean_local_db` endpoint
* Improve FFI types

# 0.7.8

* Add `clowder_id`, `betas` and `mint_keysets` to `WalletConfig` (breaking DB change)
* Improve Offline functionality and performance
    * Clowder ID is fetched at wallet initialization and cached in DB
    * Betas are fetched at wallet initialization and cached in DB
    * Mint keysets are fetched at wallet initialization and cached in DB / refetched on-demand
* We always initialize Credit Sat Pocket now with `crsat`, even if the Mint doesn't have a credit keyset
* Add endpoints `wallet_mint_is_rabid` and `wallet_mint_is_offline` to check whether a wallet mint is rabid, or offline
* Removed `purse_migrate_rabid` from daily jobs - it now has to be called directly and returns a map of migrated wallets with their new mints
* Removed the check for `default_mint_url` to have to match the wallet - it's just logged now
* Implement a hacky demo-version of `offline_pay_by_token`, where the wallet can create a token even if the alpha mint is offline

# 0.7.7

* Fix Offline intermint exchange
* Fix DLEQs being set during restoration

# 0.7.6

* Fix intermint exchange

# 0.7.5

* Adapt to new Clowder URLs

# 0.7.4

* Add `is_valid_token` utility method to expose our token checking
* Fix Nostr event loop to not fail on invalid events
* Add Threshold for minting and melting

# 0.7.3

* Improve API for `wallet_check_received_payment` to give the caller more control
    * It now takes `initial_delay_sec`, `max_wait_sec` and `check_interval_sec` to control when to start polling, how often to poll and how long
* Fixed timestamp for receiving a nut-18 payment via Nostr, which used the randomized Nostr timestamp

# 0.7.2

* Add endpoints to refresh transactions and reclaim unspent funds
    * `wallet_refresh_tx(wallet_id, tx_id)` - refreshes a single transaction
    * `wallet_refresh_txs(wallet_id)` - refreshes all pending transactions of the given wallet
    * `wallet_reclaim_tx(wallet_id, tx_id)` - reclaims the funds from the given transaction
* Add `id` to Transaction Response
* Rename `CashedIn` to `Settled` (breaking DB Change)
* Removed `wallet_check_pending_melts` - since onchain melts execute immediately
* Add mint and melt
    * Add `wallet_prepare_melt` - prepares a melt, returns a payment summary
    * Add `wallet_melt` - executes the melt, returning a transaction id
    * Add optional `btc_tx_id` to `Transaction` - the Bitcoin transaction ID (e.g. from a melt operation)
    * Add optional `quote_id` to `Transaction` - the Mint quote ID (e.g. from a mint operation)
    * Add `wallet_mint` -  creates a mint request for the given amount, returns a mint summary, with the amount and BTC address to pay to
    * Add `wallet_check_pending_mints` - checks the open mint requests and attempts to mint them, if they were paid (Also called during the regular job runs)

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
