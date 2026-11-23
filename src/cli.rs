use crate::audit::{sanitize_params, AuditLogger};
use crate::bridge::{generate_session_token, run_bridge_server, BridgeConfig};
use crate::ear::wasapi::{list_audio_devices, AudioSource};
use crate::ear::{listen, ListenOptions};
use crate::eye::ocr::available_languages;
use crate::eye::windows::{init_dpi_awareness, list_windows};
use crate::eye::{screenshot, see, see_diff, wait_change, ScreenshotOptions, SeeOptions};
use crate::hand::{
    click, drag, focus_window, hotkey, scroll, type_text, ClickOptions, MouseButton, TypeOptions,
};
use crate::mcp::run_mcp_server;
use crate::safety::SafetyConfig;
use crate::state::AppState;
use crate::types::CoordStyle;
use crate::types::{estimate_text_tokens, estimate_vision_tokens, Rect, SeeMode};
use crate::watch::{WatchOptions, WatchSession};
use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use serde_json::json;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

#[derive(Parser, Debug)]
#[command(
    name = "eyra",
    version,
    about = "EYRA (Eye + Ear + Action): Hybrid OS-level vision, audio capture, watch-together timeline, and input automation MCP server & CLI for Windows."
)]
pub struct Cli {
    /// Disable all mouse/keyboard input tools (eye + ear only mode).
    #[arg(long, global = true)]
    pub read_only: bool,

    /// Allow reading/interacting with password managers and sensitive windows.
    #[arg(long, global = true)]
    pub allow_sensitive: bool,

    /// Custom JSONL audit log file path (default: %LOCALAPPDATA%/eyra/audit.jsonl).
    #[arg(long, global = true)]
    pub log_file: Option<PathBuf>,

    /// Disable audit logging completely.
    #[arg(long, global = true)]
    pub no_log: bool,

    /// Allow mouse/keyboard actions only in these apps, e.g. --allow-apps chrome.exe,notepad.exe
    /// (env: EYRA_ALLOW_APPS).
    #[arg(long, global = true, value_name = "EXE,...")]
    pub allow_apps: Option<String>,

    /// Maximum input actions per minute; 0 = unlimited (default 60, env: EYRA_MAX_ACTIONS_PER_MIN).
    #[arg(long, global = true)]
    pub max_actions_per_minute: Option<u32>,

    /// Allow microphone capture in listen/watch (off by default, env: EYRA_ALLOW_MIC).
    #[arg(long, global = true)]
    pub allow_mic: bool,

    /// Ask in a Windows dialog before actions: risky (default: delete, send, pay, publish...),
    /// all, or off (env: EYRA_CONFIRM).
    #[arg(long, global = true, value_name = "risky|all|off")]
    pub confirm: Option<String>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Start the stdio MCP JSON-RPC server.
    Mcp,

    /// Start the local 127.0.0.1 HTTP bridge for EYRA Web Studio and Chrome Side Panel.
    Serve {
        /// Localhost TCP port (default 47821).
        #[arg(short, long, default_value_t = 47821)]
        port: u16,
        /// Optional static Bearer token (auto-generated random token if omitted).
        #[arg(long)]
        token: Option<String>,
        /// Exact extra CORS origin(s) allowed besides loopback pages, e.g.
        /// chrome-extension://<extension id> or https://studio.example.com (no wildcards).
        #[arg(long = "allow-origin")]
        allow_origins: Vec<String>,
    },

    /// List visible desktop windows.
    Windows {
        #[arg(long)]
        json: bool,
    },

    /// Read a window or screen as compact numbered UI elements (UIA + WinRT OCR).
    See {
        /// Window hwnd (0x...), title substring, process name, or 'screen'.
        #[arg(short, long)]
        window: Option<String>,
        /// auto, uia, ocr, or both.
        #[arg(short, long, default_value = "auto")]
        mode: String,
        #[arg(long, default_value_t = 120)]
        max_elements: usize,
        #[arg(long)]
        interactive_only: bool,
        /// Position format: rect ([x,y,w,h]) or center (@x,y, fewer tokens).
        #[arg(long, default_value = "rect")]
        coords: String,
        #[arg(long)]
        ocr_lang: Option<String>,
        #[arg(long)]
        json: bool,
    },

