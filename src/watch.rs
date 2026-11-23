use crate::ear::vad::{
    frame_rms_db, resample_mono, trim_silence, DEFAULT_VAD_THRESHOLD_DB, TARGET_SAMPLE_RATE,
};
use crate::ear::wasapi::{AudioSource, StreamKind, WasapiStream};
use crate::ear::wav::{estimate_audio_tokens, write_wav_pcm16};
use crate::eye::capture::{apply_redactions, capture_screen, capture_window, encode_frame};
use crate::eye::diff::diff_frames;
use crate::eye::windows::{get_window_info_by_hwnd, list_windows, resolve_target, CaptureTarget};
use crate::safety::{SafetyConfig, SafetyGuard};
use crate::types::{estimate_vision_tokens, CapturedFrame, Rect};
use anyhow::{bail, Result};
use std::collections::VecDeque;
use std::fmt::Write as FmtWrite;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone)]
pub struct WatchOptions {
    pub window: Option<String>,
    pub audio: AudioSource,
    pub capture_video: bool,
    pub chunk_sec: f32,
    pub scene_threshold_pct: f32,
    pub frame_poll_ms: u64,
    pub max_frame_gap_sec: u64,
    pub max_width: u32,
    pub jpeg_quality: u8,
    pub retention_sec: u64,
    pub threshold_db: f32,
}

impl Default for WatchOptions {
    fn default() -> Self {
        Self {
            window: None,
            audio: AudioSource::Both,
            capture_video: true,
            chunk_sec: 8.0,
            scene_threshold_pct: 8.0,
            frame_poll_ms: 1000,
            max_frame_gap_sec: 18,
            max_width: 768,
            jpeg_quality: 60,
            retention_sec: 300,
            threshold_db: DEFAULT_VAD_THRESHOLD_DB,
        }
    }
}

#[derive(Debug, Clone)]
pub enum WatchEventKind {
    Scene {
        diff_pct: f32,
        width: u32,
        height: u32,
        bytes: usize,
        est_tokens: usize,
        path: PathBuf,
        window_title: Option<String>,
    },
    Audio {
        channel: &'static str,
        label: &'static str,
        saved_sec: f32,
        raw_sec: f32,
        rms_db: f32,
        bytes: usize,
        est_tokens: usize,
        path: PathBuf,
    },
    Info {
        message: String,
    },
}

#[derive(Debug, Clone)]
pub struct WatchEvent {
    pub seq: u64,
    pub created_at: Instant,
    pub offset_sec: u64,
    pub kind: WatchEventKind,
}

impl WatchEvent {
    pub fn file_path(&self) -> Option<&PathBuf> {
        match &self.kind {
            WatchEventKind::Scene { path, .. } => Some(path),
            WatchEventKind::Audio { path, .. } => Some(path),
            WatchEventKind::Info { .. } => None,
        }
    }

    pub fn to_compact_line(&self) -> String {
        let mins = self.offset_sec / 60;
        let secs = self.offset_sec % 60;
        match &self.kind {
            WatchEventKind::Scene {
                diff_pct,
                width,
                height,
                bytes,
                est_tokens,
                path,
                window_title,
            } => {
                let title_part = window_title
                    .as_deref()
                    .map(|t| format!(" title=\"{}\"", crate::types::truncate_clean(t, 44)))
                    .unwrap_or_default();
                format!(
                    "#{} t={:02}:{:02} SCENE diff={:.1}% {}x{} {}KB (~{}tok){} path={}",
                    self.seq,
                    mins,
                    secs,
                    diff_pct,
                    width,
                    height,
                    bytes / 1024,
                    est_tokens,
                    title_part,
                    path.display()
                )
            }
            WatchEventKind::Audio {
                channel,
                label,
                saved_sec,
                raw_sec,
                rms_db,
                bytes,
                est_tokens,
                path,
            } => format!(
                "#{} t={:02}:{:02} AUDIO[{}] {} {:.1}s/{:.1}s {:.1}dB {}KB (~{}tok) path={}",
                self.seq,
                mins,
                secs,
                channel,
                label,
                saved_sec,
                raw_sec,
                rms_db,
                bytes / 1024,
                est_tokens,
                path.display()
            ),
            WatchEventKind::Info { message } => {
                format!("#{} t={:02}:{:02} INFO {}", self.seq, mins, secs, message)
            }
        }
    }
}

