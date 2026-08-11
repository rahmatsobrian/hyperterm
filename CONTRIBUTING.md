# Contributing to HyperTerm

## Getting set up

```powershell
rustup update stable
git clone https://github.com/rahmatsobrian/hyperterm.git
cd hyperterm
cargo build
cargo test
```

## Toolchain notes (read this if `cargo check` fails with `edition2024`)

This crate targets current stable Rust. If you're on an old, distro-packaged
`rustc` (for example Ubuntu's apt `rustc` package, which lags upstream
stable by a long way), you may see errors like:

```
error: package `...` cannot be built because it requires rustc 1.80 or newer
```

or

```
The package requires the Cargo feature called `edition2024`, but that
feature is not stabilized in this version of Cargo.
```

This is not a bug in HyperTerm — it means your toolchain is older than the
MSRV of one of the dependency tree's transitive crates. Fix: `rustup update
stable` (or install Rust via rustup if you're currently on a system package
manager's Rust). CI always builds with current stable via
`dtolnay/rust-toolchain@stable`, so `main` is guaranteed to build there even
if it doesn't on an outdated local toolchain.

`Cargo.lock` is committed and several transitive dependencies are pinned to
specific patch versions purely for reproducibility (see the comments next to
`winresource` in `Cargo.toml`, for instance) — not because they're required
for correctness. Feel free to `cargo update` if you have a reason to.

## Before opening a PR

```powershell
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

CI (`lint` job) will reject unformatted code or new clippy warnings.

## Project structure & where to add things

See the "Project layout" section of `README.md`. A few guidelines:

- **Don't reach into `terminal_core`'s grid from outside `terminal_core`.**
  Mutation should go through its public methods (`print`, `cursor_to`,
  `erase_in_line`, etc.) so the scroll→history handoff into `VirtualBuffer`
  stays correct in one place.
- **New ANSI sequences** go in `ansi_parser/mod.rs`'s `csi_dispatch`/
  `esc_dispatch`/`osc_dispatch`; add a matching `TerminalCore` method rather
  than mutating grid state directly from the parser.
- **New config fields** need a `#[serde(default)]`-compatible default (see
  existing `*Config` structs in `config/mod.rs`) so old `config.toml` files
  from before your change still load.
- **Anything touching disk I/O on the input/render hot path is a bug.**
  `VirtualBuffer::push_line` batches fsyncs (`DEFAULT_SYNC_EVERY_N_LINES`)
  specifically so a single keystroke never blocks on disk.

## Security-sensitive areas

`src/ssh_engine/mod.rs`'s `check_server_key` currently accepts any host key
(see the module doc and ROADMAP.md Phase 2). If you're picking that up:
please design the `known_hosts`-equivalent store as a separate, testable
module (not inline in the handler) so it can be unit tested without a live
SSH server.

## Reporting bugs

Please attach a diagnostic report if you can:
```powershell
hyperterm --export-diagnostics   # Phase 2: CLI flag; today, call
                                  # hyperterm::logger::crash::export_diagnostic_zip
                                  # programmatically or via the (planned) UI button
```
Until the CLI flag lands, zip up your `logs/` directory manually — it never
contains SSH credentials, only connection metadata and terminal output.
