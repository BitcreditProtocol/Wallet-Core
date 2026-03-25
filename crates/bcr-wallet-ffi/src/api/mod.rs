use bcr_wallet_core::types::{
    PaymentResultCallback, get_btc_alpha_tx_id, get_btc_beta_tx_id, get_payment_type,
    get_transaction_status,
};
use nostr_sdk::RelayUrl;
use once_cell::sync::Lazy;
use std::{panic, path::PathBuf, str::FromStr, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

#[cfg(target_os = "android")]
use android_logger::FilterBuilder;
use bcr_common::{
    cashu::{self, MintUrl},
    cdk,
};
use bcr_wallet_api::{
    AppState,
    config::{AppStateConfig, SameMintSafeMode},
    error::Error as BcrWalletError,
};
use flutter_rust_bridge::{DartFnFuture, JoinHandle, frb};
use log::{error, info};
use tokio::sync::Mutex;
use uuid::Uuid;

pub const VERSION: &str = env!("CRATE_VERSION");

static WALLET_RUNTIME: Lazy<Mutex<WalletRuntime>> = Lazy::new(|| Mutex::new(WalletRuntime::new()));

// This needs to happen
#[flutter_rust_bridge::frb(init)]
pub fn init_app() {
    flutter_rust_bridge::setup_default_user_utils();
}

struct WalletRuntime {
    app_state: Option<Arc<AppState>>,
    jobs_cancel: Option<CancellationToken>,
    jobs_handle: Option<JoinHandle<()>>,
    logging_initialized: bool,
    panic_hook_initialized: bool,
}

impl WalletRuntime {
    fn new() -> Self {
        Self {
            app_state: None,
            jobs_cancel: None,
            jobs_handle: None,
            logging_initialized: false,
            panic_hook_initialized: false,
        }
    }
}

async fn reset_runtime(rt: &mut WalletRuntime) {
    info!("Resetting Rust Wallet FFI Runtime");
    if let Some(ref token) = rt.jobs_cancel {
        token.cancel();
    }

    if let Some(ref handle) = rt.jobs_handle {
        handle.abort();
    }

    rt.app_state = None;
    info!("Rust Wallet FFI Runtime Reset Done");
}

// ------------------------------------------------------------- Initialization

#[derive(Debug, Clone)]
pub struct WalletFfiConfig {
    // Path to the DB file
    pub db_folder_path: String,
    // The log level to be used
    pub log_level: String,
    // The amount of seconds between each job run
    pub job_interval_secs: u64,
    // The amount of seconds to initially wait before running jobs
    pub job_initial_delay_secs: u64,
    // The default mint URL for the wallet and restoration
    pub default_mint_url: String,
    // The bitcoin_network to use. Options are: bitcoin, testnet, testnet4, signet, regtest
    pub bitcoin_network: String,
    // The mnemonic to use
    pub mnemonic: String,
    // The nostr relays to use
    pub nostr_relays: Vec<String>,
    // Whether to use same-mint-safe-mode
    pub use_same_mint_safe_mode: bool,
}

#[frb]
pub async fn init_wallet_ffi(conf: WalletFfiConfig) {
    info!("Initializing Rust Wallet FFI");
    let parsed_path = PathBuf::from_str(&conf.db_folder_path.clone())
        .expect("Not a valid file path for the database");
    let log_level = conf.log_level.clone();
    let job_interval_secs = conf.job_interval_secs;
    let job_initial_delay_secs = conf.job_initial_delay_secs;
    let parsed_url = MintUrl::from_str(&conf.default_mint_url).expect("Not a valid mint URL");
    let parsed_mnemonic =
        bip39::Mnemonic::from_str(&conf.mnemonic).expect("Not a valid bip39 mnemonic");
    let parsed_nostr_relays: Vec<RelayUrl> = conf
        .nostr_relays
        .into_iter()
        .map(|u| RelayUrl::from_str(&u).expect("Not a valid nostr relay url"))
        .collect();
    let parsed_network = bitcoin::Network::from_str(&conf.bitcoin_network).expect(
        "Not a valid bitcoin network - use one of bitcoin, testnet, testnet4, signet, regtest",
    );
    let same_mint_safe_mode = if conf.use_same_mint_safe_mode {
        SameMintSafeMode::Enabled {
            expiration: chrono::TimeDelta::minutes(15),
        }
    } else {
        SameMintSafeMode::Disabled
    };

    let mut rt = WALLET_RUNTIME.lock().await;

    // reset on initialization
    reset_runtime(&mut rt).await;

    // only initialize logging once
    if !rt.logging_initialized {
        init_logging(&log_level);
        rt.logging_initialized = true;
    }

    // only initialize panic hook once
    if !rt.panic_hook_initialized {
        init_panic_hook();
        rt.panic_hook_initialized = true;
    }

    let app_state_cfg = AppStateConfig {
        db_path: parsed_path,
        network: parsed_network,
        nostr_relays: parsed_nostr_relays,
        mnemonic: parsed_mnemonic,
        same_mint_safe_mode,
        default_mint_url: parsed_url,
    };

    let app_state = AppState::initialize(app_state_cfg)
        .await
        .expect("Could not initialize Wallet Core FFI App State");

    rt.app_state = Some(Arc::new(app_state));

    let cancel = CancellationToken::new();
    let handle = start_jobs(job_interval_secs, job_initial_delay_secs, cancel.clone());

    rt.jobs_cancel = Some(cancel);
    rt.jobs_handle = Some(handle);

    info!("Initialized Rust Wallet FFI");
}

async fn get_app_state() -> Arc<AppState> {
    let rt = WALLET_RUNTIME.lock().await;
    rt.app_state.clone().expect("Wallet API not initialized")
}

fn start_jobs(
    job_interval_secs: u64,
    job_initial_delay_secs: u64,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    let interval = if job_interval_secs < 1 {
        1
    } else {
        job_interval_secs
    };

    flutter_rust_bridge::spawn(async move {
        // initial delay
        info!(
            "Waiting {job_initial_delay_secs} seconds to run jobs for the first time. Afterwards, jobs will run every {job_interval_secs} seconds."
        );
        tokio::time::sleep(Duration::from_secs(job_initial_delay_secs)).await;

        let mut ticker = tokio::time::interval(Duration::from_secs(interval));

        // run job loop
        loop {
            tokio::select! {
                _ = ticker.tick() => {

            let app_state = get_app_state().await;

            info!("Running jobs");
            if let Err(e) = app_state.run_jobs().await {
                error!("Error running jobs: {e}");
            } else {
                info!("Jobs ran successfully");
            }
                },
                _ = cancel.cancelled() => break,
            }
        }
    })
}

/// initialize logging
fn init_logging(log_level: &str) {
    info!("Initializing Rust logging");
    let level = log::LevelFilter::from_str(log_level).expect("invalid log level");
    #[cfg(target_os = "android")]
    android_logger::init_once(
        android_logger::Config::default()
            .with_tag("WalletFfi")
            .with_max_level(level),
    );

    #[cfg(not(target_os = "android"))]
    env_logger::builder().filter_level(level).init();

    info!("Rust logging initialized");
}

fn init_panic_hook() {
    info!("Initializing Rust panic hook");
    panic::set_hook(Box::new(|info| {
        error!("Rust panic: {info}");
    }));
    info!("Rust panic hook initialized");
}

// ------------------------------------------------------------- API
#[frb]
pub async fn wallet_add() -> Result<AddWalletResponse, WalletError> {
    let name = Uuid::new_v4().to_string();
    let app_state = get_app_state().await;
    let wallet_id = match app_state.purse_add_wallet(name).await {
        Ok(id) => id,
        Err(e) => {
            error!("ERROR ADD WALLET: {e}");
            return Err(e.into());
        }
    };
    Ok(AddWalletResponse { wallet_id })
}

#[frb]
pub async fn wallet_restore() -> Result<RestoreWalletResponse, WalletError> {
    let name = Uuid::new_v4().to_string();
    let app_state = get_app_state().await;
    let wallet_id = app_state.purse_restore_wallet(name).await?;
    Ok(RestoreWalletResponse { wallet_id })
}

#[frb]
pub async fn wallet_delete(req: WalletRequest) -> Result<(), WalletError> {
    let app_state = get_app_state().await;
    app_state.purse_delete_wallet(req.wallet_id).await?;
    Ok(())
}

#[frb]
pub async fn wallet_get_name(req: WalletRequest) -> Result<WalletNameResponse, WalletError> {
    let app_state = get_app_state().await;
    let name = app_state.wallet_name(req.wallet_id).await?;
    Ok(WalletNameResponse { name })
}

#[frb]
pub async fn wallet_get_mint_url(req: WalletRequest) -> Result<WalletMintUrlResponse, WalletError> {
    let app_state = get_app_state().await;
    let mint_url = app_state.wallet_mint_url(req.wallet_id).await?;
    Ok(WalletMintUrlResponse { mint_url })
}

#[frb]
pub async fn wallet_get_currency_unit(
    req: WalletRequest,
) -> Result<WalletCurrencyUnitResponse, WalletError> {
    let app_state = get_app_state().await;
    let currency_unit = app_state.wallet_currency_unit(req.wallet_id).await?;
    Ok(WalletCurrencyUnitResponse {
        debit: currency_unit.debit,
        credit: currency_unit.credit,
    })
}

#[frb]
pub async fn wallet_get_balance(req: WalletRequest) -> Result<WalletBalanceResponse, WalletError> {
    let app_state = get_app_state().await;
    let balance = app_state.wallet_balance(req.wallet_id).await?;
    Ok(WalletBalanceResponse {
        debit: u64::from(balance.debit),
        credit: u64::from(balance.credit),
    })
}

#[frb]
pub async fn wallet_receive(
    req: WalletReceiveRequest,
) -> Result<WalletTransactionIdResponse, WalletError> {
    let app_state = get_app_state().await;
    let tx_id = app_state
        .wallet_receive_token(req.wallet_id, req.token)
        .await?;
    Ok(WalletTransactionIdResponse {
        tx_id: tx_id.to_string(),
    })
}

#[frb]
pub async fn wallet_list_redemptions(
    req: WalletListRedemptionsRequest,
) -> Result<WalletListRedemptionsResponse, WalletError> {
    let app_state = get_app_state().await;
    let redemptions = app_state
        .wallet_list_redemptions(
            req.wallet_id,
            std::time::Duration::from_secs(req.payment_window_seconds),
        )
        .await?;
    Ok(WalletListRedemptionsResponse {
        redemptions: redemptions
            .into_iter()
            .map(|r| RedemptionSummary {
                tstamp: r.tstamp,
                amount: u64::from(r.amount),
            })
            .collect(),
    })
}

#[frb]
pub async fn wallet_load_transaction(
    req: WalletTransactionRequest,
) -> Result<WalletTransactionResponse, WalletError> {
    let app_state = get_app_state().await;
    let transaction = app_state.wallet_load_tx(req.wallet_id, &req.tx_id).await?;
    Ok(WalletTransactionResponse {
        transaction: transaction.into(),
    })
}

#[frb]
pub async fn wallet_refresh_transaction(
    req: WalletRefreshTransactionRequest,
) -> Result<WalletRefreshTransactionResponse, WalletError> {
    let app_state = get_app_state().await;
    let updated = app_state
        .wallet_refresh_tx(req.wallet_id, &req.tx_id)
        .await?;
    Ok(WalletRefreshTransactionResponse { updated })
}

#[frb]
pub async fn wallet_refresh_transactions(
    req: WalletRequest,
) -> Result<WalletRefreshTransactionsResponse, WalletError> {
    let app_state = get_app_state().await;
    let updated = app_state.wallet_refresh_txs(req.wallet_id).await?;
    Ok(WalletRefreshTransactionsResponse { updated })
}

#[frb]
pub async fn wallet_reclaim_transaction(
    req: WalletReclaimTransactionRequest,
) -> Result<WalletReclaimTransactionResponse, WalletError> {
    let app_state = get_app_state().await;
    let amount = app_state
        .wallet_reclaim_tx(req.wallet_id, &req.tx_id)
        .await?;
    Ok(WalletReclaimTransactionResponse {
        amount: u64::from(amount),
    })
}

#[frb]
pub async fn wallet_prepare_melt(
    req: WalletPrepareMeltRequest,
) -> Result<WalletPreparePaymentResponse, WalletError> {
    let app_state = get_app_state().await;
    let payment_summary = app_state
        .wallet_prepare_melt(req.wallet_id, req.amount, req.address, req.description)
        .await?;
    Ok(WalletPreparePaymentResponse {
        payment_summary: PaymentSummary {
            request_id: payment_summary.request_id.to_string(),
            unit: payment_summary.unit.to_string(),
            amount: u64::from(payment_summary.amount),
            fees: u64::from(payment_summary.fees),
            reserved_fees: u64::from(payment_summary.reserved_fees),
            expiry: payment_summary.expiry,
            ptype: PaymentType::from(bcr_wallet_core::types::PaymentType::from(
                payment_summary.ptype,
            )),
        },
    })
}

#[frb]
pub async fn wallet_melt(
    req: WalletPayRequest,
) -> Result<WalletTransactionIdResponse, WalletError> {
    let app_state = get_app_state().await;
    let tx_id = app_state.wallet_melt(req.wallet_id, req.rid).await?;
    Ok(WalletTransactionIdResponse {
        tx_id: tx_id.to_string(),
    })
}

#[frb]
pub async fn wallet_mint(req: WalletMintRequest) -> Result<WalletMintSummaryResponse, WalletError> {
    let app_state = get_app_state().await;
    let mint_summary = app_state.wallet_mint(req.wallet_id, req.amount).await?;
    Ok(WalletMintSummaryResponse {
        quote_id: mint_summary.quote_id.to_string(),
        amount: mint_summary.amount.to_sat(),
        address: mint_summary.address.assume_checked().to_string(),
        expiry: mint_summary.expiry,
    })
}

#[frb]
pub async fn wallet_prepare_payment(
    req: WalletPreparePaymentRequest,
) -> Result<WalletPreparePaymentResponse, WalletError> {
    let app_state = get_app_state().await;
    let payment_summary = app_state
        .wallet_prepare_payment(req.wallet_id, req.input)
        .await?;
    Ok(WalletPreparePaymentResponse {
        payment_summary: PaymentSummary {
            request_id: payment_summary.request_id.to_string(),
            unit: payment_summary.unit.to_string(),
            amount: u64::from(payment_summary.amount),
            fees: u64::from(payment_summary.fees),
            reserved_fees: u64::from(payment_summary.reserved_fees),
            expiry: payment_summary.expiry,
            ptype: PaymentType::from(bcr_wallet_core::types::PaymentType::from(
                payment_summary.ptype,
            )),
        },
    })
}

#[frb]
pub async fn wallet_pay(req: WalletPayRequest) -> Result<WalletTransactionIdResponse, WalletError> {
    let app_state = get_app_state().await;
    let tx_id = app_state.wallet_pay(req.wallet_id, req.rid).await?;
    Ok(WalletTransactionIdResponse {
        tx_id: tx_id.to_string(),
    })
}

#[frb]
pub async fn wallet_prepare_pay_by_token(
    req: WalletPreparePaymentByTokenRequest,
) -> Result<WalletPreparePaymentResponse, WalletError> {
    let app_state = get_app_state().await;
    let payment_summary = app_state
        .wallet_prepare_pay_by_token(req.wallet_id, req.amount, req.unit, req.description)
        .await?;
    Ok(WalletPreparePaymentResponse {
        payment_summary: PaymentSummary {
            request_id: payment_summary.request_id.to_string(),
            unit: payment_summary.unit.to_string(),
            amount: u64::from(payment_summary.amount),
            fees: u64::from(payment_summary.fees),
            reserved_fees: u64::from(payment_summary.reserved_fees),
            expiry: payment_summary.expiry,
            ptype: PaymentType::from(bcr_wallet_core::types::PaymentType::from(
                payment_summary.ptype,
            )),
        },
    })
}

#[frb]
pub async fn wallet_pay_by_token(
    req: WalletPaymentByTokenRequest,
) -> Result<WalletPaymentByTokenResponse, WalletError> {
    let app_state = get_app_state().await;
    let res = app_state
        .wallet_pay_by_token(req.wallet_id, req.rid)
        .await?;
    Ok(WalletPaymentByTokenResponse {
        tx_id: res.tx_id.to_string(),
        token: res.token.to_string(),
    })
}

#[frb]
pub async fn wallet_prepare_payment_request(
    req: WalletPreparePaymentReqRequest,
) -> Result<WalletPreparePaymentReqResponse, WalletError> {
    let app_state = get_app_state().await;
    let payment_request = app_state
        .wallet_prepare_payment_request(req.wallet_id, req.amount, req.unit, req.description)
        .await?;
    Ok(WalletPreparePaymentReqResponse {
        payment_request: PaymentRequest {
            request: payment_request.request,
            p_id: payment_request.p_id,
        },
    })
}

#[frb]
pub async fn wallet_check_received_payment(
    req: WalletCheckReceivedPaymentRequest,
    result_callback: impl Fn(WalletMaybeTransactionIdResponse) -> DartFnFuture<()>
    + Send
    + Sync
    + 'static,
) -> Result<WalletPaymentCheckHandle, WalletError> {
    let app_state = get_app_state().await;

    let dart_callback = Arc::new(result_callback);
    let callback: PaymentResultCallback = Arc::new(move |tx_id| {
        let dart_callback = dart_callback.clone();
        flutter_rust_bridge::spawn(async move {
            let _ = dart_callback(WalletMaybeTransactionIdResponse {
                tx_id: tx_id.map(|t| t.to_string()),
            })
            .await;
        });
    });

    let cancel_token = CancellationToken::new();
    let handle = WalletPaymentCheckHandle {
        cancel_token: cancel_token.clone(),
    };
    flutter_rust_bridge::spawn(async move {
        if let Err(e) = app_state
            .wallet_check_received_payment(
                req.wallet_id,
                req.max_wait_sec,
                req.p_id,
                cancel_token,
                callback.clone(),
            )
            .await
        {
            error!("Error during wallet_check_received_payment: {e}");
            callback(None);
        }
    });
    Ok(handle)
}

#[frb]
pub async fn wallet_check_pending_mints(
    req: WalletRequest,
) -> Result<WalletCheckPendingMintsResponse, WalletError> {
    let app_state = get_app_state().await;
    let tx_ids = app_state.wallet_check_pending_mints(req.wallet_id).await?;
    Ok(WalletCheckPendingMintsResponse {
        tx_ids: tx_ids.into_iter().map(|tx_id| tx_id.to_string()).collect(),
    })
}

#[frb]
pub async fn wallet_get_transaction_ids(
    req: WalletRequest,
) -> Result<WalletTransactionIdsResponse, WalletError> {
    let app_state = get_app_state().await;
    let ids = app_state.wallet_list_tx_ids(req.wallet_id).await?;
    Ok(WalletTransactionIdsResponse {
        tx_ids: ids.into_iter().map(|t| t.to_string()).collect(),
    })
}

#[frb]
pub async fn wallet_get_transactions(
    req: WalletRequest,
) -> Result<WalletTransactionsResponse, WalletError> {
    let app_state = get_app_state().await;
    let ids = app_state.wallet_list_txs(req.wallet_id).await?;
    Ok(WalletTransactionsResponse {
        txs: ids.into_iter().map(|t| t.into()).collect(),
    })
}

#[frb]
pub async fn wallet_get_ids() -> Result<WalletsIdsResponse, WalletError> {
    let app_state = get_app_state().await;
    let ids = app_state.purse_wallets_ids().await?;
    Ok(WalletsIdsResponse { ids })
}

#[frb]
pub async fn generate_random_mnemonic(
    req: MnemonicRequest,
) -> Result<MnemonicResponse, WalletError> {
    let mnemonic = bcr_wallet_api::generate_random_mnemonic(req.length);
    Ok(MnemonicResponse { mnemonic })
}

#[frb]
pub async fn is_valid_token(req: IsValidTokenRequest) -> Result<IsValidTokenResponse, WalletError> {
    let token = bcr_wallet_api::is_valid_token(&req.token)?;
    Ok(IsValidTokenResponse {
        amount: u64::from(token.value().unwrap_or(cashu::Amount::ZERO)),
        memo: token.memo().to_owned(),
        mint_url: token.mint_url().to_string(),
        unit: token.unit().map(|cu| cu.to_string()),
    })
}

#[frb]
pub async fn wallet_redeem_credit(
    req: WalletRequest,
) -> Result<WalletRedeemCreditResponse, WalletError> {
    let app_state = get_app_state().await;
    let amount = app_state.wallet_redeem_credit(req.wallet_id).await?;
    Ok(WalletRedeemCreditResponse {
        amount: u64::from(amount),
    })
}

#[frb]
pub async fn wallet_get_status() -> Result<StatusResponse, WalletError> {
    Ok(StatusResponse {
        app_version: VERSION.to_owned(),
    })
}

#[frb]
pub async fn wallet_migrate_rabid() -> Result<MigrateRabidResponse, WalletError> {
    let app_state = get_app_state().await;
    let migrated = app_state.purse_migrate_rabid().await?;
    let migrated_to_mint = if migrated.is_empty() {
        None
    } else {
        migrated.iter().next().map(|(_, mint)| mint.to_string())
    };
    Ok(MigrateRabidResponse { migrated_to_mint })
}

#[frb]
pub async fn wallet_mint_is_offline(
    req: WalletRequest,
) -> Result<MintIsOfflineResponse, WalletError> {
    let app_state = get_app_state().await;
    let is_offline = app_state.wallet_mint_is_offline(req.wallet_id).await?;
    Ok(MintIsOfflineResponse {
        offline: is_offline,
    })
}

#[frb]
pub async fn wallet_mint_is_rabid(req: WalletRequest) -> Result<MintIsRabidResponse, WalletError> {
    let app_state = get_app_state().await;
    let is_rabid = app_state.wallet_mint_is_rabid(req.wallet_id).await?;
    Ok(MintIsRabidResponse { rabid: is_rabid })
}

// -------------------------------------------------------------- Data types
#[derive(Debug, Clone)]
pub struct AddWalletResponse {
    pub wallet_id: usize,
}

#[derive(Debug, Clone)]
pub struct RestoreWalletResponse {
    pub wallet_id: usize,
}

#[derive(Debug, Clone)]
pub struct WalletRequest {
    pub wallet_id: usize,
}

#[derive(Debug, Clone)]
pub struct WalletTransactionRequest {
    pub wallet_id: usize,
    pub tx_id: String,
}

#[derive(Debug, Clone)]
pub struct WalletNameResponse {
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct WalletMintUrlResponse {
    pub mint_url: String,
}

#[derive(Debug, Clone)]
pub struct WalletCurrencyUnitResponse {
    pub credit: String,
    pub debit: String,
}

#[derive(Debug, Clone)]
pub struct WalletBalanceResponse {
    pub debit: u64,
    pub credit: u64,
}

#[derive(Debug, Clone)]
pub struct WalletReceiveRequest {
    pub wallet_id: usize,
    pub token: String,
}

#[derive(Debug, Clone)]
pub struct WalletRedeemCreditResponse {
    pub amount: u64,
}

#[derive(Debug, Clone)]
pub struct WalletListRedemptionsRequest {
    pub wallet_id: usize,
    pub payment_window_seconds: u64,
}

#[derive(Debug, Clone)]
pub struct WalletListRedemptionsResponse {
    pub redemptions: Vec<RedemptionSummary>,
}

#[derive(Debug, Clone)]
pub struct RedemptionSummary {
    pub tstamp: u64,
    pub amount: u64,
}

#[derive(Debug, Clone)]
pub struct WalletTransactionIdResponse {
    pub tx_id: String,
}

#[derive(Debug, Clone)]
pub struct WalletTransactionIdsResponse {
    pub tx_ids: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct WalletTransactionsResponse {
    pub txs: Vec<Transaction>,
}

#[derive(Debug, Clone)]
pub struct WalletCheckReceivedPaymentRequest {
    pub wallet_id: usize,
    pub max_wait_sec: u64,
    pub p_id: String,
}

#[derive(Debug, Clone)]
pub struct WalletMaybeTransactionIdResponse {
    pub tx_id: Option<String>,
}

#[derive(Clone)]
pub struct WalletPaymentCheckHandle {
    cancel_token: CancellationToken,
}

#[frb]
impl WalletPaymentCheckHandle {
    pub fn cancel(&self) {
        self.cancel_token.cancel();
    }
}

#[derive(Clone, Copy, Default, Debug)]
pub enum TransactionDirection {
    #[default]
    Incoming,
    Outgoing,
}
impl std::convert::From<cdk::wallet::types::TransactionDirection> for TransactionDirection {
    fn from(dir: cdk::wallet::types::TransactionDirection) -> Self {
        match dir {
            cdk::wallet::types::TransactionDirection::Incoming => TransactionDirection::Incoming,
            cdk::wallet::types::TransactionDirection::Outgoing => TransactionDirection::Outgoing,
        }
    }
}

#[derive(Clone, Copy, Default, Debug)]
pub enum PaymentType {
    #[default]
    NotApplicable,
    Token,
    Cdk18,
    OnChain,
}

impl std::convert::From<bcr_wallet_core::types::PaymentType> for PaymentType {
    fn from(ptype: bcr_wallet_core::types::PaymentType) -> Self {
        match ptype {
            bcr_wallet_core::types::PaymentType::NotApplicable => PaymentType::NotApplicable,
            bcr_wallet_core::types::PaymentType::Token => PaymentType::Token,
            bcr_wallet_core::types::PaymentType::Cdk18 => PaymentType::Cdk18,
            bcr_wallet_core::types::PaymentType::OnChain => PaymentType::OnChain,
        }
    }
}

#[derive(Clone, Copy, Default, Debug)]
pub enum TransactionStatus {
    #[default]
    NotApplicable,
    Pending,
    Settled,
    Canceled,
}

impl std::convert::From<bcr_wallet_core::types::TransactionStatus> for TransactionStatus {
    fn from(status: bcr_wallet_core::types::TransactionStatus) -> Self {
        match status {
            bcr_wallet_core::types::TransactionStatus::NotApplicable => {
                TransactionStatus::NotApplicable
            }
            bcr_wallet_core::types::TransactionStatus::Pending => TransactionStatus::Pending,
            bcr_wallet_core::types::TransactionStatus::Settled => TransactionStatus::Settled,
            bcr_wallet_core::types::TransactionStatus::Canceled => TransactionStatus::Canceled,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Transaction {
    pub id: String,
    pub amount: u64,
    pub fees: u64,
    pub unit: String,
    pub tstamp: u64,
    pub direction: TransactionDirection,
    pub memo: Option<String>,
    pub ptype: PaymentType,
    pub status: TransactionStatus,
    pub melt_tx: MeltTx,
    pub quote_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct MeltTx {
    pub alpha_tx_id: Option<String>,
    pub beta_tx_id: Option<String>,
}

impl std::convert::From<cdk::wallet::types::Transaction> for Transaction {
    fn from(tx: cdk::wallet::types::Transaction) -> Self {
        let status = get_transaction_status(&tx.metadata);
        let ptype = get_payment_type(&tx.metadata);
        let alpha_btc_tx_id = get_btc_alpha_tx_id(&tx.metadata);
        let beta_btc_tx_id = get_btc_beta_tx_id(&tx.metadata);
        Self {
            id: tx.id().to_string(),
            amount: u64::from(tx.amount),
            fees: u64::from(tx.fee),
            unit: tx.unit.to_string(),
            direction: TransactionDirection::from(tx.direction),
            tstamp: tx.timestamp,
            memo: tx.memo,
            ptype: ptype.into(),
            status: status.into(),
            melt_tx: MeltTx {
                alpha_tx_id: alpha_btc_tx_id.map(|txid| txid.to_string()),
                beta_tx_id: beta_btc_tx_id.map(|txid| txid.to_string()),
            },
            quote_id: tx.quote_id,
        }
    }
}

#[derive(Debug, Clone)]
pub struct WalletTransactionResponse {
    pub transaction: Transaction,
}

#[derive(Debug, Clone)]
pub struct WalletRefreshTransactionRequest {
    pub wallet_id: usize,
    pub tx_id: String,
}

#[derive(Debug, Clone)]
pub struct WalletReclaimTransactionRequest {
    pub wallet_id: usize,
    pub tx_id: String,
}

#[derive(Debug, Clone)]
pub struct WalletReclaimTransactionResponse {
    pub amount: u64,
}

#[derive(Debug, Clone)]
pub struct WalletRefreshTransactionResponse {
    pub updated: bool,
}

#[derive(Debug, Clone)]
pub struct WalletRefreshTransactionsResponse {
    pub updated: usize,
}

#[derive(Debug, Clone)]
pub struct WalletPrepareMeltRequest {
    pub wallet_id: usize,
    pub amount: u64,
    pub address: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone)]
pub struct WalletMintRequest {
    pub wallet_id: usize,
    pub amount: u64,
}

#[derive(Debug, Clone)]
pub struct WalletMintSummaryResponse {
    pub quote_id: String,
    pub amount: u64,
    pub address: String,
    pub expiry: u64,
}

#[derive(Debug, Clone)]
pub struct WalletPreparePaymentRequest {
    pub wallet_id: usize,
    pub input: String,
}

#[derive(Debug, Clone)]
pub struct WalletPreparePaymentResponse {
    pub payment_summary: PaymentSummary,
}

#[derive(Debug, Clone)]
pub struct WalletPreparePaymentReqRequest {
    pub wallet_id: usize,
    pub amount: u64,
    pub unit: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone)]
pub struct WalletPreparePaymentReqResponse {
    pub payment_request: PaymentRequest,
}

#[derive(Debug, Clone)]
pub struct WalletCheckPendingMintsResponse {
    pub tx_ids: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct PaymentSummary {
    pub request_id: String,
    pub unit: String,
    pub amount: u64,
    pub fees: u64,
    pub reserved_fees: u64,
    pub expiry: u64,
    pub ptype: PaymentType,
}

#[derive(Debug, Clone)]
pub struct CurrencyUnit {
    pub credit: String,
    pub debit: String,
}

#[derive(Debug, Clone)]
pub struct WalletCleanLocalDbResponse {
    pub cleaned_proofs: u32,
}

#[derive(Debug, Clone)]
pub struct WalletsNamesResponse {
    pub names: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct WalletsIdsResponse {
    pub ids: Vec<usize>,
}

#[derive(Debug, Clone)]
pub struct MnemonicRequest {
    pub length: u32,
}

#[derive(Debug, Clone)]
pub struct IsValidTokenRequest {
    pub token: String,
}

#[derive(Debug, Clone)]
pub struct IsValidTokenResponse {
    pub amount: u64,
    pub memo: Option<String>,
    pub mint_url: String,
    pub unit: Option<String>,
}

#[derive(Debug, Clone)]
pub struct MnemonicResponse {
    pub mnemonic: String,
}

#[derive(Debug, Clone)]
pub struct StatusResponse {
    pub app_version: String,
}

#[derive(Debug, Clone)]
pub struct MintIsOfflineResponse {
    pub offline: bool,
}

#[derive(Debug, Clone)]
pub struct MintIsRabidResponse {
    pub rabid: bool,
}

#[derive(Debug, Clone)]
pub struct MigrateRabidResponse {
    pub migrated_to_mint: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PaymentRequest {
    pub request: String,
    pub p_id: String,
}

#[derive(Debug, Clone)]
pub struct WalletPayRequest {
    pub wallet_id: usize,
    pub rid: String,
}

#[derive(Debug, Clone)]
pub struct WalletPreparePaymentByTokenRequest {
    pub wallet_id: usize,
    pub amount: u64,
    pub unit: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone)]
pub struct WalletPaymentByTokenRequest {
    pub wallet_id: usize,
    pub rid: String,
}

#[derive(Debug, Clone)]
pub struct WalletPaymentByTokenResponse {
    pub tx_id: String,
    pub token: String,
}

// -------------------------------------------------------------- Errors
#[derive(Debug, Clone)]
pub struct WalletError {
    pub kind: WalletErrorKind,
    pub msg: String,
}

impl WalletError {
    pub fn bad_request(msg: String) -> Self {
        WalletError {
            kind: WalletErrorKind::BadRequest,
            msg,
        }
    }

    pub fn internal() -> Self {
        WalletError {
            kind: WalletErrorKind::Internal,
            msg: String::default(),
        }
    }

    pub fn not_found(msg: String) -> Self {
        WalletError {
            kind: WalletErrorKind::NotFound,
            msg: format!("Not found: {msg}"),
        }
    }

    pub fn network(msg: String) -> Self {
        WalletError {
            kind: WalletErrorKind::Network,
            msg: format!("Network: {msg}"),
        }
    }
}

#[derive(Debug, Clone)]
pub enum WalletErrorKind {
    BadRequest,
    NotFound,
    Network,
    Internal,
    Initialization,
    Unsupported,
}

impl From<BcrWalletError> for WalletError {
    fn from(value: BcrWalletError) -> Self {
        error!("Error: {value}");
        match value {
            BcrWalletError::BorshSignature(_) => WalletError::network(value.to_string()),
            BcrWalletError::Borsh(_error) => WalletError::internal(),
            BcrWalletError::CashuMintUrl(_) => WalletError::bad_request(value.to_string()),
            BcrWalletError::Cdk(_error) => WalletError::internal(),
            BcrWalletError::Bip39(_error) => WalletError::internal(),
            BcrWalletError::Cdk00(_error) => WalletError::internal(),
            BcrWalletError::Cdk01(_error) => WalletError::internal(),
            BcrWalletError::Cdk13(_error) => WalletError::internal(),
            BcrWalletError::Cdk11(_error) => WalletError::internal(),
            BcrWalletError::Cdk10(_error) => WalletError::internal(),
            BcrWalletError::CdkAmount(_error) => WalletError::internal(),
            BcrWalletError::CdkDhke(_error) => WalletError::internal(),
            BcrWalletError::BtcBip32(_error) => WalletError::internal(),
            BcrWalletError::Uuid(_error) => WalletError::internal(),
            BcrWalletError::Nip19(_error) => WalletError::internal(),
            BcrWalletError::Nip06(_error) => WalletError::internal(),
            BcrWalletError::NostrClient(_) => WalletError::network(value.to_string()),
            BcrWalletError::SerdeJson(_error) => WalletError::internal(),
            BcrWalletError::Url(_) => WalletError::bad_request(value.to_string()),
            BcrWalletError::ReqwestClient(_) => WalletError::network(value.to_string()),
            BcrWalletError::InsufficientFunds => WalletError::internal(),
            BcrWalletError::WalletNotFound(id) => WalletError::not_found(id.to_string()),
            BcrWalletError::EmptyToken(_) => WalletError::bad_request(value.to_string()),
            BcrWalletError::InvalidToken(_) => WalletError::bad_request(value.to_string()),
            BcrWalletError::InvalidHashLock(_, _) => WalletError::bad_request(value.to_string()),
            BcrWalletError::NoActiveKeyset => WalletError::bad_request(value.to_string()),
            BcrWalletError::UnknownKeysetId(_id) => WalletError::bad_request(value.to_string()),
            BcrWalletError::InvalidCurrencyUnit(_) => WalletError::bad_request(value.to_string()),
            BcrWalletError::UnknownMint(_) => WalletError::bad_request(value.to_string()),
            BcrWalletError::CurrencyUnitMismatch(_, _) => {
                WalletError::bad_request(value.to_string())
            }
            BcrWalletError::NoPrepareRef(_) => WalletError::bad_request(value.to_string()),
            BcrWalletError::InactiveKeyset(_) => WalletError::bad_request(value.to_string()),
            BcrWalletError::NoDebitCurrencyInMint(_) => WalletError::bad_request(value.to_string()),
            BcrWalletError::InvalidNetwork(_, _) => WalletError::bad_request(value.to_string()),
            BcrWalletError::MissingAmount => WalletError::bad_request(value.to_string()),
            BcrWalletError::UnknownPaymentRequest(_) => WalletError::bad_request(value.to_string()),
            BcrWalletError::PaymentExpired => WalletError::bad_request(value.to_string()),
            BcrWalletError::MeltUnpaid(_) => WalletError::bad_request(value.to_string()),
            BcrWalletError::InterMint => WalletError::internal(),
            BcrWalletError::SpendingConditions => WalletError::internal(),
            BcrWalletError::NoTransport => WalletError::network(value.to_string()),
            BcrWalletError::MaxExchangeAttempts => WalletError::internal(),
            BcrWalletError::InvalidClowderPath => WalletError::internal(),
            BcrWalletError::BetaNotFound(_mint_url) => WalletError::internal(),
            BcrWalletError::NoSubstitute => WalletError::internal(),
            BcrWalletError::Unsupported(_) => WalletError {
                kind: WalletErrorKind::Unsupported,
                msg: String::default(),
            },
            BcrWalletError::External(_) => WalletError::internal(),
            BcrWalletError::WalletAlreadyExists => WalletError::bad_request(value.to_string()),
            BcrWalletError::InvalidMnemonic => WalletError::bad_request(value.to_string()),
            BcrWalletError::InvalidMintUrl(_, _) => WalletError::bad_request(value.to_string()),
            BcrWalletError::InvalidBitcoinAddress(_) => WalletError::bad_request(value.to_string()),
            BcrWalletError::TransactionCantBeReclaimed(_) => {
                WalletError::bad_request(value.to_string())
            }
            BcrWalletError::MintingError(_) => WalletError::internal(),
            BcrWalletError::InsufficientOnChainMeltAmount(_) => {
                WalletError::bad_request(value.to_string())
            }
            BcrWalletError::InsufficientOnChainMintAmount(_) => {
                WalletError::bad_request(value.to_string())
            }
            BcrWalletError::MissingDleq => WalletError::internal(),
            BcrWalletError::InterMintButNoClowderPath => WalletError::internal(),
            BcrWalletError::SchnorrSignature(_) => WalletError::internal(),
            BcrWalletError::Database(_) => WalletError::internal(),
        }
    }
}
