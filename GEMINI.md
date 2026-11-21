# EYRA: Eye + Ear + Action on Windows

The `eyra` MCP server lets you see, hear and operate the Windows desktop. Follow this order to keep token use low:

1. **Find the window:** `list_windows`.
2. **Look as text, not pixels:** `see(window=..., interactive_only=true, coords="center")` returns a numbered map such as `#2 inp "Search" @700,48`.
3. **Act by `#id`:** `click(id=...)`, `type_text(id=..., text=...)`. These use background UI Automation first, so the mouse doesn't move.
4. **Observe the result:** `see_diff(window=...)` returns only added (`+`), changed (`~`) and removed (`-`) elements, usually under 40 tokens. `#id`s always refer to the latest `see`/`see_diff` of that window.
5. **No UI Automation tree** (games, canvas, remote desktop): use `see(mode="ocr")`.
6. **Pixels only when needed:** `screenshot(crop=[x,y,w,h], max_width=960)`, or `diff_only=true` to get just the changed region.
7. **Audio:** `listen(seconds=8, source="system"|"mic"|"both")` saves a 16 kHz WAV clip you can listen to. For longer media use `watch_start` → `watch_poll` → `watch_stop`.

If `see` reports `WINDOW MINIMIZED`, call `focus_window(window=...)` first.

Safety: password fields are always `[REDACTED]`, password-manager windows are blocked, and the user can stop all input with `Ctrl+Alt+Esc` or by moving the mouse to the top-left corner.
