# AGENTS.md

Agent-specific context for Wallet-Core; what it is, CLI usage and how apps consume the package
live in [README.md](README.md), API history in [CHANGELOG.md](CHANGELOG.md). These are defaults,
not laws: the developer's instructions win, and if a rule fights the task, say so before breaking it.

## Project Map

One repository, two build systems: the Cargo workspace under `crates/` is the wallet; the repository
root is also the Flutter plugin package `wallet_ffi` that wraps it. A Dart call goes `lib/src/rust/api.dart`
-> `crates/bcr-wallet-ffi/src/api/mod.rs` -> `AppState` (`bcr-wallet-api`) -> `Purse` -> `Wallet` ->
debit `Pocket`, which talks to the mint over HTTP, `bcr-wallet-persistence` (redb) and `bcr-wallet-transport` (Nostr).

    crates/bcr-wallet-api/          # AppState, purse/wallet/pocket logic, mint and bitcoin clients
    crates/bcr-wallet-persistence/  # redb repositories; redb/migration/ holds schema migrations
    crates/bcr-wallet-ffi/          # crate `wallet_ffi`; src/api/mod.rs is the hand-written FFI surface
    lib/src/rust/                   # GENERATED Dart bindings; lib/wallet_ffi.dart is the hand-written entry
    cargokit/                       # vendored, locally patched; android/ ios/ macos/ linux/ windows/ call it

## Quality Gates

Full gate before every PR; the suite is fast:

    just check   # fmt --check, cargo check, clippy -D warnings (all targets/features), cargo deny
    just test    # cargo test across the workspace

CI (`.github/workflows/rust.yml`, `test.yml`) fails only on check, build and tests; fmt,
clippy and deny are advisory there, so `just check` is the real lint gate. No workflow runs
Dart: stale generated bindings surface only in a consuming app.

Non-negotiables:
- NEVER hand-edit `crates/bcr-wallet-ffi/src/frb_generated.rs` or `lib/src/rust/**`;
  regenerate them (Key Patterns). Edits are overwritten and the two sides drift.
- NEVER commit a wallet database or a mnemonic holding real funds: the wallet stores bearer ecash
  proofs, so a leaked seed is spent money. `crates/bcr-wallet-cli/*.toml` are testnet dev wallets;
  the `.db` files beside them are gitignored on purpose.
- A change to a persisted record layout MUST ship a migration (Key Patterns); a record the
  new code cannot read is the user's funds, not a bug report.

## Key Patterns

