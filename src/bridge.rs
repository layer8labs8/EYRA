use crate::audit::sanitize_params;
use crate::ear::wasapi::{list_audio_devices, AudioSource};
use crate::ear::{listen, ListenOptions};
use crate::eye::windows::{init_dpi_awareness, list_windows};
use crate::eye::{screenshot, see, see_diff, ScreenshotOptions, SeeOptions};
use crate::hand::{
    click, drag, focus_window, hotkey, scroll, type_text, ClickOptions, MouseButton, TypeOptions,
};
use crate::state::AppState;
use crate::types::CoordStyle;
use crate::types::{estimate_text_tokens, estimate_vision_tokens, Rect, SeeMode};
use crate::watch::{WatchOptions, WatchSession};
use anyhow::{anyhow, Result};
use base64::Engine;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const MAX_BODY_BYTES: usize = 2 * 1024 * 1024; // 2 MB request body cap

#[derive(Debug, Clone)]
pub struct BridgeConfig {
    pub port: u16,
    pub token: String,
    /// Exact origins allowed in addition to loopback (see `validate_extra_origins`).
    pub extra_origins: Vec<String>,
    /// Set when the token was generated and written to disk (never printed).
    pub token_file: Option<PathBuf>,
}

/// 256-bit session token from the OS cryptographic RNG (BCryptGenRandom), as 64 hex chars.
pub fn generate_session_token() -> Result<String> {
    use windows::Win32::Security::Cryptography::{
        BCryptGenRandom, BCRYPT_ALG_HANDLE, BCRYPT_USE_SYSTEM_PREFERRED_RNG,
    };
    let mut bytes = [0u8; 32];
    let status = unsafe {
        BCryptGenRandom(
            BCRYPT_ALG_HANDLE::default(),
            &mut bytes,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status.is_err() {
        return Err(anyhow!("BCryptGenRandom failed (NTSTATUS {:#x})", status.0));
    }
    Ok(bytes.iter().map(|b| format!("{:02x}", b)).collect())
}

/// Where `eyra serve` stores a generated token: a file in the user's own profile, so the token
/// never appears on screen, in terminal scrollback or in a parent process's log.
pub fn token_file_path() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("eyra")
        .join("bridge.token")
}

/// Extra CORS origins must be exact `scheme://host[:port]` values; wildcards are refused.
pub fn validate_extra_origins(origins: &[String]) -> Result<Vec<String>> {
    origins
        .iter()
        .map(|o| {
            let o = o.trim().trim_end_matches('/');
            let ok_scheme = ["http://", "https://", "chrome-extension://", "moz-extension://"]
                .iter()
                .any(|s| o.starts_with(s) && o.len() > s.len());
            if o.contains('*') || !ok_scheme || o[o.find("://").unwrap_or(0) + 3..].contains('/') {
                Err(anyhow!(
                    "--allow-origin \"{}\" must be an exact origin such as https://studio.example.com or chrome-extension://<extension id> (no wildcards or paths)",
                    o
                ))
            } else {
                Ok(o.to_string())
            }
        })
        .collect()
}

pub fn run_bridge_server(state: AppState, cfg: BridgeConfig) -> Result<()> {
    init_dpi_awareness();
    let addr = format!("127.0.0.1:{}", cfg.port);
    let listener = TcpListener::bind(&addr)
        .map_err(|e| anyhow!("Failed to bind local bridge on {}: {}", addr, e))?;

    println!("EYRA_BRIDGE_READY url=http://{}", addr);
    if let Some(path) = &cfg.token_file {
        println!("EYRA_BRIDGE_TOKEN_FILE {}", path.display());
    }
    println!(
        "EYRA_BRIDGE_MODE {}",
        if state.safety.is_read_only() {
            "read-only"
        } else {
            "full (eye+ear+action)"
        }
    );
    let _ = std::io::stdout().flush();

    let shared_state = Arc::new(Mutex::new(state));
    let shared_cfg = Arc::new(cfg);

    for stream_res in listener.incoming() {
        let Ok(stream) = stream_res else {
            continue;
        };
        let st = Arc::clone(&shared_state);
        let cf = Arc::clone(&shared_cfg);
        thread::spawn(move || {
            let _ = handle_connection(stream, &st, &cf);
        });
    }

    Ok(())
}

struct HttpRequest {
    method: String,
    path: String,
    query: HashMap<String, String>,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

struct HttpResponse {
    status: u16,
    reason: &'static str,
    content_type: &'static str,
    body: Vec<u8>,
}

impl HttpResponse {
    fn json(status: u16, reason: &'static str, val: Value) -> Self {
        Self {
            status,
            reason,
            content_type: "application/json; charset=utf-8",
            body: serde_json::to_vec(&val).unwrap_or_else(|_| b"{}".to_vec()),
        }
    }

    fn ok(val: Value) -> Self {
        Self::json(200, "OK", val)
    }

    fn err(status: u16, reason: &'static str, msg: impl Into<String>) -> Self {
        Self::json(status, reason, json!({ "error": msg.into() }))
    }

    fn empty(status: u16, reason: &'static str) -> Self {
        Self {
            status,
            reason,
            content_type: "text/plain",
            body: Vec::new(),
        }
    }
}

fn handle_connection(
    mut stream: TcpStream,
    state: &Arc<Mutex<AppState>>,
    cfg: &BridgeConfig,
) -> Result<()> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(15)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));

    let Some(req) = parse_http_request(&stream)? else {
        return Ok(());
    };

    let origin = req.headers.get("origin").cloned();
    let cors_origin = resolve_cors_origin(origin.as_deref(), &cfg.extra_origins);

    let resp = if req.method == "OPTIONS" {
        HttpResponse::empty(204, "No Content")
    } else {
        route_request(&req, state, cfg)
    };

    write_http_response(&mut stream, &resp, cors_origin.as_deref())?;
    Ok(())
}

