use crate::audit::sanitize_params;
use crate::ear::wasapi::{list_audio_devices, AudioSource};
use crate::ear::{listen, ListenOptions};
use crate::eye::windows::{init_dpi_awareness, list_windows};
use crate::eye::{screenshot, see, see_diff, wait_change, ScreenshotOptions, SeeOptions};
use crate::hand::{
    click, drag, focus_window, hotkey, scroll, type_text, ClickOptions, MouseButton, TypeOptions,
};
use crate::state::AppState;
use crate::types::{estimate_text_tokens, estimate_vision_tokens, CoordStyle, Rect, SeeMode};
use crate::watch::{WatchOptions, WatchSession};
use anyhow::Result;
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};
use std::time::Instant;

/// Newest MCP protocol revision this server speaks; older ones listed below are also accepted.
const PROTOCOL_VERSION: &str = "2025-06-18";
const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

/// Sent to the host in `initialize` so agents follow the cheap workflow without the skill file.
const SERVER_INSTRUCTIONS: &str = "EYRA gives you eyes, ears and hands on the Windows desktop. Cheapest workflow: list_windows -> see(window, interactive_only=true, coords=\"center\") -> act by #id (click/type_text) -> see_diff(window) to observe the result (only changed elements, usually <50 tokens). Use see(mode=\"ocr\") when a canvas/game/remote window has no UI Automation tree. Take a screenshot only when pixels matter; prefer crop, max_width<=960, diff_only=true or som=true. #ids come from the most recent see/see_diff of that window: re-run see after the UI changes. Password fields are always [REDACTED]; password-manager windows are blocked.";

pub fn run_mcp_server(state: &mut AppState) -> Result<()> {
    init_dpi_awareness();
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = stdout.lock();

    for line_res in stdin.lock().lines() {
        let line = match line_res {
            Ok(l) => l,
            Err(_) => break,
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let req: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                let err_resp = json!({
                    "jsonrpc": "2.0",
                    "id": Value::Null,
                    "error": { "code": -32700, "message": format!("Parse error: {}", e) }
                });
                writeln!(out, "{}", err_resp)?;
                out.flush()?;
                continue;
            }
        };

        if let Some(resp) = handle_rpc_request(state, &req) {
            writeln!(out, "{}", resp)?;
            out.flush()?;
        }
    }

    Ok(())
}

