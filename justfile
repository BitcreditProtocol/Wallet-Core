check:
    cargo fmt -- --check
    cargo check
    cargo clippy --all-targets --all-features -- -D warnings
    cargo deny check

test:
    cargo test

clean:
    cargo clean

cli *args:
    (cd crates/bcr-wallet-cli && cargo run -- {{args}})