fn parse_http_request(stream: &TcpStream) -> Result<Option<HttpRequest>> {
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    let n = reader.read_line(&mut request_line)?;
    if n == 0 {
        return Ok(None);
    }

    let parts: Vec<&str> = request_line.split_whitespace().collect();
    if parts.len() < 2 {
        return Ok(None);
    }
    let method = parts[0].to_ascii_uppercase();
    let raw_target = parts[1];

    let (path, query) = split_path_query(raw_target);

    let mut headers = HashMap::new();
    let mut content_length: usize = 0;

    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line)?;
        if read == 0 || line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim().to_ascii_lowercase();
            let val = v.trim().to_string();
            if key == "content-length" {
                content_length = val.parse::<usize>().unwrap_or(0).min(MAX_BODY_BYTES);
            }
            headers.insert(key, val);
        }
    }

    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body)?;
    }

    Ok(Some(HttpRequest {
        method,
        path,
        query,
        headers,
        body,
    }))
}

fn split_path_query(raw: &str) -> (String, HashMap<String, String>) {
    let mut query = HashMap::new();
    if let Some((p, q)) = raw.split_once('?') {
        for pair in q.split('&') {
            if let Some((k, v)) = pair.split_once('=') {
                query.insert(k.to_string(), v.to_string());
            }
        }
        (p.to_string(), query)
    } else {
        (raw.to_string(), query)
    }
}

/// `http://localhost`, `http://127.0.0.1` or `http://[::1]`, optionally with a numeric port.
fn is_loopback_origin(origin: &str) -> bool {
    ["http://localhost", "http://127.0.0.1", "http://[::1]"]
        .iter()
        .any(|prefix| match origin.strip_prefix(prefix) {
            Some("") => true,
            Some(rest) => rest.strip_prefix(':').is_some_and(|port| {
                !port.is_empty() && port.len() <= 5 && port.bytes().all(|b| b.is_ascii_digit())
            }),
            None => false,
        })
}

/// Browsers may call the bridge only from a loopback page or from an origin the user listed
/// exactly with `--allow-origin` (no blanket chrome-extension:// or *.web.app access).
pub fn resolve_cors_origin(origin: Option<&str>, extra: &[String]) -> Option<String> {
    let orig = origin?.trim();
    if orig.is_empty() {
        return None;
    }
    let allowed = is_loopback_origin(orig) || extra.iter().any(|e| e == orig);
    if allowed {
        Some(orig.to_string())
    } else {
        None
    }
}

