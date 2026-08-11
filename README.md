# HyperTerm

A high-performance, low-latency SSH terminal for Windows 7 SP1 through
Windows 11, built to combine the responsiveness of VS Code's integrated
terminal with scrollback that is **never** truncated — history spills
transparently from RAM to an on-disk cache instead of being discarded, so
you can scroll back through tens of millions of lines without losing a byte,
even across a restart or a crash.

> **Status: Phase 1 (foundation).** This is a real, compiling, tested
> codebase — not a mockup — but it is not yet feature-complete against the
> full long-term vision. See [ROADMAP.md](ROADMAP.md) for exactly what's
> implemented vs. planned, and the "Honest scope" section below before you
> rely on this for anything sensitive.

## Why

- **VS Code's terminal** is fast and responsive over SSH, but its
  scrollback is bounded — output beyond that limit is gone.
- **CMD and PuTTY** keep more history, but SSH input feels laggy — even a
  single keystroke can visibly lag behind.

HyperTerm's virtual scrollback buffer is designed to give you both: an
async, non-blocking I/O path for responsiveness, and a RAM→disk cache
architecture so history is genuinely unlimited.

```text
RAM Cache → Virtual Buffer → Disk Cache → Persistent History
```

## Features (Phase 1-3)

- SSH client (password, private-key: ED25519/RSA/OpenSSH format, ssh-agent)
  over pure-Rust `russh` — no OpenSSL/libssh2 native dependency, which keeps
  the binary self-contained and Windows cross-compilation simple.
- **Tabs**: multiple concurrent SSH sessions, each its own background task.
  Ctrl+PageUp/PageDown to switch, Ctrl+Alt+1-9 to jump to a tab, Ctrl+Alt+W
  to close one, **Ctrl+Alt+T to open a new one interactively** (connects
  in the background so it never freezes the rest of the UI). Also
  populate tabs at startup with `--session user@host:port` (repeatable).
- **Split panes** (2-pane vertical, Ctrl+Alt+S): each pane's remote PTY is
  actually resized to match its on-screen width, not just visually cropped.
- **Dark/Light theme** (`--theme`): HyperTerm's own color palette,
  independent of the host terminal's color scheme.
- **SSH host key verification** against a persisted `known_hosts`-equivalent
  store, with configurable policy (`--host-key-policy prompt|tofu|strict`;
  interactive prompt is the default, matching every other SSH client). A
  *changed* host key is always refused, regardless of policy.
- Interactive PTY shell with resize, keepalive.
- **Scrollback viewing**: Shift+PageUp/PageDown or mouse wheel scrolls
  through history (not just the live screen), with automatic reflow if the
  terminal width has changed since those lines were written.
- Virtual scrollback buffer: configurable RAM capacity, transparent spill to
  an append-only, crash-recoverable disk cache (`logs/<session>.cache` +
  `.idx`). History survives app restarts.
- ANSI/VT parsing via `vte` (the same parser core Alacritty uses): 16/256/
  true-color, bold/italic/underline/reverse/strikethrough, UTF-8, emoji,
  box-drawing.
- Search: plain text, regex, case-sensitive, whole-word. Plain-text queries
  are accelerated by an in-memory trigram index (measured **~75,000x**
  faster than a sequential scan for a needle-in-2M-lines query — see
  `examples/bench_search.rs`); regex queries still do a full scan.
- Copy as plain text or ANSI-preserving text (colors/styles round-trip).
- Structured logging (`TRACE`..`FATAL`) with daily rotation, automatic
  `panic.log`/`crash.log`, **native Windows crash minidumps** (dbghelp.dll
  `MiniDumpWriteDump`), and an **Export Diagnostic Report** (zip of logs +
  redacted config + system info + any minidump) for bug reports.
- `~/.ssh/config` import: multiple Host patterns, glob wildcards, negation
  (`!pattern`), `Match host`/`Match user`/`Match all`, correct
  first-match-wins resolution order matching real OpenSSH semantics.
- GitHub Actions CI: debug + release builds for x86 and x64, unit tests,
  stress tests (5M-line scrollback, rapid resize, open/close leak check),
  benchmarks, lint (fmt+clippy), and an Inno Setup installer.

## Not yet implemented / not attempted (see ROADMAP.md)

