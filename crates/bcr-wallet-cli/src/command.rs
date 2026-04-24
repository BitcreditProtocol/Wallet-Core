use std::sync::Arc;

use anyhow::Result;
use bcr_common::cdk_common::wallet::TransactionId;
use bcr_wallet_api::AppState;
use bcr_wallet_core::types::{
    PaymentResultCallback, get_btc_alpha_tx_id, get_btc_beta_tx_id, get_payment_type,
    get_transaction_status,
};
use chrono::{DateTime, Utc};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tracing::info;

pub async fn cmd_info(app_state: &AppState) -> Result<String> {
    let mut res = String::new();
    let wallet_ids = app_state.purse_wallets_ids().await?;

    push_break(&mut res);
    push_break(&mut res);
    res.push_str(&format!("{} Wallet(s) found.\n", wallet_ids.len()));
    push_line(&mut res);

    for id in wallet_ids.iter() {
        let name = app_state.wallet_name(*id).await?;
        let mint_url = app_state.wallet_mint_url(*id).await?;
        let unit = app_state.wallet_currency_unit(*id).await?.unit;
        let balance = app_state.wallet_balance(*id).await?;
        let dev_mode_detailed_balance = app_state.wallet_dev_mode_detailed_balance(*id).await?;

        let transactions = app_state.wallet_list_txs(*id).await?;

        res.push_str(&format!("Name: {name}\n"));
        res.push_str(&format!("Wallet ID: {id}\n"));
        res.push_str(&format!("Mint URL: {mint_url}\n"));
        res.push_str(&format!("Debit Balance: {} {}\n", balance.debit, unit));
        res.push_str(&format!("Credit Balance: {} {}\n", balance.credit, unit));
        res.push_str(&format!("Total Balance: {} {}\n", balance.total, unit));

        if !dev_mode_detailed_balance.is_empty() {
            res.push_str("Dev Mode Detailed Balance:");
            for entry in dev_mode_detailed_balance.iter() {
                res.push_str(&format!(
                    "\t\tId: {} \t Expiry: {} \t Amount: {}",
                    entry.kid,
                    if let Some(exp) = entry.final_expiry {
                        format_timestamp(exp)
                    } else {
                        "None".to_owned()
                    },
                    entry.amount
                ));
                push_break(&mut res);
            }
        }

        if !transactions.is_empty() {
            res.push_str("Transactions:");
            push_break(&mut res);

            for (idx, tx) in transactions.iter().enumerate() {
                let status = get_transaction_status(&tx.metadata);
                let ptype = get_payment_type(&tx.metadata);
                let alpha_btc_tx_id = get_btc_alpha_tx_id(&tx.metadata);
                let beta_btc_tx_id = get_btc_beta_tx_id(&tx.metadata);
                let quote_or_btc_tx_id = match (beta_btc_tx_id, alpha_btc_tx_id, &tx.quote_id) {
                    (Some(_), Some(_), Some(_)) => String::default(),
                    (None, Some(_), Some(_)) => String::default(),
                    (Some(_), None, Some(_)) => String::default(),
                    (Some(beta_btc_tx_id), None, None) => beta_btc_tx_id.to_string(),
                    (None, Some(alpha_btc_tx_id), None) => alpha_btc_tx_id.to_string(),
                    (Some(beta_btc_tx_id), Some(alpha_btc_tx_id), None) => {
                        format!("alpha: {}, beta: {}", alpha_btc_tx_id, beta_btc_tx_id)
                    }
                    (None, None, Some(quote_id)) => quote_id.to_string(),
                    (None, None, None) => String::default(),
                };
                res.push_str(&format!(
                    "\t\tId: {} \t Amount: {} {} \t Fees: {}  \t Status: {:?} \t {} \tType: {:<10} \t {:?} \t Memo: {} \t BTC TxID/Quote ID: {}",
                    tx.id(), tx.amount, tx.unit, tx.fee,  status, format_timestamp(tx.timestamp), &format!("{:?}", ptype), tx.direction, tx.memo.clone().unwrap_or_default(), quote_or_btc_tx_id
                ));
                push_break(&mut res);
                if idx > 20 {
                    break;
                }
            }
        }
    }
    Ok(res)
}

