# Clang/LLVM issues on MAC
export RUSTFLAGS='--cfg getrandom_backend="wasm_js"'
export AR=/opt/homebrew/opt/llvm/bin/llvm-ar
export CC=/opt/homebrew/opt/llvm/bin/clang
export WASM_BINDGEN_USE_BROWSER=1
wasm-pack test --headless --firefox