struct WatchTimeline {
    next_seq: u64,
    last_polled_seq: u64,
    events: VecDeque<WatchEvent>,
    total_audio_clips: usize,
    total_keyframes: usize,
}

impl WatchTimeline {
    fn new() -> Self {
        Self {
            next_seq: 1,
            last_polled_seq: 0,
            events: VecDeque::new(),
            total_audio_clips: 0,
            total_keyframes: 0,
        }
    }

    fn alloc_seq(&mut self) -> u64 {
        let s = self.next_seq;
        self.next_seq += 1;
        s
    }

    fn push(&mut self, event: WatchEvent, retention: Duration) {
        match &event.kind {
            WatchEventKind::Scene { .. } => self.total_keyframes += 1,
            WatchEventKind::Audio { .. } => self.total_audio_clips += 1,
            WatchEventKind::Info { .. } => {}
        }
        self.events.push_back(event);
        self.evict_expired(retention);
    }

    fn evict_expired(&mut self, retention: Duration) {
        let now = Instant::now();
        while let Some(front) = self.events.front() {
            if now.duration_since(front.created_at) > retention {
                if let Some(ev) = self.events.pop_front() {
                    if let Some(path) = ev.file_path() {
                        let _ = fs::remove_file(path);
                    }
                }
            } else {
                break;
            }
        }
    }
}

pub struct WatchSession {
    pub id: String,
    pub session_dir: PathBuf,
    pub target_desc: String,
    pub started_at: Instant,
    pub retention: Duration,
    stop_flag: Arc<AtomicBool>,
    timeline: Arc<Mutex<WatchTimeline>>,
    handles: Vec<JoinHandle<()>>,
    cleaned_up: bool,
    /// Visible "recording" badge for the whole session
    _indicator: Option<crate::indicator::RecordingIndicator>,
}

impl WatchSession {
    pub fn start(
        opts: WatchOptions,
        locked_hwnd: Option<usize>,
        safety_cfg: SafetyConfig,
    ) -> Result<Self> {
        let safety = SafetyGuard::new(safety_cfg.clone());

        let (target_hwnd, target_rect, target_desc) = if opts.capture_video {
            match resolve_target(opts.window.as_deref(), locked_hwnd, &safety)? {
                CaptureTarget::Window(w) => {
                    if safety.should_redact_window(&w) {
                        bail!(
                            "Cannot start watch session on sensitive window \"{}\" ({}).",
                            w.title,
                            w.process
                        );
                    }
                    let desc = format!(
                        "{} ({}) \"{}\"",
                        w.hwnd_hex(),
                        w.process,
                        crate::types::truncate_clean(&w.title, 48)
                    );
                    (Some(w.hwnd), w.rect, desc)
                }
                CaptureTarget::Screen(r) => (None, r, format!("screen {}x{}", r.w, r.h)),
            }
        } else {
            (None, Rect::default(), "audio-only".to_string())
        };

        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let id = format!("w_{}", stamp);
        sweep_empty_session_dirs(&default_sessions_dir());
        let session_dir = default_sessions_dir().join(&id);
        fs::create_dir_all(&session_dir)?;

        let started_at = Instant::now();
        let retention = Duration::from_secs(opts.retention_sec.clamp(30, 3600));
        let stop_flag = Arc::new(AtomicBool::new(false));
        let timeline = Arc::new(Mutex::new(WatchTimeline::new()));
        let mut handles = Vec::new();

        // Spawn video scene-change worker
        if opts.capture_video {
            let stop_c = Arc::clone(&stop_flag);
            let tl_c = Arc::clone(&timeline);
            let dir_c = session_dir.clone();
            let safety_c = safety_cfg.clone();
            let opts_c = opts.clone();
            handles.push(thread::spawn(move || {
                video_worker(
                    stop_c,
                    tl_c,
                    dir_c,
                    started_at,
                    retention,
                    target_hwnd,
                    target_rect,
                    safety_c,
                    opts_c,
                );
            }));
        }

        // Spawn audio worker(s); the microphone needs the user's opt-in
        let (audio, mic_note) = crate::ear::effective_source(opts.audio, safety_cfg.allow_mic)?;
        if let Some(note) = mic_note {
            push_info(&timeline, started_at, retention, note);
        }
        let channels: Vec<(&'static str, StreamKind)> = match audio {
            AudioSource::System => vec![("system", StreamKind::Loopback)],
            AudioSource::Mic => vec![("mic", StreamKind::Mic)],
            AudioSource::Both => vec![("system", StreamKind::Loopback), ("mic", StreamKind::Mic)],
        };

        for (chan_label, stream_kind) in channels {
            let stop_c = Arc::clone(&stop_flag);
            let tl_c = Arc::clone(&timeline);
            let dir_c = session_dir.clone();
            let chunk_sec = opts.chunk_sec.clamp(3.0, 20.0);
            let threshold_db = opts.threshold_db;
            handles.push(thread::spawn(move || {
                audio_worker(
                    stop_c,
                    tl_c,
                    dir_c,
                    started_at,
                    retention,
                    chan_label,
                    stream_kind,
                    chunk_sec,
                    threshold_db,
                );
            }));
        }

        Ok(Self {
            id,
            session_dir,
            target_desc,
            started_at,
            retention,
            stop_flag,
            timeline,
            handles,
            cleaned_up: false,
            _indicator: crate::indicator::RecordingIndicator::show(&format!(
                "{}{}",
                crate::ear::source_label(audio),
                if opts.capture_video { " + screen" } else { "" }
            )),
        })
    }

