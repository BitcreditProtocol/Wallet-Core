async function run() {
  const wasmModule = await import("./pkg/bcr_wallet_core.js");
  await wasmModule.default();

  await wasmModule.initialize_api();

  const wallet_name = wasmModule.get_wallet_name();
  document.getElementById("walletname").innerHTML = wallet_name;

  document.getElementById("importbtn").addEventListener("click", async () => {
    let idx = document.getElementById("walletlist").selectedIndex;
    let token = prompt("Enter V3 token");
    await wasmModule.import_token(token, idx);
    let balance = await wasmModule.get_balance(idx);
    document.getElementById("balance").innerHTML = String(balance) + " crsat";

    let proofs = await wasmModule.print_proofs(idx);
    document.getElementById("output").innerHTML = proofs;
  });

  document.getElementById("exportbtn").addEventListener("click", async () => {
    let idx = document.getElementById("walletlist").selectedIndex;
    let amount = Math.round(Number(prompt("Enter amount to send")));
    let token = await wasmModule.send(BigInt(amount), idx);

    let balance = await wasmModule.get_balance(idx);
    document.getElementById("balance").innerHTML = String(balance) + " crsat";

    let proofs = await wasmModule.print_proofs(idx);
    document.getElementById("output").innerHTML = proofs + "\ntoken:\n" + token;
  });

  document
    .getElementById("walletlist")
    .addEventListener("change", async (event) => {
      const selectedWallet = event.target.value;
      let idx = document.getElementById("walletlist").selectedIndex;

      console.log("Selected wallet:", idx);

      let wallet_url = await wasmModule.get_wallet_url(idx);
      document.getElementById("walletname").innerHTML =
        "[" + wallet_name + "] " + selectedWallet + " @ " + wallet_url + "  ";

      document.getElementById("balance").innerHTML = "0 crsat";
      document.getElementById("output").innerHTML = "";

      let balance = await wasmModule.get_balance(idx);
      document.getElementById("balance").innerHTML = String(balance) + " crsat";

      let proofs = await wasmModule.print_proofs(idx);
      document.getElementById("output").innerHTML = proofs;
    });

  document.getElementById("walletlist").selectedIndex = 0;
  document.getElementById("walletlist").dispatchEvent(new Event("change"));
}

run().catch(console.error);