pub async fn cmd_add_wallet(app_state: &AppState, name: &str) -> Result<String> {
    let mut res = String::new();
    let id = app_state.purse_add_wallet(name.to_owned()).await?;
    push_break(&mut res);
    push_break(&mut res);
    res.push_str(&format!("Created Wallet for {name} - Wallet ID: {id}.\n"));
    Ok(res)
}

pub async fn cmd_delete_wallet(app_state: &AppState, name: &str, id: usize) -> Result<String> {
    let mut res = String::new();
    app_state.purse_delete_wallet(id).await?;
    push_break(&mut res);
    push_break(&mut res);
    res.push_str(&format!("Deleted Wallet for {name} - Wallet ID: {id}.\n"));
    Ok(res)
}

pub async fn cmd_restore_wallet(app_state: &AppState, name: &str) -> Result<String> {
    let mut res = String::new();
    let id = app_state.purse_restore_wallet(name.to_owned()).await?;
    push_break(&mut res);
    push_break(&mut res);
    res.push_str(&format!("Restored Wallet for {name} - Wallet ID: {id}.\n"));
    Ok(res)
}

pub async fn cmd_receive(
    app_state: &AppState,
    name: &str,
    token: &str,
    id: usize,
) -> Result<String> {
    let mut res = String::new();
    let swapped = app_state.wallet_receive_token(id, token.to_owned()).await?;
    let tx = app_state.wallet_load_tx(id, &swapped.to_string()).await?;
    push_break(&mut res);
    push_break(&mut res);
    res.push_str(&format!(
        "Received token {token}, returned {swapped} for {name} - Wallet ID: {id}.\n"
    ));
    res.push_str(&format!("tx: {tx:?}.\n"));
    Ok(res)
}

pub async fn cmd_request_payment(
    app_state: &AppState,
    name: &str,
    amount: u64,
    id: usize,
    description: Option<String>,
) -> Result<String> {
    let req = app_state
        .wallet_prepare_payment_request(id, amount, description)
        .await?;
    info!("Payment Request: {}, {}", &req.request, &req.p_id);

    let cancel_token = CancellationToken::new();
    // Uncomment to test cancellation
    // let cancel_token_clone = cancel_token.clone();
    // tokio::spawn(async move {
    //     tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
    //     cancel_token_clone.cancel();
    // });
    let (tx, rx) = oneshot::channel::<Option<TransactionId>>();

    let tx = Arc::new(std::sync::Mutex::new(Some(tx)));

    let res_cb: PaymentResultCallback = Arc::new(move |tx_id| {
        if let Some(sender) = tx.lock().unwrap().take() {
            let _ = sender.send(tx_id);
        }
    });

    app_state
        .wallet_check_received_payment(id, 60, req.p_id.clone(), cancel_token, res_cb)
        .await?;

    let Ok(tx_id) = rx.await else {
        return Ok("Cancelled".to_string());
    };

    let mut res = String::new();
    push_break(&mut res);
    push_break(&mut res);
    res.push_str(&format!(
        "Request Payment for {name}, Amount: {amount} - Wallet ID: {id}.\n"
    ));
    push_break(&mut res);
    res.push_str(&format!(
        "Transaction ID: {:?}",
        tx_id.map(|t| t.to_string())
    ));

    Ok(res)
}

pub async fn cmd_pay_by_token(
    app_state: &AppState,
    name: &str,
    id: usize,
    amount: u64,
    description: Option<String>,
) -> Result<String> {
    let mut res = String::new();
    let payment_summary = app_state
        .wallet_prepare_pay_by_token(id, amount, description)
        .await?;

    info!(
        "Payment Summary: Amount: {}, Unit: {}, Fees: {}",
        &payment_summary.amount, &payment_summary.unit, &payment_summary.fees,
    );
    let result = app_state
        .wallet_pay_by_token(id, payment_summary.request_id.to_string())
        .await?;

    push_break(&mut res);
    push_break(&mut res);
    res.push_str(&format!("Pay by Token for {name}, Wallet ID: {id}.\n"));
    push_break(&mut res);
    res.push_str(&format!("Payment Summary: {}", &payment_summary.request_id));
    res.push_str(&format!(
        "Unit: {}, Amount: {}",
        &payment_summary.unit, &payment_summary.amount
    ));
    push_break(&mut res);
    res.push_str(&format!("Transaction ID: {}", result.tx_id));
    push_break(&mut res);
    push_break(&mut res);
    res.push_str(&format!("Token: {}", result.token));

    Ok(res)
}

