use crate::WalletSettings;
use anyhow::Result;
use bcr_common::cashu;
use bcr_wallet_api::{AppState, config::CreateWalletConfig};
use bcr_wallet_core::types::{
    PaymentRequestDirection, PaymentResultCallback, PendingPaymentSubscriptionCallback,
    TransactionFees, TransactionFilters, TransactionSort,
};
use chrono::{DateTime, Utc};
use std::sync::Arc;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tracing::info;
use uuid::Uuid;

pub async fn cmd_info(app_state: &AppState) -> Result<String> {
    let mut res = String::new();
    let wallet_ids = app_state.purse_wallets_ids().await?;

    push_break(&mut res);
    push_break(&mut res);
    res.push_str(&format!("{} Wallet(s) found.\n", wallet_ids.len()));
    push_line(&mut res);
    app_state.set_dev_mode(true);

    for id in wallet_ids.iter() {
        let info = app_state.wallet_info(id.clone()).await?;
        let unit = app_state.wallet_currency_unit(id.clone()).await?.unit;
        let balance = app_state.wallet_balance(id.clone()).await?;
        let dev_mode_detailed_balance = app_state
            .wallet_dev_mode_detailed_balance(id.clone())
            .await?;

        let mut transactions = vec![];
        let mut cursor = None;
        loop {
            let res = app_state
                .wallet_list_txs(
                    id.clone(),
                    TransactionFilters {
                        ..Default::default()
                    },
                    TransactionSort::TimeDesc,
                    20,
                    cursor,
                )
                .await?;

            transactions.extend(res.txs);

            if res.next_cursor.is_none() {
                break;
            }

            cursor = res.next_cursor;
        }

        res.push_str(&format!("Name: {}\n", info.name));
        res.push_str(&format!("NodeId: {}\n", info.node_id));
        res.push_str(&format!("Wallet ID: {id}\n"));
        res.push_str(&format!("Mint URL: {}\n", info.default_mint_url));
        res.push_str(&format!("Network: {}\n", info.network));
        res.push_str(&format!("Debit Balance: {} {}\n", balance.debit, unit));
        res.push_str(&format!("Credit Balance: {} {}\n", balance.credit, unit));
        res.push_str(&format!("Total Balance: {} {}\n", balance.total, unit));

        if !dev_mode_detailed_balance.is_empty() {
            res.push_str("Dev Mode Detailed Balance:");
            push_break(&mut res);
            for entry in dev_mode_detailed_balance.iter() {
                res.push_str(&format!(
                    "\t\tId: {} \t Amount: {} \t Expiry: {}",
                    entry.kid,
                    entry.amount,
                    if let Some(exp) = entry.final_expiry {
                        format_timestamp(exp)
                    } else {
                        "None".to_owned()
                    },
                ));
                push_break(&mut res);
            }
        }

        if !transactions.is_empty() {
            res.push_str(&format!("Transactions ({}):", transactions.len()));
            push_break(&mut res);

            for tx in transactions.iter() {
                let quote_or_btc_tx_id = match (tx.btc_tx_id, &tx.quote_id) {
                    (Some(txid), _) => txid.to_string(),
                    (None, Some(quote_id)) => quote_id.to_string(),
                    (None, None) => String::default(),
                };
                let tx_links = tx
                    .linked_txs
                    .iter()
                    .map(|lnk| format!("{} - {}", lnk.reason, lnk.tx_id))
                    .collect::<Vec<_>>()
                    .join(", ");
                res.push_str(&format!(
                    "\t\tId: {} \t Amount: {:8} {} \t {}  \t Status: {:?} \t {} \tType: {:<10} \t {:?} \t Memo: {} \t BTC TxID/Quote ID: {} \t Contact: {} \t Payment Request ID: {} \t Links: {}",
                    tx.id, tx.amount, tx.unit, format_fees(tx.fees),  tx.status, format_timestamp(tx.tstamp), format!("{:?}", tx.payment_type), tx.direction, tx.memo.clone().unwrap_or_default(), quote_or_btc_tx_id, tx.contact_node_id.as_ref().map(|n| n.to_string()).unwrap_or_default(), tx.payment_request_id.map(|id| id.to_string()).unwrap_or_default(), tx_links
                ));
                push_break(&mut res);
            }
        }
        push_break(&mut res);
    }
    Ok(res)
}

