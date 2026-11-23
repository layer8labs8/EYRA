# Changelog

## 0.3.4

### One-command setup for any AI app
- `eyra setup <app>` registers EYRA in the MCP config of Google Antigravity, Gemini CLI, Cursor,
  VS Code, Windsurf or Claude Desktop (and prints the command for Claude Code). It keeps every other
  server, setting and extra field (such as `env`), writes a timestamped backup first, refuses to
  rewrite files that aren't plain JSON, and supports `--read-only`, `--remove` and `--dry-run`.
  `eyra setup --list` shows which apps are installed and whether EYRA is configured.

### Trusted builds
- Continuous integration: every push and pull request runs `cargo fmt --check`, `clippy -D warnings`
  and the full test suite on Windows.
- Releases are built on GitHub Actions from the tagged source. Every asset carries a signed build
  provenance attestation, verifiable with `gh attestation verify eyra.exe -R layer8labs8/EYRA`.
- The repository no longer contains a prebuilt binary: the Claude Code plugin now installs the
  `.mcpb` bundle from the matching GitHub release.

## 0.3.3

### Privacy
- Release binaries no longer contain the builder's local paths. Rust embeds absolute source
  paths of dependencies (for error messages); `scripts/package.ps1` now remaps them with
  `--remap-path-prefix`, builds into its own `target\package` folder, and refuses to package an
  exe that still contains the local user name or home path.

### License
- `LICENSE` is now the exact official Apache License 2.0 text (the previous file had altered
  wording, so GitHub could not detect it).


## 0.3.2

### Fixes
- **`see` on a minimized Chromium window took 2.1 s** (the warm-up waited for a page that a
  minimized window never exposes) and returned junk coordinates around (-32000,-32000). Minimized
  windows now return at once with `[WINDOW MINIMIZED: restore it with focus_window(window) first]`.
- RuntimeId is read from the scan's cache request instead of `GetRuntimeId()`. Measured gain is small
  (2-3 ms per scan, ids verified identical), because `GetRuntimeId()` turned out to be served from
  the client-side element reference, not by a cross-process call.

### Measured
- File Explorer on System32: `see` ~195 ms (0.1.0: ~430 ms). The time is constant from 30 to 400
  elements and from 8 to 15 cached properties, and the same with or without the off-screen filter:
  it is spent in Explorer's UIA provider, not in eyra.


## 0.3.1

### Ear fix
- **Silent stretches no longer vanish from recordings.** WASAPI loopback delivers no packets at
  all while nothing is playing, and eyra used to glue the audible parts together: a 7 s
  `listen --no-trim` came back as 2.15 s with two separate tones merged. Packets are now placed by
  their QPC timestamps and gaps are filled with silence (also at the end of a clip and before each
  `watch` chunk is closed). Same test now: 6.98 s, the 1.6 s pause between the tones preserved.

### Watch fixes
- **Empty session folders were left behind** (one per `watch` session, in
  `%LOCALAPPDATA%\eyra\sessions`). `fs::remove_dir_all` failed with ERROR_SHARING_VIOLATION on an
  empty folder while plain `RemoveDirectoryW` works; files are now removed one by one, then the
  folder, with a short retry. Empty leftovers from older versions are swept when a session starts.
- **The last audio chunk was lost**: it is written while the workers shut down, after the final
  poll. `watch_stop` / `eyra watch` now report those final events in the stop summary.

### Verified live (Windows 10, Realtek)
- `listen` system loopback: test tone (440.0 Hz measured after resampling) and TTS speech; VAD
  segments match the source file.
- `listen --source both`: microphone captured in parallel (full 6.0 s).
- `watch_start` / `watch_poll` / `watch_stop` over MCP and `eyra watch` CLI: speech chunks labelled
  `speech`, temporary files cleaned up on stop.


## 0.3.0

### Token savings
- New `coords="center"` option for `see` / `see_diff` (MCP, CLI `--coords center`, HTTP bridge):
  prints `@x,y` click points instead of `[x,y,w,h]`. With `interactive_only` it brings the densest
  page tested (GitHub repo, Chrome) under the cost of a screenshot.
- Redundant elements are pruned from the map: static labels that repeat their parent control,
  list/grid rows that wrap a same-named link, unnamed grouping rows, unnamed nested documents.
  Same dense GitHub page: 6,742 → 4,563 characters (−32%) in the default format.
- `eyra bench` reports the `interactive_only + coords=center` mode.

### Fixes
- Cold Chrome/Electron windows: the warm-up now triggers when no web document is exposed (the
  toolbar alone used to pass the old element-count check), nudges Chromium with a screen-reader
  style `AccessibleObjectFromWindow(OBJID_CLIENT)` request on the frame and render widgets, and
  polls for up to 2 s. Live result on a freshly started Chrome: the first `see` returns the whole
  page (119 elements) instead of the toolbar (16).
- `see` now includes the window's open Win32 popup menus (`#32768`), so opening a menu and calling
  `see_diff` shows the menu items. Verified live in Notepad.

### Code quality
- All clippy warnings fixed (0 left; one documented `allow` on the 8-column audit logger).
- `cargo fmt` applied to the whole crate.

### Verified live (Windows 10)
- Notepad: background UIA `type_text` (Unicode, including Azerbaijani letters), physical typing with
  Enter, background `click` on menus, `hotkey`, menu items visible via `see_diff`.
- Fresh-profile Edge and Chrome: first-call accessibility on a real GitHub page.


## 0.2.0

### Performance
- UI Automation scan now fetches the whole on-screen control tree of a window in **one
  cross-process call** (cached `TreeScope_Subtree` request with an off-screen filter) instead of
  ~12 `Current*` calls per element. Measured: MCP `see` 175 ms → 22 ms, `see_diff` 175 ms → 23 ms,
  one-shot CLI `see` on Chrome ~205 ms → ~45 ms. Output is byte-identical to 0.1.0.
- `IUIAutomation` and the WinRT `OcrEngine` are created once per thread and reused.
- Background `click`/`type_text` by `#id` locate the element by hit-testing its last known
  position first, avoiding a full tree walk.

### Fixes
- First `see` of a Chromium/Electron window no longer degrades to OCR: eyra detects
  `Chrome_WidgetWin_*` windows and re-scans while Chromium builds its accessibility tree.
- `see_diff` keeps a baseline per window, so switching between windows no longer forces a full
  re-dump. The CLI state file stores all baselines (old single-snapshot files still load).
- Set-of-Mark badges no longer overlap each other and are placed outside the element where
  possible, so they don't cover the text they label. Outlines are drawn before badges.
- MCP `initialize` now negotiates the protocol revision (2025-06-18 / 2025-03-26 / 2024-11-05)
  instead of echoing any client value.

### Token savings
- Hyperlink hrefs (exposed by browsers as UIA values) and values that only repeat the element
  name are omitted from the compact map: −23% tokens on a busy GitHub page.
- `eyra bench` also reports `interactive_only` and states how tokens are estimated.

### MCP
- `instructions` are sent on `initialize` (the low-token workflow).
- Every tool carries `readOnlyHint` / `destructiveHint` annotations.

### Docs
- README rewritten around measured numbers, a when-to-use guide, per-host setup (including
  Claude Code), a tool reference, the element-map format, safety and troubleshooting.
