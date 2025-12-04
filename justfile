check:
    cargo fmt -- --check
    cargo check
    cargo clippy --all-targets --all-features -- -D warnings
    cargo deny check

test:
    cargo test

clean:
    cargo clean

cli:
    cargo run --package bcr-wallet-cli

