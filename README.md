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

### Output modes

By default the CLI is made for humans: results and logs are written to stderr as readable text.

With `--json`, every command writes one JSON document to stdout instead, so scripts and tests can
parse it. Logs stay on stderr, and a failing command exits non-zero with an empty stdout. The flag is
global, so it works before or after the subcommand:

```
just cli -w alice --json info
just cli -w alice mint $id 1200 --json
```

## wallet_ffi

Rust<>Flutter FFI for [Wallet-Core](https://github.com/BitcreditProtocol/Wallet-Core/)

## Prerequisites

* Install Rust through [rustup](https://rustup.rs). The build hook installs
  the toolchain and targets pinned in `crates/bcr-wallet-ffi/rust-toolchain.toml`.
* Install Flutter 3.50.0 or newer
* `cargo install flutter_rust_bridge_codegen --version 2.13.0` (only to regenerate bindings)
* For Android builds: an Android NDK, r27 or newer

## Generate bindings

```bash
flutter_rust_bridge_codegen generate
```

## How the Rust library is built

`wallet_ffi` is a Dart package with a [build hook](https://dart.dev/tools/hooks)
(`hook/build.dart`), not a Flutter plugin: it has no `android/`, `ios/`,
`macos/`, `linux/` or `windows/` folders, no CocoaPods podspec and no Swift
package. When an app builds, Flutter runs the hook for each target. The hook
compiles `crates/bcr-wallet-ffi` with cargo (through
[`flutter_rust_bridge_hooks`](https://pub.dev/packages/flutter_rust_bridge_hooks)
and `native_toolchain_rust`), or downloads a signed precompiled binary. Flutter
then bundles the result as a dynamic library. On iOS and macOS it becomes
`wallet_ffi.framework`, which Flutter signs and embeds. Android 64-bit
libraries are linked for 16 KB pages, and the hook checks this.

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

`.github/workflows/cd_precompiled.yml` publishes signed precompiled binaries
for iOS (device and simulator, iOS 15+), macOS (13+) and Android (API 24+) to a
`precompiled_<crate hash>` release. The crate hash covers the sources of every
workspace crate, `Cargo.toml`/`Cargo.lock`, `rust-toolchain.toml` and
`precompiled_binaries.yaml`. The hook verifies each download against the
Ed25519 `public_key` in `precompiled_binaries.yaml`. If no signed binary
exists for the target, or the app builds for an older platform version, the
hook builds locally with cargo instead.

Precompiled binaries are used when the app opts in, or when rustup is not
installed. To opt in (for example in app CI), add to the app's `pubspec.yaml`:

```yaml
hooks:
  user_defines:
    wallet_ffi:
      use_precompiled_binaries: true
```

Set it to `false` to always build locally. The old `cargokit_options.yaml` and
`CARGOKIT_USE_PRECOMPILED_BINARIES` no longer have any effect.

To publish binaries for the current sources, the workflow runs
`tool/precompile_binaries.dart`, a standalone Dart package. Run
`dart run precompile_binaries.dart --print-hash` in `tool/` to print the crate
hash.

Call `RustLib.init()` without an `externalLibrary`: flutter_rust_bridge then
opens `wallet_ffi.framework/wallet_ffi` (iOS, macOS) or `libwallet_ffi.so` /
`wallet_ffi.dll`. Flutter embeds the framework but does not link it into the
app, so `ExternalLibrary.process(...)` finds no symbols.

Any `dart run` in the app, such as `build_runner`, also runs the hook for the
host. The first run builds the Rust library for the host machine, and later
runs reuse it.

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
