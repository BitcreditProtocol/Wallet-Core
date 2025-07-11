async function run() {
  const wasmModule = await import("./pkg/bcr_wallet_core.js");
  await wasmModule.default();

  await wasmModule.initialize_api("test");

  let update_wallets = async () => {
    let wallets = await wasmModule.get_wallets_names();

    document.getElementById("walletlist").innerHTML = wallets
      .map((name, idx) => `<option value="${idx}">${name}</option>`)
      .join("");

    await update_balance();
  };
  let update_balance = async () => {
    let ids = await wasmModule.get_wallets_ids();
    if (ids.length > 0) {
      let idx = Number(
        ids[document.getElementById("walletlist").selectedIndex],
      );
      let wallet_name = await wasmModule.get_wallet_name(idx);
      let debit_balance = await wasmModule.get_wallet_debit_balance(idx);
      let debit_unit = await wasmModule.get_wallet_debit_unit(idx);
      let credit_balance = await wasmModule.get_wallet_credit_balance(idx);
      let credit_unit = await wasmModule.get_wallet_credit_unit(idx);
      document.getElementById("balance").innerHTML =
        "Wallet: " + wallet_name + "\n\t" +
        String(credit_balance) + " " + credit_unit + "\n\t" +
        String(debit_balance) + " " + debit_unit;
    }
  };

  await update_wallets();

  document.getElementById("addbtn").addEventListener("click", async () => {
    //test
    let name = prompt("Enter wallet name");
    let mint_url = prompt("Enter mint url");
    let mnemonic = prompt("Enter mnemonic");

    await wasmModule.add_wallet(name, mint_url, mnemonic);
    await update_wallets();
  });

  document.getElementById("refreshbtn").addEventListener("click", async () => {
    await update_wallets();
  });

  document.getElementById("redeembtn").addEventListener("click", async () => {
    let ids = await wasmModule.get_wallets_ids();
    let idx = Number(ids[document.getElementById("walletlist").selectedIndex]);

    // let unit = await wasmModule.get_unit(idx);
    // if (unit.toLowerCase() == "crsat") {
    //   let token = await wasmModule.redeem_inactive(idx);
    //   await update_balance();
    //   document.getElementById("output").innerHTML += "\ntoken:\n" + token;
    // }
  });

  document.getElementById("importbtn").addEventListener("click", async () => {
    let ids = await wasmModule.get_wallets_ids();
    let idx = Number(ids[document.getElementById("walletlist").selectedIndex]);
    let token = prompt("Enter token");
    await wasmModule.wallet_receive_token(idx, token);

    await update_balance();
  });

  document.getElementById("exportbtn").addEventListener("click", async () => {
    let ids = await wasmModule.get_wallets_ids();
    let idx = Number(ids[document.getElementById("walletlist").selectedIndex]);
    let amount = Math.round(Number(prompt("Enter amount to send")));
    let summary = await wasmModule.wallet_prepare_send(idx, BigInt(amount), "");
    
    prompt("send summary, currency unit: " + summary.unit + ", total fees: " + String(summary.send_fees + summary.swap_fees));
    let token = await wasmModule.wallet_send(idx, summary.request_id);

    await update_balance();

    document.getElementById("output").innerHTML += "\ntoken:\n" + token;
  });

  document.getElementById("recheckbtn").addEventListener("click", async () => {
    let ids = await wasmModule.get_wallets_ids();
    let idx = Number(ids[document.getElementById("walletlist").selectedIndex]);
    // await wasmModule.recheck(idx);

    await update_balance();
  });

  document.getElementById("recoverbtn").addEventListener("click", async () => {
    let ids = await wasmModule.get_wallets_ids();
    let idx = Number(ids[document.getElementById("walletlist").selectedIndex]);
    // await wasmModule.recover(idx);

    await update_balance();
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