pub async fn cmd_status(app_state: &AppState) -> Result<String> {
    let mut res = String::new();
    // wait until nostr is connected
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    let nostr_connected = app_state.purse_wallets_nostr_connected().await;
    push_break(&mut res);
    for (wid, connected) in nostr_connected {
        res.push_str(&format!("Wallet {} connected: {}", wid, connected));
        push_break(&mut res);
    }
    Ok(res)
}

pub async fn cmd_add_wallet(
    app_state: &AppState,
    name: &str,
    settings: &WalletSettings,
) -> Result<String> {
    let mut res = String::new();
    let wallet_ids = app_state.purse_wallets_ids().await?;
    let cfg = CreateWalletConfig {
        name: format!("{name}{}", wallet_ids.len()),
        network: settings.network,
        nostr_relays: settings.nostr_relays.clone(),
        mnemonic: settings.mnemonic.clone(),
        default_mint_url: settings.mint_url.clone(),
    };
    let id = app_state.purse_add_wallet(cfg).await?;
    push_break(&mut res);
    push_break(&mut res);
    res.push_str(&format!("Created Wallet for {name} - Wallet ID: {id}.\n"));
    Ok(res)
}

pub async fn cmd_delete_wallet(app_state: &AppState, name: &str, id: &str) -> Result<String> {
    let mut res = String::new();
    app_state.purse_delete_wallet(id.to_owned()).await?;
    push_break(&mut res);
    push_break(&mut res);
    res.push_str(&format!("Deleted Wallet for {name} - Wallet ID: {id}.\n"));
    Ok(res)
}

pub async fn cmd_restore_wallet(
    app_state: &AppState,
    name: &str,
    settings: &WalletSettings,
) -> Result<String> {
    let mut res = String::new();
    let wallet_ids = app_state.purse_wallets_ids().await?;
    let cfg = CreateWalletConfig {
        name: format!("{name}{}", wallet_ids.len()),
        network: settings.network,
        nostr_relays: settings.nostr_relays.clone(),
        mnemonic: settings.mnemonic.clone(),
        default_mint_url: settings.mint_url.clone(),
    };
    let id = app_state.purse_restore_wallet(cfg).await?;
    push_break(&mut res);
    push_break(&mut res);
    res.push_str(&format!("Restored Wallet for {name} - Wallet ID: {id}.\n"));
    Ok(res)
}

