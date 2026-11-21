# EYRA plugin

EYRA gives Claude eyes, ears and hands on the Windows desktop. It reads any window (Chrome, Edge, File Explorer, Office, Settings, Electron apps such as VS Code or Discord, and games through OCR) as a compact numbered map of the controls on screen. Claude then clicks and types by element number instead of guessing pixel positions from screenshots. After each step, `see_diff` returns only what changed, which usually costs fewer than 40 tokens. EYRA can also record system audio and the microphone as short WAV clips.

This plugin adds two things:

- **The EYRA MCP server** (`bin/eyra.exe`): a single standalone Windows executable, built from the open-source Rust code at [github.com/layer8labs8/EYRA](https://github.com/layer8labs8/EYRA). It runs locally and sends nothing over the network.
- **The `eyra` skill**: teaches Claude the low-token workflow (look as text, act by number, check with `see_diff`).

## Install

In Claude Code:

```
/plugin install eyra --marketplace layer8labs8/EYRA
```

## Requirements

Windows 10 or 11 (x64). The server runs where local MCP servers run: Claude Desktop on Windows and Claude Code on Windows.

## Settings

- **Read-only mode**: turn it on to let Claude only look and listen. The mouse and keyboard tools are then removed.

## Safety

Password fields are always redacted, and password-manager windows are blocked. Press `Ctrl+Alt+Esc` or move the mouse to the top-left corner of the screen to stop all input at once. Every tool call is written to a local audit log at `%LOCALAPPDATA%\eyra\audit.jsonl`.

## License

Apache-2.0. Source code, documentation and the full tool reference: [github.com/layer8labs8/EYRA](https://github.com/layer8labs8/EYRA).
