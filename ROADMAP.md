# Roadmap

Status legend: ✅ done (Phase 1) · 🚧 partial/stubbed · ⏳ not started

## Phase 1 — Foundation (this repository)

- ✅ Project scaffold, `Cargo.toml`, GitHub Actions CI (build matrix, tests,
  benchmarks, stress tests, lint, installer)
- ✅ SSH engine (password + private-key auth, PTY, interactive shell, resize,
  keepalive) via `russh`
- ✅ SSH host key verification (`known_hosts`-equivalent store, TOFU/Prompt/
  Strict policies, unconditional refusal on a changed key)
- ✅ ANSI/VT parser (`vte`): 16/256/truecolor SGR, cursor movement, erase,
  scroll region, UTF-8/emoji/box-drawing
- ✅ Terminal core: live grid, cursor, SGR state
- ✅ Virtual scrollback buffer: RAM ring + transparent disk spill
- ✅ Disk cache: append-only, crash-recoverable, mmap-backed reads
- ✅ Search: plain/regex/case-sensitive/whole-word (sequential scan)
- ✅ Renderer: crossterm console-mode, dirty-region diffing
- ✅ Logging: TRACE..FATAL, daily rotation, panic hook, crash log
- ✅ Diagnostic report export (zip: logs + redacted config + system info)
- ✅ Config manager (TOML) + OpenSSH config import
- ✅ Unit tests, stress tests (5M lines), benchmarks
- ✅ Windows installer (Inno Setup)
- 🚧 Copy to clipboard — `Line::to_ansi_string()`/`plain_text()` produce the
  right *content*; wiring an actual OS clipboard write (`arboard` or
  Win32 `OpenClipboard`) from a keybinding is Phase 2 UI work.

## Phase 2 — Security & correctness hardening

- ✅ SSH host key verification (done — see Phase 1 list above; moved up
  from its original Phase 2 slot since it was the highest-priority gap)
- ✅ ssh-agent authentication — Unix (`$SSH_AUTH_SOCK`) via `russh-keys`'
  built-in client; **Windows via a hand-written named-pipe transport**
  (`\\.\pipe\openssh-ssh-agent`, the standard Win32 OpenSSH agent pipe),
  since `russh-keys` itself has no real non-Unix implementation. Pageant
  is not supported yet (different IPC mechanism) — use a key file with
  `-i` if you're on Pageant. **Disclosure**: the Windows path compiles in
  CI but has not been exercised against a live agent in this project's
  sandboxed dev environment — please validate against a real OpenSSH
  Authentication Agent service before relying on it.
- ✅ `known_hosts` maintenance: `hyperterm --forget-host <host:port>` CLI
  flag to remove a stale entry after a legitimate key change.
- ✅ Indexed (trigram) search for plain-text queries — measured ~75,000x
  faster than sequential scan on a 2M-line needle-in-haystack benchmark.
  Regex queries are not accelerated (documented limitation, see
  `src/search/trigram_index.rs`).
- ⏳ Import full OpenSSH config semantics (`Match`, wildcard host merging)
- ⏳ Native crash minidump capture (`MiniDumpWriteDump` via `dbghelp.dll`)
  embedded in diagnostic reports