    /// Show only UI elements added/changed/removed since the last `see` command.
    Diff {
        #[arg(short, long)]
        window: Option<String>,
        #[arg(short, long, default_value = "auto")]
        mode: String,
        #[arg(long, default_value_t = 120)]
        max_elements: usize,
        #[arg(long)]
        interactive_only: bool,
        /// Position format: rect ([x,y,w,h]) or center (@x,y, fewer tokens).
        #[arg(long, default_value = "rect")]
        coords: String,
    },

    /// Capture a window (OBS-style PrintWindow) or screen image to disk.
    Screenshot {
        #[arg(short, long)]
        window: Option<String>,
        #[arg(short, long, default_value = "eyra_screenshot.jpg")]
        out: PathBuf,
        /// Draw Set-of-Mark numbered bounding boxes matching `see` IDs.
        #[arg(long)]
        som: bool,
        /// Optional screen-space crop: x,y,w,h.
        #[arg(long)]
        crop: Option<String>,
        #[arg(long, default_value_t = 1024)]
        max_width: u32,
        #[arg(long, default_value_t = 65)]
        quality: u8,
        #[arg(long, default_value = "jpeg")]
        format: String,
    },

    /// Click an element by #id (from `see`) or coordinates (--x --y).
    Click {
        #[arg(long)]
        id: Option<u32>,
        #[arg(long)]
        x: Option<i32>,
        #[arg(long)]
        y: Option<i32>,
        #[arg(short, long)]
        window: Option<String>,
        #[arg(long, default_value = "left")]
        button: String,
        #[arg(long, default_value_t = 1)]
        clicks: u32,
        /// Skip background UIA invoke and use physical SendInput directly.
        #[arg(long)]
        no_background: bool,
    },

    /// Type Unicode text into an element (#id) or the currently focused input.
    Type {
        text: String,
        #[arg(long)]
        id: Option<u32>,
        #[arg(short, long)]
        window: Option<String>,
        #[arg(long)]
        clear: bool,
        #[arg(long)]
        enter: bool,
        #[arg(long)]
        no_background: bool,
    },

    /// Send a keyboard shortcut (e.g. ctrl+c, alt+tab, enter).
    Hotkey {
        keys: String,
        #[arg(short, long)]
        window: Option<String>,
    },

    /// Scroll vertically (positive up, negative down) or horizontally.
    Scroll {
        #[arg(allow_hyphen_values = true)]
        delta_y: i32,
        #[arg(long, default_value_t = 0, allow_hyphen_values = true)]
        delta_x: i32,
        #[arg(long)]
        x: Option<i32>,
        #[arg(long)]
        y: Option<i32>,
        #[arg(short, long)]
        window: Option<String>,
    },

    /// Drag mouse from (x1, y1) to (x2, y2).
    Drag {
        x1: i32,
        y1: i32,
        x2: i32,
        y2: i32,
        #[arg(long, default_value_t = 12)]
        steps: u32,
        #[arg(short, long)]
        window: Option<String>,
    },

    /// Restore and bring a window to the foreground.
    Focus { window: String },

    /// Wait until pixels in the window/screen change above threshold percentage.
    WaitChange {
        #[arg(short, long)]
        window: Option<String>,
        #[arg(long, default_value_t = 5000)]
        timeout_ms: u64,
        #[arg(long, default_value_t = 250)]
        poll_ms: u64,
        /// Percent of 16x16 pixel blocks that must change (default 0.2: catches a status line
        /// changing, ignores a blinking caret).
        #[arg(long, default_value_t = 0.2)]
        threshold: f32,
    },

    /// Inspect or clear eyra's JSONL audit log.
    Logs {
        /// Number of most recent entries to display.
        #[arg(short = 'n', long, default_value_t = 25)]
        tail: usize,
        /// Output raw JSON lines.
        #[arg(long)]
        json: bool,
        /// Clear the audit log file.
        #[arg(long)]
        clear: bool,
    },

    /// List active Windows output (loopback) and input (microphone) audio endpoints.
    AudioDevices {
        #[arg(long)]
        json: bool,
    },

