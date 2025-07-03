# Clang/LLVM issues on linux
export RUSTFLAGS='--cfg getrandom_backend="wasm_js"'
wasm-pack build --target web --debug --no-opt --no-pack