- ⏳ Reflow-aware line wrapping in scrollback (currently: hard-wrapped per
  live grid width at time of scroll; resizing doesn't re-flow history)
- ⏳ Indexed (trigram) search so "Ctrl+F" on 10M+ lines doesn't degrade to a
  multi-second scan

## Phase 3 — Rendering & UX

- ✅ Tabs (multiple concurrent SSH sessions, one background task each,
  switch with Ctrl+PageUp/PageDown or jump with Ctrl+Alt+1-9, close with
  Ctrl+Alt+W)
- ✅ **Interactive new-tab dialog** (Ctrl+Alt+T): type `[user@]host[:port]`,
  connects in the background so a slow DNS lookup/host-key prompt never
  freezes the rest of the UI. Reuses the auth/host-key-policy resolved at
  startup (no per-tab credential picker).
- ✅ **Split panes**: 2-pane vertical split (Ctrl+Alt+S to split with the
  next tab, Ctrl+Alt+O to swap focus, Ctrl+Alt+S again or Ctrl+Alt+U to
  unsplit). Each pane's remote PTY is actually resized to match its
  on-screen width. Deliberately scoped to 2 panes, not arbitrary
  recursive/resizable tmux-style layouts -- see `session_manager::split_widths`.
- ✅ **Dark/Light theme switcher** (`--theme dark|light`, or `config.toml`):
  console-mode rendering can't control the host terminal's font, but it
  *can* control color -- `renderer::palette` remaps the 16 standard ANSI
  colors and default fg/bg per theme, independent of the host terminal's
  own color scheme. 256-color/truecolor output passes through unchanged
  (matches how real terminal theming works).
- 🚫 **GPU-accelerated renderer (DirectWrite/Direct2D): not implemented,
  and deliberately not attempted as unverified code.** This is
  categorically different from every other Windows-only piece in this
  project (minidumps, the OpenSSH-pipe/Pageant agent transports): those
  are each one or two isolated FFI calls whose exact signatures could be
  verified field-by-field against the real crate source. A GPU renderer
  needs an entire Win32 GUI application -- window class registration
  (with generic `IntoParam<PCWSTR>`-style trait bounds on
  `CreateWindowExW` that are easy to get subtly wrong blind), a message
  pump on its own OS thread bridged back to the async session logic,
  COM object lifetime management across resize events, and DPI handling
  -- dozens of interdependent calls where a single mistake anywhere
  breaks the whole pipeline, with zero ability to compile-check any of it
  in this project's Linux dev sandbox. Shipping that as if it were
  "done" would mean handing over code with a real chance of not even
  compiling, which is a worse outcome than clearly saying so. What *is*
  done: the constructor signatures for `D2D1CreateFactory`,
  `DWriteCreateFactory`, `ID2D1Factory::CreateHwndRenderTarget`, and the
  core `D2D1_RENDER_TARGET_PROPERTIES` / `D2D1_HWND_RENDER_TARGET_PROPERTIES`
  / `D2D1_COLOR_F` struct layouts were verified against the `windows`
  crate's real generated bindings during this pass, as a documented
  starting point for whoever picks this up next with an actual Windows
  dev machine to iterate against.
- ⏳ Ctrl+Wheel zoom, font picker (Cascadia Code / JetBrains Mono / Consolas)
  -- not meaningful until a GPU renderer exists (see above); `crossterm`
  draws into the *host* terminal's own grid, which controls font
  rendering entirely.

## Phase 4 — Distribution polish

- ⏳ Combined x86+x64 installer (current installer targets x64 only per
  build; see `installer/hyperterm.iss` comments)
- 🚫 Code signing -- **cannot be done from this environment at all**: real
  code signing needs a paid certificate from a CA (or Microsoft's
  Trusted Signing service) tied to a verified publisher identity, which
  is an organizational/financial step, not an engineering one. The CI
  workflow has no signing step because there is nothing to sign with.
- 🚫 Auto-update channel -- needs actual release hosting infrastructure
  (a server or CDN serving version manifests) that doesn't exist for this
  project; not implementable as pure client-side code.

## Explicitly deferred / open questions

- **GPU renderer is the single largest remaining item**, and honestly:
  building it well requires iterating against real Windows + GPU hardware
  (glyph atlas correctness, DPI scaling, ClearType-equivalent subpixel
  rendering) in a way that can't be done blind. It is not started, not
  scaffolded with placeholder code, and not claimed as partially done --
  the console renderer is the real, complete Phase 1-3 renderer.
- **Win7 SP1 in practice**: the codebase avoids Win8+-only APIs where
  known, and `crossterm` supports the legacy console API, but Phase 1 has
  not been hardware-tested on real Windows 7. Treat "Win7 support" as
  "designed for, not yet verified on real hardware" until a contributor
  confirms it (tracked as an open item, not silently assumed to work).
- **x86 (32-bit) build**: included in the CI matrix and should build, but
  is lower-priority for hand-testing than x64.
- **ssh-agent Pageant support**: implemented (`src/ssh_engine/pageant.rs`),
  same disclosure as the named-pipe transport applies -- not exercised
  against a live Pageant process in this sandbox.
- **Multi-tab auth**: all tabs (whether opened at startup or via the
  Ctrl+Alt+T dialog) share one auth method (one password prompt, one key,
  or one agent). Per-tab credentials would need a fuller connection dialog
  (host + username + auth method picker, not just a host string).
