use anyhow::Result;
use bcr_wallet_core::AppState;

use crate::WalletSettings;

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
                res.push_str(&format!("\t\t{} - {}", r.tstamp, r.amount));
            }
        }
    }
    Ok(res)
}

pub async fn cmd_add_wallet(
    app_state: &AppState,
    settings: &WalletSettings,
    name: &str,
) -> Result<String> {
    let mut res = String::new();
    let id = app_state
        .add_wallet(
            name.to_owned(),
            settings.mint_url.to_owned(),
            settings.mnemonic.to_owned(),
        )
        .await?;
    push_break(&mut res);
    push_break(&mut res);
    res.push_str(&format!("Created Wallet for {name} - Wallet ID: {id}.\n"));
    Ok(res)
}

pub async fn cmd_restore_wallet(
    app_state: &AppState,
    settings: &WalletSettings,
    name: &str,
) -> Result<String> {
    let mut res = String::new();
    let id = app_state
        .restore_wallet(
            name.to_owned(),
            settings.mint_url.to_owned(),
            settings.mnemonic.to_owned(),
        )
        .await?;
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
    id: usize,
) -> Result<String> {
    let mut res = String::new();
    let req = app_state
        .wallet_prepare_payment_request(id, amount, String::default(), String::default())
        .await?;

    let tx_id = app_state
        .wallet_check_received_payment(30, req.p_id.clone())
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
    res.push_str(&format!("Transaction ID: {tx_id:?}"));

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
    res.push_str(&format!("Transaction ID: {tx_id:?}"));

    Ok(res)
}

pub async fn cmd_run_jobs(app_state: &AppState) -> Result<()> {
    app_state.execute_jobs().await;
    Ok(())
}

fn push_line(res: &mut String) {
    res.push_str("-----------------------\n");
}

fn push_break(res: &mut String) {
    res.push('\n');
}
