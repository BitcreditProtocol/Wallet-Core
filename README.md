# Wallet Core

Bitcredit wallet core in Rust

## CLI

1. git clone git@github.com:BitcreditProtocol/Wallet-Core.git
2. install just (https://github.com/casey/just)
3. Use wallet:

Set configs in crates/bcr-wallet-cli/alice.toml

To reset just rm crates/bcr-wallet-cli/alice.db

```
// with $id = ddfb860cf982e17b6a45ce073823bf722d903c8a176de99e786c7f8b582dd6d6
just cli -w alice restore_wallet $id

just cli -w alice info

just cli -w alice receive $id $token

just cli -w alice send_payment $id $token

just cli -w alice request_payment $id 150

just cli -w alice melt $id 1000 $btcaddress

just cli -w alice mint $id 1200

just cli -w alice pay_by_token $id 100

just cli -w alice reclaim $id $txid
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

### Precompiled binaries

This package publishes signed precompiled iOS and Android Rust binaries from
`.github/workflows/cd_precompiled.yml`. App CI can opt in by adding
`cargokit_options.yaml` at the Flutter app root:

```yaml
use_precompiled_binaries: true
```

Alternatively set `CARGOKIT_USE_PRECOMPILED_BINARIES=true` in the app build
environment. Cargokit falls back to a local Rust build if a signed binary for
the current crate hash and target is not available.

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