fn write_http_response(
    stream: &mut TcpStream,
    resp: &HttpResponse,
    cors_origin: Option<&str>,
) -> Result<()> {
    let mut header = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n",
        resp.status,
        resp.reason,
        resp.content_type,
        resp.body.len()
    );
    if let Some(orig) = cors_origin {
        header.push_str(&format!(
            "Access-Control-Allow-Origin: {}\r\nAccess-Control-Allow-Methods: GET, POST, OPTIONS\r\nAccess-Control-Allow-Headers: Authorization, Content-Type\r\nVary: Origin\r\n",
            orig
        ));
    }
    header.push_str("\r\n");
    stream.write_all(header.as_bytes())?;
    if !resp.body.is_empty() {
        stream.write_all(&resp.body)?;
    }
    stream.flush()?;
    Ok(())
}

fn is_authorized(req: &HttpRequest, cfg: &BridgeConfig, allow_query_token: bool) -> bool {
    if let Some(auth) = req.headers.get("authorization") {
        if let Some(tok) = auth
            .strip_prefix("Bearer ")
            .or_else(|| auth.strip_prefix("bearer "))
        {
            if constant_time_eq(tok.trim(), &cfg.token) {
                return true;
            }
        }
    }
    if allow_query_token {
        if let Some(tok) = req.query.get("token") {
            if constant_time_eq(tok.trim(), &cfg.token) {
                return true;
            }
        }
    }
    false
}

