# Wallet Core

Bitcredit wallet core in Rust

## CLI

1. git clone git@github.com:BitcreditProtocol/Wallet-Core.git
2. install just (https://github.com/casey/just)
3. Use wallet:

Set configs in crates/bcr-wallet-cli/alice.toml

To reset just rm crates/bcr-wallet-cli/alice.db

```
just cli -w alice restore_wallet

just cli -w alice info

just cli -w alice receive 0 $token

just cli -w alice send_payment 0 $token

just cli -w alice request_payment 0 150 sat

just cli -w alice melt 0 1000 $btcaddress

just cli -w alice pay_by_token 0 100 sat

just cli -w alice mint 0 1200

just cli -w alice reclaim 0 $txid
```

## wallet_ffi

Rust<>Flutter FFI for [Wallet-Core](https://github.com/BitcreditProtocol/Wallet-Core/)

## Prerequisites

* Install Rust
* Install Flutter
* `cargo install flutter_rust_bridge_codegen`

## Generate bindings

```bash
flutter_rust_bridge_codegen generate
```

## Import this package

This package can be imported as follows:

In `pubspec.yaml`:

```yaml
   wallet_ffi:
     git:
       url: git@github.com:BitcreditProtocol/Wallet-Core.git
       ref: vx.x.x
```

The `ref` can either be a commit hash, a branch or a tag.

Then, in `main.dart`:

```dart
import 'package:wallet_ffi/wallet_ffi.dart';

void main() async {
  WidgetsFlutterBinding.ensureInitialized();
  final conf = WalletFfiConfig(dbFolderPath: '..db directory..');
  await RustLib.init();

  await initWalletFfi(conf: conf);
  runApp(const MyApp());
}
```

