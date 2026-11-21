---
name: eyra
description: >-
  OS-level Eye + Ear + Action skill powered by the EYRA Rust MCP server (`eyra.exe`).
  Use whenever you need to see real desktop windows, read screen controls as exact text (see_diff: ~97% fewer tokens than screenshots),
  hear system playback or microphone audio without subtitles, watch video/audio timelines together,
  or control Windows apps via background UIA and physical SendInput.
---

# EYRA — OS-Level Eye + Ear + Action Skill

## Core Low-Token Workflow (Always Follow This Order)

1. **Start with text, not screenshots (exact coordinates; 20–80% fewer tokens on the first look):**
   - Call `list_windows` to find the target window (`hwnd`, process, title).
   - Call `see(window=..., mode="auto", interactive_only=true, coords="center")` to get the cheapest compact numbered map (`#1 btn "Pause" @412,630`, `#2 inp "Search" @700,48`). Use `coords="rect"` (the default) only when you need element sizes.
   - On subsequent turns in the same window, call `see_diff` instead of `see` (~97% token savings; usually under 40 tokens). Element `#id`s refer to the latest `see`/`see_diff` of that window.
2. **Use L2 WinRT OCR when UIA is sparse:**
   - For Electron/canvas/game windows where `see(mode="uia")` returns only titlebar controls, call `see(window=..., mode="ocr")` to get exact bounding boxes for visible text with zero image tokens.
3. **Use `screenshot` only when visual understanding is actually needed:**
   - Prefer `crop=[x,y,w,h]` and `max_width=768..960`, `quality=65`.
   - Use `diff_only=true` to inspect only changed pixels (returns 0 image bytes when unchanged).
   - Use `som=true` to draw `#id` badges matching `see` element IDs.
4. **Hear audio without subtitles (`Ear` + `Watch Together`):**
   - Call `listen(seconds=8, source="system"|"mic"|"both")` — pure-Rust WASAPI loopback + VAD trims silence automatically and saves compact `16 kHz` `.wav` files. Read the returned `.wav` path directly with your multimodal audio reader.
   - For continuous video/movie sessions, call `watch_start(window=...)` -> `watch_poll()` -> `watch_stop()`.
5. **Act cleanly & safely (`Action`):**
   - Prefer `click(id=...)` and `type_text(id=..., text=...)` with `background=true` (uses Windows UIA patterns without stealing focus or hijacking the mouse).
   - Fall back to `background=false` or `(x, y)` physical `SendInput` when needed (`hotkey`, `scroll`, `drag`, `focus_window`).
   - Respect safety guardrails: password fields (`[REDACTED]`), sensitive windows (password managers), `--read-only`, corner `(0,0)` failsafe, and `Ctrl+Alt+Esc` kill switch.