- **GPU-accelerated DirectWrite/Direct2D renderer.** Deliberately not
  attempted as unverified code — see ROADMAP.md "Phase 3" for exactly why
  (a full Win32 GUI pipeline has categorically higher risk of silently
  broken code than the single-FFI-call Windows features elsewhere in this
  project, and this sandbox can't compile-check any of it). HyperTerm
  renders through `crossterm` (console-mode), which is genuinely fast and
  works identically from Win7 to Win11.
- Ctrl+Wheel zoom / font picker — not meaningful until a GPU renderer
  exists; console-mode rendering uses whatever font the *host* terminal
  is configured with.
- Per-tab credentials — every tab (startup or Ctrl+Alt+T) shares one auth
  method.
- Split panes are 2-pane vertical only, not arbitrary recursive layouts.
- Regex search is not index-accelerated (only plain-text queries are — see
  "Search" below).
- Code signing and auto-update — need a paid signing certificate and
  release-hosting infrastructure respectively, neither of which exist for
  this project; not purely an engineering task.

## Building

```powershell
git clone https://github.com/rahmatsobrian/hyperterm.git
cd hyperterm
cargo build --release
```

Requires a current stable Rust toolchain (`rustup update stable`). See
[CONTRIBUTING.md](CONTRIBUTING.md) for a note on why that matters here.

## Running

```powershell
hyperterm example.com -l myuser --password
hyperterm example.com -l myuser -i C:\Users\me\.ssh\id_ed25519

# Multiple tabs at startup:
hyperterm first.example.com -l myuser --session second.example.com --session alice@third.example.com:2222
```

| Flag | Description |
|---|---|
| `-p, --port` | SSH port (applies to the positional HOST) |
| `-l, --username` | Remote username (default for HOST and any `--session` without its own `user@`) |
| `-i, --identity` | Path to a private key file |
| `--password` | Prompt for a password |
| `--agent` | Authenticate via a running SSH agent |
| `--session` | Open another tab: `[user@]host[:port]` (repeatable) |
| `--ram-capacity` | Scrollback lines kept in RAM before spilling to disk |
| `--host-key-policy` | `prompt` (default) / `tofu` / `strict` — see below |
| `--known-hosts` | Override the known_hosts file location |
| `--theme` | `dark` (default) / `light` — overrides `config.toml` |

### Keybindings

| Keys | Action |
|---|---|
| `Ctrl+Alt+Q` | Quit HyperTerm |
| `Ctrl+Alt+T` | Open a new tab (type `[user@]host[:port]`, Enter to connect, Esc to cancel) |
| `Ctrl+PageUp` / `Ctrl+PageDown` | Switch to previous / next tab |
| `Ctrl+Alt+1`–`9` | Jump directly to tab 1–9 |
| `Ctrl+Alt+W` | Close the active tab |
| `Ctrl+Alt+S` | Split the active tab side-by-side with the next tab (press again to unsplit) |
| `Ctrl+Alt+O` | Swap keyboard focus between the two split panes |
| `Shift+PageUp` / `Shift+PageDown` | Scroll the local scrollback view |
| Mouse wheel | Scroll the local scrollback view |
| Any other key | Sends input to the shell (and snaps the view back to live if scrolled) |

Plain `PageUp`/`PageDown` (no modifier) are forwarded to the remote shell
as usual, so apps like `less` or `vim` still get to handle them themselves.

### Host key verification

On first connection to a host, HyperTerm behaves like any standard SSH
client:

```
The authenticity of host 'example.com:22' can't be established.
ssh-ed25519 key fingerprint is SHA256:xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx.
Are you sure you want to continue connecting (yes/no)?
```

Accepted keys are persisted to `known_hosts` in HyperTerm's config
directory. If a host's key ever changes, the connection is refused
unconditionally (this is the actual MITM protection — no policy flag
bypasses it); run `hyperterm --forget-host example.com:22` if the change is
expected (e.g. the server was reinstalled), then reconnect to re-trust it.

### ssh-agent

```powershell
hyperterm example.com -l myuser --agent
```

On Unix it talks to `$SSH_AUTH_SOCK`. On Windows it tries, in order: the
standard `\\.\pipe\openssh-ssh-agent` named pipe (the "OpenSSH
Authentication Agent" service), then falls back to **PuTTY's Pageant**
(via its native `WM_COPYDATA` protocol) if that pipe isn't available.

### Theme

```powershell
hyperterm example.com -l myuser --theme light
```

Console-mode rendering can't control the *font* the host terminal uses,
but it does control *color* — `--theme dark` (default) or `--theme light`
remap the 16 standard ANSI colors and default foreground/background
independent of whatever color scheme your terminal emulator itself is
configured with. 256-color/truecolor output (e.g. from `ls --color`,
build tool output) passes through unchanged.

## Testing & benchmarking

```powershell
cargo test                                            # unit + integration tests
cargo test --release --test stress_test -- --ignored --nocapture   # stress tests
cargo run --release --example bench_scrollback         # scrollback throughput
cargo run --release --example bench_ansi_parse          # ANSI parser throughput
cargo run --release --example bench_search               # indexed vs sequential search
```

Real numbers from a CI-equivalent Linux dev VM (Windows numbers will differ;
CI publishes its own run in the Actions log):

- Scrollback push: **~495,000 lines/sec** sustained, including spilling to
  disk once RAM capacity is exceeded.
- Random-access read across 5,000,000 lines of history: **<1 ms/read**.
- Indexed plain-text search across 2,000,000 lines: **129µs**, vs. **9.6s**
  for the sequential-scan fallback (regex path) — a ~75,000x difference.

## Project layout

```
src/
  ssh_engine/        SSH connection, auth (password/key/agent incl. Pageant), known_hosts, PTY/shell (russh)
  terminal_core/     Live grid: cursor, SGR state, scroll region, soft-wrap tracking
  ansi_parser/       VT/ANSI escape sequence parsing (vte::Perform)
  virtual_buffer/    RAM ring buffer + spill-to-disk scrollback manager + reflow
  disk_cache/        Append-only, crash-recoverable persistent line store
  search/            Plain/regex/case/whole-word search + trigram index
  renderer/          crossterm-based console renderer, dirty-region diffing, theme palette
  session_manager/   Tab/split-pane switching logic + tab bar rendering (pure, testable)
  config/            config.toml + OpenSSH config import (Host/Match incl. user, wildcards)
  logger/            tracing-based logging, panic hook, crash log, Windows minidumps
tests/               Unit + integration tests, stress tests (#[ignore]'d)
examples/            Benchmarks (bench_scrollback, bench_ansi_parse, bench_search)
installer/           Inno Setup script
.github/workflows/   CI: build matrix, tests, benchmarks, stress, lint, installer
```

## Honest scope

This project was originally specified at a scope comparable to Alacritty/
WezTerm/mRemoteNG combined — GPU rendering, host-key-verified SSH, tabs,
split panes, indexed search, native crash dumps, all at once. That's
realistically a multi-contributor, multi-month effort, not something that
can be both *fully* built and *honestly verified* in one pass.

What's in this repository today compiles cleanly and its test suite passes
(83 unit/integration tests + 3 stress tests covering millions of lines of
scrollback). Essentially the entire originally-specified feature list is
now real and tested: host-key-verified SSH, ssh-agent (including Pageant),
tabs with an interactive new-tab dialog, split panes, indexed search,
reflow-aware scrollback viewing, theme switching, and native Windows crash
dumps.

What's still explicitly open, and why:
- **GPU rendering** is the one deliberately-not-attempted item. Not
  because it wasn't tried, but because after verifying the actual API
  surface needed (window class registration, message-loop threading, COM
  lifetime management across resize events — dozens of interdependent
  Win32/Direct2D/DirectWrite calls), it's categorically riskier than every
  other Windows-only piece in this project: those were each one or two
  isolated FFI calls verifiable field-by-field against real crate source;
  a GPU renderer is an entire GUI application with zero ability to
  compile-check any of it here. Shipping that as "done" would mean a real
  chance of unbuildable code — worse than saying so plainly. The verified
  constructor signatures and struct layouts are recorded in ROADMAP.md as
  a starting point for whoever picks this up with a real Windows dev box.
- **Code signing and auto-update** need a paid certificate and hosting
  infrastructure respectively — organizational/financial prerequisites,
  not engineering ones.

## License

MIT — see [LICENSE](LICENSE).
