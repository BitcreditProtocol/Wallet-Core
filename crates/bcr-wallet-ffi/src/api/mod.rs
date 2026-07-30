use bcr_wallet_core::types::{
    ListTransactionsResult, MeltEstimation, PaymentResultCallback,
    PendingPaymentSubscriptionCallback,
};
use nostr::RelayUrl;
use once_cell::sync::Lazy;
use std::{collections::HashMap, panic, path::PathBuf, str::FromStr, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[cfg(target_os = "android")]
use android_logger::FilterBuilder;
use bcr_common::{
    cashu::{self},
    cdk_common::{self},
};
use bcr_wallet_api::{
    AppState,
    config::{AppStateConfig, CreateWalletConfig},
    error::Error as BcrWalletError,
};
use flutter_rust_bridge::{DartFnFuture, JoinHandle, frb};
use log::{error, info};
use tokio::sync::Mutex;

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
    // The mnemonics for the existing wallets, WalletId -> Mnemonic
    pub mnemonics: HashMap<String, String>,
    // The esplora base urls to use for bitcoin API calls in order of priority
    // The first one will be taken, if it fails, it goes down the list as fallbacks
    pub esplora_base_urls: Vec<String>,
    // The nostr relays to use
    pub swap_expiry_minutes: u32,
    // Dev Mode Enabled
    pub dev_mode: bool,
}

#[frb]
pub async fn init_wallet_ffi(conf: WalletFfiConfig) {
    info!("Initializing Rust Wallet FFI");
    let parsed_path = PathBuf::from_str(&conf.db_folder_path.clone())
        .expect("Not a valid file path for the database");
    let log_level = conf.log_level.clone();
    let job_interval_secs = conf.job_interval_secs;
    let job_initial_delay_secs = conf.job_initial_delay_secs;
    let parsed_mnemonics: HashMap<String, bip39::Mnemonic> = conf
        .mnemonics
        .iter()
        .map(|(k, v)| {
            let mnemonic = bip39::Mnemonic::from_str(v).expect("Not a valid bip39 mnemonic");
            (k.to_owned(), mnemonic)
        })
        .collect();
    if conf.esplora_base_urls.is_empty() {
        panic!("Esplora base urls has to have at least one valid URL");
    }
    let parsed_esplora_base_urls: Vec<url::Url> = conf
        .esplora_base_urls
        .into_iter()
        .map(|u| url::Url::from_str(&u).expect("esplora base URLs have to be valid URLs"))
        .collect();
    let swap_expiry = chrono::TimeDelta::minutes(conf.swap_expiry_minutes as i64);

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
        mnemonics: parsed_mnemonics,
        swap_expiry,
        esplora_base_urls: parsed_esplora_base_urls,
        dev_mode: conf.dev_mode.into(),
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
pub async fn wallet_add(req: CreateWalletRequest) -> Result<AddWalletResponse, WalletError> {
    let parsed_url = url::Url::from_str(&req.default_mint_url).expect("Not a valid mint URL");
    let parsed_mnemonic =
        bip39::Mnemonic::from_str(&req.mnemonic).expect("Not a valid bip39 mnemonic");
    let parsed_nostr_relays: Vec<RelayUrl> = req
        .nostr_relays
        .into_iter()
        .map(|u| RelayUrl::from_str(&u).expect("Not a valid nostr relay url"))
        .collect();
    let parsed_network = bitcoin::Network::from_str(&req.bitcoin_network).expect(
        "Not a valid bitcoin network - use one of bitcoin, testnet, testnet4, signet, regtest",
    );
    let cfg = CreateWalletConfig {
        name: req.name,
        network: parsed_network,
        nostr_relays: parsed_nostr_relays,
        mnemonic: parsed_mnemonic,
        default_mint_url: parsed_url,
    };
    let app_state = get_app_state().await;
    let wallet_id = match app_state.purse_add_wallet(cfg).await {
        Ok(id) => id,
        Err(e) => {
            error!("ERROR ADD WALLET: {e}");
            return Err(e.into());
        }
    };
    Ok(AddWalletResponse { wallet_id })
}

#[frb]
pub async fn wallet_restore(
    req: CreateWalletRequest,
) -> Result<RestoreWalletResponse, WalletError> {
    let parsed_url = url::Url::from_str(&req.default_mint_url).expect("Not a valid mint URL");
    let parsed_mnemonic =
        bip39::Mnemonic::from_str(&req.mnemonic).expect("Not a valid bip39 mnemonic");
    let parsed_nostr_relays: Vec<RelayUrl> = req
        .nostr_relays
        .into_iter()
        .map(|u| RelayUrl::from_str(&u).expect("Not a valid nostr relay url"))
        .collect();
    let parsed_network = bitcoin::Network::from_str(&req.bitcoin_network).expect(
        "Not a valid bitcoin network - use one of bitcoin, testnet, testnet4, signet, regtest",
    );
    let cfg = CreateWalletConfig {
        name: req.name,
        network: parsed_network,
        nostr_relays: parsed_nostr_relays,
        mnemonic: parsed_mnemonic,
        default_mint_url: parsed_url,
    };
    let app_state = get_app_state().await;
    let wallet_id = app_state.purse_restore_wallet(cfg).await?;
    Ok(RestoreWalletResponse { wallet_id })
}

#[frb]
pub async fn wallet_delete(req: WalletRequest) -> Result<(), WalletError> {
    let app_state = get_app_state().await;
    app_state.purse_delete_wallet(req.wallet_id).await?;
    Ok(())
}

#[frb]
pub async fn wallet_get_info(req: WalletRequest) -> Result<WalletInfoResponse, WalletError> {
    let app_state = get_app_state().await;
    let info = app_state.wallet_info(req.wallet_id).await?;
    Ok(WalletInfoResponse {
        name: info.name,
        node_id: info.node_id.to_string(),
        network: info.network.to_string(),
        default_mint_url: info.default_mint_url.to_string(),
        nostr_relays: info
            .nostr_relays
            .into_iter()
            .map(|r| r.to_string())
            .collect(),
    })
}

#[frb]
pub async fn wallet_get_node_id(req: WalletRequest) -> Result<WalletNodeIdResponse, WalletError> {
    let app_state = get_app_state().await;
    let node_id = app_state.wallet_node_id(req.wallet_id).await?;
    Ok(WalletNodeIdResponse {
        node_id: node_id.to_string(),
    })
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
        unit: currency_unit.unit,
    })
}