    /// Capture system audio (WASAPI loopback), microphone, or both with VAD silence trimming.
    Listen {
        /// Capture duration in seconds (1-60).
        #[arg(short = 's', long, default_value_t = 8.0)]
        seconds: f32,
        /// Audio source: both, system, or mic.
        #[arg(long, default_value = "both")]
        source: String,
        /// Disable VAD silence trimming and save the full raw clip.
        #[arg(long)]
        no_trim: bool,
        /// VAD threshold in dB.
        #[arg(long, default_value_t = -42.0, allow_hyphen_values = true)]
        threshold_db: f32,
        /// Optional custom output WAV file path.
        #[arg(short, long)]
        out: Option<PathBuf>,
    },

    /// Run a watch-together session capturing scene-change keyframes + VAD audio clips in real time.
    Watch {
        /// Target window hwnd, title substring, process name, or 'screen'.
        #[arg(short, long)]
        window: Option<String>,
        /// Audio source: both, system, or mic.
        #[arg(long, default_value = "both")]
        audio: String,
        /// Duration to watch in seconds before stopping cleanly.
        #[arg(short = 'd', long, default_value_t = 30)]
        duration: u64,
        /// Disable video keyframe capture (audio-only mode).
        #[arg(long)]
        audio_only: bool,
        /// Max audio clip slice length in seconds.
        #[arg(long, default_value_t = 8.0)]
        chunk_sec: f32,
        /// Minimum % pixel diff to capture a new scene keyframe.
        #[arg(long, default_value_t = 8.0)]
        scene_threshold: f32,
        /// Rolling ring buffer retention in seconds.
        #[arg(long, default_value_t = 300)]
        retention: u64,
        /// Keep saved session WAV/JPG files on disk after stopping.
        #[arg(long)]
        keep_files: bool,
    },

    /// List installed Windows WinRT OCR languages.
    OcrLangs,

    /// Benchmark token savings of `eyra see` and `eyra diff` versus raw Vision LLM screenshots.
    Bench {
        #[arg(short, long)]
        window: Option<String>,
    },

    /// Add EYRA to an AI app's MCP config: antigravity, gemini-cli, cursor, vscode, windsurf,
    /// claude-desktop, claude-code. Run without an app (or with --list) to see what's installed.
    Setup {
        /// App to configure.
        app: Option<String>,
        /// Show every supported app and whether EYRA is configured in it.
        #[arg(long)]
        list: bool,
        /// Remove EYRA from the app's config instead of adding it.
        #[arg(long)]
        remove: bool,
        /// Show what would change without writing anything.
        #[arg(long)]
        dry_run: bool,
        /// Path to eyra.exe to register (default: this executable).
        #[arg(long)]
        exe: Option<std::path::PathBuf>,
    },
}