- **FFI surface and generated bindings.** Only `crates/bcr-wallet-ffi/src/api/mod.rs` is hand-written.
  After changing it, run `flutter_rust_bridge_codegen generate` from the repo root (`flutter_rust_bridge.yaml`)
  and commit the regenerated Rust and Dart with the change, as #338 does. The codegen tool must match
  the exact `flutter_rust_bridge` version pinned in `crates/bcr-wallet-ffi/Cargo.toml` and `pubspec.yaml`;
  `RustLib.init` checks the embedded codegen version, so bumping FRB means bumping all three (#337).
- **Persistence is versioned and migrated.** Every stored record is a versioned borsh envelope, some
  encrypted with the wallet key. Changing a `Stored*` struct means a new `migration_000N.rs` in
  `crates/bcr-wallet-persistence/src/redb/migration/`, registered in its `mod.rs`; migrations run at
  DB open and are tracked in the `migrations` table. Flag it as a breaking DB change in the CHANGELOG.
- **One currency unit.** The debit unit is Cashu `CurrencyUnit::Sat` from `bcr-common`; wallet
  creation rejects mints without a `Sat` keyset or with more than one unit. `credit` vs `debit`
  in a balance derives from each keyset's `final_expiry`, not from a separate unit; the credit
  pocket and `crsat` were removed in 0.9.0. Do not reintroduce custom unit strings.
- **`bcr-common` is the protocol.** Pinned by git `rev` in `Cargo.toml` and shared with the
  mint; offline exchange needs wallet and mint on the same revision (CHANGELOG 0.9.12). Bump it
  as a coordinated change, not a dependency update. `deny.toml` allows git dependencies only
  from the BitcreditProtocol org.
- **Workspace dependencies are enforced.** `cargo deny` fails on a dependency declared directly
  in a crate while it exists in `[workspace.dependencies]`, and on a workspace dependency no
  crate uses. Add shared deps to the root `Cargo.toml` and reference them with `workspace = true`.
- **Version, CHANGELOG and release.** `[workspace.package] version` in `Cargo.toml` and `version` in
  `pubspec.yaml` are one number; a bump commit changes both and opens a new top section in `CHANGELOG.md`,
  and every user-visible change adds a line there naming the endpoint or type it touches. Pushing a
  `v*` tag runs `.github/workflows/cd_precompiled.yml`, which builds and signs iOS and Android binaries
  of `crates/bcr-wallet-ffi` via `cargokit/build_tool`; apps fetch them by crate hash, else build locally.

## Common Gotchas

Traps that cost real time. Append when you hit one; prune when the edge is gone.

1. **A `Cargo.lock` inside a member crate is ignored by cargo but not by cargokit** (#318):
   cargo resolves against the root lock, yet cargokit's crate hash includes any nested lock, so
   a stray one busts the precompiled-binary cache and misleads Dependabot. Only the root lock counts.
2. **`cargokit/` carries local patches** (#269: Android 16 KB page-size linker flags in
   `cargokit/build_tool/lib/src/android_environment.dart`; #337 touched more). Re-syncing from
   upstream wholesale silently drops them.

## Hit every surface

A change to the wallet API is done only when every seam has moved. Before calling it done,
say which of these applied:

- **FFI surface** in `crates/bcr-wallet-ffi/src/api/mod.rs`: new core behaviour is unusable
  until the bridge exposes it.
- **Generated bindings**, both `frb_generated.rs` and `lib/src/rust/`: the Rust gate passes
  with stale Dart, and nothing here runs Dart.
- **Consuming app**: this package cannot prove the wallet app compiles or calls the new API;
  build it against the branch.
- **Stored records**: a changed `Stored*` struct needs its migration; fresh databases hide
  upgrade bugs.
- **CHANGELOG and version** per Key Patterns; consumers pick tags by what the log says.

## Plans and work artifacts

- Plans, research notes and scratch files stay outside the worktree or gitignored; the
  merged PR is the implementation record and `CHANGELOG.md` the user-facing one. Do not add
  a second checklist or PR summary to the repo.

## Working Agreements

Organisation-wide rules (branch protection, reviews, labels, Dependabot) live in the
[contributing guide](https://github.com/BitcreditProtocol/.github/blob/master/CONTRIBUTING.md).
This section is the per-task delta.

- Open pull requests against `master`. Branch from it too: basing work on another branch
  conflicts in exactly the files other people are changing.
- Never open, mark ready or merge a PR, and never push a tag, unless the developer
  explicitly asks. Each is visible to the whole team, and a tag also starts a release.
- Commit small and often. Each commit is self-contained, passes the gate above and is
  reviewable on its own; the subject says why, not just what. Reviewers only catch
  mistakes in changes they can hold in their head.
- Titles: conventional-commit style in plain language, e.g. `fix(ffi): melt URL no longer
  drops the path`. Mark breaking changes with the `breaking` label, not a `!` in the
  title; release notes are built from labels (see
  [`.github/release.yml`](.github/release.yml)), so also label `bug`, `enhancement`,
  `documentation` or `dependencies`.
- Body: the problem in a sentence or two, then how it was fixed, then how it was verified.
  The [organisation PR
  template](https://github.com/BitcreditProtocol/.github/blob/master/.github/PULL_REQUEST_TEMPLATE.md)
  asks exactly that. End with the model and harness that did the work.
- Evidence: the test that failed before and passes now; for FFI changes, the regenerated
  bindings and the app build that consumed them. Upload evidence to the PR on GitHub;
  never commit PR-only screenshots or assets.
- One concern per PR. If the description needs an "also", split it.
- Babysitting a PR: poll checks and comments newer than the last push; verify each bot
  finding against the source, fix the real ones, dismiss false positives with a written
  reason. No status check is required to merge, so a red check may predate your change:
  confirm that before blaming it, and say so in the PR. Stay quiet when nothing is new;
  stop when checks are green on the latest commit.
- Rebase on current `master` before opening a PR. Resolve conflicts in generated bindings
  by regenerating them, never by hand-merging; a hand-merged binding drifts from its Rust
  side.

## See Also

- [README.md](README.md) — what this is, CLI usage, generating bindings, precompiled binaries
- [CHANGELOG.md](CHANGELOG.md) — API and DB changes per version, including which were breaking