pub fn handle_rpc_request(state: &mut AppState, req: &Value) -> Option<Value> {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");

    // Notifications (no id) must not receive a response
    if id.is_none() || method.starts_with("notifications/") {
        return None;
    }
    let id_val = id.unwrap();

    let params = req.get("params").cloned().unwrap_or(Value::Null);

    match method {
        "initialize" => {
            // Echo the client's revision when we support it, otherwise offer our newest one
            // (per the MCP version negotiation rules).
            let client_ver = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .filter(|v| SUPPORTED_PROTOCOL_VERSIONS.contains(v))
                .unwrap_or(PROTOCOL_VERSION);
            Some(json!({
                "jsonrpc": "2.0",
                "id": id_val,
                "result": {
                    "protocolVersion": client_ver,
                    "capabilities": { "tools": {} },
                    "serverInfo": {
                        "name": "eyra",
                        "version": env!("CARGO_PKG_VERSION")
                    },
                    "instructions": SERVER_INSTRUCTIONS
                }
            }))
        }
        "ping" => Some(json!({
            "jsonrpc": "2.0",
            "id": id_val,
            "result": {}
        })),
        "tools/list" => Some(json!({
            "jsonrpc": "2.0",
            "id": id_val,
            "result": {
                "tools": tool_definitions(state.safety.is_read_only())
            }
        })),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let t0 = Instant::now();
            let is_pwd_target = state.is_password_element(arg_u32(&args, "id"));
            let (tool_result, extra_vision_tokens) = dispatch_tool_with_metrics(state, name, &args);
            let elapsed_ms = t0.elapsed().as_millis() as u64;
            let ok = !tool_result
                .get("isError")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let text_body = tool_result
                .get("content")
                .and_then(Value::as_array)
                .and_then(|arr| arr.first())
                .and_then(|c| c.get("text"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let est_tokens = estimate_text_tokens(text_body) + extra_vision_tokens;
            let first_line = text_body.lines().next().unwrap_or("");
            let safe_params = sanitize_params(name, &args, is_pwd_target);
            state.audit.log(
                "mcp",
                name,
                ok,
                elapsed_ms,
                est_tokens,
                Some(safe_params),
                first_line,
            );
            Some(json!({
                "jsonrpc": "2.0",
                "id": id_val,
                "result": tool_result
            }))
        }
        _ => Some(json!({
            "jsonrpc": "2.0",
            "id": id_val,
            "error": {
                "code": -32601,
                "message": format!("Method not found: {}", method)
            }
        })),
    }
}

pub fn tool_definitions(read_only: bool) -> Vec<Value> {
    let mut tools = vec![
        json!({
            "name": "list_windows",
            "description": "List visible desktop windows (hwnd, process, title, rect, flags).",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": "see",
            "description": "Read a window or screen as compact numbered UI elements (#id role name [x,y,w,h]) using UIA + WinRT OCR fallback. Typically 50-90% fewer tokens than a screenshot and gives exact click coordinates.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "window": { "type": "string", "description": "Window hwnd (0x...), title substring, process name, or 'screen'. Omit for locked/foreground window." },
                    "mode": { "type": "string", "enum": ["auto", "uia", "ocr", "both"], "description": "auto (UIA with OCR fallback), uia, ocr, or both." },
                    "max_elements": { "type": "integer", "description": "Max elements to return (default 120)." },
                    "interactive_only": { "type": "boolean", "description": "Only return clickable/editable controls." },
                    "coords": { "type": "string", "enum": ["rect", "center"], "description": "Position format: 'rect' = [x,y,w,h] (default), 'center' = @x,y click point (about a third fewer tokens on dense pages)." },
                    "ocr_lang": { "type": "string", "description": "Optional Windows OCR language tag (e.g. en-US, ru)." }
                }
            }
        }),
        json!({
            "name": "see_diff",
            "description": "Like see, but returns only added (+), changed (~), and removed (-) elements since the previous see call.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "window": { "type": "string" },
                    "mode": { "type": "string", "enum": ["auto", "uia", "ocr", "both"] },
                    "max_elements": { "type": "integer" },
                    "interactive_only": { "type": "boolean" },
                    "coords": { "type": "string", "enum": ["rect", "center"] }
                }
            }
        }),
        json!({
            "name": "screenshot",
            "description": "Capture a compact JPEG/PNG of a window (OBS-style, works in background) or screen. Supports diff-only cropping and Set-of-Mark (#id) overlay.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "window": { "type": "string", "description": "Target window or 'screen'." },
                    "diff_only": { "type": "boolean", "description": "Crop to only the pixel region changed since last screenshot." },
                    "som": { "type": "boolean", "description": "Draw numbered Set-of-Mark boxes matching see() element IDs." },
                    "crop": {
                        "type": "array",
                        "items": { "type": "integer" },
                        "minItems": 4,
                        "maxItems": 4,
                        "description": "Optional screen-space crop [x, y, w, h]."
                    },
                    "max_width": { "type": "integer", "description": "Max output width in pixels (default 1024)." },
                    "quality": { "type": "integer", "description": "JPEG quality 20-95 (default 65)." },
                    "format": { "type": "string", "enum": ["jpeg", "png"] }
                }
            }
        }),
        json!({
            "name": "wait_change",
            "description": "Wait until pixels in the target window or screen change above threshold_pct.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "window": { "type": "string" },
                    "timeout_ms": { "type": "integer", "description": "Max wait in ms (default 5000)." },
                    "poll_ms": { "type": "integer", "description": "Poll interval in ms (default 250)." },
                    "threshold_pct": { "type": "number", "description": "Minimum percent of screen blocks changed (default 1.0)." }
                }
            }
        }),
        json!({
            "name": "list_audio_devices",
            "description": "List active Windows output (loopback) and input (microphone) audio endpoints.",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": "listen",
            "description": "Capture system audio (WASAPI loopback), microphone, or both with silence trimming (VAD) and save compact 16kHz WAV file(s) for multimodal listening.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "seconds": { "type": "number", "description": "Capture duration in seconds (1-60, default 8)." },
                    "source": { "type": "string", "enum": ["both", "system", "mic"], "description": "Audio channel(s) to record (default both)." },
                    "trim_silence": { "type": "boolean", "description": "Strip silent spans via VAD (default true)." },
                    "threshold_db": { "type": "number", "description": "VAD energy threshold in dB (default -42)." }
                }
            }
        }),
        json!({
            "name": "watch_start",
            "description": "Start a background watch-together session capturing scene-change keyframes (.jpg) and VAD-trimmed system/mic audio clips (.wav) in an auto-pruning ring buffer.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "window": { "type": "string", "description": "Target window or 'screen' (omit for foreground)." },
                    "audio": { "type": "string", "enum": ["both", "system", "mic"], "description": "Audio source(s) to monitor (default both)." },
                    "capture_video": { "type": "boolean", "description": "Capture scene-change keyframes (default true)." },
                    "chunk_sec": { "type": "number", "description": "Max audio clip slice in seconds (3-20, default 8)." },
                    "scene_threshold_pct": { "type": "number", "description": "Min % pixel diff to save a keyframe (default 8.0)." },
                    "retention_sec": { "type": "integer", "description": "Rolling ring buffer retention in seconds (default 300)." }
                }
            }
        }),
        json!({
            "name": "watch_poll",
            "description": "Return new timeline events (scene keyframes + audio clips with file paths) from the active watch session.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "since_seq": { "type": "integer", "description": "Optional sequence number to read after (defaults to last polled)." },
                    "max_events": { "type": "integer", "description": "Max events to return (default 20)." }
                }
            }
        }),
        json!({
            "name": "watch_stop",
            "description": "Stop the active watch session and delete temporary clips/keyframes unless keep_files=true.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "keep_files": { "type": "boolean", "description": "Keep session WAV/JPG files on disk (default false)." }
                }
            }
        }),
    ];

    if !read_only {
        tools.extend([
            json!({
                "name": "click",
                "description": "Click an element by #id (tries background UIA first) or screen coordinates (x, y).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "id": { "type": "integer", "description": "Element ID from last see()." },
                        "x": { "type": "integer" },
                        "y": { "type": "integer" },
                        "window": { "type": "string" },
                        "button": { "type": "string", "enum": ["left", "right", "middle"] },
                        "clicks": { "type": "integer", "description": "1 for single click, 2 for double click." },
                        "background": { "type": "boolean", "description": "Try background UIA invoke before SendInput (default true)." }
                    }
                }
            }),
            json!({
                "name": "type_text",
                "description": "Type Unicode text into an element (#id) or focused input. Supports background UIA SetValue.",
                "inputSchema": {
                    "type": "object",
                    "required": ["text"],
                    "properties": {
                        "text": { "type": "string" },
                        "id": { "type": "integer", "description": "Optional element ID from last see()." },
                        "window": { "type": "string" },
                        "clear": { "type": "boolean", "description": "Select all and clear before typing." },
                        "press_enter": { "type": "boolean", "description": "Press Enter after typing." },
                        "background": { "type": "boolean", "description": "Try background UIA SetValue first (default true)." }
                    }
                }
            }),
            json!({
                "name": "hotkey",
                "description": "Send a keyboard shortcut (e.g. 'ctrl+c', 'alt+tab', 'enter', 'f5').",
                "inputSchema": {
                    "type": "object",
                    "required": ["keys"],
                    "properties": {
                        "keys": { "type": "string" },
                        "window": { "type": "string" }
                    }
                }
            }),
            json!({
                "name": "scroll",
                "description": "Scroll vertically (delta_y: positive up, negative down) or horizontally (delta_x).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "delta_y": { "type": "integer", "description": "Wheel detents vertically (default -3)." },
                        "delta_x": { "type": "integer", "description": "Wheel detents horizontally (default 0)." },
                        "x": { "type": "integer" },
                        "y": { "type": "integer" },
                        "window": { "type": "string" }
                    }
                }
            }),
            json!({
                "name": "drag",
                "description": "Drag mouse with left button held from (x1, y1) to (x2, y2).",
                "inputSchema": {
                    "type": "object",
                    "required": ["x1", "y1", "x2", "y2"],
                    "properties": {
                        "x1": { "type": "integer" },
                        "y1": { "type": "integer" },
                        "x2": { "type": "integer" },
                        "y2": { "type": "integer" },
                        "steps": { "type": "integer" },
                        "window": { "type": "string" }
                    }
                }
            }),
            json!({
                "name": "focus_window",
                "description": "Restore and focus a window by hwnd, title substring, or process name; optionally lock it as default target.",
                "inputSchema": {
                    "type": "object",
                    "required": ["window"],
                    "properties": {
                        "window": { "type": "string", "description": "Target window or 'unlock' to clear window lock." },
                        "lock": { "type": "boolean", "description": "Pin this window as default target for subsequent calls." }
                    }
                }
            }),
        ]);
    }

    for tool in tools.iter_mut() {
        let name = tool["name"].as_str().unwrap_or_default().to_string();
        tool["annotations"] = tool_annotations(&name);
    }
    tools
}