pub async fn cmd_receive(
    app_state: &AppState,
    name: &str,
    token: &str,
    id: &str,
) -> Result<String> {
    let mut res = String::new();
    let swapped = app_state
        .wallet_receive_token(id.to_owned(), token.to_owned())
        .await?;
    let tx = app_state
        .wallet_load_tx(id.to_owned(), &swapped.to_string())
        .await?;
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
    id: &str,
    description: Option<String>,
) -> Result<String> {
    let req = app_state
        .wallet_prepare_payment_request(id.to_owned(), amount, description)
        .await?;
    info!("Payment Request: {}, {}", &req.request, &req.p_id);

    let cancel_token = CancellationToken::new();
    // Uncomment to test cancellation
    // let cancel_token_clone = cancel_token.clone();
    // tokio::spawn(async move {
    //     tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
    //     cancel_token_clone.cancel();
    // });
    let (tx, rx) = oneshot::channel::<Option<Uuid>>();

    let tx = Arc::new(std::sync::Mutex::new(Some(tx)));

    let res_cb: PaymentResultCallback = Arc::new(move |tx_id| {
        if let Some(sender) = tx.lock().unwrap().take() {
            let _ = sender.send(tx_id);
        }
    });

    app_state
        .wallet_check_received_payment(id.to_owned(), 60, req.p_id.clone(), cancel_token, res_cb)
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
    id: &str,
    amount: u64,
    description: Option<String>,
) -> Result<String> {
    let mut res = String::new();
    let payment_summary = app_state
        .wallet_prepare_pay_by_token(id.to_owned(), amount, description)
        .await?;

    info!(
        "Payment Summary: Amount: {}, Unit: {}, {}",
        payment_summary.amount,
        payment_summary.unit,
        format_fees(payment_summary.fees),
    );
    let result = app_state
        .wallet_pay_by_token(id.to_owned(), payment_summary.request_id.to_string())
        .await?;

    push_break(&mut res);
    push_break(&mut res);
    res.push_str(&format!("Pay by Token for {name}, Wallet ID: {id}.\n"));
    push_break(&mut res);
    res.push_str(&format!("Payment Summary: {}", payment_summary.request_id));
    res.push_str(&format!(
        "Unit: {}, Amount: {}, {}",
        payment_summary.unit,
        payment_summary.amount,
        format_fees(payment_summary.fees)
    ));
    push_break(&mut res);
    res.push_str(&format!("Transaction ID: {}", result.tx_id));
    push_break(&mut res);
    push_break(&mut res);
    res.push_str(&format!("Token: {}", result.token));

    Ok(res)
}

pub async fn cmd_pay_to_contact(
    app_state: &AppState,
    name: &str,
    id: &str,
    contact_id: &str,
    amount: u64,
    description: Option<String>,
) -> Result<String> {
    let mut res = String::new();
    let payment_summary = app_state
        .wallet_prepare_pay_to_contact(id.to_owned(), contact_id.to_owned(), amount, description)
        .await?;

    info!(
        "Payment Summary: Amount: {}, Unit: {}, {}",
        payment_summary.amount,
        payment_summary.unit,
        format_fees(payment_summary.fees),
    );
    let result = app_state
        .wallet_pay_to_contact(id.to_owned(), payment_summary.request_id.to_string())
        .await?;

    push_break(&mut res);
    push_break(&mut res);
    res.push_str(&format!(
        "Pay to Contact {contact_id} for {name}, Wallet ID: {id}.\n"
    ));
    push_break(&mut res);
    res.push_str(&format!("Payment Summary: {}", payment_summary.request_id));
    res.push_str(&format!(
        "Unit: {}, Amount: {}, {}",
        payment_summary.unit,
        payment_summary.amount,
        format_fees(payment_summary.fees)
    ));
    push_break(&mut res);
    res.push_str(&format!("Transaction ID: {}", result));

    Ok(res)
}

