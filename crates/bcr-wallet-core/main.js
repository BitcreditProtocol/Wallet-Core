async function run() {
  const wasmModule = await import("./pkg/bcr_wallet_core.js");
  await wasmModule.default();

  await wasmModule.initialize_api();

  const wallet_name = wasmModule.get_wallet_name();
  document.getElementById("walletname").innerHTML = wallet_name;

  document.getElementById("importbtn").addEventListener("click", async () => {
    let token = prompt("Enter V3 token");
    await wasmModule.import_token(token);
    let balance = await wasmModule.get_balance();
    document.getElementById("balance").innerHTML = String(balance) + " crsats";

    let proofs = await wasmModule.print_proofs();
    document.getElementById("output").innerHTML = proofs;
  });

  document.getElementById("exportbtn").addEventListener("click", async () => {
    let amount = Math.round(Number(prompt("Enter amount to send")));

    let token = await wasmModule.send(BigInt(amount));

    let balance = await wasmModule.get_balance();
    document.getElementById("balance").innerHTML = String(balance) + " crsats";

    let proofs = await wasmModule.print_proofs();
    document.getElementById("output").innerHTML = proofs + "\ntoken:\n" + token;
  });
}

run().catch(console.error);