    pub fn poll_structured(&self, since_seq: Option<u64>, max_events: usize) -> serde_json::Value {
        let mut tl = self.timeline.lock().unwrap();
        tl.evict_expired(self.retention);

        let start_after = since_seq.unwrap_or(tl.last_polled_seq);
        let matching: Vec<WatchEvent> = tl
            .events
            .iter()
            .filter(|e| e.seq > start_after)
            .cloned()
            .collect();

        let total_new = matching.len();
        let slice: &[WatchEvent] = if total_new > max_events {
            &matching[total_new - max_events..]
        } else {
            &matching[..]
        };

        if let Some(last) = slice.last() {
            tl.last_polled_seq = tl.last_polled_seq.max(last.seq);
        }

        let events_json: Vec<serde_json::Value> = slice
            .iter()
            .map(|ev| {
                let file_name = ev
                    .file_path()
                    .and_then(|p| p.file_name())
                    .and_then(|s| s.to_str())
                    .map(|s| s.to_string());
                match &ev.kind {
                    WatchEventKind::Scene {
                        diff_pct,
                        width,
                        height,
                        bytes,
                        est_tokens,
                        window_title,
                        ..
                    } => serde_json::json!({
                        "seq": ev.seq,
                        "offset_sec": ev.offset_sec,
                        "type": "scene",
                        "diff_pct": diff_pct,
                        "width": width,
                        "height": height,
                        "bytes": bytes,
                        "est_tokens": est_tokens,
                        "window_title": window_title,
                        "file": file_name,
                        "line": ev.to_compact_line()
                    }),
                    WatchEventKind::Audio {
                        channel,
                        label,
                        saved_sec,
                        raw_sec,
                        rms_db,
                        bytes,
                        est_tokens,
                        ..
                    } => serde_json::json!({
                        "seq": ev.seq,
                        "offset_sec": ev.offset_sec,
                        "type": "audio",
                        "channel": channel,
                        "label": label,
                        "saved_sec": saved_sec,
                        "raw_sec": raw_sec,
                        "rms_db": rms_db,
                        "bytes": bytes,
                        "est_tokens": est_tokens,
                        "file": file_name,
                        "line": ev.to_compact_line()
                    }),
                    WatchEventKind::Info { message } => serde_json::json!({
                        "seq": ev.seq,
                        "offset_sec": ev.offset_sec,
                        "type": "info",
                        "message": message,
                        "line": ev.to_compact_line()
                    }),
                }
            })
            .collect();

        serde_json::json!({
            "session_id": self.id,
            "active": !self.stop_flag.load(Ordering::Relaxed),
            "elapsed_sec": self.started_at.elapsed().as_secs(),
            "target": self.target_desc,
            "total_keyframes": tl.total_keyframes,
            "total_audio_clips": tl.total_audio_clips,
            "next_seq": tl.next_seq,
            "events": events_json
        })
    }