fn constant_time_eq(a: &str, b: &str) -> bool {
    let ab = a.as_bytes();
    let bb = b.as_bytes();
    if ab.len() != bb.len() {
        return false;
    }
    let mut diff = 0u8;
    for (&x, &y) in ab.iter().zip(bb.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

pub fn is_safe_media_filename(name: &str) -> bool {
    if name.is_empty() || name.len() > 96 {
        return false;
    }
    if name.contains('/') || name.contains('\\') || name.contains("..") || name.contains('\0') {
        return false;
    }
    let valid_ext = name.ends_with(".jpg") || name.ends_with(".wav");
    let valid_chars = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.');
    valid_ext && valid_chars
}

fn parse_json_body(req: &HttpRequest) -> Value {
    if req.body.is_empty() {
        json!({})
    } else {
        serde_json::from_slice(&req.body).unwrap_or_else(|_| json!({}))
    }
}

fn route_request(
    req: &HttpRequest,
    state_arc: &Arc<Mutex<AppState>>,
    cfg: &BridgeConfig,
) -> HttpResponse {
    // Public health probe (exposes zero user content)
    if req.method == "GET" && req.path == "/api/health" {
        let read_only = state_arc.lock().unwrap().safety.is_read_only();
        let audio_devices = list_audio_devices().unwrap_or_default();
        return HttpResponse::ok(json!({
            "name": "eyra",
            "version": env!("CARGO_PKG_VERSION"),
            "status": "ok",
            "read_only": read_only,
            "auth_required": true,
            "audio_devices": audio_devices
        }));
    }

    // Media serving route (allows ?token= for <img>/<audio> tags, strict filename check)
    if req.method == "GET" && req.path.starts_with("/api/watch/media/") {
        if !is_authorized(req, cfg, true) {
            return HttpResponse::err(401, "Unauthorized", "Missing or invalid Bearer token.");
        }
        let fname = req.path.trim_start_matches("/api/watch/media/");
        if !is_safe_media_filename(fname) {
            return HttpResponse::err(400, "Bad Request", "Invalid media filename.");
        }
        let session_dir_opt = {
            let st = state_arc.lock().unwrap();
            st.watch_session.as_ref().map(|s| s.session_dir.clone())
        };
        let Some(dir) = session_dir_opt else {
            return HttpResponse::err(404, "Not Found", "No active watch session.");
        };
        let full_path = dir.join(fname);
        let Ok(bytes) = fs::read(&full_path) else {
            return HttpResponse::err(404, "Not Found", "Media file not found or expired.");
        };
        let mime = if fname.ends_with(".wav") {
            "audio/wav"
        } else {
            "image/jpeg"
        };
        return HttpResponse {
            status: 200,
            reason: "OK",
            content_type: mime,
            body: bytes,
        };
    }

    // All remaining /api/* routes require Authorization: Bearer <token>
    if !is_authorized(req, cfg, false) {
        return HttpResponse::err(401, "Unauthorized", "Missing or invalid Bearer token.");
    }

    let t0 = Instant::now();
    let args = parse_json_body(req);

    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/api/windows") => {
            let st = state_arc.lock().unwrap();
            match list_windows(&st.safety) {
                Ok(wins) => {
                    st.audit.log(
                        "http",
                        "list_windows",
                        true,
                        t0.elapsed().as_millis() as u64,
                        wins.len() * 14,
                        None,
                        &format!("Listed {} visible windows", wins.len()),
                    );
                    HttpResponse::ok(json!({ "windows": wins }))
                }
                Err(e) => HttpResponse::err(500, "Internal Server Error", e.to_string()),
            }
        }
        ("POST", "/api/see") => {
            let opts = SeeOptions {
                window: args.get("window").and_then(Value::as_str).map(String::from),
                mode: args
                    .get("mode")
                    .and_then(Value::as_str)
                    .map(SeeMode::parse)
                    .unwrap_or(SeeMode::Auto),
                max_elements: args
                    .get("max_elements")
                    .and_then(Value::as_u64)
                    .unwrap_or(120)
                    .clamp(10, 400) as usize,
                max_depth: 14,
                ocr_lang: args
                    .get("ocr_lang")
                    .and_then(Value::as_str)
                    .map(String::from),
                interactive_only: args
                    .get("interactive_only")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                coords: args
                    .get("coords")
                    .and_then(Value::as_str)
                    .map(CoordStyle::parse)
                    .unwrap_or_default(),
            };
            let mut st = state_arc.lock().unwrap();
            match see(&mut st, &opts) {
                Ok(snap) => {
                    let compact = snap.to_compact_text();
                    let est_tokens = estimate_text_tokens(&compact);
                    let raw_vision_tokens = estimate_vision_tokens(
                        snap.screen_rect.w.max(1) as u32,
                        snap.screen_rect.h.max(1) as u32,
                    );
                    st.audit.log(
                        "http",
                        "see",
                        true,
                        t0.elapsed().as_millis() as u64,
                        est_tokens,
                        Some(args),
                        compact.lines().next().unwrap_or(""),
                    );
                    HttpResponse::ok(json!({
                        "text": compact,
                        "snapshot": snap,
                        "est_tokens": est_tokens,
                        "raw_vision_tokens": raw_vision_tokens
                    }))
                }
                Err(e) => HttpResponse::err(400, "Bad Request", e.to_string()),
            }
        }
        ("POST", "/api/see_diff") => {
            let opts = SeeOptions {
                window: args.get("window").and_then(Value::as_str).map(String::from),
                mode: args
                    .get("mode")
                    .and_then(Value::as_str)
                    .map(SeeMode::parse)
                    .unwrap_or(SeeMode::Auto),
                max_elements: args
                    .get("max_elements")
                    .and_then(Value::as_u64)
                    .unwrap_or(120)
                    .clamp(10, 400) as usize,
                max_depth: 14,
                ocr_lang: None,
                interactive_only: args
                    .get("interactive_only")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                coords: args
                    .get("coords")
                    .and_then(Value::as_str)
                    .map(CoordStyle::parse)
                    .unwrap_or_default(),
            };
            let mut st = state_arc.lock().unwrap();
            match see_diff(&mut st, &opts) {
                Ok((text, snap)) => {
                    let est_tokens = estimate_text_tokens(&text);
                    st.audit.log(
                        "http",
                        "see_diff",
                        true,
                        t0.elapsed().as_millis() as u64,
                        est_tokens,
                        Some(args),
                        text.lines().next().unwrap_or(""),
                    );
                    HttpResponse::ok(json!({
                        "text": text,
                        "snapshot": snap,
                        "est_tokens": est_tokens
                    }))
                }
                Err(e) => HttpResponse::err(400, "Bad Request", e.to_string()),
            }
        }
        ("POST", "/api/screenshot") => {
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
                window: args.get("window").and_then(Value::as_str).map(String::from),
                diff_only: args
                    .get("diff_only")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                crop,
                som: args.get("som").and_then(Value::as_bool).unwrap_or(false),
                max_width: args
                    .get("max_width")
                    .and_then(Value::as_u64)
                    .unwrap_or(1024)
                    .clamp(128, 3840) as u32,
                quality: args
                    .get("quality")
                    .and_then(Value::as_u64)
                    .unwrap_or(65)
                    .clamp(20, 95) as u8,
                format: args
                    .get("format")
                    .and_then(Value::as_str)
                    .unwrap_or("jpeg")
                    .to_string(),
            };
            let mut st = state_arc.lock().unwrap();
            match screenshot(&mut st, &opts) {
                Ok(res) => {
                    let vis_toks = if res.no_change {
                        0
                    } else {
                        estimate_vision_tokens(res.width, res.height)
                    };
                    st.audit.log(
                        "http",
                        "screenshot",
                        true,
                        t0.elapsed().as_millis() as u64,
                        vis_toks,
                        Some(args),
                        &res.summary,
                    );
                    HttpResponse::ok(json!({
                        "summary": res.summary,
                        "mime_type": res.mime_type,
                        "base64": res.base64,
                        "width": res.width,
                        "height": res.height,
                        "no_change": res.no_change,
                        "est_tokens": vis_toks
                    }))
                }
                Err(e) => HttpResponse::err(400, "Bad Request", e.to_string()),
            }
        }
        ("POST", "/api/listen") => {
            // Release lock while capturing audio so other read/health requests don't block
            let opts = ListenOptions {
                seconds: args
                    .get("seconds")
                    .and_then(Value::as_f64)
                    .unwrap_or(6.0)
                    .clamp(1.0, 30.0) as f32,
                source: args
                    .get("source")
                    .and_then(Value::as_str)
                    .map(AudioSource::parse)
                    .unwrap_or(AudioSource::Both),
                trim_silence: args
                    .get("trim_silence")
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
                threshold_db: args
                    .get("threshold_db")
                    .and_then(Value::as_f64)
                    .unwrap_or(-42.0) as f32,
                out_path: None,
            };
            match listen(&opts) {
                Ok(res) => {
                    let clips_json: Vec<Value> =
                        res.clips
                            .iter()
                            .map(|c| {
                                let b64 =
                                    c.path.as_ref().and_then(|p| fs::read(p).ok()).map(|b| {
                                        base64::engine::general_purpose::STANDARD.encode(b)
                                    });
                                json!({
                                    "channel": c.channel,
                                    "device": c.device,
                                    "label": c.label,
                                    "saved_sec": c.saved_sec,
                                    "raw_sec": c.raw_sec,
                                    "rms_db": c.rms_db,
                                    "bytes": c.bytes,
                                    "est_tokens": c.est_tokens,
                                    "mime_type": "audio/wav",
                                    "base64": b64
                                })
                            })
                            .collect();
                    {
                        let st = state_arc.lock().unwrap();
                        st.audit.log(
                            "http",
                            "listen",
                            true,
                            res.elapsed_ms,
                            res.total_audio_tokens,
                            Some(args),
                            res.summary.lines().next().unwrap_or(""),
                        );
                    }
                    HttpResponse::ok(json!({
                        "summary": res.summary,
                        "total_audio_tokens": res.total_audio_tokens,
                        "clips": clips_json
                    }))
                }
                Err(e) => HttpResponse::err(400, "Bad Request", e.to_string()),
            }
        }
        ("POST", "/api/watch/start") => {
            let mut st = state_arc.lock().unwrap();
            if let Some(mut old) = st.watch_session.take() {
                let _ = old.stop(false);
            }
            let opts = WatchOptions {
                window: args.get("window").and_then(Value::as_str).map(String::from),
                audio: args
                    .get("audio")
                    .and_then(Value::as_str)
                    .map(AudioSource::parse)
                    .unwrap_or(AudioSource::Both),
                capture_video: args
                    .get("capture_video")
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
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
            match WatchSession::start(opts, st.locked_window, st.safety.config.clone()) {
                Ok(sess) => {
                    let info = json!({
                        "session_id": sess.id,
                        "target": sess.target_desc,
                        "retention_sec": sess.retention.as_secs()
                    });
                    st.audit.log(
                        "http",
                        "watch_start",
                        true,
                        t0.elapsed().as_millis() as u64,
                        20,
                        Some(args),
                        &format!("Started watch session {}", sess.id),
                    );
                    st.watch_session = Some(sess);
                    HttpResponse::ok(info)
                }
                Err(e) => HttpResponse::err(400, "Bad Request", e.to_string()),
            }
        }
        ("GET", "/api/watch/poll") => {
            let st = state_arc.lock().unwrap();
            let Some(sess) = &st.watch_session else {
                return HttpResponse::ok(json!({ "active": false, "events": [] }));
            };
            let since_seq = req
                .query
                .get("since_seq")
                .and_then(|s| s.parse::<u64>().ok());
            let max_events = req
                .query
                .get("max_events")
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(20)
                .clamp(1, 100);
            let data = sess.poll_structured(since_seq, max_events);
            HttpResponse::ok(data)
        }
        ("POST", "/api/watch/stop") => {
            let mut st = state_arc.lock().unwrap();
            let Some(mut sess) = st.watch_session.take() else {
                return HttpResponse::ok(json!({ "message": "No active watch session." }));
            };
            let keep = args
                .get("keep_files")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let msg = sess.stop(keep);
            st.audit.log(
                "http",
                "watch_stop",
                true,
                t0.elapsed().as_millis() as u64,
                16,
                None,
                &msg,
            );
            HttpResponse::ok(json!({ "message": msg }))
        }
        ("GET", "/api/logs") => {
            let tail = req
                .query
                .get("tail")
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(25)
                .clamp(1, 200);
            let st = state_arc.lock().unwrap();
            let entries = st.audit.read_tail(tail);
            HttpResponse::ok(json!({ "entries": entries }))
        }
        ("POST", action_path) if action_path.starts_with("/api/action/") => {
            handle_action_route(action_path, &args, state_arc, t0)
        }
        _ => HttpResponse::err(
            404,
            "Not Found",
            format!("Unknown route: {} {}", req.method, req.path),
        ),
    }
}