pub async fn cmd_send_payment(
    app_state: &AppState,
    name: &str,
    input: &str,
    id: usize,
) -> Result<String> {
    let mut res = String::new();
    let payment_summary = app_state
        .wallet_prepare_payment(id, input.to_owned())
        .await?;

    info!(
        "Payment Summary: Amount: {}, Unit: {}",
        &payment_summary.amount, &payment_summary.unit,
    );

    let tx_id = app_state
        .wallet_pay(id, payment_summary.request_id.to_string())
        .await?;

    push_break(&mut res);
    push_break(&mut res);
    res.push_str(&format!(
        "Send Payment for {name}, Input: {input} - Wallet ID: {id}.\n"
    ));
    push_break(&mut res);
    res.push_str(&format!("Payment Summary: {}", &payment_summary.request_id));
    res.push_str(&format!(
        "Unit: {}, Amount: {}, Fees: {}",
        &payment_summary.unit, &payment_summary.amount, &payment_summary.fees
    ));
    push_break(&mut res);
    res.push_str(&format!("Transaction ID: {tx_id}"));

    Ok(res)
}

pub async fn cmd_run_jobs(app_state: &AppState) -> Result<()> {
    app_state.execute_regular_jobs().await;
    Ok(())
}

pub async fn cmd_reclaim(
    app_state: &AppState,
    name: &str,
    id: usize,
    tx_id: &str,
) -> Result<String> {
    let mut res = String::new();
    let reclaimed = app_state.wallet_reclaim_tx(id, tx_id).await?;

    push_break(&mut res);
    push_break(&mut res);
    res.push_str(&format!(
        "Reclaim Funds for {name}, Tx: {tx_id} - Wallet ID: {id} - Reclaimed: {reclaimed}.\n"
    ));
    Ok(res)
}

pub async fn cmd_recover_stale(app_state: &AppState, name: &str, id: usize) -> Result<String> {
    let mut res = String::new();
    let recovered = app_state.wallet_recover_pending_stale_proofs(id).await?;

    push_break(&mut res);
    push_break(&mut res);
    res.push_str(&format!(
        "Recover Stale Proofs Funds for {name} - Wallet ID: {id} - Recovered: {recovered}.\n"
    ));
    Ok(res)
}

pub async fn cmd_melt(
    app_state: &AppState,
    name: &str,
    id: usize,
    amount: u64,
    address: &str,
    description: &Option<String>,
) -> Result<String> {
    let mut res = String::new();
    let melt_summary = app_state
        .wallet_prepare_melt(id, amount, address.to_owned(), description.to_owned())
        .await?;

    info!(
        "Melt Summary: Amount: {}, Unit: {}",
        &melt_summary.amount, &melt_summary.unit
    );

    let tx_id = app_state
        .wallet_melt(id, melt_summary.request_id.to_string())
        .await?;

    push_break(&mut res);
    push_break(&mut res);
    res.push_str(&format!(
        "Melt for {name}, Amount: {amount}, Address: {address} - Wallet ID: {id}.\n"
    ));
    push_break(&mut res);
    res.push_str(&format!("Transaction ID: {tx_id}"));

    Ok(res)
}

pub async fn cmd_mint(app_state: &AppState, name: &str, id: usize, amount: u64) -> Result<String> {
    let mut res = String::new();

    let mint_summary = app_state.wallet_mint(id, amount).await?;

    push_break(&mut res);
    push_break(&mut res);
    res.push_str(&format!(
        "Mint for {name}, Amount: {amount} - Wallet ID: {id}.\n"
    ));
    push_break(&mut res);
    res.push_str(&format!(
        "Mint Summary - Pay {amount} to address {}",
        mint_summary.address.assume_checked()
    ));

    Ok(res)
}