    pub fn poll(&self, since_seq: Option<u64>, max_events: usize) -> String {
        let mut tl = self.timeline.lock().unwrap();
        tl.evict_expired(self.retention);

        let start_after = since_seq.unwrap_or(tl.last_polled_seq);
        let matching: Vec<WatchEvent> = tl
            .events
            .iter()
            .filter(|e| e.seq > start_after)
            .cloned()
            .collect();

        let total_new = matching.len();
        let slice: &[WatchEvent] = if total_new > max_events {
            &matching[total_new - max_events..]
        } else {
            &matching[..]
        };

        if let Some(last) = slice.last() {
            tl.last_polled_seq = tl.last_polled_seq.max(last.seq);
        }

        let elapsed_sec = self.started_at.elapsed().as_secs();
        let mut out = String::new();
        let _ = writeln!(
            out,
            "WATCH session={} active={} elapsed={}s target={} retained={} new={} (frames={}, audio={}) next_seq={}",
            self.id,
            !self.stop_flag.load(Ordering::Relaxed),
            elapsed_sec,
            self.target_desc,
            tl.events.len(),
            slice.len(),
            tl.total_keyframes,
            tl.total_audio_clips,
            tl.next_seq
        );

        if slice.is_empty() {
            out.push_str("(no new audio or scene events since last poll)\n");
        } else {
            for ev in slice {
                out.push_str(&ev.to_compact_line());
                out.push('\n');
            }
        }
        out
    }

    pub fn stop(&mut self, keep_files: bool) -> String {
        if self.cleaned_up {
            return format!("Watch session {} already stopped.", self.id);
        }
        self.stop_flag.store(true, Ordering::SeqCst);
        for h in self.handles.drain(..) {
            let _ = h.join();
        }
        self._indicator = None;
        let elapsed_sec = self.started_at.elapsed().as_secs();
        // Events flushed while the workers shut down (e.g. the last audio chunk) would otherwise
        // never reach the caller, so report them with the stop summary.
        let final_events: Vec<String> = self
            .poll(None, 50)
            .lines()
            .skip(1)
            .filter(|l| !l.trim().is_empty() && !l.starts_with("(no new"))
            .map(str::to_string)
            .collect();
        let (frames, clips) = {
            let tl = self.timeline.lock().unwrap();
            (tl.total_keyframes, tl.total_audio_clips)
        };
        if !keep_files {
            remove_dir_with_retry(&self.session_dir);
        }
        self.cleaned_up = true;
        let mut tail = String::new();
        for line in &final_events {
            tail.push('\n');
            tail.push_str(line);
            if !keep_files && line.contains("path=") {
                tail.push_str(" (deleted)");
            }
        }
        format!(
            "WATCH_STOP session={} elapsed={}s frames={} audio_clips={} files={}{}",
            self.id,
            elapsed_sec,
            frames,
            clips,
            if keep_files {
                format!("kept at {}", self.session_dir.display())
            } else {
                "cleaned up".to_string()
            },
            tail
        )
    }
}

impl Drop for WatchSession {
    fn drop(&mut self) {
        if !self.cleaned_up {
            let _ = self.stop(false);
        }
    }
}

pub fn default_sessions_dir() -> PathBuf {
    if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
        if !local_app_data.trim().is_empty() {
            return PathBuf::from(local_app_data).join("eyra").join("sessions");
        }
    }
    std::env::temp_dir().join("eyra").join("sessions")
}