/// MCP tool annotations let hosts auto-approve observation tools and prompt for input tools.
fn tool_annotations(name: &str) -> Value {
    let read_only = !matches!(
        name,
        "click" | "type_text" | "hotkey" | "scroll" | "drag" | "focus_window"
    );
    json!({
        "readOnlyHint": read_only,
        "destructiveHint": !read_only,
        "idempotentHint": read_only,
        "openWorldHint": false
    })
}

fn ok_text(text: String) -> Value {
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": false
    })
}

fn err_text(msg: String) -> Value {
    json!({
        "content": [{ "type": "text", "text": format!("ERROR: {}", msg) }],
        "isError": true
    })
}

fn arg_str(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(|s| s.to_string())
}

fn arg_i32(args: &Value, key: &str) -> Option<i32> {
    args.get(key).and_then(Value::as_i64).map(|n| n as i32)
}

fn arg_u32(args: &Value, key: &str) -> Option<u32> {
    args.get(key).and_then(Value::as_u64).map(|n| n as u32)
}

fn arg_bool(args: &Value, key: &str, default: bool) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(default)
}

fn dispatch_tool_with_metrics(state: &mut AppState, name: &str, args: &Value) -> (Value, usize) {
    if name == "screenshot" {
        let crop = args.get("crop").and_then(Value::as_array).and_then(|arr| {
            if arr.len() == 4 {
                Some(Rect::new(
                    arr[0].as_i64()? as i32,
                    arr[1].as_i64()? as i32,
                    arr[2].as_i64()? as i32,
                    arr[3].as_i64()? as i32,
                ))
            } else {
                None
            }
        });
        let opts = ScreenshotOptions {
            window: arg_str(args, "window"),
            diff_only: arg_bool(args, "diff_only", false),
            crop,
            som: arg_bool(args, "som", false),
            max_width: arg_u32(args, "max_width").unwrap_or(1024).clamp(128, 3840),
            quality: arg_u32(args, "quality").unwrap_or(65).clamp(20, 95) as u8,
            format: arg_str(args, "format").unwrap_or_else(|| "jpeg".to_string()),
        };
        return match screenshot(state, &opts) {
            Ok(res) => {
                if res.no_change {
                    (ok_text(res.summary), 0)
                } else {
                    let vis_toks = estimate_vision_tokens(res.width, res.height);
                    (
                        json!({
                            "content": [
                                { "type": "text", "text": res.summary },
                                { "type": "image", "data": res.base64, "mimeType": res.mime_type }
                            ],
                            "isError": false
                        }),
                        vis_toks,
                    )
                }
            }
            Err(e) => (err_text(e.to_string()), 0),
        };
    }

    (dispatch_tool(state, name, args), 0)
}