fn handle_action_route(
    path: &str,
    args: &Value,
    state_arc: &Arc<Mutex<AppState>>,
    t0: Instant,
) -> HttpResponse {
    let mut st = state_arc.lock().unwrap();
    if st.safety.is_read_only() {
        return HttpResponse::err(
            403,
            "Forbidden",
            "Action blocked: eyra bridge is running in --read-only mode.",
        );
    }

    let action_name = path.trim_start_matches("/api/action/");
    let is_pwd = st.is_password_element(args.get("id").and_then(Value::as_u64).map(|n| n as u32));
    let safe_params = sanitize_params(action_name, args, is_pwd);

    let res: Result<String> = match action_name {
        "click" => {
            let opts = ClickOptions {
                id: args.get("id").and_then(Value::as_u64).map(|n| n as u32),
                x: args.get("x").and_then(Value::as_i64).map(|n| n as i32),
                y: args.get("y").and_then(Value::as_i64).map(|n| n as i32),
                window: args.get("window").and_then(Value::as_str).map(String::from),
                button: args
                    .get("button")
                    .and_then(Value::as_str)
                    .map(MouseButton::parse)
                    .unwrap_or(MouseButton::Left),
                clicks: args.get("clicks").and_then(Value::as_u64).unwrap_or(1) as u32,
                background: args
                    .get("background")
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
            };
            click(&st, &opts)
        }
        "type" | "type_text" => {
            let Some(text) = args.get("text").and_then(Value::as_str) else {
                return HttpResponse::err(400, "Bad Request", "Missing required parameter: text");
            };
            let opts = TypeOptions {
                text: text.to_string(),
                id: args.get("id").and_then(Value::as_u64).map(|n| n as u32),
                window: args.get("window").and_then(Value::as_str).map(String::from),
                clear: args.get("clear").and_then(Value::as_bool).unwrap_or(false),
                press_enter: args
                    .get("press_enter")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                background: args
                    .get("background")
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
            };
            type_text(&st, &opts)
        }
        "hotkey" => {
            let Some(keys) = args.get("keys").and_then(Value::as_str) else {
                return HttpResponse::err(400, "Bad Request", "Missing required parameter: keys");
            };
            let win = args.get("window").and_then(Value::as_str);
            hotkey(&st, keys, win)
        }
        "scroll" => {
            let dy = args.get("delta_y").and_then(Value::as_i64).unwrap_or(-3) as i32;
            let dx = args.get("delta_x").and_then(Value::as_i64).unwrap_or(0) as i32;
            let x = args.get("x").and_then(Value::as_i64).map(|n| n as i32);
            let y = args.get("y").and_then(Value::as_i64).map(|n| n as i32);
            let win = args.get("window").and_then(Value::as_str);
            scroll(&st, dy, dx, x, y, win)
        }
        "drag" => {
            let (Some(x1), Some(y1), Some(x2), Some(y2)) = (
                args.get("x1").and_then(Value::as_i64).map(|n| n as i32),
                args.get("y1").and_then(Value::as_i64).map(|n| n as i32),
                args.get("x2").and_then(Value::as_i64).map(|n| n as i32),
                args.get("y2").and_then(Value::as_i64).map(|n| n as i32),
            ) else {
                return HttpResponse::err(
                    400,
                    "Bad Request",
                    "Missing required parameters: x1, y1, x2, y2",
                );
            };
            let steps = args.get("steps").and_then(Value::as_u64).unwrap_or(12) as u32;
            let win = args.get("window").and_then(Value::as_str);
            drag(&st, x1, y1, x2, y2, steps, win)
        }
        "focus" | "focus_window" => {
            let Some(win) = args.get("window").and_then(Value::as_str) else {
                return HttpResponse::err(400, "Bad Request", "Missing required parameter: window");
            };
            let lock = args.get("lock").and_then(Value::as_bool).unwrap_or(false);
            focus_window(&mut st, win, lock)
        }
        _ => {
            return HttpResponse::err(404, "Not Found", format!("Unknown action: {}", action_name))
        }
    };

    let ms = t0.elapsed().as_millis() as u64;
    match res {
        Ok(msg) => {
            st.audit.log(
                "http",
                action_name,
                true,
                ms,
                estimate_text_tokens(&msg),
                Some(safe_params),
                &msg,
            );
            HttpResponse::ok(json!({ "ok": true, "message": msg }))
        }
        Err(e) => {
            let emsg = e.to_string();
            st.audit
                .log("http", action_name, false, ms, 0, Some(safe_params), &emsg);
            HttpResponse::err(400, "Bad Request", emsg)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cors_allows_loopback_and_exact_listed_origins_only() {
        assert!(resolve_cors_origin(Some("http://localhost:5173"), &[]).is_some());
        assert!(resolve_cors_origin(Some("http://127.0.0.1:47821"), &[]).is_some());
        assert!(resolve_cors_origin(Some("http://[::1]:8080"), &[]).is_some());
        // no blanket access for extensions or hosted sites any more
        assert!(resolve_cors_origin(Some("chrome-extension://abcdefg"), &[]).is_none());
        assert!(resolve_cors_origin(Some("https://eyra-hybrid.web.app"), &[]).is_none());
        assert!(resolve_cors_origin(Some("https://evil.example.com"), &[]).is_none());
        // look-alike loopback origins are refused
        assert!(resolve_cors_origin(Some("http://localhost.evil.com"), &[]).is_none());
        assert!(resolve_cors_origin(Some("http://localhost:80.evil.com"), &[]).is_none());
        assert!(resolve_cors_origin(Some("http://127.0.0.1.evil.com"), &[]).is_none());
        // an exactly listed origin is allowed, a sibling isn't
        let extra = vec!["chrome-extension://abcdefg".to_string()];
        assert!(resolve_cors_origin(Some("chrome-extension://abcdefg"), &extra).is_some());
        assert!(resolve_cors_origin(Some("chrome-extension://other"), &extra).is_none());
    }

    #[test]
    fn extra_origins_must_be_exact() {
        assert!(validate_extra_origins(&["*".into()]).is_err());
        assert!(validate_extra_origins(&["https://*.web.app".into()]).is_err());
        assert!(validate_extra_origins(&["https://a.web.app/path".into()]).is_err());
        assert!(validate_extra_origins(&["evil.com".into()]).is_err());
        assert_eq!(
            validate_extra_origins(&["https://studio.example.com/".into()]).unwrap(),
            vec!["https://studio.example.com".to_string()]
        );
    }

    #[test]
    fn media_filename_blocks_path_traversal() {
        assert!(is_safe_media_filename("frame_0001.jpg"));
        assert!(is_safe_media_filename("sys_0002.wav"));
        assert!(!is_safe_media_filename("../secret.jpg"));
        assert!(!is_safe_media_filename("sub/frame_0001.jpg"));
        assert!(!is_safe_media_filename("frame_0001.exe"));
        assert!(!is_safe_media_filename(""));
    }

    #[test]
    fn session_token_is_256_bit_random_hex() {
        let a = generate_session_token().unwrap();
        let b = generate_session_token().unwrap();
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }
}