#[allow(clippy::too_many_arguments)]
fn video_worker(
    stop: Arc<AtomicBool>,
    timeline: Arc<Mutex<WatchTimeline>>,
    session_dir: PathBuf,
    started_at: Instant,
    retention: Duration,
    target_hwnd: Option<usize>,
    fallback_rect: Rect,
    safety_cfg: SafetyConfig,
    opts: WatchOptions,
) {
    let safety = SafetyGuard::new(safety_cfg);
    let poll_dur = Duration::from_millis(opts.frame_poll_ms.clamp(250, 10_000));
    let max_gap = Duration::from_secs(opts.max_frame_gap_sec.clamp(3, 120));

    let mut prev_saved: Option<CapturedFrame> = None;
    let mut last_save_time = Instant::now() - max_gap;
    let mut last_title = String::new();

    while !stop.load(Ordering::Relaxed) {
        let capture_res = if let Some(hwnd) = target_hwnd {
            if let Some(w) = get_window_info_by_hwnd(hwnd, &safety) {
                if safety.should_redact_window(&w) {
                    thread::sleep(poll_dur);
                    continue;
                }
                if w.is_minimized {
                    thread::sleep(poll_dur);
                    continue;
                }
                capture_window(w.hwnd, w.rect).map(|f| (f, Some(w.title)))
            } else {
                push_info(
                    &timeline,
                    started_at,
                    retention,
                    "Target window closed; pausing video capture.".to_string(),
                );
                break;
            }
        } else {
            capture_screen(fallback_rect).map(|f| (f, None))
        };

        if let Ok((mut frame, title_opt)) = capture_res {
            // Redact any visible sensitive windows
            if !safety.config.allow_sensitive {
                if let Ok(wins) = list_windows(&safety) {
                    let redacts: Vec<Rect> = wins
                        .into_iter()
                        .filter(|w| w.is_sensitive && !w.is_minimized && w.rect.is_valid())
                        .map(|w| w.rect)
                        .collect();
                    if !redacts.is_empty() {
                        apply_redactions(&mut frame, &redacts);
                    }
                }
            }

            let (should_save, diff_pct) = match &prev_saved {
                None => (true, 100.0),
                Some(prev) => {
                    let pd = diff_frames(prev, &frame, 18);
                    let scene_changed = pd.changed_percent >= opts.scene_threshold_pct;
                    let periodic_active =
                        last_save_time.elapsed() >= max_gap && pd.changed_percent >= 1.0;
                    (scene_changed || periodic_active, pd.changed_percent)
                }
            };

            if should_save {
                if let Ok((bytes, out_w, out_h, _)) =
                    encode_frame(&frame, opts.max_width, opts.jpeg_quality, "jpeg")
                {
                    let seq = {
                        let mut tl = timeline.lock().unwrap();
                        tl.alloc_seq()
                    };
                    let file_path = session_dir.join(format!("frame_{:04}.jpg", seq));
                    if fs::write(&file_path, &bytes).is_ok() {
                        let title_changed = match &title_opt {
                            Some(t) if t != &last_title => {
                                last_title = t.clone();
                                Some(t.clone())
                            }
                            _ => None,
                        };
                        let ev = WatchEvent {
                            seq,
                            created_at: Instant::now(),
                            offset_sec: started_at.elapsed().as_secs(),
                            kind: WatchEventKind::Scene {
                                diff_pct,
                                width: out_w,
                                height: out_h,
                                bytes: bytes.len(),
                                est_tokens: estimate_vision_tokens(out_w, out_h),
                                path: file_path,
                                window_title: title_changed,
                            },
                        };
                        let mut tl = timeline.lock().unwrap();
                        tl.push(ev, retention);
                        prev_saved = Some(frame);
                        last_save_time = Instant::now();
                    }
                }
            }
        }

        // Sleep in small increments so stop_flag responds quickly
        let sleep_step = Duration::from_millis(100);
        let mut slept = Duration::ZERO;
        while slept < poll_dur && !stop.load(Ordering::Relaxed) {
            thread::sleep(sleep_step.min(poll_dur - slept));
            slept += sleep_step;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn audio_worker(
    stop: Arc<AtomicBool>,
    timeline: Arc<Mutex<WatchTimeline>>,
    session_dir: PathBuf,
    started_at: Instant,
    retention: Duration,
    channel: &'static str,
    kind: StreamKind,
    chunk_sec: f32,
    threshold_db: f32,
) {
    let stream = match WasapiStream::open(kind) {
        Ok(s) => s,
        Err(e) => {
            push_info(
                &timeline,
                started_at,
                retention,
                format!("Audio[{}] unavailable: {}", channel, e),
            );
            return;
        }
    };

    let max_chunk_dur = Duration::from_secs_f32(chunk_sec);
    let mut native_buf: Vec<f32> =
        Vec::with_capacity((stream.sample_rate as f32 * chunk_sec) as usize);
    let mut chunk_start = Instant::now();
    let mut active_since: Option<Instant> = None;
    let mut last_voice_time: Option<Instant> = None;

    while !stop.load(Ordering::Relaxed) {
        let prev_len = native_buf.len();
        if stream.drain_mono(&mut native_buf).is_err() {
            thread::sleep(Duration::from_millis(50));
            continue;
        }

        if native_buf.len() > prev_len {
            let db = frame_rms_db(&native_buf[prev_len..]);
            if db > threshold_db {
                let now = Instant::now();
                if active_since.is_none() {
                    active_since = Some(now);
                }
                last_voice_time = Some(now);
            }
        }

        // Flush early on natural utterance end (>= 2.0s speech followed by >= 0.85s silence)
        let natural_pause = match (active_since, last_voice_time) {
            (Some(a), Some(lv)) => {
                lv.duration_since(a) >= Duration::from_millis(2000)
                    && lv.elapsed() >= Duration::from_millis(850)
            }
            _ => false,
        };

        let chunk_ready = chunk_start.elapsed() >= max_chunk_dur || natural_pause;
        if chunk_ready {
            // Keep silent stretches inside the chunk so offsets within a clip stay true.
            stream.pad_to_now(&mut native_buf);
            let raw_sec = chunk_start.elapsed().as_secs_f32();
            flush_audio_chunk(
                &native_buf,
                stream.sample_rate,
                raw_sec,
                channel,
                threshold_db,
                &session_dir,
                started_at,
                retention,
                &timeline,
            );
            native_buf.clear();
            chunk_start = Instant::now();
            active_since = None;
            last_voice_time = None;
        }

        thread::sleep(Duration::from_millis(25));
    }

    // Flush any remaining active audio on stop
    if !native_buf.is_empty() && active_since.is_some() {
        let raw_sec = chunk_start.elapsed().as_secs_f32();
        flush_audio_chunk(
            &native_buf,
            stream.sample_rate,
            raw_sec,
            channel,
            threshold_db,
            &session_dir,
            started_at,
            retention,
            &timeline,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn flush_audio_chunk(
    native_mono: &[f32],
    native_rate: u32,
    raw_sec: f32,
    channel: &'static str,
    threshold_db: f32,
    session_dir: &Path,
    started_at: Instant,
    retention: Duration,
    timeline: &Arc<Mutex<WatchTimeline>>,
) {
    if native_mono.is_empty() {
        return;
    }
    let samples_16k = resample_mono(native_mono, native_rate, TARGET_SAMPLE_RATE);
    let (trimmed, metrics) = trim_silence(&samples_16k, TARGET_SAMPLE_RATE, threshold_db, 250);
    if trimmed.is_empty() || metrics.is_silent() {
        return;
    }

    let seq = {
        let mut tl = timeline.lock().unwrap();
        tl.alloc_seq()
    };
    let prefix = if channel == "system" { "sys" } else { "mic" };
    let file_path = session_dir.join(format!("{}_{:04}.wav", prefix, seq));
    if let Ok(bytes) = write_wav_pcm16(&file_path, &trimmed, TARGET_SAMPLE_RATE) {
        let saved_sec = trimmed.len() as f32 / TARGET_SAMPLE_RATE as f32;
        let ev = WatchEvent {
            seq,
            created_at: Instant::now(),
            offset_sec: started_at.elapsed().as_secs(),
            kind: WatchEventKind::Audio {
                channel,
                label: metrics.label,
                saved_sec,
                raw_sec,
                rms_db: metrics.rms_db,
                bytes,
                est_tokens: estimate_audio_tokens(saved_sec),
                path: file_path,
            },
        };
        let mut tl = timeline.lock().unwrap();
        tl.push(ev, retention);
    }
}

/// Deletes a session folder. `fs::remove_dir_all` was observed failing with
/// ERROR_SHARING_VIOLATION (32) on an *empty* session folder on Windows 10, leaving one empty
/// `w_*` folder per session behind, while plain `RemoveDirectoryW` succeeds. So delete the
/// files one by one, then the folder, retrying briefly for antivirus/indexer handles.
fn remove_dir_with_retry(dir: &Path) {
    for attempt in 0..10u64 {
        if let Ok(entries) = fs::read_dir(dir) {
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    remove_dir_with_retry(&p);
                } else {
                    let _ = fs::remove_file(&p);
                }
            }
        }
        let _ = fs::remove_dir(dir);
        if !dir.exists() {
            return;
        }
        thread::sleep(Duration::from_millis(25 * (attempt + 1)));
    }
}

/// Removes empty `w_*` folders left behind by earlier sessions (never touches non-empty ones).
fn sweep_empty_session_dirs(root: &Path) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        let is_session = p.is_dir()
            && p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("w_"));
        if is_session
            && fs::read_dir(&p)
                .map(|mut d| d.next().is_none())
                .unwrap_or(false)
        {
            let _ = fs::remove_dir(&p);
        }
    }
}

fn push_info(
    timeline: &Arc<Mutex<WatchTimeline>>,
    started_at: Instant,
    retention: Duration,
    message: String,
) {
    let mut tl = timeline.lock().unwrap();
    let seq = tl.alloc_seq();
    let ev = WatchEvent {
        seq,
        created_at: Instant::now(),
        offset_sec: started_at.elapsed().as_secs(),
        kind: WatchEventKind::Info { message },
    };
    tl.push(ev, retention);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_buffer_evicts_expired_events_and_deletes_files() {
        let tmp = std::env::temp_dir().join(format!("eyra_watch_evict_{}.wav", std::process::id()));
        fs::write(&tmp, b"dummy").unwrap();
        assert!(tmp.exists());

        let mut tl = WatchTimeline::new();
        let old_ev = WatchEvent {
            seq: tl.alloc_seq(),
            created_at: Instant::now() - Duration::from_secs(120),
            offset_sec: 1,
            kind: WatchEventKind::Audio {
                channel: "system",
                label: "speech",
                saved_sec: 2.0,
                raw_sec: 4.0,
                rms_db: -20.0,
                bytes: 64_000,
                est_tokens: 64,
                path: tmp.clone(),
            },
        };
        tl.push(old_ev, Duration::from_secs(60));
        assert!(tl.events.is_empty());
        assert!(!tmp.exists());
    }

    #[test]
    fn compact_line_formats_scene_and_audio_cleanly() {
        let ev = WatchEvent {
            seq: 3,
            created_at: Instant::now(),
            offset_sec: 65,
            kind: WatchEventKind::Audio {
                channel: "mic",
                label: "speech",
                saved_sec: 3.2,
                raw_sec: 8.0,
                rms_db: -18.5,
                bytes: 102_400,
                est_tokens: 103,
                path: PathBuf::from("C:\\tmp\\mic_0003.wav"),
            },
        };
        let line = ev.to_compact_line();
        assert!(line.contains("#3 t=01:05 AUDIO[mic] speech 3.2s/8.0s -18.5dB"));
        assert!(line.contains("mic_0003.wav"));
    }

    #[test]
    fn sweep_removes_only_empty_session_dirs() {
        let root = std::env::temp_dir().join(format!("eyra_sweep_{}", std::process::id()));
        let empty = root.join("w_1");
        let full = root.join("w_2");
        let other = root.join("keep_me");
        for d in [&empty, &full, &other] {
            fs::create_dir_all(d).unwrap();
        }
        fs::write(full.join("sys_0001.wav"), b"x").unwrap();
        sweep_empty_session_dirs(&root);
        assert!(!empty.exists());
        assert!(full.exists());
        assert!(other.exists());
        remove_dir_with_retry(&root);
        assert!(!root.exists());
    }
}
