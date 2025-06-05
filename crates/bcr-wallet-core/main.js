async function run() {
  const wasmModule = await import("./pkg/bcr_wallet_core.js");
  await wasmModule.default();

  await wasmModule.initialize_api();

  const wallet_name = wasmModule.get_wallet_name();
  document.getElementById("walletname").innerHTML = wallet_name;

  let update_wallets = async () => {
    // test
    let wallets = await wasmModule.get_wallets_names();
    let ids = await wasmModule.get_wallets_ids();
    console.log(ids);
    console.log(wallets);

    // set walletlist items to wallets
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
      let balance = await wasmModule.get_balance(idx);
      document.getElementById("balance").innerHTML = String(balance) + " crsat";

      let proofs = await wasmModule.print_proofs(idx);
      document.getElementById("output").innerHTML = proofs;
    }
  };

  await update_wallets();

  document.getElementById("addbtn").addEventListener("click", async () => {
    //test
    let name = prompt("Enter wallet name");
    let mint_url = prompt("Enter mint url");
    let mnemonic = prompt("Enter mnemonic");
    let unit = prompt("Enter unit");
    let credit = confirm("Is this a wildcat credit wallet ?");

    await wasmModule.add_wallet(name, mint_url, mnemonic, unit, credit);
    await update_wallets();
  });

  document.getElementById("refreshbtn").addEventListener("click", async () => {
    console.log("refresh");
    await update_wallets();
  });

  document.getElementById("importbtn").addEventListener("click", async () => {
    let ids = await wasmModule.get_wallets_ids();
    let idx = Number(ids[document.getElementById("walletlist").selectedIndex]);
    let token = prompt("Enter V3 token");
    await wasmModule.import_token(token, idx);

    await update_balance();
  });

  document.getElementById("exportbtn").addEventListener("click", async () => {
    let ids = await wasmModule.get_wallets_ids();
    let idx = Number(ids[document.getElementById("walletlist").selectedIndex]);
    let amount = Math.round(Number(prompt("Enter amount to send")));
    let token = await wasmModule.send(BigInt(amount), idx);

    await update_balance();

    document.getElementById("output").innerHTML += "\ntoken:\n" + token;
  });

  document.getElementById("recheckbtn").addEventListener("click", async () => {
    let ids = await wasmModule.get_wallets_ids();
    let idx = Number(ids[document.getElementById("walletlist").selectedIndex]);
    await wasmModule.recheck(idx);

    await update_balance();
  });

  document.getElementById("recoverbtn").addEventListener("click", async () => {
    let ids = await wasmModule.get_wallets_ids();
    let idx = Number(ids[document.getElementById("walletlist").selectedIndex]);
    await wasmModule.recover(idx);

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

        console.log("Selected wallet:", idx);

        let wallet_url = await wasmModule.get_wallet_url(idx);
        document.getElementById("walletname").innerHTML =
          "[" + wallet_name + "] " + String(idx) + " @ " + wallet_url + "  ";

        document.getElementById("balance").innerHTML = "0 crsat";
        document.getElementById("output").innerHTML = "";

        await update_balance();

        let keyset_info = await wasmModule.list_keysets(idx);
        document.getElementById("keyset").innerHTML = keyset_info;
      }
    });

  document.getElementById("walletlist").selectedIndex = 0;
  document.getElementById("walletlist").dispatchEvent(new Event("change"));
}

run().catch(console.error);