pub async fn cmd_send_payment(
    app_state: &AppState,
    name: &str,
    input: &str,
    id: &str,
) -> Result<String> {
    let mut res = String::new();
    let payment_summary = app_state
        .wallet_prepare_cdk18_payment(id.to_owned(), input.to_owned())
        .await?;

    info!(
        "Payment Summary: Amount: {}, Unit: {}, {}",
        payment_summary.amount,
        payment_summary.unit,
        format_fees(payment_summary.fees),
    );

    let tx_id = app_state
        .wallet_pay(id.to_owned(), payment_summary.request_id.to_string())
        .await?;

    push_break(&mut res);
    push_break(&mut res);
    res.push_str(&format!(
        "Send Payment for {name}, Input: {input} - Wallet ID: {id}.\n"
    ));
    push_break(&mut res);
    res.push_str(&format!("Payment Summary: {}", payment_summary.request_id));
    res.push_str(&format!(
        "Unit: {}, Amount: {}, {}",
        payment_summary.unit,
        payment_summary.amount,
        format_fees(payment_summary.fees)
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
    id: &str,
    tx_id: &str,
) -> Result<String> {
    let mut res = String::new();
    let reclaimed = app_state.wallet_reclaim_tx(id.to_owned(), tx_id).await?;

    push_break(&mut res);
    push_break(&mut res);
    res.push_str(&format!(
        "Reclaim Funds for {name}, Tx: {tx_id} - Wallet ID: {id} - Reclaimed: {reclaimed}.\n"
    ));
    Ok(res)
}

pub async fn cmd_recover_stale(app_state: &AppState, name: &str, id: &str) -> Result<String> {
    let mut res = String::new();
    let recovered = app_state
        .wallet_recover_pending_stale_proofs(id.to_owned())
        .await?;

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
    id: &str,
    amount: u64,
    address: &str,
    description: &Option<String>,
) -> Result<String> {
    let mut res = String::new();
    let melt_estimate = app_state
        .wallet_estimate_melt(id.to_owned(), amount)
        .await?;
    info!("Melt Estimate for Amount: {}", amount,);
    let selected_network_fee =
        melt_estimate.tx_vsize * melt_estimate.fee_rates[0].sat_per_vb as u64;
    info!(
        "Tx Size: {}, Melt Fee: {}, FeeRates: {:?}",
        melt_estimate.tx_vsize, melt_estimate.melt_fee, melt_estimate.fee_rates,
    );
    info!("Using {selected_network_fee} sat as network fee");
    let melt_summary = app_state
        .wallet_prepare_melt(
            id.to_owned(),
            amount,
            selected_network_fee,
            melt_estimate.melt_fee,
            address.to_owned(),
            description.to_owned(),
        )
        .await?;

    info!(
        "Melt Summary: Amount: {}, Unit: {}, {}",
        &melt_summary.amount,
        &melt_summary.unit,
        &format_fees(melt_summary.fees)
    );

    let tx_id = app_state
        .wallet_melt(id.to_owned(), melt_summary.request_id.to_string())
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

pub async fn cmd_mint(app_state: &AppState, name: &str, id: &str, amount: u64) -> Result<String> {
    let mut res = String::new();

    let mint_summary = app_state.wallet_mint(id.to_owned(), amount).await?;

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
    id: &str,
    quote_id: &str,
) -> Result<String> {
    let mut res = String::new();

    let (status, amount) = app_state
        .wallet_protest_mint(id.to_owned(), quote_id.to_owned())
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
        bcr_common::wire::common::ProtestStatus::Offline => {
            res.push_str("Protest returned Offline - mint declared offline by betas");
        }
    }

    Ok(res)
}

pub async fn cmd_protest_swap(
    app_state: &AppState,
    name: &str,
    id: &str,
    commitment_sig: &str,
) -> Result<String> {
    let mut res = String::new();

    let (status, amount) = app_state
        .wallet_protest_swap(id.to_owned(), commitment_sig.to_owned())
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
        bcr_common::wire::common::ProtestStatus::Offline => {
            res.push_str("Protest returned Offline - mint declared offline by betas");
        }
    }

    Ok(res)
}

pub async fn cmd_protest_melt(
    app_state: &AppState,
    name: &str,
    id: &str,
    quote_id: &str,
) -> Result<String> {
    let mut res = String::new();

    let (status, amount) = app_state
        .wallet_protest_melt(id.to_owned(), quote_id.to_owned())
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
        bcr_common::wire::common::ProtestStatus::Offline => {
            res.push_str("Protest returned Offline - mint declared offline by betas");
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

pub async fn cmd_edit_tx_memo(
    app_state: &AppState,
    name: &str,
    id: &str,
    tx_id: &str,
    new_memo: &Option<String>,
) -> Result<String> {
    let mut res = String::new();
    app_state
        .wallet_edit_tx_memo(id.to_owned(), tx_id.to_owned(), new_memo.to_owned())
        .await?;
    push_break(&mut res);
    push_break(&mut res);
    res.push_str(&format!("Edited Memo for Tx {tx_id} for {name}:\n"));
    push_break(&mut res);
    Ok(res)
}

pub async fn cmd_add_contact(
    app_state: &AppState,
    name: &str,
    network: bitcoin::Network,
    contact_name: &str,
    contact_node_id: &str,
) -> Result<String> {
    let mut res = String::new();
    let contact_id = app_state
        .purse_add_contact(
            network.to_string(),
            Some(contact_node_id.to_owned()),
            None,
            Some(contact_name.to_owned()),
            None,
        )
        .await?;
    push_break(&mut res);
    push_break(&mut res);
    res.push_str(&format!("Added Contact for {contact_id} for {name}:\n"));
    push_break(&mut res);
    Ok(res)
}

pub async fn cmd_edit_contact(
    app_state: &AppState,
    name: &str,
    network: bitcoin::Network,
    contact_id: &str,
    contact_node_id: &str,
    contact_email: &str,
    contact_name: &str,
    contact_company: &str,
) -> Result<String> {
    let mut res = String::new();
    app_state
        .purse_edit_contact(
            network.to_string(),
            contact_id.to_owned(),
            if contact_node_id == "None" {
                None
            } else {
                Some(contact_node_id.to_owned())
            },
            if contact_email == "None" {
                None
            } else {
                Some(contact_email.to_owned())
            },
            if contact_name == "None" {
                None
            } else {
                Some(contact_name.to_owned())
            },
            if contact_company == "None" {
                None
            } else {
                Some(contact_company.to_owned())
            },
        )
        .await?;
    push_break(&mut res);
    push_break(&mut res);
    res.push_str(&format!("Edited Contact for {contact_id} for {name}:\n"));
    push_break(&mut res);
    Ok(res)
}

pub async fn cmd_delete_contact(
    app_state: &AppState,
    name: &str,
    network: bitcoin::Network,
    contact_id: &str,
) -> Result<String> {
    let mut res = String::new();
    app_state
        .purse_delete_contact(network.to_string(), contact_id.to_owned())
        .await?;
    push_break(&mut res);
    push_break(&mut res);
    res.push_str(&format!("Deleted Contact for {contact_id} for {name}:\n"));
    push_break(&mut res);
    Ok(res)
}

pub async fn cmd_get_contact(
    app_state: &AppState,
    name: &str,
    network: bitcoin::Network,
    contact_id: &str,
) -> Result<String> {
    let mut res = String::new();
    let contact = app_state
        .purse_get_contact(network.to_string(), contact_id.to_owned())
        .await?;
    push_break(&mut res);
    push_break(&mut res);
    res.push_str(&format!(
        "Contact for contact_id: {contact_id} for {name}:\n"
    ));
    res.push_str(&format!(
        "ContactId: {} Name: {:?}, Node_id: {}, Email: {:?}, Company: {:?}\n",
        contact.id,
        contact.name,
        contact.node_id.map(|n| n.to_string()).unwrap_or_default(),
        contact.email,
        contact.company
    ));
    push_break(&mut res);
    Ok(res)
}

pub async fn cmd_list_contacts(
    app_state: &AppState,
    name: &str,
    network: bitcoin::Network,
    search_term: &Option<String>,
) -> Result<String> {
    let mut res = String::new();
    let contacts = app_state
        .purse_list_contacts(network.to_string(), search_term.to_owned())
        .await?;
    push_break(&mut res);
    push_break(&mut res);
    res.push_str(&format!(
        "Contacts for search_term: {search_term:?} for {name}:\n"
    ));
    push_break(&mut res);
    for c in contacts {
        res.push_str(&format!(
            "ContactId: {} Name: {:?}, Node_id: {}, Email: {:?}, Company: {:?}\n",
            c.id,
            c.name,
            c.node_id.map(|n| n.to_string()).unwrap_or_default(),
            c.email,
            c.company
        ));
        push_break(&mut res);
    }
    push_break(&mut res);
    Ok(res)
}

pub async fn cmd_request_payment_from_contact(
    app_state: &AppState,
    name: &str,
    id: &str,
    contact_id: &str,
    amount: u64,
) -> Result<String> {
    let mut res = String::new();
    app_state
        .wallet_request_payment_from_contact(
            id.to_owned(),
            contact_id.to_owned(),
            amount,
            None,
            None,
        )
        .await?;
    push_break(&mut res);
    push_break(&mut res);
    res.push_str(&format!(
        "Request Payment over {amount} from Contact {contact_id} for {name}:\n"
    ));
    push_break(&mut res);
    Ok(res)
}

pub async fn cmd_subscribe_to_prs(app_state: &AppState, _name: &str, id: &str) -> Result<String> {
    let mut res = String::new();
    let cancel_token = CancellationToken::new();
    let cancel_token_clone = cancel_token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;
        cancel_token_clone.cancel();
    });

    let (tx, rx) = oneshot::channel::<Uuid>();

    let tx = Arc::new(std::sync::Mutex::new(Some(tx)));

    let res_cb: PendingPaymentSubscriptionCallback = Arc::new(move |id| {
        if let Some(sender) = tx.lock().unwrap().take() {
            let _ = sender.send(id);
        }
    });

    app_state
        .wallet_subscribe_to_payment_requests(id.to_owned(), cancel_token, res_cb)
        .await?;

    let Ok(id) = rx.await else {
        return Ok("Cancelled".to_string());
    };
    push_break(&mut res);
    push_break(&mut res);
    res.push_str(&format!("ID: {id}"));
    push_break(&mut res);
    Ok(res)
}

pub async fn cmd_list_prs(app_state: &AppState, name: &str, id: &str) -> Result<String> {
    let mut res = String::new();
    let incoming_pprs = app_state
        .wallet_list_payment_requests(id.to_owned(), PaymentRequestDirection::Incoming, vec![])
        .await?;
    let outgoing_pprs = app_state
        .wallet_list_payment_requests(id.to_owned(), PaymentRequestDirection::Outgoing, vec![])
        .await?;
    push_break(&mut res);
    push_break(&mut res);
    res.push_str(&format!("Incoming Payment Requests for {name}:\n"));
    push_break(&mut res);
    for ppr in incoming_pprs {
        res.push_str(&format!(
            "Id: {}, NodeId: {}, Amount: {}, Direction: {:?}, State: {:?}\n",
            ppr.id, ppr.node_id, ppr.amount, ppr.direction, ppr.state
        ));
        push_break(&mut res);
    }
    push_break(&mut res);
    res.push_str(&format!("Outgoing Payment Requests for {name}:\n"));
    push_break(&mut res);
    for ppr in outgoing_pprs {
        res.push_str(&format!(
            "Id: {}, NodeId: {}, Amount: {}, Direction: {:?}, State: {:?}\n",
            ppr.id, ppr.node_id, ppr.amount, ppr.direction, ppr.state
        ));
        push_break(&mut res);
    }
    push_break(&mut res);
    Ok(res)
}

pub async fn cmd_get_pr(
    app_state: &AppState,
    name: &str,
    id: &str,
    payment_req_id: &str,
) -> Result<String> {
    let mut res = String::new();
    let ppr = app_state
        .wallet_get_payment_request(id.to_owned(), payment_req_id.to_owned())
        .await?;
    push_break(&mut res);
    push_break(&mut res);
    res.push_str(&format!(
        "Pending Payment Request for {payment_req_id} for {name}:\n"
    ));
    push_break(&mut res);
    res.push_str(&format!(
        "Id: {} NodeId: {} Amount: {}\n",
        ppr.id, ppr.node_id, ppr.amount
    ));
    push_break(&mut res);
    push_break(&mut res);
    Ok(res)
}

pub async fn cmd_pay_pr(
    app_state: &AppState,
    name: &str,
    id: &str,
    payment_req_id: &str,
) -> Result<String> {
    let mut res = String::new();
    let payment_summary = app_state
        .wallet_prepare_pay_payment_request(id.to_owned(), payment_req_id.to_owned())
        .await?;
    info!(
        "Payment Summary: Amount: {}, Unit: {}, {}",
        payment_summary.amount,
        payment_summary.unit,
        format_fees(payment_summary.fees),
    );
    let result = app_state
        .wallet_pay_payment_request(id.to_owned(), payment_summary.request_id.to_string())
        .await?;
    push_break(&mut res);
    push_break(&mut res);
    res.push_str(&format!(
        "Pay Payment Request {payment_req_id} for {name}, Wallet ID: {id}.\n"
    ));
    res.push_str(&format!(
        "Unit: {}, Amount: {}, {}",
        payment_summary.unit,
        payment_summary.amount,
        format_fees(payment_summary.fees)
    ));
    push_break(&mut res);
    res.push_str(&format!("Transaction ID: {}", result));
    push_break(&mut res);
    Ok(res)
}

pub async fn cmd_reject_pr(
    app_state: &AppState,
    name: &str,
    id: &str,
    payment_req_id: &str,
) -> Result<String> {
    let mut res = String::new();
    app_state
        .wallet_reject_payment_request(id.to_owned(), payment_req_id.to_owned())
        .await?;
    push_break(&mut res);
    push_break(&mut res);
    res.push_str(&format!(
        "Reject Pending Payment Request for {payment_req_id} for {name}:\n"
    ));
    push_break(&mut res);
    Ok(res)
}

pub async fn cmd_cancel_pr(
    app_state: &AppState,
    name: &str,
    id: &str,
    payment_req_id: &str,
) -> Result<String> {
    let mut res = String::new();
    app_state
        .wallet_cancel_payment_request(id.to_owned(), payment_req_id.to_owned())
        .await?;
    push_break(&mut res);
    push_break(&mut res);
    res.push_str(&format!(
        "Cancel Pending Payment Request for {payment_req_id} for {name}:\n"
    ));
    push_break(&mut res);
    Ok(res)
}

pub async fn cmd_check_btc_tx(
    app_state: &AppState,
    name: &str,
    tx_id: &bitcoin::Txid,
    network: bitcoin::Network,
) -> Result<String> {
    let mut res = String::new();
    push_break(&mut res);
    res.push_str(&format!("Check Btc Tx for {tx_id} for {name}:\n"));
    let status = app_state
        .check_btc_tx_status(tx_id.to_string(), network.to_string())
        .await?;
    res.push_str(&format!(
        "Confirmations: {}, Fee: {} sat, Confirmed at: {}:\n",
        status.confirmations,
        status.fee.to_sat(),
        format_timestamp(status.confirmation_tstamp.unwrap_or(0))
    ));
    res.push_str("Receiver Addresses:\n");
    for receiver in status.receivers {
        res.push_str(&format!(
            "Address: {}, Amount: {} sat",
            receiver.address.assume_checked(),
            receiver.amount.to_sat()
        ));
        push_break(&mut res);
    }
    push_break(&mut res);
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

fn format_fees(fees: TransactionFees) -> String {
    let mut parts: Vec<String> = Vec::new();
    if fees.swap > cashu::Amount::ZERO {
        parts.push(format!("swap = {}", fees.swap));
    }
    if fees.network > cashu::Amount::ZERO {
        parts.push(format!("network = {}", fees.network));
    }
    if fees.melt > cashu::Amount::ZERO {
        parts.push(format!("melt = {}", fees.melt));
    }
    if parts.is_empty() {
        "Fees: 0".to_string()
    } else {
        format!("Fees: {}", parts.join(", "))
    }
}
