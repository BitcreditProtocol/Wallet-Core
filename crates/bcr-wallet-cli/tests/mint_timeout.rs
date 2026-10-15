use std::{
    net::TcpListener,
    process::Command,
    time::{Duration, Instant},
};

const MNEMONIC: &str =
    "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
const REQUEST_TIMEOUT_SECS: u64 = 2;
const EXIT_BOUND: Duration = Duration::from_secs(20);

#[test]
fn add_wallet_fails_within_timeout_when_mint_never_responds() {
    let mint = TcpListener::bind("127.0.0.1:0").expect("bind mock mint");
    let mint_addr = mint.local_addr().unwrap();
    let held = std::thread::spawn(move || {
        let mut conns = Vec::new();
        while let Ok((stream, _)) = mint.accept() {
            conns.push(stream);
        }
    });

    let dir = std::env::temp_dir().join(format!("mint_timeout_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let settings = dir.join("settings.toml");
    std::fs::write(
        &settings,
        format!(
            r#"log_level = "INFO"
db_path = "{db}"
esplora_base_urls = ["http://127.0.0.1:1"]
mint_connect_timeout_secs = 1
mint_request_timeout_secs = {REQUEST_TIMEOUT_SECS}

[wallets.w1]
network = "testnet"
nostr_relays = ["ws://127.0.0.1:1"]
mint_url = "http://{mint_addr}"
mnemonic = "{MNEMONIC}"
"#,
            db = dir.join("wallet.db").display(),
        ),
    )
    .unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_bcr-wallet-cli"))
        .args(["add_wallet", "w1"])
        .env("SETTINGS_FILE", &settings)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn bcr-wallet-cli");

    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if start.elapsed() > EXIT_BOUND {
            child.kill().unwrap();
            child.wait().unwrap();
            break None;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    drop(held);
    let _ = std::fs::remove_dir_all(&dir);

    let status = status.expect("bcr-wallet-cli hung on a mint that never responds");
    assert!(
        !status.success(),
        "add_wallet must fail against a dead mint"
    );
}