#[frb]
pub async fn wallet_get_balance(req: WalletRequest) -> Result<WalletBalanceResponse, WalletError> {
    let app_state = get_app_state().await;
    let balance = app_state.wallet_balance(req.wallet_id).await?;
    Ok(WalletBalanceResponse {
        debit: u64::from(balance.debit),
        credit: u64::from(balance.credit),
        total: u64::from(balance.total),
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
pub async fn wallet_edit_transaction_memo(
    req: WalletEditTransactionMemoRequest,
) -> Result<WalletEditTransactionMemoResponse, WalletError> {
    let app_state = get_app_state().await;
    app_state
        .wallet_edit_tx_memo(req.wallet_id, req.tx_id, req.new_memo)
        .await?;
    Ok(WalletEditTransactionMemoResponse { updated: true })
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
pub async fn wallet_recover_pending_stale_proofs(
    req: WalletRequest,
) -> Result<WalletRecoverStaleTransactionResponse, WalletError> {
    let app_state = get_app_state().await;
    let recovered = app_state
        .wallet_recover_pending_stale_proofs(req.wallet_id)
        .await?;
    Ok(WalletRecoverStaleTransactionResponse {
        amount: u64::from(recovered),
    })
}

#[frb]
pub async fn wallet_estimate_melt(
    req: WalletEstimateMeltRequest,
) -> Result<WalletEstimateMeltResponse, WalletError> {
    let app_state = get_app_state().await;
    let estimate = app_state
        .wallet_estimate_melt(req.wallet_id, req.amount)
        .await?;
    Ok(estimate.into())
}

#[frb]
pub async fn wallet_prepare_melt(
    req: WalletPrepareMeltRequest,
) -> Result<WalletPreparePaymentResponse, WalletError> {
    let app_state = get_app_state().await;
    let payment_summary = app_state
        .wallet_prepare_melt(
            req.wallet_id,
            req.amount,
            req.network_fee,
            req.melt_fee,
            req.address,
            req.description,
        )
        .await?;
    Ok(WalletPreparePaymentResponse {
        payment_summary: PaymentSummary {
            request_id: payment_summary.request_id.to_string(),
            unit: payment_summary.unit.to_string(),
            amount: u64::from(payment_summary.amount),
            fees: payment_summary.fees.into(),
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
        .wallet_prepare_cdk18_payment(req.wallet_id, req.input)
        .await?;
    Ok(WalletPreparePaymentResponse {
        payment_summary: PaymentSummary {
            request_id: payment_summary.request_id.to_string(),
            unit: payment_summary.unit.to_string(),
            amount: u64::from(payment_summary.amount),
            fees: payment_summary.fees.into(),
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
        .wallet_prepare_pay_by_token(req.wallet_id, req.amount, req.description)
        .await?;
    Ok(WalletPreparePaymentResponse {
        payment_summary: PaymentSummary {
            request_id: payment_summary.request_id.to_string(),
            unit: payment_summary.unit.to_string(),
            amount: u64::from(payment_summary.amount),
            fees: payment_summary.fees.into(),
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
        .wallet_prepare_payment_request(req.wallet_id, req.amount, req.description)
        .await?;
    Ok(WalletPreparePaymentReqResponse {
        payment_request: Cdk18PaymentRequest {
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
pub async fn wallet_protest_mint(
    req: WalletProtestMintRequest,
) -> Result<WalletProtestMintResponse, WalletError> {
    let app_state = get_app_state().await;
    let (status, amount) = app_state
        .wallet_protest_mint(req.wallet_id, req.quote_id)
        .await?;
    Ok(WalletProtestMintResponse {
        status: status.into(),
        amount: amount.map(|a| u64::from(a)),
    })
}

#[frb]
pub async fn wallet_protest_swap(
    req: WalletProtestSwapRequest,
) -> Result<WalletProtestSwapResponse, WalletError> {
    let app_state = get_app_state().await;
    let (status, amount) = app_state
        .wallet_protest_swap(req.wallet_id, req.commitment_sig)
        .await?;
    Ok(WalletProtestSwapResponse {
        status: status.into(),
        amount: amount.map(|a| u64::from(a)),
    })
}

#[frb]
pub async fn wallet_protest_melt(
    req: WalletProtestMeltRequest,
) -> Result<WalletProtestMeltResponse, WalletError> {
    let app_state = get_app_state().await;
    let (status, amount) = app_state
        .wallet_protest_melt(req.wallet_id, req.quote_id)
        .await?;
    Ok(WalletProtestMeltResponse {
        status: status.into(),
        amount: amount.map(|a| u64::from(a)),
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
    req: WalletListTransactionsRequest,
) -> Result<WalletListTransactionsResponse, WalletError> {
    let app_state = get_app_state().await;
    let limit = req.limit.clamp(5, 100);
    let ListTransactionsResult {
        txs,
        next_cursor,
        fees_by_month,
    } = app_state
        .wallet_list_txs(
            req.wallet_id,
            req.filter.into(),
            req.sort.into(),
            limit,
            req.cursor.map(|c| c.try_into()).transpose()?,
        )
        .await?;
    Ok(WalletListTransactionsResponse {
        txs: txs.into_iter().map(|t| t.into()).collect(),
        next_cursor: next_cursor.map(|nc| nc.into()),
        fees_by_month: fees_by_month.into_iter().map(|fbm| fbm.into()).collect(),
    })
}

#[frb]
pub async fn wallet_get_ids() -> Result<WalletsIdsResponse, WalletError> {
    let app_state = get_app_state().await;
    let ids = app_state.purse_wallets_ids().await?;
    Ok(WalletsIdsResponse { ids })
}

#[frb]
pub async fn add_contact(req: AddContactRequest) -> Result<AddContactResponse, WalletError> {
    let app_state = get_app_state().await;
    let res = app_state
        .purse_add_contact(
            req.bitcoin_network,
            req.node_id,
            req.email,
            req.name,
            req.company,
        )
        .await?;
    Ok(AddContactResponse {
        contact_id: res.to_string(),
    })
}

#[frb]
pub async fn edit_contact(req: EditContactRequest) -> Result<EditContactResponse, WalletError> {
    let app_state = get_app_state().await;
    app_state
        .purse_edit_contact(
            req.bitcoin_network,
            req.contact_id.clone(),
            req.node_id,
            req.email,
            req.name,
            req.company,
        )
        .await?;
    Ok(EditContactResponse {
        contact_id: req.contact_id,
    })
}

#[frb]
pub async fn delete_contact(
    req: DeleteContactRequest,
) -> Result<DeleteContactResponse, WalletError> {
    let app_state = get_app_state().await;
    app_state
        .purse_delete_contact(req.bitcoin_network, req.contact_id.clone())
        .await?;
    Ok(DeleteContactResponse {
        contact_id: req.contact_id,
    })
}

#[frb]
pub async fn get_contact(req: GetContactRequest) -> Result<GetContactResponse, WalletError> {
    let app_state = get_app_state().await;
    let contact = app_state
        .purse_get_contact(req.bitcoin_network, req.contact_id.clone())
        .await?;
    Ok(GetContactResponse {
        contact: contact.into(),
    })
}

#[frb]
pub async fn list_contacts(req: ListContactsRequest) -> Result<ListContactsResponse, WalletError> {
    let app_state = get_app_state().await;
    let contacts = app_state
        .purse_list_contacts(req.bitcoin_network, req.search_term.clone())
        .await?;
    Ok(ListContactsResponse {
        contacts: contacts.into_iter().map(|c| c.into()).collect(),
    })
}

#[frb]
pub async fn wallet_prepare_pay_to_contact(
    req: WalletPreparePaymentByContactRequest,
) -> Result<WalletPreparePaymentResponse, WalletError> {
    let app_state = get_app_state().await;
    let payment_summary = app_state
        .wallet_prepare_pay_to_contact(req.wallet_id, req.contact_id, req.amount, req.description)
        .await?;
    Ok(WalletPreparePaymentResponse {
        payment_summary: PaymentSummary {
            request_id: payment_summary.request_id.to_string(),
            unit: payment_summary.unit.to_string(),
            amount: u64::from(payment_summary.amount),
            fees: payment_summary.fees.into(),
            reserved_fees: u64::from(payment_summary.reserved_fees),
            expiry: payment_summary.expiry,
            ptype: PaymentType::from(bcr_wallet_core::types::PaymentType::from(
                payment_summary.ptype,
            )),
        },
    })
}

#[frb]
pub async fn wallet_pay_to_contact(
    req: WalletPaymentByContactRequest,
) -> Result<WalletTransactionIdResponse, WalletError> {
    let app_state = get_app_state().await;
    let res = app_state
        .wallet_pay_to_contact(req.wallet_id, req.rid)
        .await?;
    Ok(WalletTransactionIdResponse {
        tx_id: res.to_string(),
    })
}

#[frb]
pub async fn wallet_request_payment_from_contact(
    req: WalletRequestPaymentFromContactRequest,
) -> Result<WalletRequestPaymentFromContactResponse, WalletError> {
    let app_state = get_app_state().await;
    let res = app_state
        .wallet_request_payment_from_contact(
            req.wallet_id,
            req.contact_id,
            req.amount,
            req.description,
            req.deadline,
        )
        .await?;
    Ok(WalletRequestPaymentFromContactResponse {
        payment_request_id: res.to_string(),
    })
}

#[frb]
pub async fn wallet_list_payment_requests(
    req: WalletListPaymentRequestsRequest,
) -> Result<WalletListPaymentRequestsResponse, WalletError> {
    let app_state = get_app_state().await;
    let res = app_state
        .wallet_list_payment_requests(
            req.wallet_id,
            req.direction.into(),
            req.states.into_iter().map(|s| s.into()).collect(),
        )
        .await?;
    Ok(WalletListPaymentRequestsResponse {
        payment_requests: res.into_iter().map(|p| p.into()).collect(),
    })
}

#[frb]
pub async fn wallet_get_payment_request(
    req: WalletGetPaymentRequestRequest,
) -> Result<WalletGetPaymentRequestResponse, WalletError> {
    let app_state = get_app_state().await;
    let res = app_state
        .wallet_get_payment_request(req.wallet_id, req.payment_request_id)
        .await?;
    Ok(WalletGetPaymentRequestResponse {
        payment_request: res.into(),
    })
}

#[frb]
pub async fn wallet_prepare_pay_payment_request(
    req: WalletPreparePayPaymentRequestRequest,
) -> Result<WalletPreparePaymentResponse, WalletError> {
    let app_state = get_app_state().await;
    let payment_summary = app_state
        .wallet_prepare_pay_payment_request(req.wallet_id, req.payment_request_id)
        .await?;
    Ok(WalletPreparePaymentResponse {
        payment_summary: PaymentSummary {
            request_id: payment_summary.request_id.to_string(),
            unit: payment_summary.unit.to_string(),
            amount: u64::from(payment_summary.amount),
            fees: payment_summary.fees.into(),
            reserved_fees: u64::from(payment_summary.reserved_fees),
            expiry: payment_summary.expiry,
            ptype: PaymentType::from(bcr_wallet_core::types::PaymentType::from(
                payment_summary.ptype,
            )),
        },
    })
}

#[frb]
pub async fn wallet_pay_payment_request(
    req: WalletPayRequest,
) -> Result<WalletTransactionIdResponse, WalletError> {
    let app_state = get_app_state().await;
    let res = app_state
        .wallet_pay_payment_request(req.wallet_id, req.rid)
        .await?;
    Ok(WalletTransactionIdResponse {
        tx_id: res.to_string(),
    })
}

#[frb]
pub async fn wallet_reject_payment_request(
    req: WalletRejectPaymentRequestRequest,
) -> Result<WalletRejectPaymentRequestResponse, WalletError> {
    let app_state = get_app_state().await;
    app_state
        .wallet_reject_payment_request(req.wallet_id, req.payment_request_id.clone())
        .await?;
    Ok(WalletRejectPaymentRequestResponse {
        payment_request_id: req.payment_request_id.clone(),
    })
}

#[frb]
pub async fn wallet_cancel_payment_request(
    req: WalletCancelPaymentRequestRequest,
) -> Result<WalletCancelPaymentRequestResponse, WalletError> {
    let app_state = get_app_state().await;
    app_state
        .wallet_cancel_payment_request(req.wallet_id, req.payment_request_id.clone())
        .await?;
    Ok(WalletCancelPaymentRequestResponse {
        payment_request_id: req.payment_request_id.clone(),
    })
}

#[frb]
pub async fn wallet_subscribe_to_payment_requests(
    req: WalletSubscribeToPaymentRequestsRequest,
    result_callback: impl Fn(WalletPendingPaymentRequestResponse) -> DartFnFuture<()>
    + Send
    + Sync
    + 'static,
) -> Result<WalletPaymentCheckHandle, WalletError> {
    let app_state = get_app_state().await;

    let dart_callback = Arc::new(result_callback);
    let callback: PendingPaymentSubscriptionCallback = Arc::new(move |id| {
        let dart_callback = dart_callback.clone();
        flutter_rust_bridge::spawn(async move {
            let _ = dart_callback(WalletPendingPaymentRequestResponse { id: id.to_string() }).await;
        });
    });

    let cancel_token = CancellationToken::new();
    let handle = WalletPaymentCheckHandle {
        cancel_token: cancel_token.clone(),
    };
    flutter_rust_bridge::spawn(async move {
        if let Err(e) = app_state
            .wallet_subscribe_to_payment_requests(req.wallet_id, cancel_token, callback.clone())
            .await
        {
            error!("Error during wallet_subscribe_to_payment_requests: {e}");
        }
    });
    Ok(handle)
}

#[frb]
pub async fn wallet_dev_mode_get_detailed_balance(
    req: WalletRequest,
) -> Result<WalletDevModeDetailedBalanceResponse, WalletError> {
    let app_state = get_app_state().await;
    let detailed_balance = app_state
        .wallet_dev_mode_detailed_balance(req.wallet_id)
        .await?;
    Ok(WalletDevModeDetailedBalanceResponse {
        entries: detailed_balance
            .into_iter()
            .map(|entry| WalletDevModeDetailedBalanceEntry {
                kid: entry.kid.to_string(),
                final_expiry: entry.final_expiry,
                amount: u64::from(entry.amount),
            })
            .collect(),
    })
}

#[frb]
pub async fn generate_random_mnemonic(
    req: MnemonicRequest,
) -> Result<MnemonicResponse, WalletError> {
    let parsed_network = bitcoin::Network::from_str(&req.bitcoin_network).expect(
        "Not a valid bitcoin network - use one of bitcoin, testnet, testnet4, signet, regtest",
    );
    let (mnemonic, wallet_id) =
        bcr_wallet_api::generate_random_mnemonic(req.length, parsed_network);
    Ok(MnemonicResponse {
        wallet_id,
        mnemonic,
    })
}

#[frb]
pub async fn check_btc_tx_status(
    req: BtcTxStatusRequest,
) -> Result<BtcTxStatusResponse, WalletError> {
    let app_state = get_app_state().await;
    let resp = app_state
        .check_btc_tx_status(req.tx_id, req.bitcoin_network)
        .await?;
    Ok(resp.into())
}

#[frb]
pub async fn wallet_id_for_mnemonic_and_network(
    req: WalletIdForMnemonicAndNetworkRequest,
) -> Result<WalletIdForMnemonicAndNetworkResponse, WalletError> {
    let parsed_network = bitcoin::Network::from_str(&req.bitcoin_network).expect(
        "Not a valid bitcoin network - use one of bitcoin, testnet, testnet4, signet, regtest",
    );
    let parsed_mnemonic =
        bip39::Mnemonic::from_str(&req.mnemonic).expect("Not a valid bip39 mnemonic");
    let wallet_id = bcr_wallet_api::get_wallet_id(&parsed_mnemonic, parsed_network);
    Ok(WalletIdForMnemonicAndNetworkResponse { wallet_id })
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
pub async fn wallet_get_status() -> Result<StatusResponse, WalletError> {
    // nostr connection status for each wallet
    let app_state = get_app_state().await;
    let nostr_connected_map = app_state.purse_wallets_nostr_connected().await;
    Ok(StatusResponse {
        app_version: VERSION.to_owned(),
        nostr_connected: nostr_connected_map,
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

#[frb]
pub async fn wallet_set_dev_mode(
    req: SetDevModeRequest,
) -> Result<SetDevModeResponse, WalletError> {
    let app_state = get_app_state().await;
    app_state.set_dev_mode(req.dev_mode);
    Ok(SetDevModeResponse {
        dev_mode: req.dev_mode,
    })
}

// -------------------------------------------------------------- Data types
#[derive(Debug, Clone)]
pub struct CreateWalletRequest {
    // The name of the wallet to create
    pub name: String,
    // The default mint URL for the wallet and restoration
    pub default_mint_url: String,
    // The bitcoin_network to use. Options are: bitcoin, testnet, testnet4, signet, regtest
    pub bitcoin_network: String,
    // The mnemonic to use
    pub mnemonic: String,
    // The nostr relays to use
    pub nostr_relays: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct AddWalletResponse {
    pub wallet_id: String,
}

#[derive(Debug, Clone)]
pub struct RestoreWalletResponse {
    pub wallet_id: String,
}

#[derive(Debug, Clone)]
pub struct WalletRequest {
    pub wallet_id: String,
}

#[derive(Debug, Clone)]
pub struct SetDevModeRequest {
    pub dev_mode: bool,
}

#[derive(Debug, Clone)]
pub struct SetDevModeResponse {
    pub dev_mode: bool,
}

#[derive(Debug, Clone)]
pub struct WalletListTransactionsRequest {
    pub wallet_id: String,
    pub filter: TransactionFilters,
    pub sort: TransactionSort,
    // returned entries limit - clamped to 5-100 per call
    pub limit: usize,
    pub cursor: Option<TransactionCursor>,
}

#[derive(Debug, Clone)]
pub struct WalletListTransactionsResponse {
    pub txs: Vec<Transaction>,
    pub next_cursor: Option<TransactionCursor>,
    pub fees_by_month: Vec<FeesByMonth>,
}

#[derive(Debug, Clone)]
pub struct WalletTransactionRequest {
    pub wallet_id: String,
    pub tx_id: String,
}

#[derive(Debug, Clone)]
pub struct WalletEditTransactionMemoRequest {
    pub wallet_id: String,
    pub tx_id: String,
    pub new_memo: Option<String>,
}

#[derive(Debug, Clone)]
pub struct WalletEditTransactionMemoResponse {
    pub updated: bool,
}

#[derive(Debug, Clone)]
pub struct WalletInfoResponse {
    pub name: String,
    pub node_id: String,
    pub network: String,
    pub default_mint_url: String,
    pub nostr_relays: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct WalletNameResponse {
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct WalletNodeIdResponse {
    pub node_id: String,
}

#[derive(Debug, Clone)]
pub struct WalletMintUrlResponse {
    pub mint_url: String,
}

#[derive(Debug, Clone)]
pub struct WalletCurrencyUnitResponse {
    pub unit: String,
}

#[derive(Debug, Clone)]
pub struct WalletBalanceResponse {
    pub debit: u64,
    pub credit: u64,
    pub total: u64,
}

#[derive(Debug, Clone)]
pub struct WalletDevModeDetailedBalanceResponse {
    pub entries: Vec<WalletDevModeDetailedBalanceEntry>,
}

#[derive(Debug, Clone)]
pub struct WalletDevModeDetailedBalanceEntry {
    pub kid: String,
    pub final_expiry: Option<u64>,
    pub amount: u64,
}

#[derive(Debug, Clone)]
pub struct WalletReceiveRequest {
    pub wallet_id: String,
    pub token: String,
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
pub struct WalletCheckReceivedPaymentRequest {
    pub wallet_id: String,
    pub max_wait_sec: u64,
    pub p_id: String,
}

#[derive(Debug, Clone)]
pub struct WalletMaybeTransactionIdResponse {
    pub tx_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct WalletPendingPaymentRequestResponse {
    pub id: String,
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

impl From<cdk_common::wallet::TransactionDirection> for TransactionDirection {
    fn from(dir: cdk_common::wallet::TransactionDirection) -> Self {
        match dir {
            cdk_common::wallet::TransactionDirection::Incoming => TransactionDirection::Incoming,
            cdk_common::wallet::TransactionDirection::Outgoing => TransactionDirection::Outgoing,
        }
    }
}

impl From<TransactionDirection> for cdk_common::wallet::TransactionDirection {
    fn from(dir: TransactionDirection) -> Self {
        match dir {
            TransactionDirection::Incoming => cdk_common::wallet::TransactionDirection::Incoming,
            TransactionDirection::Outgoing => cdk_common::wallet::TransactionDirection::Outgoing,
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
    Swap,
    Contact,
}

impl From<bcr_wallet_core::types::PaymentType> for PaymentType {
    fn from(ptype: bcr_wallet_core::types::PaymentType) -> Self {
        match ptype {
            bcr_wallet_core::types::PaymentType::NotApplicable => PaymentType::NotApplicable,
            bcr_wallet_core::types::PaymentType::Token => PaymentType::Token,
            bcr_wallet_core::types::PaymentType::Cdk18 => PaymentType::Cdk18,
            bcr_wallet_core::types::PaymentType::OnChain => PaymentType::OnChain,
            bcr_wallet_core::types::PaymentType::Swap => PaymentType::Swap,
            bcr_wallet_core::types::PaymentType::Contact => PaymentType::Contact,
        }
    }
}

impl From<PaymentType> for bcr_wallet_core::types::PaymentType {
    fn from(ptype: PaymentType) -> Self {
        match ptype {
            PaymentType::NotApplicable => bcr_wallet_core::types::PaymentType::NotApplicable,
            PaymentType::Token => bcr_wallet_core::types::PaymentType::Token,
            PaymentType::Cdk18 => bcr_wallet_core::types::PaymentType::Cdk18,
            PaymentType::OnChain => bcr_wallet_core::types::PaymentType::OnChain,
            PaymentType::Swap => bcr_wallet_core::types::PaymentType::Swap,
            PaymentType::Contact => bcr_wallet_core::types::PaymentType::Contact,
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

impl From<bcr_wallet_core::types::TransactionStatus> for TransactionStatus {
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

impl From<TransactionStatus> for bcr_wallet_core::types::TransactionStatus {
    fn from(status: TransactionStatus) -> Self {
        match status {
            TransactionStatus::NotApplicable => {
                bcr_wallet_core::types::TransactionStatus::NotApplicable
            }
            TransactionStatus::Pending => bcr_wallet_core::types::TransactionStatus::Pending,
            TransactionStatus::Settled => bcr_wallet_core::types::TransactionStatus::Settled,
            TransactionStatus::Canceled => bcr_wallet_core::types::TransactionStatus::Canceled,
        }
    }
}

#[derive(Clone, Copy, Default, Debug)]
pub enum ProtestStatus {
    #[default]
    Resolved,
    Rabid,
    Offline,
}

impl From<bcr_common::wire::common::ProtestStatus> for ProtestStatus {
    fn from(s: bcr_common::wire::common::ProtestStatus) -> Self {
        match s {
            bcr_common::wire::common::ProtestStatus::Resolved => ProtestStatus::Resolved,
            bcr_common::wire::common::ProtestStatus::Rabid => ProtestStatus::Rabid,
            bcr_common::wire::common::ProtestStatus::Offline => ProtestStatus::Offline,
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum PaymentRequestState {
    Pending,
    Paid { tx_id: Uuid },
    Canceled,
    Rejected,
}

impl From<PaymentRequestState> for bcr_wallet_core::types::PaymentRequestState {
    fn from(value: PaymentRequestState) -> Self {
        match value {
            PaymentRequestState::Pending => bcr_wallet_core::types::PaymentRequestState::Pending,
            PaymentRequestState::Paid { tx_id } => {
                bcr_wallet_core::types::PaymentRequestState::Paid { tx_id }
            }
            PaymentRequestState::Canceled => bcr_wallet_core::types::PaymentRequestState::Canceled,
            PaymentRequestState::Rejected => bcr_wallet_core::types::PaymentRequestState::Rejected,
        }
    }
}

impl From<bcr_wallet_core::types::PaymentRequestState> for PaymentRequestState {
    fn from(value: bcr_wallet_core::types::PaymentRequestState) -> Self {
        match value {
            bcr_wallet_core::types::PaymentRequestState::Pending => PaymentRequestState::Pending,
            bcr_wallet_core::types::PaymentRequestState::Paid { tx_id } => {
                PaymentRequestState::Paid { tx_id }
            }
            bcr_wallet_core::types::PaymentRequestState::Canceled => PaymentRequestState::Canceled,
            bcr_wallet_core::types::PaymentRequestState::Rejected => PaymentRequestState::Rejected,
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum PaymentRequestListState {
    Pending,
    Paid,
    Canceled,
    Rejected,
}

impl From<PaymentRequestListState> for bcr_wallet_core::types::PaymentRequestState {
    fn from(value: PaymentRequestListState) -> Self {
        match value {
            PaymentRequestListState::Pending => {
                bcr_wallet_core::types::PaymentRequestState::Pending
            }
            PaymentRequestListState::Paid => bcr_wallet_core::types::PaymentRequestState::Paid {
                tx_id: Uuid::default(), // use default transactionid, since we're only interested in the type for listing
            },
            PaymentRequestListState::Canceled => {
                bcr_wallet_core::types::PaymentRequestState::Canceled
            }
            PaymentRequestListState::Rejected => {
                bcr_wallet_core::types::PaymentRequestState::Rejected
            }
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum PaymentRequestDirection {
    Incoming,
    Outgoing,
}

impl From<PaymentRequestDirection> for bcr_wallet_core::types::PaymentRequestDirection {
    fn from(value: PaymentRequestDirection) -> Self {
        match value {
            PaymentRequestDirection::Incoming => {
                bcr_wallet_core::types::PaymentRequestDirection::Incoming
            }
            PaymentRequestDirection::Outgoing => {
                bcr_wallet_core::types::PaymentRequestDirection::Outgoing
            }
        }
    }
}

impl From<bcr_wallet_core::types::PaymentRequestDirection> for PaymentRequestDirection {
    fn from(value: bcr_wallet_core::types::PaymentRequestDirection) -> Self {
        match value {
            bcr_wallet_core::types::PaymentRequestDirection::Incoming => {
                PaymentRequestDirection::Incoming
            }
            bcr_wallet_core::types::PaymentRequestDirection::Outgoing => {
                PaymentRequestDirection::Outgoing
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct PaymentRequest {
    pub id: String,
    pub node_id: String,
    pub amount: u64,
    pub unit: String,
    pub description: Option<String>,
    pub deadline: Option<u64>,
    pub created_at: u64,
}

impl From<bcr_wallet_core::types::PaymentRequest> for PaymentRequest {
    fn from(value: bcr_wallet_core::types::PaymentRequest) -> Self {
        Self {
            id: value.id.to_string(),
            node_id: value.node_id.to_string(),
            amount: value.amount.to_u64(),
            unit: value.unit.to_string(),
            description: value.description,
            deadline: value.deadline,
            created_at: value.created_at,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Contact {
    pub id: String,
    pub node_id: Option<String>,
    pub email: Option<String>,
    pub name: Option<String>,
    pub company: Option<String>,
}

impl From<bcr_wallet_core::contact::Contact> for Contact {
    fn from(v: bcr_wallet_core::contact::Contact) -> Self {
        Self {
            id: v.id.to_string(),
            node_id: v.node_id.map(|n| n.to_string()),
            email: v.email.map(|e| e.to_string()),
            name: v.name.map(|n| n.to_string()),
            company: v.company.map(|c| c.to_string()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Transaction {
    pub id: String,
    pub amount: u64,
    pub fees: TransactionFees,
    pub unit: String,
    pub tstamp: u64,
    pub direction: TransactionDirection,
    pub memo: Option<String>,
    pub ptype: PaymentType,
    pub status: TransactionStatus,
    pub btc_tx_id: Option<String>,
    pub quote_id: Option<String>,
    pub contact_node_id: Option<String>,
    pub payment_request_id: Option<String>,
    pub linked_txs: Vec<TransactionLink>,
}

#[derive(Debug, Clone)]
pub struct TransactionFees {
    pub swap: u64,
    pub network: u64,
    pub melt: u64,
}

impl From<bcr_wallet_core::types::TransactionFees> for TransactionFees {
    fn from(value: bcr_wallet_core::types::TransactionFees) -> Self {
        Self {
            swap: u64::from(value.swap),
            network: u64::from(value.network),
            melt: u64::from(value.melt),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TransactionLink {
    pub tx_id: String,
    pub reason: TransactionLinkReason,
}

impl From<bcr_wallet_core::types::TransactionLink> for TransactionLink {
    fn from(value: bcr_wallet_core::types::TransactionLink) -> Self {
        Self {
            tx_id: value.tx_id.to_string(),
            reason: value.reason.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionLinkReason {
    Reclaim,
}

impl From<bcr_wallet_core::types::TransactionLinkReason> for TransactionLinkReason {
    fn from(value: bcr_wallet_core::types::TransactionLinkReason) -> Self {
        match value {
            bcr_wallet_core::types::TransactionLinkReason::Reclaim => {
                TransactionLinkReason::Reclaim
            }
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct TransactionFilters {
    /// Empty means all payment types
    pub payment_types: Vec<PaymentType>,
    /// Empty means all statuses
    pub statuses: Vec<TransactionStatus>,
    /// None means both incoming and outgoing
    pub direction: Option<TransactionDirection>,
    /// None means no time restriction
    pub time_range: Option<TimeRange>,
}

impl From<TransactionFilters> for bcr_wallet_core::types::TransactionFilters {
    fn from(value: TransactionFilters) -> Self {
        Self {
            payment_types: value
                .payment_types
                .into_iter()
                .map(|pt| pt.into())
                .collect(),
            statuses: value.statuses.into_iter().map(|s| s.into()).collect(),
            direction: value.direction.map(|d| d.into()),
            time_range: value.time_range.map(|t| t.into()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeRange {
    /// Inclusive lower bound
    pub from: Option<u64>,
    /// Inclusive upper bound
    pub to: Option<u64>,
}

impl From<TimeRange> for bcr_wallet_core::types::TimeRange {
    fn from(value: TimeRange) -> Self {
        Self {
            from: value.from,
            to: value.to,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TransactionSort {
    TimeAsc,
    #[default]
    TimeDesc,
    AmountAsc,
    AmountDesc,
}

impl From<TransactionSort> for bcr_wallet_core::types::TransactionSort {
    fn from(value: TransactionSort) -> Self {
        match value {
            TransactionSort::TimeAsc => bcr_wallet_core::types::TransactionSort::TimeAsc,
            TransactionSort::TimeDesc => bcr_wallet_core::types::TransactionSort::TimeDesc,
            TransactionSort::AmountAsc => bcr_wallet_core::types::TransactionSort::AmountAsc,
            TransactionSort::AmountDesc => bcr_wallet_core::types::TransactionSort::AmountDesc,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransactionCursor {
    pub sort: TransactionSort,
    pub tstamp: Option<u64>,
    pub amount: Option<u64>,
    pub id: String,
}

impl TryFrom<TransactionCursor> for bcr_wallet_core::types::TransactionCursor {
    type Error = BcrWalletError;

    fn try_from(value: TransactionCursor) -> Result<Self, Self::Error> {
        Ok(match value.sort {
            TransactionSort::TimeDesc => bcr_wallet_core::types::TransactionCursor::TimeDesc {
                tstamp: value.tstamp.ok_or(BcrWalletError::InvalidCursor)?,
                id: Uuid::from_str(&value.id).map_err(|_| BcrWalletError::InvalidTransactionId)?,
            },
            TransactionSort::TimeAsc => bcr_wallet_core::types::TransactionCursor::TimeAsc {
                tstamp: value.tstamp.ok_or(BcrWalletError::InvalidCursor)?,
                id: Uuid::from_str(&value.id).map_err(|_| BcrWalletError::InvalidTransactionId)?,
            },
            TransactionSort::AmountDesc => bcr_wallet_core::types::TransactionCursor::AmountDesc {
                amount: cashu::Amount::from(value.amount.ok_or(BcrWalletError::InvalidCursor)?),
                id: Uuid::from_str(&value.id).map_err(|_| BcrWalletError::InvalidTransactionId)?,
            },
            TransactionSort::AmountAsc => bcr_wallet_core::types::TransactionCursor::AmountAsc {
                amount: cashu::Amount::from(value.amount.ok_or(BcrWalletError::InvalidCursor)?),
                id: Uuid::from_str(&value.id).map_err(|_| BcrWalletError::InvalidTransactionId)?,
            },
        })
    }
}

impl From<bcr_wallet_core::types::TransactionCursor> for TransactionCursor {
    fn from(value: bcr_wallet_core::types::TransactionCursor) -> Self {
        match value {
            bcr_wallet_core::types::TransactionCursor::TimeDesc { tstamp, id } => {
                TransactionCursor {
                    sort: TransactionSort::TimeDesc,
                    tstamp: Some(tstamp),
                    amount: None,
                    id: id.to_string(),
                }
            }
            bcr_wallet_core::types::TransactionCursor::TimeAsc { tstamp, id } => {
                TransactionCursor {
                    sort: TransactionSort::TimeAsc,
                    tstamp: Some(tstamp),
                    amount: None,
                    id: id.to_string(),
                }
            }
            bcr_wallet_core::types::TransactionCursor::AmountDesc { amount, id } => {
                TransactionCursor {
                    sort: TransactionSort::AmountDesc,
                    amount: Some(u64::from(amount)),
                    tstamp: None,
                    id: id.to_string(),
                }
            }
            bcr_wallet_core::types::TransactionCursor::AmountAsc { amount, id } => {
                TransactionCursor {
                    sort: TransactionSort::AmountAsc,
                    amount: Some(u64::from(amount)),
                    tstamp: None,
                    id: id.to_string(),
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct FeesByMonth {
    pub year: i32,
    pub month: u32,
    pub fees: TransactionFees,
}

impl From<bcr_wallet_core::types::FeesByMonth> for FeesByMonth {
    fn from(value: bcr_wallet_core::types::FeesByMonth) -> Self {
        FeesByMonth {
            year: value.year,
            month: value.month,
            fees: value.fees.into(),
        }
    }
}

impl From<bcr_wallet_core::types::Transaction> for Transaction {
    fn from(value: bcr_wallet_core::types::Transaction) -> Self {
        Self {
            id: value.id.to_string(),
            amount: u64::from(value.amount),
            fees: value.fees.into(),
            unit: value.unit.to_string(),
            tstamp: value.tstamp,
            direction: TransactionDirection::from(value.direction),
            memo: value.memo,
            ptype: value.payment_type.into(),
            status: value.status.into(),
            btc_tx_id: value.btc_tx_id.map(|tx_id| tx_id.to_string()),
            quote_id: value.quote_id.map(|q_id| q_id.to_string()),
            contact_node_id: value.contact_node_id.map(|node_id| node_id.to_string()),
            payment_request_id: value.payment_request_id.map(|pr_id| pr_id.to_string()),
            linked_txs: value
                .linked_txs
                .into_iter()
                .map(|linked_tx| linked_tx.into())
                .collect(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct WalletTransactionResponse {
    pub transaction: Transaction,
}

#[derive(Debug, Clone)]
pub struct WalletRefreshTransactionRequest {
    pub wallet_id: String,
    pub tx_id: String,
}

#[derive(Debug, Clone)]
pub struct WalletReclaimTransactionRequest {
    pub wallet_id: String,
    pub tx_id: String,
}

#[derive(Debug, Clone)]
pub struct WalletReclaimTransactionResponse {
    pub amount: u64,
}

#[derive(Debug, Clone)]
pub struct WalletRecoverStaleTransactionResponse {
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
pub struct WalletEstimateMeltRequest {
    pub wallet_id: String,
    pub amount: u64,
}

#[derive(Debug, Clone)]
pub struct WalletEstimateMeltResponse {
    pub tx_vsize: u64,
    pub fee_rates: Vec<WalletEstimateMeltFeeRate>,
    pub melt_fee: u64,
    pub melt_fee_ppk: u64,
}

impl From<MeltEstimation> for WalletEstimateMeltResponse {
    fn from(value: MeltEstimation) -> Self {
        Self {
            tx_vsize: value.tx_vsize,
            fee_rates: value
                .fee_rates
                .into_iter()
                .map(|fr| WalletEstimateMeltFeeRate {
                    target_blocks: fr.target_blocks,
                    sat_per_vb: fr.sat_per_vb,
                })
                .collect(),
            melt_fee: value.melt_fee,
            melt_fee_ppk: value.melt_fee_ppk,
        }
    }
}

#[derive(Debug, Clone)]
pub struct WalletPrepareMeltRequest {
    pub wallet_id: String,
    pub amount: u64,
    pub network_fee: u64,
    pub melt_fee: u64,
    pub address: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone)]
pub struct WalletEstimateMeltFeeRate {
    pub target_blocks: u16,
    pub sat_per_vb: f32,
}

#[derive(Debug, Clone)]
pub struct WalletMintRequest {
    pub wallet_id: String,
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
    pub wallet_id: String,
    pub input: String,
}

#[derive(Debug, Clone)]
pub struct WalletPreparePaymentResponse {
    pub payment_summary: PaymentSummary,
}

#[derive(Debug, Clone)]
pub struct WalletPreparePaymentReqRequest {
    pub wallet_id: String,
    pub amount: u64,
    pub unit: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone)]
pub struct WalletPreparePaymentReqResponse {
    pub payment_request: Cdk18PaymentRequest,
}

#[derive(Debug, Clone)]
pub struct WalletCheckPendingMintsResponse {
    pub tx_ids: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct WalletProtestMintRequest {
    pub wallet_id: String,
    pub quote_id: String,
}

#[derive(Debug, Clone)]
pub struct WalletProtestMintResponse {
    pub status: ProtestStatus,
    pub amount: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct WalletProtestSwapRequest {
    pub wallet_id: String,
    pub commitment_sig: String,
}

#[derive(Debug, Clone)]
pub struct WalletProtestSwapResponse {
    pub status: ProtestStatus,
    pub amount: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct WalletProtestMeltRequest {
    pub wallet_id: String,
    pub quote_id: String,
}

#[derive(Debug, Clone)]
pub struct WalletProtestMeltResponse {
    pub status: ProtestStatus,
    pub amount: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct PaymentSummary {
    pub request_id: String,
    pub unit: String,
    pub amount: u64,
    pub fees: TransactionFees,
    pub reserved_fees: u64,
    pub expiry: u64,
    pub ptype: PaymentType,
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
    pub ids: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct MnemonicRequest {
    pub length: u32,
    pub bitcoin_network: String,
}

#[derive(Debug, Clone)]
pub struct BtcTxStatusRequest {
    pub tx_id: String,
    pub bitcoin_network: String,
}

#[derive(Debug, Clone)]
pub struct BtcTxStatusResponse {
    pub tx_id: String,
    pub bitcoin_network: String,
    pub receivers: Vec<BtcTxStatusReceiver>,
    pub fee: u64,
    pub confirmations: u64,
    pub confirmation_tstamp: Option<u64>,
}

impl From<bcr_wallet_core::types::BtcTxStatus> for BtcTxStatusResponse {
    fn from(value: bcr_wallet_core::types::BtcTxStatus) -> Self {
        Self {
            tx_id: value.tx_id.to_string(),
            bitcoin_network: value.bitcoin_network.to_string(),
            receivers: value
                .receivers
                .into_iter()
                .map(|rec| BtcTxStatusReceiver {
                    address: rec.address.assume_checked().to_string(),
                    amount: rec.amount.to_sat(),
                })
                .collect(),
            fee: value.fee.to_sat(),
            confirmations: value.confirmations,
            confirmation_tstamp: value.confirmation_tstamp,
        }
    }
}

#[derive(Debug, Clone)]
pub struct BtcTxStatusReceiver {
    pub address: String,
    pub amount: u64,
}

#[derive(Debug, Clone)]
pub struct WalletIdForMnemonicAndNetworkRequest {
    pub bitcoin_network: String,
    pub mnemonic: String,
}

#[derive(Debug, Clone)]
pub struct WalletIdForMnemonicAndNetworkResponse {
    pub wallet_id: String,
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
    pub wallet_id: String,
    pub mnemonic: String,
}

#[derive(Debug, Clone)]
pub struct StatusResponse {
    pub app_version: String,
    pub nostr_connected: HashMap<String, bool>,
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
pub struct Cdk18PaymentRequest {
    pub request: String,
    pub p_id: String,
}

#[derive(Debug, Clone)]
pub struct WalletPayRequest {
    pub wallet_id: String,
    pub rid: String,
}

#[derive(Debug, Clone)]
pub struct WalletPreparePaymentByContactRequest {
    pub wallet_id: String,
    pub contact_id: String,
    pub amount: u64,
    pub description: Option<String>,
}

#[derive(Debug, Clone)]
pub struct WalletPaymentByContactRequest {
    pub wallet_id: String,
    pub rid: String,
}

#[derive(Debug, Clone)]
pub struct WalletPreparePaymentByTokenRequest {
    pub wallet_id: String,
    pub amount: u64,
    pub description: Option<String>,
}

#[derive(Debug, Clone)]
pub struct WalletPaymentByTokenRequest {
    pub wallet_id: String,
    pub rid: String,
}

#[derive(Debug, Clone)]
pub struct WalletPaymentByTokenResponse {
    pub tx_id: String,
    pub token: String,
}

#[derive(Debug, Clone)]
pub struct AddContactRequest {
    pub bitcoin_network: String,
    pub email: Option<String>,
    pub node_id: Option<String>,
    pub name: Option<String>,
    pub company: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AddContactResponse {
    pub contact_id: String,
}

#[derive(Debug, Clone)]
pub struct EditContactRequest {
    pub bitcoin_network: String,
    pub contact_id: String,
    pub node_id: Option<String>,
    pub email: Option<String>,
    pub name: Option<String>,
    pub company: Option<String>,
}

#[derive(Debug, Clone)]
pub struct EditContactResponse {
    pub contact_id: String,
}

#[derive(Debug, Clone)]
pub struct DeleteContactRequest {
    pub bitcoin_network: String,
    pub contact_id: String,
}

#[derive(Debug, Clone)]
pub struct DeleteContactResponse {
    pub contact_id: String,
}

#[derive(Debug, Clone)]
pub struct GetContactRequest {
    pub bitcoin_network: String,
    pub contact_id: String,
}

#[derive(Debug, Clone)]
pub struct GetContactResponse {
    pub contact: Contact,
}

#[derive(Debug, Clone)]
pub struct ListContactsRequest {
    pub bitcoin_network: String,
    pub search_term: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ListContactsResponse {
    pub contacts: Vec<Contact>,
}

#[derive(Debug, Clone)]
pub struct WalletRequestPaymentFromContactRequest {
    pub wallet_id: String,
    pub contact_id: String,
    pub amount: u64,
    pub description: Option<String>,
    pub deadline: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct WalletRequestPaymentFromContactResponse {
    pub payment_request_id: String,
}

#[derive(Debug, Clone)]
pub struct WalletListPaymentRequestsRequest {
    pub wallet_id: String,
    pub direction: PaymentRequestDirection,
    pub states: Vec<PaymentRequestListState>,
}

#[derive(Debug, Clone)]
pub struct WalletSubscribeToPaymentRequestsRequest {
    pub wallet_id: String,
}

#[derive(Debug, Clone)]
pub struct WalletListPaymentRequestsResponse {
    pub payment_requests: Vec<PaymentRequest>,
}

#[derive(Debug, Clone)]
pub struct WalletGetPaymentRequestRequest {
    pub wallet_id: String,
    pub payment_request_id: String,
}

#[derive(Debug, Clone)]
pub struct WalletGetPaymentRequestResponse {
    pub payment_request: PaymentRequest,
}

#[derive(Debug, Clone)]
pub struct WalletPreparePayPaymentRequestRequest {
    pub wallet_id: String,
    pub payment_request_id: String,
}

#[derive(Debug, Clone)]
pub struct WalletRejectPaymentRequestRequest {
    pub wallet_id: String,
    pub payment_request_id: String,
}

#[derive(Debug, Clone)]
pub struct WalletRejectPaymentRequestResponse {
    pub payment_request_id: String,
}

#[derive(Debug, Clone)]
pub struct WalletCancelPaymentRequestRequest {
    pub wallet_id: String,
    pub payment_request_id: String,
}

#[derive(Debug, Clone)]
pub struct WalletCancelPaymentRequestResponse {
    pub payment_request_id: String,
}

// -------------------------------------------------------------- Errors
#[derive(Debug, Clone)]
pub struct WalletError {
    pub kind: WalletErrorKind,
    pub code: WalletErrorCode,
    pub msg: String,
}

impl WalletError {
    pub fn bad_request(msg: String, code: WalletErrorCode) -> Self {
        WalletError {
            kind: WalletErrorKind::BadRequest,
            code,
            msg,
        }
    }

    pub fn internal(msg: String) -> Self {
        WalletError {
            kind: WalletErrorKind::Internal,
            code: WalletErrorCode::Internal,
            msg,
        }
    }

    pub fn not_found(msg: String, code: WalletErrorCode) -> Self {
        WalletError {
            kind: WalletErrorKind::NotFound,
            code,
            msg,
        }
    }

    pub fn unavailable(msg: String, code: WalletErrorCode) -> Self {
        WalletError {
            kind: WalletErrorKind::Unavailable,
            code,
            msg,
        }
    }

    pub fn network(msg: String) -> Self {
        WalletError {
            kind: WalletErrorKind::Network,
            code: WalletErrorCode::Network,
            msg,
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
    Unavailable,
    Unsupported,
}

#[derive(Debug, Clone)]
pub enum WalletErrorCode {
    Internal,
    Network,
    WalletNotFound,
    ContactNotFound,
    ContactMustHaveNodeId,
    PaymentRequestNotFound,
    PaymentRequestInWrongState,
    ContactAlreadyExists,
    EmptyToken,
    InvalidToken,
    CashuMintUrl,
    Url,
    Uuid,
    Amount,
    InsufficientBalance,
    NoActiveKeyset,
    UnknownKeysetId,
    InvalidCurrencyUnit,
    NoPrepareRef,
    InactiveKeyset,
    NoDebitCurrencyInMint,
    InvalidNetwork,
    InvalidBitcoinNetwork,
    InvalidBitcoinTxId,
    MissingAmount,
    UnknownPaymentRequest,
    Unsupported,
    TransactionCantBeReclaimed,
    InsufficientOnChainMeltAmount,
    InsufficientOnChainMintAmount,
    NoDevMode,
    MeltQuoteMismatch,
    SwapCommitmentMismatch,
    InvalidBitcoinAddress,
    InvalidMnemonic,
    InvalidTransactionId,
    InvalidCursor,
    SortMismatch,
    MnemonicNotFound,
    WalletUniqueName,
    WalletUniqueId,
    InvalidNodeId,
    InvalidBillId,
    InvalidName,
    EmptyName,
    InvalidEmail,
    EmptyEmail,
    InvalidContact,
    MintClientResourceNotFound,
    MintClientServiceUnavailable,
    MintClientBadRequest,
    MintClientKeysetNotFound,
    MintClientMeltOpSuspended,
    MintClientCommitmentMismatch,
    AttestationInvalidProof,
    AttestationDigestMismatch,
    AttestationUnknownBeta,
    AttestationVerifyNotFound,
    AttestationSignature,
    BitcoinApi,
}

impl From<BcrWalletError> for WalletError {
    fn from(value: BcrWalletError) -> Self {
        error!("Error: {value}");
        match value {
            BcrWalletError::BorshSignature(_) => WalletError::internal(value.to_string()),
            BcrWalletError::Borsh(_) => WalletError::internal(value.to_string()),
            BcrWalletError::CashuMintUrl(_) => {
                WalletError::bad_request(value.to_string(), WalletErrorCode::CashuMintUrl)
            }
            BcrWalletError::Mint(_) => WalletError::internal(value.to_string()),
            BcrWalletError::Cdk(_) => WalletError::internal(value.to_string()),
            BcrWalletError::Bip39(_) => WalletError::internal(value.to_string()),
            BcrWalletError::Cdk00(_) => WalletError::internal(value.to_string()),
            BcrWalletError::Cdk01(_) => WalletError::internal(value.to_string()),
            BcrWalletError::Cdk13(_) => WalletError::internal(value.to_string()),
            BcrWalletError::Cdk11(_) => WalletError::internal(value.to_string()),
            BcrWalletError::Cdk10(_) => WalletError::internal(value.to_string()),
            BcrWalletError::Cdk14(_) => WalletError::internal(value.to_string()),
            BcrWalletError::CdkAmount(_) => {
                WalletError::bad_request(value.to_string(), WalletErrorCode::Amount)
            }
            BcrWalletError::CdkDhke(_) => WalletError::internal(value.to_string()),
            BcrWalletError::Uuid(_) => {
                WalletError::bad_request(value.to_string(), WalletErrorCode::Uuid)
            }
            BcrWalletError::SerdeJson(_) => WalletError::internal(value.to_string()),
            BcrWalletError::Url(_) => {
                WalletError::bad_request(value.to_string(), WalletErrorCode::Url)
            }
            BcrWalletError::ReqwestClient(_) => WalletError::network(value.to_string()),
            BcrWalletError::InsufficientBalance(_, _) => {
                WalletError::bad_request(value.to_string(), WalletErrorCode::InsufficientBalance)
            }
            BcrWalletError::InvalidSplitTarget => WalletError::internal(value.to_string()),
            BcrWalletError::WalletNotFound(id) => {
                WalletError::not_found(id.to_string(), WalletErrorCode::WalletNotFound)
            }
            BcrWalletError::ContactNotFound(id) => {
                WalletError::not_found(id.to_string(), WalletErrorCode::ContactNotFound)
            }
            BcrWalletError::PaymentRequestNotFound(id) => {
                WalletError::not_found(id.to_string(), WalletErrorCode::PaymentRequestNotFound)
            }
            BcrWalletError::PaymentRequestInWrongState(id) => WalletError::bad_request(
                id.to_string(),
                WalletErrorCode::PaymentRequestInWrongState,
            ),
            BcrWalletError::ContactAlreadyExists(_) => {
                WalletError::bad_request(value.to_string(), WalletErrorCode::ContactAlreadyExists)
            }
            BcrWalletError::WalletUniqueId(_) => {
                WalletError::bad_request(value.to_string(), WalletErrorCode::WalletUniqueId)
            }
            BcrWalletError::WalletUniqueName(_, _) => {
                WalletError::bad_request(value.to_string(), WalletErrorCode::WalletUniqueName)
            }
            BcrWalletError::EmptyToken(_) => {
                WalletError::bad_request(value.to_string(), WalletErrorCode::EmptyToken)
            }
            BcrWalletError::InvalidToken(_) => {
                WalletError::bad_request(value.to_string(), WalletErrorCode::InvalidToken)
            }
            BcrWalletError::NoActiveKeyset => {
                WalletError::bad_request(value.to_string(), WalletErrorCode::NoActiveKeyset)
            }
            BcrWalletError::UnknownKeysetId(_id) => {
                WalletError::bad_request(value.to_string(), WalletErrorCode::UnknownKeysetId)
            }
            BcrWalletError::InvalidCurrencyUnit(_) => {
                WalletError::bad_request(value.to_string(), WalletErrorCode::InvalidCurrencyUnit)
            }
            BcrWalletError::NoPrepareRef(_) => {
                WalletError::bad_request(value.to_string(), WalletErrorCode::NoPrepareRef)
            }
            BcrWalletError::InactiveKeyset(_) => {
                WalletError::bad_request(value.to_string(), WalletErrorCode::InactiveKeyset)
            }
            BcrWalletError::NoDebitCurrencyInMint(_) => {
                WalletError::bad_request(value.to_string(), WalletErrorCode::NoDebitCurrencyInMint)
            }
            BcrWalletError::InvalidNetwork(_, _) => {
                WalletError::bad_request(value.to_string(), WalletErrorCode::InvalidNetwork)
            }
            BcrWalletError::InvalidBitcoinNetwork(_) => {
                WalletError::bad_request(value.to_string(), WalletErrorCode::InvalidBitcoinNetwork)
            }
            BcrWalletError::InvalidBitcoinTxId(_) => {
                WalletError::bad_request(value.to_string(), WalletErrorCode::InvalidBitcoinTxId)
            }
            BcrWalletError::MissingAmount => {
                WalletError::bad_request(value.to_string(), WalletErrorCode::MissingAmount)
            }
            BcrWalletError::UnknownPaymentRequest(_) => {
                WalletError::bad_request(value.to_string(), WalletErrorCode::UnknownPaymentRequest)
            }
            BcrWalletError::InterMint => WalletError::internal(value.to_string()),
            BcrWalletError::SpendingConditions => WalletError::internal(value.to_string()),
            BcrWalletError::NoTransport => WalletError::network(value.to_string()),
            BcrWalletError::MaxExchangeAttempts => WalletError::internal(value.to_string()),
            BcrWalletError::InvalidClowderPath => WalletError::internal(value.to_string()),
            BcrWalletError::BetaNotFound(_) => WalletError::internal(value.to_string()),
            BcrWalletError::NoSubstitute => WalletError::internal(value.to_string()),
            BcrWalletError::Unsupported(_) => WalletError {
                kind: WalletErrorKind::Unsupported,
                code: WalletErrorCode::Unsupported,
                msg: String::default(),
            },
            BcrWalletError::MnemonicNotFound(_) => {
                WalletError::bad_request(value.to_string(), WalletErrorCode::MnemonicNotFound)
            }
            BcrWalletError::InvalidMnemonic => {
                WalletError::bad_request(value.to_string(), WalletErrorCode::InvalidMnemonic)
            }
            BcrWalletError::InvalidTransactionId => {
                WalletError::bad_request(value.to_string(), WalletErrorCode::InvalidTransactionId)
            }
            BcrWalletError::InvalidName => {
                WalletError::bad_request(value.to_string(), WalletErrorCode::InvalidName)
            }
            BcrWalletError::EmptyName => {
                WalletError::bad_request(value.to_string(), WalletErrorCode::EmptyName)
            }
            BcrWalletError::InvalidNodeId => {
                WalletError::bad_request(value.to_string(), WalletErrorCode::InvalidNodeId)
            }
            BcrWalletError::InvalidBillId => {
                WalletError::bad_request(value.to_string(), WalletErrorCode::InvalidBillId)
            }
            BcrWalletError::InvalidCursor => {
                WalletError::bad_request(value.to_string(), WalletErrorCode::InvalidCursor)
            }
            BcrWalletError::SortMismatch => {
                WalletError::bad_request(value.to_string(), WalletErrorCode::SortMismatch)
            }
            BcrWalletError::InvalidBitcoinAddress(_) => {
                WalletError::bad_request(value.to_string(), WalletErrorCode::InvalidBitcoinAddress)
            }
            BcrWalletError::TransactionCantBeReclaimed(_) => WalletError::bad_request(
                value.to_string(),
                WalletErrorCode::TransactionCantBeReclaimed,
            ),
            BcrWalletError::MintingError(_) => WalletError::internal(value.to_string()),
            BcrWalletError::InsufficientOnChainMeltAmount(_) => WalletError::bad_request(
                value.to_string(),
                WalletErrorCode::InsufficientOnChainMeltAmount,
            ),
            BcrWalletError::InsufficientOnChainMintAmount(_) => WalletError::bad_request(
                value.to_string(),
                WalletErrorCode::InsufficientOnChainMintAmount,
            ),
            BcrWalletError::MissingDleq => WalletError::internal(value.to_string()),
            BcrWalletError::InterMintButNoClowderPath => WalletError::internal(value.to_string()),
            BcrWalletError::SchnorrSignature(_) => WalletError::internal(value.to_string()),
            BcrWalletError::Database(_) => WalletError::internal(value.to_string()),
            BcrWalletError::Transport(_) => WalletError::internal(value.to_string()),
            BcrWalletError::Swap(_) => WalletError::internal(value.to_string()),
            BcrWalletError::NoBetas => WalletError::internal(value.to_string()),
            BcrWalletError::NoDevMode => {
                WalletError::bad_request(value.to_string(), WalletErrorCode::NoDevMode)
            }
            BcrWalletError::MeltQuoteMismatch => {
                WalletError::bad_request(value.to_string(), WalletErrorCode::MeltQuoteMismatch)
            }
            BcrWalletError::SwapCommitmentMismatch => {
                WalletError::bad_request(value.to_string(), WalletErrorCode::SwapCommitmentMismatch)
            }
            BcrWalletError::MintClientInternal(_) => WalletError::internal(value.to_string()),
            BcrWalletError::MintClientResourceNotFound(err) => {
                WalletError::not_found(err.to_string(), WalletErrorCode::MintClientResourceNotFound)
            }
            BcrWalletError::MintClientServiceUnavailable(err) => WalletError::unavailable(
                err.to_string(),
                WalletErrorCode::MintClientServiceUnavailable,
            ),
            BcrWalletError::MintClientBadRequest(err) => {
                WalletError::bad_request(err.to_string(), WalletErrorCode::MintClientBadRequest)
            }
            BcrWalletError::MintClientKeysetNotFound(err) => {
                WalletError::not_found(err.to_string(), WalletErrorCode::MintClientKeysetNotFound)
            }
            BcrWalletError::MintClientMeltOpSuspended(err) => WalletError::unavailable(
                err.to_string(),
                WalletErrorCode::MintClientMeltOpSuspended,
            ),
            BcrWalletError::MintClientCommitmentMismatch(err) => WalletError::bad_request(
                err.to_string(),
                WalletErrorCode::MintClientCommitmentMismatch,
            ),
            BcrWalletError::AttestationInvalidProof(_) => WalletError::bad_request(
                value.to_string(),
                WalletErrorCode::AttestationInvalidProof,
            ),
            BcrWalletError::AttestationDigestMismatch => WalletError::bad_request(
                value.to_string(),
                WalletErrorCode::AttestationDigestMismatch,
            ),
            BcrWalletError::AttestationUnknownBeta(_) => {
                WalletError::bad_request(value.to_string(), WalletErrorCode::AttestationUnknownBeta)
            }
            BcrWalletError::AttestationVerifyNotFound(_) => WalletError::not_found(
                value.to_string(),
                WalletErrorCode::AttestationVerifyNotFound,
            ),
            BcrWalletError::AttestationSignature(_) => {
                WalletError::bad_request(value.to_string(), WalletErrorCode::AttestationSignature)
            }
            BcrWalletError::BitcoinClient(_) => {
                WalletError::bad_request(value.to_string(), WalletErrorCode::BitcoinApi)
            }
            BcrWalletError::InvalidEmail => {
                WalletError::bad_request(value.to_string(), WalletErrorCode::InvalidEmail)
            }
            BcrWalletError::EmptyEmail => {
                WalletError::bad_request(value.to_string(), WalletErrorCode::EmptyEmail)
            }
            BcrWalletError::InvalidContact => {
                WalletError::bad_request(value.to_string(), WalletErrorCode::InvalidContact)
            }
            BcrWalletError::ContactMustHaveNodeId(_) => {
                WalletError::bad_request(value.to_string(), WalletErrorCode::ContactMustHaveNodeId)
            }
        }
    }
}