fn dispatch_tool(state: &mut AppState, name: &str, args: &Value) -> Value {
    match name {
        "list_windows" => match list_windows(&state.safety) {
            Ok(wins) => {
                let mut out = format!("WINDOWS ({}):\n", wins.len());
                for w in &wins {
                    out.push_str(&w.to_compact_line());
                    out.push('\n');
                }
                ok_text(out)
            }
            Err(e) => err_text(e.to_string()),
        },
        "see" => {
            let opts = SeeOptions {
                window: arg_str(args, "window"),
                mode: arg_str(args, "mode")
                    .map(|s| SeeMode::parse(&s))
                    .unwrap_or(SeeMode::Auto),
                max_elements: arg_u32(args, "max_elements").unwrap_or(120).clamp(10, 400) as usize,
                max_depth: 14,
                ocr_lang: arg_str(args, "ocr_lang"),
                interactive_only: arg_bool(args, "interactive_only", false),
                coords: arg_str(args, "coords")
                    .map(|c| CoordStyle::parse(&c))
                    .unwrap_or_default(),
            };
            match see(state, &opts) {
                Ok(snap) => ok_text(snap.to_compact_text()),
                Err(e) => err_text(e.to_string()),
            }
        }
        "see_diff" => {
            let opts = SeeOptions {
                window: arg_str(args, "window"),
                mode: arg_str(args, "mode")
                    .map(|s| SeeMode::parse(&s))
                    .unwrap_or(SeeMode::Auto),
                max_elements: arg_u32(args, "max_elements").unwrap_or(120).clamp(10, 400) as usize,
                max_depth: 14,
                ocr_lang: None,
                interactive_only: arg_bool(args, "interactive_only", false),
                coords: arg_str(args, "coords")
                    .map(|c| CoordStyle::parse(&c))
                    .unwrap_or_default(),
            };
            match see_diff(state, &opts) {
                Ok((text, _)) => ok_text(text),
                Err(e) => err_text(e.to_string()),
            }
        }
        "screenshot" => {
            let crop = args.get("crop").and_then(Value::as_array).and_then(|arr| {
                if arr.len() == 4 {
                    Some(Rect::new(
                        arr[0].as_i64()? as i32,
                        arr[1].as_i64()? as i32,
                        arr[2].as_i64()? as i32,
                        arr[3].as_i64()? as i32,
                    ))
                } else {
                    None
                }
            });
            let opts = ScreenshotOptions {
                window: arg_str(args, "window"),
                diff_only: arg_bool(args, "diff_only", false),
                crop,
                som: arg_bool(args, "som", false),
                max_width: arg_u32(args, "max_width").unwrap_or(1024).clamp(128, 3840),
                quality: arg_u32(args, "quality").unwrap_or(65).clamp(20, 95) as u8,
                format: arg_str(args, "format").unwrap_or_else(|| "jpeg".to_string()),
            };
            match screenshot(state, &opts) {
                Ok(res) => {
                    if res.no_change {
                        ok_text(res.summary)
                    } else {
                        json!({
                            "content": [
                                { "type": "text", "text": res.summary },
                                { "type": "image", "data": res.base64, "mimeType": res.mime_type }
                            ],
                            "isError": false
                        })
                    }
                }
                Err(e) => err_text(e.to_string()),
            }
        }
        "wait_change" => {
            let window = arg_str(args, "window");
            let timeout_ms = args
                .get("timeout_ms")
                .and_then(Value::as_u64)
                .unwrap_or(5000);
            let poll_ms = args.get("poll_ms").and_then(Value::as_u64).unwrap_or(250);
            let threshold_pct = args
                .get("threshold_pct")
                .and_then(Value::as_f64)
                .unwrap_or(1.0) as f32;
            match wait_change(state, window.as_deref(), timeout_ms, poll_ms, threshold_pct) {
                Ok(r) => {
                    let rect_str = r
                        .changed_rect
                        .map(|rc| format!(" changed_rect={},{},{},{}", rc.x, rc.y, rc.w, rc.h))
                        .unwrap_or_default();
                    ok_text(format!(
                        "WAIT_CHANGE changed={} diff={:.2}%{} ms={}",
                        r.changed, r.changed_percent, rect_str, r.elapsed_ms
                    ))
                }
                Err(e) => err_text(e.to_string()),
            }
        }
        "list_audio_devices" => match list_audio_devices() {
            Ok(devs) => {
                let mut out = format!("AUDIO_DEVICES ({}):\n", devs.len());
                for d in &devs {
                    out.push_str(&d.to_compact_line());
                    out.push('\n');
                }
                ok_text(out)
            }
            Err(e) => err_text(e.to_string()),
        },
        "listen" => {
            let opts = ListenOptions {
                seconds: args.get("seconds").and_then(Value::as_f64).unwrap_or(8.0) as f32,
                source: arg_str(args, "source")
                    .map(|s| AudioSource::parse(&s))
                    .unwrap_or(AudioSource::Both),
                trim_silence: arg_bool(args, "trim_silence", true),
                threshold_db: args
                    .get("threshold_db")
                    .and_then(Value::as_f64)
                    .unwrap_or(-42.0) as f32,
                out_path: None,
            };
            match listen(&opts) {
                Ok(res) => ok_text(res.summary),
                Err(e) => err_text(e.to_string()),
            }
        }
        "watch_start" => {
            if let Some(mut old) = state.watch_session.take() {
                let _ = old.stop(false);
            }
            let opts = WatchOptions {
                window: arg_str(args, "window"),
                audio: arg_str(args, "audio")
                    .map(|s| AudioSource::parse(&s))
                    .unwrap_or(AudioSource::Both),
                capture_video: arg_bool(args, "capture_video", true),
                chunk_sec: args.get("chunk_sec").and_then(Value::as_f64).unwrap_or(8.0) as f32,
                scene_threshold_pct: args
                    .get("scene_threshold_pct")
                    .and_then(Value::as_f64)
                    .unwrap_or(8.0) as f32,
                retention_sec: args
                    .get("retention_sec")
                    .and_then(Value::as_u64)
                    .unwrap_or(300),
                ..Default::default()
            };
            match WatchSession::start(opts, state.locked_window, state.safety.config.clone()) {
                Ok(sess) => {
                    let msg = format!(
                        "WATCH_START session={} target={} retention={}s dir={}",
                        sess.id,
                        sess.target_desc,
                        sess.retention.as_secs(),
                        sess.session_dir.display()
                    );
                    state.watch_session = Some(sess);
                    ok_text(msg)
                }
                Err(e) => err_text(e.to_string()),
            }
        }
        "watch_poll" => {
            let Some(sess) = &state.watch_session else {
                return err_text("No active watch session. Call watch_start first.".to_string());
            };
            let since_seq = args.get("since_seq").and_then(Value::as_u64);
            let max_events = arg_u32(args, "max_events").unwrap_or(20).clamp(1, 100) as usize;
            ok_text(sess.poll(since_seq, max_events))
        }
        "watch_stop" => {
            let Some(mut sess) = state.watch_session.take() else {
                return err_text("No active watch session.".to_string());
            };
            let keep_files = arg_bool(args, "keep_files", false);
            ok_text(sess.stop(keep_files))
        }
        "click" => {
            let opts = ClickOptions {
                id: arg_u32(args, "id"),
                x: arg_i32(args, "x"),
                y: arg_i32(args, "y"),
                window: arg_str(args, "window"),
                button: arg_str(args, "button")
                    .map(|s| MouseButton::parse(&s))
                    .unwrap_or(MouseButton::Left),
                clicks: arg_u32(args, "clicks").unwrap_or(1),
                background: arg_bool(args, "background", true),
            };
            match click(state, &opts) {
                Ok(msg) => ok_text(msg),
                Err(e) => err_text(e.to_string()),
            }
        }
        "type_text" => {
            let Some(text) = arg_str(args, "text") else {
                return err_text("Missing required parameter: text".to_string());
            };
            let opts = TypeOptions {
                text,
                id: arg_u32(args, "id"),
                window: arg_str(args, "window"),
                clear: arg_bool(args, "clear", false),
                press_enter: arg_bool(args, "press_enter", false),
                background: arg_bool(args, "background", true),
            };
            match type_text(state, &opts) {
                Ok(msg) => ok_text(msg),
                Err(e) => err_text(e.to_string()),
            }
        }
        "hotkey" => {
            let Some(keys) = arg_str(args, "keys") else {
                return err_text("Missing required parameter: keys".to_string());
            };
            let window = arg_str(args, "window");
            match hotkey(state, &keys, window.as_deref()) {
                Ok(msg) => ok_text(msg),
                Err(e) => err_text(e.to_string()),
            }
        }
        "scroll" => {
            let delta_y = arg_i32(args, "delta_y").unwrap_or(-3);
            let delta_x = arg_i32(args, "delta_x").unwrap_or(0);
            let x = arg_i32(args, "x");
            let y = arg_i32(args, "y");
            let window = arg_str(args, "window");
            match scroll(state, delta_y, delta_x, x, y, window.as_deref()) {
                Ok(msg) => ok_text(msg),
                Err(e) => err_text(e.to_string()),
            }
        }
        "drag" => {
            let (Some(x1), Some(y1), Some(x2), Some(y2)) = (
                arg_i32(args, "x1"),
                arg_i32(args, "y1"),
                arg_i32(args, "x2"),
                arg_i32(args, "y2"),
            ) else {
                return err_text("Missing required parameters: x1, y1, x2, y2".to_string());
            };
            let steps = arg_u32(args, "steps").unwrap_or(12);
            let window = arg_str(args, "window");
            match drag(state, x1, y1, x2, y2, steps, window.as_deref()) {
                Ok(msg) => ok_text(msg),
                Err(e) => err_text(e.to_string()),
            }
        }
        "focus_window" => {
            let Some(window) = arg_str(args, "window") else {
                return err_text("Missing required parameter: window".to_string());
            };
            if window.eq_ignore_ascii_case("unlock") {
                state.locked_window = None;
                return ok_text("Unlocked default target window.".to_string());
            }
            let lock = arg_bool(args, "lock", false);
            match focus_window(state, &window, lock) {
                Ok(msg) => ok_text(msg),
                Err(e) => err_text(e.to_string()),
            }
        }
        _ => err_text(format!("Unknown tool: {}", name)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::safety::SafetyConfig;

    #[test]
    fn mcp_initialize_and_read_only_tools_list() {
        let cfg = SafetyConfig {
            read_only: true,
            ..Default::default()
        };
        let mut state = AppState::new(cfg);

        let init_req = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "protocolVersion": "2024-11-05" }
        });
        let resp = handle_rpc_request(&mut state, &init_req).unwrap();
        assert_eq!(resp["result"]["serverInfo"]["name"], "eyra");

        let list_req = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list"
        });
        let list_resp = handle_rpc_request(&mut state, &list_req).unwrap();
        let tools = list_resp["result"]["tools"].as_array().unwrap();
        // Read-only should expose only the 10 eye/ear/watch tools
        assert_eq!(tools.len(), 10);
        assert!(tools.iter().all(|t| t["name"] != "click"));

        // Write call in read-only returns isError: true
        let click_req = json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": { "name": "click", "arguments": { "x": 100, "y": 100 } }
        });
        let click_resp = handle_rpc_request(&mut state, &click_req).unwrap();
        assert_eq!(click_resp["result"]["isError"], true);
    }

    #[test]
    fn mcp_full_mode_exposes_eye_and_hand_tools() {
        let cfg = SafetyConfig {
            read_only: false,
            ..Default::default()
        };
        let mut state = AppState::new(cfg);

        let list_req = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/list"
        });
        let list_resp = handle_rpc_request(&mut state, &list_req).unwrap();
        let tools = list_resp["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 16);
    }

    #[test]
    fn mcp_negotiates_protocol_and_annotates_tools() {
        let mut state = AppState::new(SafetyConfig::default());
        let init = |v: &str| {
            json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize",
                    "params": { "protocolVersion": v } })
        };
        // Supported revision is echoed back
        let r = handle_rpc_request(&mut state, &init("2025-03-26")).unwrap();
        assert_eq!(r["result"]["protocolVersion"], "2025-03-26");
        assert!(r["result"]["instructions"]
            .as_str()
            .unwrap()
            .contains("see_diff"));
        // Unknown revision -> server offers its newest one
        let r = handle_rpc_request(&mut state, &init("1999-01-01")).unwrap();
        assert_eq!(r["result"]["protocolVersion"], PROTOCOL_VERSION);

        let list = handle_rpc_request(
            &mut state,
            &json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
        )
        .unwrap();
        for t in list["result"]["tools"].as_array().unwrap() {
            let name = t["name"].as_str().unwrap();
            let ro = t["annotations"]["readOnlyHint"].as_bool().unwrap();
            let is_action = matches!(
                name,
                "click" | "type_text" | "hotkey" | "scroll" | "drag" | "focus_window"
            );
            assert_eq!(ro, !is_action, "wrong readOnlyHint for {}", name);
        }
    }
}
