use anyhow::Result;
use bcr_wallet_core::AppState;
use chrono::{DateTime, Utc};
use tracing::info;

pub async fn cmd_info(app_state: &AppState) -> Result<String> {
    let mut res = String::new();
    let wallet_ids = app_state.get_wallets_ids().await?;

    push_break(&mut res);
    push_break(&mut res);
    res.push_str(&format!("{} Wallet(s) found.\n", wallet_ids.len()));
    push_line(&mut res);

    for id in wallet_ids.iter() {
        let name = app_state.get_wallet_name(*id).await?;
        let mint_url = app_state.get_wallet_mint_url(*id).await?;
        let unit = app_state.get_wallet_currency_unit(*id).await?;
        let balance = app_state.get_wallet_balance(*id).await?;

        let redemptions = app_state
            .wallet_list_redemptions(*id, std::time::Duration::from_hours(48))
            .await?;

        let transactions = app_state.wallet_list_txs(*id).await?;

        res.push_str(&format!("Name: {name}\n"));
        res.push_str(&format!("Wallet ID: {id}\n"));
        res.push_str(&format!("Mint URL: {mint_url}\n"));
        res.push_str(&format!(
            "Credit Balance: {} {}\n",
            balance.credit, unit.credit
        ));
        res.push_str(&format!(
            "Debit Balance: {} {}\n",
            balance.debit, unit.debit
        ));
        if !redemptions.is_empty() {
            res.push_str("Redemptions plan:");
            for r in redemptions.iter() {
                res.push_str(&format!(
                    "\t\t{} ({}) - {}",
                    format_timestamp(r.tstamp),
                    r.tstamp,
                    r.amount
                ));
            }
            push_break(&mut res);
        }
        if !transactions.is_empty() {
            res.push_str("Transactions:");
            push_break(&mut res);

            for (idx, tx) in transactions.iter().enumerate() {
                let quote_or_btc_tx_id = match (tx.btc_tx_id, &tx.quote_id) {
                    (Some(_), Some(_)) => String::default(),
                    (Some(btc_tx_id), None) => btc_tx_id.to_string(),
                    (None, Some(quote_id)) => quote_id.to_string(),
                    (None, None) => String::default(),
                };
                res.push_str(&format!(
                    "\t\tId: {} \t Amount: {} {} \t Fees: {}  \t Status: {:?} \t {} \tType: {:<10} \t {:?} \t Memo: {} \t BTC TxID/Quote ID: {}",
                    tx.id, tx.amount, tx.unit, tx.fees,  tx.status, format_timestamp(tx.tstamp), &format!("{:?}", tx.ptype), tx.direction, tx.memo, quote_or_btc_tx_id 
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
    let id = app_state.add_wallet(name.to_owned()).await?;
    push_break(&mut res);
    push_break(&mut res);
    res.push_str(&format!("Created Wallet for {name} - Wallet ID: {id}.\n"));
    Ok(res)
}

pub async fn cmd_delete_wallet(app_state: &AppState, name: &str, id: usize) -> Result<String> {
    let mut res = String::new();
    app_state.delete_wallet(id).await?;
    push_break(&mut res);
    push_break(&mut res);
    res.push_str(&format!("Deleted Wallet for {name} - Wallet ID: {id}.\n"));
    Ok(res)
}

pub async fn cmd_restore_wallet(app_state: &AppState, name: &str) -> Result<String> {
    let mut res = String::new();
    let id = app_state.restore_wallet(name.to_owned()).await?;
    push_break(&mut res);
    push_break(&mut res);
    res.push_str(&format!("Restored Wallet for {name} - Wallet ID: {id}.\n"));
    Ok(res)
}

pub async fn cmd_clear_wallet(app_state: &AppState, name: &str, id: usize) -> Result<String> {
    let mut res = String::new();
    app_state.wallet_clean_local_db(id).await?;
    push_break(&mut res);
    push_break(&mut res);
    res.push_str(&format!("Cleared Wallet for {name} - Wallet ID: {id}.\n"));
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
    push_break(&mut res);
    push_break(&mut res);
    res.push_str(&format!(
        "Received token {token}, returned {swapped} for {name} - Wallet ID: {id}.\n"
    ));
    Ok(res)
}

pub async fn cmd_redeem(app_state: &AppState, name: &str, id: usize) -> Result<String> {
    let mut res = String::new();
    let redeemed = app_state.wallet_redeem_credit(id).await?;
    push_break(&mut res);
    push_break(&mut res);
    res.push_str(&format!(
        "Redeemed {redeemed} for {name} - Wallet ID: {id}.\n"
    ));
    Ok(res)
}

pub async fn cmd_request_payment(
    app_state: &AppState,
    name: &str,
    amount: u64,
    unit: &str,
    id: usize,
    description: Option<String>,
) -> Result<String> {
    let mut res = String::new();
    let req = app_state
        .wallet_prepare_payment_request(id, amount, unit.to_string(), description)
        .await?;

    info!("Payment Request: {}, {}", &req.request, &req.p_id);
    let tx_id = app_state
        .wallet_check_received_payment(2, 60, 1, req.p_id.clone())
        .await?;

    push_break(&mut res);
    push_break(&mut res);
    res.push_str(&format!(
        "Request Payment for {name}, Amount: {amount} - Wallet ID: {id}.\n"
    ));
    push_break(&mut res);
    res.push_str(&format!(
        "Payment request: {}, p_id: {}",
        &req.request, &req.p_id
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
    unit: &str,
    description: Option<String>,
) -> Result<String> {
    let mut res = String::new();
    let payment_summary = app_state
        .wallet_prepare_pay_by_token(id, amount, unit.to_string(), description)
        .await?;

    info!(
        "Payment Summary: Amount: {}, Unit: {}",
        &payment_summary.amount, &payment_summary.unit,
    );
    let result = app_state
        .wallet_pay_by_token(payment_summary.request_id.to_string())
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
        .wallet_pay(payment_summary.request_id.to_string())
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
    app_state.execute_daily_jobs().await;
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
        .wallet_melt(melt_summary.request_id.to_string())
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