pub async fn cmd_protest_mint(
    app_state: &AppState,
    name: &str,
    id: usize,
    quote_id: &str,
) -> Result<String> {
    let mut res = String::new();

    let (status, amount) = app_state
        .wallet_protest_mint(id, quote_id.to_owned())
        .await?;

    push_break(&mut res);
    push_break(&mut res);
    res.push_str(&format!(
        "Protest Mint for {name}, Quote ID: {quote_id} - Wallet ID: {id}.\n"
    ));
    push_break(&mut res);
    match status {
        bcr_common::wire::common::ProtestStatus::Resolved => match amount {
            Some(amount) => {
                res.push_str(&format!("Protest Resolved - Received {amount}"));
            }
            None => {
                res.push_str("Protest Resolved - Warning: no amount returned despite resolution");
            }
        },
        bcr_common::wire::common::ProtestStatus::Rabid => {
            res.push_str("Protest returned Rabid - mint declared rabid by betas");
        }
    }

    Ok(res)
}

pub async fn cmd_protest_swap(
    app_state: &AppState,
    name: &str,
    id: usize,
    commitment_sig: &str,
) -> Result<String> {
    let mut res = String::new();

    let (status, amount) = app_state
        .wallet_protest_swap(id, commitment_sig.to_owned())
        .await?;

    push_break(&mut res);
    push_break(&mut res);
    res.push_str(&format!(
        "Protest Swap for {name}, Commitment: {commitment_sig} - Wallet ID: {id}.\n"
    ));
    push_break(&mut res);
    match status {
        bcr_common::wire::common::ProtestStatus::Resolved => match amount {
            Some(amount) => {
                res.push_str(&format!("Protest Resolved - Received {amount}"));
            }
            None => {
                res.push_str("Protest Resolved - Warning: no amount returned despite resolution");
            }
        },
        bcr_common::wire::common::ProtestStatus::Rabid => {
            res.push_str("Protest returned Rabid - mint declared rabid by betas");
        }
    }

    Ok(res)
}

pub async fn cmd_protest_melt(
    app_state: &AppState,
    name: &str,
    id: usize,
    quote_id: &str,
) -> Result<String> {
    let mut res = String::new();

    let (status, amount) = app_state
        .wallet_protest_melt(id, quote_id.to_owned())
        .await?;

    push_break(&mut res);
    push_break(&mut res);
    res.push_str(&format!(
        "Protest Melt for {name}, Quote ID: {quote_id} - Wallet ID: {id}.\n"
    ));
    push_break(&mut res);
    match status {
        bcr_common::wire::common::ProtestStatus::Resolved => {
            res.push_str("Protest Resolved");
            if let Some(amount) = amount {
                res.push_str(&format!(", amount: {amount}"));
            }
        }
        bcr_common::wire::common::ProtestStatus::Rabid => {
            res.push_str("Protest returned Rabid - mint declared rabid by betas");
        }
    }

    Ok(res)
}

pub async fn cmd_migrate_rabid(app_state: &AppState, name: &str) -> Result<String> {
    let mut res = String::new();

    let migrated = app_state.purse_migrate_rabid().await?;

    push_break(&mut res);
    push_break(&mut res);
    res.push_str(&format!("Migrate Rabid for {name}:\n"));
    push_break(&mut res);
    if migrated.is_empty() {
        res.push_str("Nothing migrated.\n");
    } else {
        for (k, v) in migrated.iter() {
            res.push_str(&format!("Migrated Wallet {} to {}.\n", k, v));
        }
    }
    Ok(res)
}

fn push_line(res: &mut String) {
    res.push_str("-----------------------\n");
}

fn push_break(res: &mut String) {
    res.push('\n');
}

fn format_timestamp(ts: u64) -> String {
    let datetime: DateTime<Utc> = DateTime::from_timestamp(ts as i64, 0).expect("valid timestamp");

    datetime.format("%Y-%m-%d %H:%M:%S").to_string()
}
