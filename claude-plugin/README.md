# EYRA plugin

EYRA gives Claude eyes, ears and hands on the Windows desktop. It reads any window (Chrome, Edge, File Explorer, Office, Settings, Electron apps such as VS Code or Discord, and games through OCR) as a compact numbered map of the controls on screen. Claude then clicks and types by element number instead of guessing pixel positions from screenshots. After each step, `see_diff` returns only what changed, which usually costs fewer than 40 tokens. EYRA can also record system audio and the microphone as short WAV clips.

This plugin adds two things:

- **The EYRA MCP server**: a single standalone Windows executable, built from the open-source Rust code at [github.com/layer8labs8/EYRA](https://github.com/layer8labs8/EYRA) on GitHub Actions. On install, Claude Code downloads it as an MCP bundle (`.mcpb`) from the matching GitHub release, where every file carries a signed build attestation. It runs locally and sends nothing over the network.
- **The `eyra` skill**: teaches Claude the low-token workflow (look as text, act by number, check with `see_diff`).

## Install

In Claude Code:

```
/plugin install eyra --marketplace layer8labs8/EYRA
```

## Requirements

Windows 10 or 11 (x64). The server runs where local MCP servers run: Claude Desktop on Windows and Claude Code on Windows.

## Settings

To use EYRA in look-and-listen mode only (no mouse or keyboard tools), install the Claude Desktop extension and turn on *Read-only mode*, or run `eyra.exe mcp --read-only` as a custom MCP server.

## Safety

Password fields are always redacted, and password-manager windows are blocked. Press `Ctrl+Alt+Esc` or move the mouse to the top-left corner of the screen to stop all input at once. Every tool call is written to a local audit log at `%LOCALAPPDATA%\eyra\audit.jsonl`.

## License

Apache-2.0. Source code, documentation and the full tool reference: [github.com/layer8labs8/EYRA](https://github.com/layer8labs8/EYRA).