pub fn run(cli: Cli) -> Result<()> {
    if let Command::Setup {
        app,
        list,
        remove,
        dry_run,
        exe,
    } = &cli.command
    {
        let Some(app) = app.as_deref().filter(|_| !*list) else {
            print!("{}", crate::setup::list_text());
            return Ok(());
        };
        let host = crate::setup::Host::parse(app).ok_or_else(|| {
            anyhow::anyhow!(
                "unknown app \"{}\". Supported: {}",
                app,
                crate::setup::ALL_HOSTS
                    .iter()
                    .map(|h| h.id())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
        let opts = crate::setup::SetupOptions {
            read_only: cli.read_only,
            remove: *remove,
            dry_run: *dry_run,
            exe: exe.clone(),
        };
        print!("{}", crate::setup::run_setup(host, &opts)?);
        return Ok(());
    }

    init_dpi_awareness();
    let mut cfg = SafetyConfig::default();
    if cli.read_only {
        cfg.read_only = true;
    }
    if cli.allow_sensitive {
        cfg.allow_sensitive = true;
    }
    if let Some(apps) = &cli.allow_apps {
        let list = crate::safety::parse_app_list(apps);
        cfg.allowed_apps = (!list.is_empty()).then_some(list);
    }
    if cli.allow_mic {
        cfg.allow_mic = true;
    }
    if let Some(n) = cli.max_actions_per_minute {
        cfg.max_actions_per_minute = n;
    }
    if let Some(mode) = &cli.confirm {
        cfg.confirm = crate::safety::ConfirmMode::parse(mode).ok_or_else(|| {
            anyhow::anyhow!("--confirm must be risky, all or off (got \"{}\")", mode)
        })?;
    }

    let audit = AuditLogger::new(cli.log_file, cli.no_log);
    let mut state = AppState::new_with_audit(cfg, audit);

    if matches!(cli.command, Command::Mcp) {
        return run_mcp_server(&mut state);
    }

    if let Command::Serve {
        port,
        token,
        allow_origins,
    } = cli.command
    {
        let extra_origins = crate::bridge::validate_extra_origins(&allow_origins)?;
        // A token passed with --token is already known to the caller. A generated one is
        // written to a private file in the user's profile instead of being printed.
        let (token, token_file) = match token {
            Some(t) => (t, None),
            None => {
                let t = generate_session_token()?;
                let path = crate::bridge::token_file_path();
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                std::fs::write(&path, &t)?;
                (t, Some(path))
            }
        };
        let bridge_cfg = BridgeConfig {
            port,
            token,
            extra_origins,
            token_file,
        };
        return run_bridge_server(state, bridge_cfg);
    }

    let state_path = AppState::default_cli_state_path();
    state.load_snapshot_from_file(&state_path);
    let t0 = Instant::now();

    match cli.command {
        Command::Mcp | Command::Serve { .. } | Command::Setup { .. } => unreachable!(),
        Command::Logs { tail, json, clear } => {
            if clear {
                state.audit.clear()?;
                println!(
                    "Cleared audit log: {}",
                    state
                        .audit
                        .path
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "(disabled)".to_string())
                );
                return Ok(());
            }
            let path_display = state
                .audit
                .path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "(disabled)".to_string());
            let entries = state.audit.read_tail(tail);
            if json {
                for e in &entries {
                    println!("{}", serde_json::to_string(e)?);
                }
            } else {
                println!(
                    "AUDIT LOG: {} (showing last {})",
                    path_display,
                    entries.len()
                );
                for e in &entries {
                    println!("{}", e.to_compact_line());
                }
            }
        }
        Command::Windows { json } => {
            let res = list_windows(&state.safety);
            match res {
                Ok(wins) => {
                    let mut text = format!("WINDOWS ({}):\n", wins.len());
                    for w in &wins {
                        text.push_str(&w.to_compact_line());
                        text.push('\n');
                    }
                    state.audit.log(
                        "cli",
                        "windows",
                        true,
                        t0.elapsed().as_millis() as u64,
                        estimate_text_tokens(&text),
                        None,
                        &format!("Listed {} visible windows", wins.len()),
                    );
                    if json {
                        println!("{}", serde_json::to_string_pretty(&wins)?);
                    } else {
                        print!("{}", text);
                    }
                }
                Err(e) => {
                    state.audit.log(
                        "cli",
                        "windows",
                        false,
                        t0.elapsed().as_millis() as u64,
                        0,
                        None,
                        &e.to_string(),
                    );
                    return Err(e);
                }
            }
        }
        Command::See {
            window,
            mode,
            max_elements,
            interactive_only,
            coords,
            ocr_lang,
            json,
        } => {
            let params = json!({
                "window": window,
                "mode": mode,
                "max_elements": max_elements,
                "interactive_only": interactive_only
            });
            let opts = SeeOptions {
                window,
                mode: SeeMode::parse(&mode),
                max_elements,
                max_depth: 14,
                ocr_lang,
                interactive_only,
                coords: CoordStyle::parse(&coords),
            };
            match see(&mut state, &opts) {
                Ok(snap) => {
                    state.save_snapshot_to_file(&state_path);
                    let compact = snap.to_compact_text();
                    let first = compact.lines().next().unwrap_or("");
                    state.audit.log(
                        "cli",
                        "see",
                        true,
                        t0.elapsed().as_millis() as u64,
                        estimate_text_tokens(&compact),
                        Some(params),
                        first,
                    );
                    if json {
                        println!("{}", serde_json::to_string_pretty(&snap)?);
                    } else {
                        print!("{}", compact);
                    }
                }
                Err(e) => {
                    state.audit.log(
                        "cli",
                        "see",
                        false,
                        t0.elapsed().as_millis() as u64,
                        0,
                        Some(params),
                        &e.to_string(),
                    );
                    return Err(e);
                }
            }
        }
        Command::Diff {
            window,
            mode,
            max_elements,
            interactive_only,
            coords,
        } => {
            let params = json!({ "window": window, "mode": mode });
            let opts = SeeOptions {
                window,
                mode: SeeMode::parse(&mode),
                max_elements,
                max_depth: 14,
                ocr_lang: None,
                interactive_only,
                coords: CoordStyle::parse(&coords),
            };
            match see_diff(&mut state, &opts) {
                Ok((text, _)) => {
                    state.save_snapshot_to_file(&state_path);
                    let first = text.lines().next().unwrap_or("");
                    state.audit.log(
                        "cli",
                        "diff",
                        true,
                        t0.elapsed().as_millis() as u64,
                        estimate_text_tokens(&text),
                        Some(params),
                        first,
                    );
                    print!("{}", text);
                }
                Err(e) => {
                    state.audit.log(
                        "cli",
                        "diff",
                        false,
                        t0.elapsed().as_millis() as u64,
                        0,
                        Some(params),
                        &e.to_string(),
                    );
                    return Err(e);
                }
            }
        }
        Command::Screenshot {
            window,
            out,
            som,
            crop,
            max_width,
            quality,
            format,
        } => {
            let params =
                json!({ "window": window, "som": som, "max_width": max_width, "format": format });
            let crop_rect = match crop {
                Some(s) => Some(parse_rect_csv(&s)?),
                None => None,
            };
            let opts = ScreenshotOptions {
                window,
                diff_only: false,
                crop: crop_rect,
                som,
                max_width,
                quality,
                format,
            };
            match screenshot(&mut state, &opts) {
                Ok(res) => {
                    std::fs::write(&out, &res.bytes)?;
                    let toks = estimate_text_tokens(&res.summary)
                        + estimate_vision_tokens(res.width, res.height);
                    state.audit.log(
                        "cli",
                        "screenshot",
                        true,
                        t0.elapsed().as_millis() as u64,
                        toks,
                        Some(params),
                        &res.summary,
                    );
                    println!("{} -> {}", res.summary, out.display());
                }
                Err(e) => {
                    state.audit.log(
                        "cli",
                        "screenshot",
                        false,
                        t0.elapsed().as_millis() as u64,
                        0,
                        Some(params),
                        &e.to_string(),
                    );
                    return Err(e);
                }
            }
        }
        Command::Click {
            id,
            x,
            y,
            window,
            button,
            clicks,
            no_background,
        } => {
            let params = json!({ "id": id, "x": x, "y": y, "window": window, "button": button, "clicks": clicks });
            let opts = ClickOptions {
                id,
                x,
                y,
                window,
                button: MouseButton::parse(&button),
                clicks,
                background: !no_background,
            };
            match click(&state, &opts) {
                Ok(msg) => {
                    state.audit.log(
                        "cli",
                        "click",
                        true,
                        t0.elapsed().as_millis() as u64,
                        estimate_text_tokens(&msg),
                        Some(params),
                        &msg,
                    );
                    println!("{}", msg);
                }
                Err(e) => {
                    state.audit.log(
                        "cli",
                        "click",
                        false,
                        t0.elapsed().as_millis() as u64,
                        0,
                        Some(params),
                        &e.to_string(),
                    );
                    return Err(e);
                }
            }
        }
        Command::Type {
            text,
            id,
            window,
            clear,
            enter,
            no_background,
        } => {
            let is_pwd = state.is_password_element(id);
            let raw_params =
                json!({ "id": id, "window": window, "text": text, "clear": clear, "enter": enter });
            let safe_params = sanitize_params("type", &raw_params, is_pwd);
            let opts = TypeOptions {
                text,
                id,
                window,
                clear,
                press_enter: enter,
                background: !no_background,
            };
            match type_text(&state, &opts) {
                Ok(msg) => {
                    state.audit.log(
                        "cli",
                        "type",
                        true,
                        t0.elapsed().as_millis() as u64,
                        estimate_text_tokens(&msg),
                        Some(safe_params),
                        &msg,
                    );
                    println!("{}", msg);
                }
                Err(e) => {
                    state.audit.log(
                        "cli",
                        "type",
                        false,
                        t0.elapsed().as_millis() as u64,
                        0,
                        Some(safe_params),
                        &e.to_string(),
                    );
                    return Err(e);
                }
            }
        }
        Command::Hotkey { keys, window } => {
            let params = json!({ "keys": keys, "window": window });
            match hotkey(&state, &keys, window.as_deref()) {
                Ok(msg) => {
                    state.audit.log(
                        "cli",
                        "hotkey",
                        true,
                        t0.elapsed().as_millis() as u64,
                        estimate_text_tokens(&msg),
                        Some(params),
                        &msg,
                    );
                    println!("{}", msg);
                }
                Err(e) => {
                    state.audit.log(
                        "cli",
                        "hotkey",
                        false,
                        t0.elapsed().as_millis() as u64,
                        0,
                        Some(params),
                        &e.to_string(),
                    );
                    return Err(e);
                }
            }
        }
        Command::Scroll {
            delta_y,
            delta_x,
            x,
            y,
            window,
        } => {
            let params =
                json!({ "delta_y": delta_y, "delta_x": delta_x, "x": x, "y": y, "window": window });
            match scroll(&state, delta_y, delta_x, x, y, window.as_deref()) {
                Ok(msg) => {
                    state.audit.log(
                        "cli",
                        "scroll",
                        true,
                        t0.elapsed().as_millis() as u64,
                        estimate_text_tokens(&msg),
                        Some(params),
                        &msg,
                    );
                    println!("{}", msg);
                }
                Err(e) => {
                    state.audit.log(
                        "cli",
                        "scroll",
                        false,
                        t0.elapsed().as_millis() as u64,
                        0,
                        Some(params),
                        &e.to_string(),
                    );
                    return Err(e);
                }
            }
        }
        Command::Drag {
            x1,
            y1,
            x2,
            y2,
            steps,
            window,
        } => {
            let params = json!({ "x1": x1, "y1": y1, "x2": x2, "y2": y2, "window": window });
            match drag(&state, x1, y1, x2, y2, steps, window.as_deref()) {
                Ok(msg) => {
                    state.audit.log(
                        "cli",
                        "drag",
                        true,
                        t0.elapsed().as_millis() as u64,
                        estimate_text_tokens(&msg),
                        Some(params),
                        &msg,
                    );
                    println!("{}", msg);
                }
                Err(e) => {
                    state.audit.log(
                        "cli",
                        "drag",
                        false,
                        t0.elapsed().as_millis() as u64,
                        0,
                        Some(params),
                        &e.to_string(),
                    );
                    return Err(e);
                }
            }
        }
        Command::Focus { window } => {
            let params = json!({ "window": window });
            match focus_window(&mut state, &window, false) {
                Ok(msg) => {
                    state.audit.log(
                        "cli",
                        "focus",
                        true,
                        t0.elapsed().as_millis() as u64,
                        estimate_text_tokens(&msg),
                        Some(params),
                        &msg,
                    );
                    println!("{}", msg);
                }
                Err(e) => {
                    state.audit.log(
                        "cli",
                        "focus",
                        false,
                        t0.elapsed().as_millis() as u64,
                        0,
                        Some(params),
                        &e.to_string(),
                    );
                    return Err(e);
                }
            }
        }
        Command::WaitChange {
            window,
            timeout_ms,
            poll_ms,
            threshold,
        } => {
            let params =
                json!({ "window": window, "timeout_ms": timeout_ms, "threshold": threshold });
            match wait_change(
                &mut state,
                window.as_deref(),
                timeout_ms,
                poll_ms,
                threshold,
            ) {
                Ok(r) => {
                    let msg = format!(
                        "changed={} diff={:.2}% elapsed={}ms",
                        r.changed, r.changed_percent, r.elapsed_ms
                    );
                    state.audit.log(
                        "cli",
                        "wait-change",
                        true,
                        t0.elapsed().as_millis() as u64,
                        estimate_text_tokens(&msg),
                        Some(params),
                        &msg,
                    );
                    println!("{}", msg);
                }
                Err(e) => {
                    state.audit.log(
                        "cli",
                        "wait-change",
                        false,
                        t0.elapsed().as_millis() as u64,
                        0,
                        Some(params),
                        &e.to_string(),
                    );
                    return Err(e);
                }
            }
        }
        Command::AudioDevices { json } => {
            let devs = list_audio_devices()?;
            state.audit.log(
                "cli",
                "audio-devices",
                true,
                t0.elapsed().as_millis() as u64,
                16,
                None,
                &format!("Listed {} audio devices", devs.len()),
            );
            if json {
                println!("{}", serde_json::to_string_pretty(&devs)?);
            } else {
                println!("AUDIO_DEVICES ({}):", devs.len());
                for d in &devs {
                    println!("{}", d.to_compact_line());
                }
            }
        }
        Command::Listen {
            seconds,
            source,
            no_trim,
            threshold_db,
            out,
        } => {
            let params = json!({ "seconds": seconds, "source": source, "trim_silence": !no_trim });
            let opts = ListenOptions {
                seconds,
                source: AudioSource::parse(&source),
                trim_silence: !no_trim,
                threshold_db,
                out_path: out,
                allow_mic: state.safety.config.allow_mic,
            };
            match listen(&opts) {
                Ok(res) => {
                    let first = res.summary.lines().next().unwrap_or("");
                    state.audit.log(
                        "cli",
                        "listen",
                        true,
                        res.elapsed_ms,
                        estimate_text_tokens(&res.summary) + res.total_audio_tokens,
                        Some(params),
                        first,
                    );
                    print!("{}", res.summary);
                }
                Err(e) => {
                    state.audit.log(
                        "cli",
                        "listen",
                        false,
                        t0.elapsed().as_millis() as u64,
                        0,
                        Some(params),
                        &e.to_string(),
                    );
                    return Err(e);
                }
            }
        }
        Command::Watch {
            window,
            audio,
            duration,
            audio_only,
            chunk_sec,
            scene_threshold,
            retention,
            keep_files,
        } => {
            let params = json!({ "window": window, "audio": audio, "duration": duration, "audio_only": audio_only });
            let opts = WatchOptions {
                window,
                audio: AudioSource::parse(&audio),
                capture_video: !audio_only,
                chunk_sec,
                scene_threshold_pct: scene_threshold,
                retention_sec: retention,
                ..Default::default()
            };
            let mut sess =
                WatchSession::start(opts, state.locked_window, state.safety.config.clone())?;
            println!(
                "WATCH_START session={} target={} duration={}s dir={}",
                sess.id,
                sess.target_desc,
                duration,
                sess.session_dir.display()
            );

            let watch_dur = Duration::from_secs(duration.clamp(2, 3600));
            let watch_t0 = Instant::now();
            while watch_t0.elapsed() < watch_dur {
                thread::sleep(Duration::from_secs(2));
                let out = sess.poll(None, 25);
                for line in out.lines().skip(1) {
                    if !line.starts_with("(no new") {
                        println!("{}", line);
                    }
                }
            }
            let stop_msg = sess.stop(keep_files);
            println!("{}", stop_msg);
            state.audit.log(
                "cli",
                "watch",
                true,
                watch_t0.elapsed().as_millis() as u64,
                estimate_text_tokens(&stop_msg),
                Some(params),
                &stop_msg,
            );
        }
        Command::OcrLangs => {
            let langs = available_languages();
            println!(
                "Available Windows OCR languages ({}): {:?}",
                langs.len(),
                langs
            );
        }
        Command::Bench { window } => {
            run_benchmark(&mut state, window)?;
        }
    }

    Ok(())
}

fn parse_rect_csv(s: &str) -> Result<Rect> {
    let parts: Vec<i32> = s
        .split(',')
        .map(|p| p.trim().parse::<i32>())
        .collect::<std::result::Result<_, _>>()?;
    if parts.len() != 4 {
        bail!("Crop must be x,y,w,h (got \"{}\")", s);
    }
    Ok(Rect::new(parts[0], parts[1], parts[2], parts[3]))
}

fn run_benchmark(state: &mut AppState, window: Option<String>) -> Result<()> {
    let see_opts = SeeOptions {
        window: window.clone(),
        mode: SeeMode::Auto,
        max_elements: 120,
        max_depth: 14,
        ocr_lang: None,
        interactive_only: false,
        coords: CoordStyle::Rect,
    };

    let snap = see(state, &see_opts)?;
    let see_text = snap.to_compact_text();
    let see_tokens = estimate_text_tokens(&see_text);

    let inter_snap = see(
        state,
        &SeeOptions {
            interactive_only: true,
            ..see_opts.clone()
        },
    )?;
    let inter_tokens = estimate_text_tokens(&inter_snap.to_compact_text());
    let lean_snap = see(
        state,
        &SeeOptions {
            interactive_only: true,
            coords: CoordStyle::Center,
            ..see_opts.clone()
        },
    )?;
    let lean_tokens = estimate_text_tokens(&lean_snap.to_compact_text());

    // Re-baseline with the full snapshot so the diff below compares like with like
    see(state, &see_opts)?;
    let (diff_text, _) = see_diff(state, &see_opts)?;
    let diff_tokens = estimate_text_tokens(&diff_text);

    let raw_shot = screenshot(
        state,
        &ScreenshotOptions {
            window: window.clone(),
            diff_only: false,
            crop: None,
            som: false,
            max_width: 3840,
            quality: 85,
            format: "png".to_string(),
        },
    )?;
    let raw_vision_tokens = estimate_vision_tokens(raw_shot.width, raw_shot.height);

    let compact_shot = screenshot(
        state,
        &ScreenshotOptions {
            window,
            diff_only: false,
            crop: None,
            som: true,
            max_width: 1024,
            quality: 65,
            format: "jpeg".to_string(),
        },
    )?;
    let compact_vision_tokens = estimate_vision_tokens(compact_shot.width, compact_shot.height);

    let savings = |t: usize| {
        if raw_vision_tokens > 0 {
            (1.0 - (t as f64 / raw_vision_tokens as f64)) * 100.0
        } else {
            0.0
        }
    };
    let savings_see = savings(see_tokens);
    let savings_inter = savings(inter_tokens);
    let savings_lean = savings(lean_tokens);
    let savings_diff = savings(diff_tokens);

    println!("=== eyra Token & Latency Benchmark ===");
    if let Some(w) = &snap.window {
        println!(
            "Target:            {} ({}) {}x{}",
            w.title, w.process, w.rect.w, w.rect.h
        );
    } else {
        println!(
            "Target:            Screen {}x{}",
            snap.screen_rect.w, snap.screen_rect.h
        );
    }
    println!(
        "Raw PNG Vision:    ~{} tokens ({}x{}, {} KB)",
        raw_vision_tokens,
        raw_shot.width,
        raw_shot.height,
        raw_shot.bytes.len() / 1024
    );
    println!(
        "eyra L3 SoM JPEG:  ~{} tokens ({}x{}, {} KB)",
        compact_vision_tokens,
        compact_shot.width,
        compact_shot.height,
        compact_shot.bytes.len() / 1024
    );
    println!(
        "eyra L1/L2 see:    ~{} tokens ({} elements, mode={}, {}ms) -> {:.1}% token savings",
        see_tokens,
        snap.elements.len(),
        snap.mode_used,
        snap.elapsed_ms,
        savings_see
    );
    println!(
        "eyra see interactive_only: ~{} tokens ({} elements) -> {:.1}% token savings",
        inter_tokens,
        inter_snap.elements.len(),
        savings_inter
    );
    println!(
        "eyra see interactive_only + coords=center: ~{} tokens -> {:.1}% token savings",
        lean_tokens, savings_lean
    );
    println!(
        "eyra see_diff:     ~{} tokens -> {:.1}% token savings",
        diff_tokens, savings_diff
    );
    println!("(text tokens estimated as chars/4; image tokens as w*h/750)");
    Ok(())
}
