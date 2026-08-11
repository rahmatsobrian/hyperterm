# Changelog

All notable changes to this project are documented in this file.
Format loosely follows [Keep a Changelog](https://keepachangelog.com/).

## [Unreleased]

### Added
- **Theme switcher** (`renderer/palette.rs`, `--theme dark|light`): remaps
  the 16 standard ANSI colors + default fg/bg per theme, independent of
  the host terminal's own color scheme. 256-color/truecolor passthrough
  unchanged. 6 new unit tests.
- **`Match user`** in the OpenSSH config importer, evaluated against the
  caller-supplied default username (documented as not bit-for-bit
  identical to OpenSSH's fully sequential resolution -- see module doc).
  Fixed a real bug found via testing: `Match user alice host foo` (mixed
  criteria) was being misparsed as a 3-pattern user match instead of
  correctly rejected as unsupported, because `Match` sub-criteria use
  comma-separated single-token argument lists, not space-separated lists
  like `Host` lines. 5 new tests.
- **Pageant support** for ssh-agent on Windows (`ssh_engine/pageant.rs`):
  `WM_COPYDATA` + shared-memory transport, bridged into the existing
  `russh_keys::agent::client::AgentClient` codec via a `tokio::io::duplex`
  pipe (so the security-sensitive agent-protocol parsing stays in the
  well-exercised library code, not hand-rolled). Tried automatically if
  the OpenSSH named pipe isn't available.
- **Split panes** (`session_manager::split_widths`/`SplitFocus`,
  Ctrl+Alt+S/O/U): 2-pane vertical split with real PTY resize per pane
  (not just visual cropping). 4 new unit tests for the pure width/focus
  logic.
- **Interactive new-tab dialog** (Ctrl+Alt+T): connects in a background
  task so it never blocks the rest of the UI; reuses the auth/host-key-
  policy resolved at startup.

### Changed
- `main_loop`'s `tabs` parameter is now `&mut Vec<Tab>` (was `&mut [Tab]`)
  to support tabs added at runtime.

### Deliberately not attempted
- **GPU-accelerated (DirectWrite/Direct2D) renderer.** After verifying the
  real API surface needed (window class registration with generic
  `IntoParam<PCWSTR>`-style trait bounds, message-loop threading, COM
  lifetime management), judged too high-risk to ship unverified: a full
  Win32 GUI pipeline has dozens of interdependent calls where this
  project's Linux sandbox can't compile-check any of it, unlike the
  single-FFI-call Windows features elsewhere (minidumps, agent
  transports). See ROADMAP.md for the verified signatures recorded as a
  starting point.

### Also added this cycle (earlier commits)
- **Indexed (trigram) search** (`src/search/trigram_index.rs`): plain-text
  queries are narrowed via an in-memory trigram inverted index before the
  real substring check, measured ~75,000x faster than sequential scan on a
  2M-line needle-in-haystack benchmark (`examples/bench_search.rs`). Regex
  queries remain sequential-scan (documented limitation).
- **Rewritten OpenSSH config importer**: multiple patterns per `Host`,
  glob wildcards (`*`, `?`), negation (`!pattern`), `Match host`/`Match
  all`, correct first-obtained-value-wins resolution order matching real
  `ssh_config` semantics. Unsupported `Match` criteria are skipped and
  logged, never guessed. Fixed two real bugs found via testing: a sentinel
  value leaking into the profile list, and (documented, not fixed --
  matches real OpenSSH) a wildcard-before-specific file-order gotcha.
- **Native Windows crash minidumps** (`src/logger/minidump.rs`): real
  `dbghelp.dll` `MiniDumpWriteDump` FFI via `windows-sys`, both from a
  vectored exception handler (native crashes, with full exception context)
  and best-effort from the Rust panic hook. Wired into "Export Diagnostic
  Report".
- **SSH host key verification**: `known_hosts`-equivalent store with
  `--host-key-policy prompt|tofu|strict`, `--forget-host`, changed-key
  refusal unconditional regardless of policy.
- **ssh-agent authentication** (`--agent`): Unix via `$SSH_AUTH_SOCK`,
  Windows via a hand-written named-pipe transport to
  `\\.\pipe\openssh-ssh-agent`.
- **Reflow-aware scrollback** (`src/virtual_buffer/reflow.rs`): regroups
  soft-wrapped physical rows back into logical lines and re-wraps them for
  a different column width, view-time (not rewriting the disk cache).
- **Scrollback viewing**: Shift+PageUp/PageDown and mouse wheel scroll
  through history (previously built but never wired into the render loop
  -- a real gap found and fixed this pass), composited with reflow.
- **Tabs**: multiple concurrent SSH sessions, each owned by its own
  background tokio task (avoids a borrow-checker dead end from trying to
  hold N mutable `SshSession` borrows in one `select!`), communicating via
  channels. Ctrl+PageUp/PageDown, Ctrl+Alt+1-9, Ctrl+Alt+W.
- `--session [user@]host[:port]` CLI flag (repeatable) to open additional
  tabs at startup.
- 62 new unit tests total this cycle, spanning trigram index, SSH config
  import (incl. `Match user`), known_hosts `forget()`,
  `history_window()`/reflow, tab switching/closing/rendering, split-pane
  width math, theme palette, and CLI target parsing.

### Fixed
- Scrollback buffer was fully built and tested but never actually
  reachable from the UI (no scroll keybindings wired up) -- found and
  fixed this pass.
- Removed dead code: `renderer::poll_input`/`InputEvent` were never called.
- `CrosstermRenderer::draw`'s cursor parameter is now `Option<(row, col)>`
  (`None` hides the cursor while viewing scrollback history).

## [0.1.0-phase1] - 2026-08-08

### Added
- Initial project scaffold: `Cargo.toml`, module layout, GitHub Actions CI.
- SSH engine (`russh`-based): password + private-key (ED25519/RSA/OpenSSH)
  auth, interactive PTY shell, resize, keepalive.
- ANSI/VT parser (`vte`-based): CSI cursor movement, SGR (16/256/truecolor,
  bold/italic/underline/reverse/strikethrough), erase in line/display,
  scroll regions, OSC window title, UTF-8/emoji/box-drawing passthrough.
- Terminal core: live grid, cursor state, SGR state, scroll-off → history
  handoff.
- Virtual scrollback buffer: RAM ring buffer with configurable capacity,
  transparent spill to an append-only disk cache. History is never dropped
  and survives process restarts.
- Disk cache: length-prefixed append-only log + fixed-size index, mmap-backed
  random access reads, crash recovery (truncates a dangling partial record
  left by an interrupted write).
- Search engine: plain text, regex, case-sensitive, whole-word, sequential
  scan across RAM + disk-resident history.
- Renderer: `crossterm`-based console renderer with dirty-region diffing and
  same-style run coalescing.
- Logger: `tracing`-based structured logging (TRACE..FATAL), daily file
  rotation, thread id/file/line capture, automatic panic hook → `panic.log`.
- Crash/diagnostic module: `crash.log` on controlled failure, "Export
  Diagnostic Report" zip (logs + redacted config + system info).
- Config manager: TOML config with typed defaults; `~/.ssh/config` importer
  (Host/HostName/User/Port/IdentityFile subset).
- Windows resource embedding (icon, version info) via `build.rs` +
  `winresource`.
- Inno Setup installer script.
- Test suite: 15 unit/integration tests (ANSI parsing, search, virtual
  buffer/disk cache correctness and persistence) + 3 stress tests (5M-line
  scrollback integrity, rapid resize, repeated open/close leak smoke test).
- Benchmarks: scrollback push/read throughput, ANSI parser throughput.

### Known limitations (see ROADMAP.md)
- No GPU-accelerated renderer yet (console-mode `crossterm` only).
- No tabs, split panes, or session manager UI.
- Search is sequential-scan, not indexed.
- No ssh-agent authentication.
