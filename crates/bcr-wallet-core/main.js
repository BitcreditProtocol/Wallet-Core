async function run() {
  const wasmModule = await import("./pkg/bcr_wallet_core.js");
  await wasmModule.default();

  await wasmModule.initialize_api("testnet");

  let update_wallets = async () => {
    let wallets = await wasmModule.get_wallets_names();

    document.getElementById("walletlist").innerHTML = wallets
      .map((name, idx) => `<option value="${idx}">${name}</option>`)
      .join("");

    await update_balance();
  };

  let update_balance = async () => {
    let wallet_idx = get_wallet_idx();
    if (wallet_idx < 0) {
        return;
    }
    let wallet_name = await wasmModule.get_wallet_name(wallet_idx);
    let wallet_balance = await wasmModule.get_wallet_balance(wallet_idx);
    let wallet_unit = await wasmModule.get_wallet_currency_unit(wallet_idx);
    let wallet_redemptions = await wasmModule.wallet_list_redemptions(wallet_idx, 172800);
    let formatted = `Wallet: ${wallet_name}\n\t${wallet_balance.debit} ${wallet_unit.debit}\n\t${wallet_balance.credit} ${wallet_unit.credit}\n`;
    formatted += `\tRedemptions plan:\n`;
    for (let redemption of wallet_redemptions) {
        let expiry = new Date(redemption.tstamp * 1000);
        formatted += `\t\t${expiry} - ${redemption.amount}\n`;
    }
    document.getElementById("balance").innerHTML = formatted;
  };

  let format_past_txs = async () => {
    let wallet_idx = get_wallet_idx();
    if (wallet_idx < 0) {
      console.log("No wallet selected");
      return;
    }
    let past_tx_ids = await wasmModule.wallet_list_tx_ids(wallet_idx);
    document.getElementById("transactions").innerHTML = "Transactions:\n";
    for (let txid of past_tx_ids) {
      console.log("format_past_txs txid: " + txid);
      await format_tx(wallet_idx, txid);
    }
  }

  async function format_tx(idx, tx_id) {
    console.log("format_tx txid: " + tx_id);
    let tx = await wasmModule.wallet_load_tx(idx, tx_id);
    let tx_formatted = `\n ${tx_id}: ${tx.direction} ${tx.amount} ${tx.unit} (fees: ${tx.fees})`;
    document.getElementById("transactions").innerHTML += tx_formatted;
  }

  function get_wallet_idx() {
    let ids = wasmModule.get_wallets_ids();
    if (ids.length > 0) {
      return Number(ids[document.getElementById("walletlist").selectedIndex]);
    }
    return -1;
  }

  document.getElementById("addbtn").addEventListener("click", async () => {
    //test
    let name = prompt("Enter wallet name");
    let mint_url = prompt("Enter mint url");
    let mnemonic = prompt("Enter mnemonic");

    await wasmModule.add_wallet(name, mint_url, mnemonic);
    await update_wallets();
    await format_past_txs();
  });

  document.getElementById("refreshbtn").addEventListener("click", async () => {
    await update_wallets();
  });

  document.getElementById("redeembtn").addEventListener("click", async () => {
    let ids = await wasmModule.get_wallets_ids();
    let idx = Number(ids[document.getElementById("walletlist").selectedIndex]);

    let amount_redeemed = await wasmModule.wallet_redeem_credit(idx);
    console.log("amount redeemed: " + amount_redeemed);
    await update_wallets();
  });

  document.getElementById("importbtn").addEventListener("click", async () => {
    let ids = await wasmModule.get_wallets_ids();
    let idx = Number(ids[document.getElementById("walletlist").selectedIndex]);
    let token = prompt("Enter token");
    let now = Math.floor(Date.now() / 1000);
    let tx_id = await wasmModule.wallet_receive_token(idx, token, now);
    await format_tx(idx, tx_id);

    await update_balance();
  });

  document.getElementById("exportbtn").addEventListener("click", async () => {
    let ids = await wasmModule.get_wallets_ids();
    let idx = Number(ids[document.getElementById("walletlist").selectedIndex]);
    let amount = Math.round(Number(prompt("Enter amount to send")));
    let summary = await wasmModule.wallet_prepare_send(idx, BigInt(amount), "");
    let now = Math.floor(Date.now() / 1000);
    
    prompt(`send summary, currency unit: ${summary.unit} total fees: ${summary.send_fees + summary.swap_fees}`);
    let token_tx = await wasmModule.wallet_send(idx, summary.request_id, now);

    await update_balance();
    await format_tx(idx, token_tx.tx_id);

    document.getElementById("output").innerHTML += "\ntoken:\n" + token_tx.token;
  });

  document.getElementById("reclaimbtn").addEventListener("click", async () => {
    let ids = await wasmModule.get_wallets_ids();
    let idx = Number(ids[document.getElementById("walletlist").selectedIndex]);
    await wasmModule.wallet_reclaim_funds(idx);

    await update_balance();
  });

  document.getElementById("recoverbtn").addEventListener("click", async () => {
    let ids = await wasmModule.get_wallets_ids();
    let idx = Number(ids[document.getElementById("walletlist").selectedIndex]);
    // await wasmModule.recover(idx);

    await update_balance();
  });

  document.getElementById("cleanbtn").addEventListener("click", async () => {
    let ids = await wasmModule.get_wallets_ids();
    let idx = Number(ids[document.getElementById("walletlist").selectedIndex]);
    let proofs_removed = await wasmModule.wallet_clean_local_db(idx);
    console.log("proofs removed: " + proofs_removed);
  });

  document
    .getElementById("walletlist")
    .addEventListener("change", async (event) => {
      let ids = await wasmModule.get_wallets_ids();
      if (ids.length > 0) {
        let idx = Number(
          ids[document.getElementById("walletlist").selectedIndex],
        );

        let wallet_name = await wasmModule.get_wallet_name(idx);
        let wallet_url = await wasmModule.get_wallet_mint_url(idx);
        document.getElementById("walletname").innerHTML =
          "[" + wallet_name + "] " + String(idx) + " @ " + wallet_url + "  ";

        await update_balance();

        // let keyset_info = await wasmModule.list_keysets(idx);
        // document.getElementById("keyset").innerHTML = keyset_info;
      }
    });

  document.getElementById("walletlist").selectedIndex = 0;
  document.getElementById("walletlist").dispatchEvent(new Event("change"));
}

run().catch(console.error);
