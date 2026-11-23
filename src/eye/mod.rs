pub mod capture;
pub mod diff;
pub mod ocr;
pub mod som;
pub mod uia;
pub mod windows;

use crate::state::AppState;
use crate::types::{CapturedFrame, CoordStyle, Rect, SeeMode, SeeSnapshot, UiElement, WindowInfo};
use anyhow::{bail, Result};
use base64::Engine;
use std::thread;
use std::time::{Duration, Instant};

use self::capture::{apply_redactions, capture_screen, capture_window, crop_frame, encode_frame};
use self::diff::{diff_frames, diff_snapshots};
use self::ocr::{guard_ocr_passwords, run_ocr};
use self::som::draw_som_overlay;
use self::uia::{is_chromium_window, nudge_chromium_accessibility, scan_uia};
use self::windows::{list_windows, open_popup_menus, resolve_target, CaptureTarget};

/// A document-less Chromium window with at least this many elements is treated as a real
/// UI without web content (no warm-up wait).
const CHROMIUM_WARMUP_SKIP_ABOVE: usize = 40;
/// Upper bound on how long `see` waits for a cold Chromium accessibility tree.
const CHROMIUM_WARMUP_MAX: Duration = Duration::from_millis(2000);

#[derive(Debug, Clone)]
pub struct SeeOptions {
    pub window: Option<String>,
    pub mode: SeeMode,
    pub max_elements: usize,
    pub max_depth: usize,
    pub ocr_lang: Option<String>,
    pub interactive_only: bool,
    pub coords: CoordStyle,
}

impl Default for SeeOptions {
    fn default() -> Self {
        Self {
            window: None,
            mode: SeeMode::Auto,
            max_elements: 120,
            max_depth: 14,
            ocr_lang: None,
            interactive_only: false,
            coords: CoordStyle::Rect,
        }
    }
}

pub fn see(state: &mut AppState, opts: &SeeOptions) -> Result<SeeSnapshot> {
    let t0 = Instant::now();
    let target = resolve_target(opts.window.as_deref(), state.locked_window, &state.safety)?;

    let (window_info, clip_rect, hwnd_opt) = match target {
        CaptureTarget::Window(w) => {
            if state.safety.should_redact_window(&w) {
                let snap = SeeSnapshot {
                    window: Some(w.clone()),
                    screen_rect: w.rect,
                    mode_used: "redacted".to_string(),
                    elements: Vec::new(),
                    redacted_rects: vec![w.rect],
                    sensitive_blocked: true,
                    elapsed_ms: t0.elapsed().as_millis() as u64,
                    coords: opts.coords,
                    page_not_ready: false,
                };
                state.remember_snapshot(snap.clone());
                return Ok(snap);
            }
            if w.is_minimized {
                // Minimized windows sit at (-32000,-32000): their element positions are useless
                // and their content is not rendered, so say so instead of returning junk.
                let snap = SeeSnapshot {
                    window: Some(w.clone()),
                    screen_rect: w.rect,
                    mode_used: "minimized".to_string(),
                    elements: Vec::new(),
                    redacted_rects: Vec::new(),
                    sensitive_blocked: false,
                    elapsed_ms: t0.elapsed().as_millis() as u64,
                    coords: opts.coords,
                    page_not_ready: false,
                };
                state.remember_snapshot(snap.clone());
                return Ok(snap);
            }
            let rect = w.rect;
            let hwnd = w.hwnd;
            (Some(w), rect, Some(hwnd))
        }
        CaptureTarget::Screen(r) => (None, r, None),
    };

    let mut elements: Vec<UiElement> = Vec::new();
    let mut redacted_rects: Vec<Rect> = Vec::new();
    let mut used_uia = false;
    let mut used_ocr = false;
    // Set when a Chromium/Electron window still shows no web document after the warm-up
    let mut chromium_page_missing = false;

    // Layer 1: UIA tree
    if opts.mode != SeeMode::Ocr {
        if let Ok(mut uia_res) = scan_uia(hwnd_opt, clip_rect, opts.max_elements, opts.max_depth) {
            // Chromium/Electron builds the web-content accessibility tree lazily: the first scan
            // of a cold process returns only the browser frame (tabs, toolbar) and no document.
            // Nudge it like a screen reader would and rescan until the page shows up.
            if let Some(h) = hwnd_opt {
                let has_document = |els: &[UiElement]| els.iter().any(|e| e.role == "doc");
                // A minimized window never exposes its page, so waiting would just burn 2 s.
                let minimized = window_info.as_ref().is_some_and(|w| w.is_minimized);
                if !minimized
                    && !has_document(&uia_res.elements)
                    && uia_res.elements.len() < CHROMIUM_WARMUP_SKIP_ABOVE
                    && is_chromium_window(h)
                {
                    let debug = std::env::var_os("EYRA_DEBUG").is_some();
                    let started = Instant::now();
                    if debug {
                        eprintln!("[warmup] initial={}", uia_res.elements.len());
                    }
                    nudge_chromium_accessibility(h);
                    // Poll until the tree is populated and has stopped growing, max ~2 s.
                    let mut stable_rounds = 0;
                    while started.elapsed() < CHROMIUM_WARMUP_MAX {
                        thread::sleep(Duration::from_millis(150));
                        let Ok(r) =
                            scan_uia(hwnd_opt, clip_rect, opts.max_elements, opts.max_depth)
                        else {
                            break;
                        };
                        let grew = r.elements.len() > uia_res.elements.len();
                        uia_res = r;
                        if debug {
                            eprintln!(
                                "[warmup] t={}ms n={}",
                                started.elapsed().as_millis(),
                                uia_res.elements.len()
                            );
                        }
                        stable_rounds = if grew { 0 } else { stable_rounds + 1 };
                        if has_document(&uia_res.elements) && stable_rounds >= 1 {
                            break;
                        }
                    }
                    chromium_page_missing = !has_document(&uia_res.elements);
                }
            }
            used_uia = true;
            redacted_rects.extend(uia_res.redacted_rects);
            elements.extend(uia_res.elements);

            // Include popup menus the window has open (File/Edit menus, context menus).
            if let Some(h) = hwnd_opt {
                for popup in open_popup_menus(h) {
                    if let Ok(menu) = scan_uia(
                        Some(popup),
                        Rect::default(),
                        opts.max_elements,
                        opts.max_depth,
                    ) {
                        redacted_rects.extend(menu.redacted_rects);
                        elements.extend(menu.elements);
                    }
                }
            }
        }
    }

    // Layer 2: WinRT OCR (explicit, hybrid, or automatic fallback when UIA is sparse)
    let need_ocr = match opts.mode {
        SeeMode::Ocr | SeeMode::Both => true,
        // A browser whose page tree isn't ready yet only shows its toolbar: read the page by OCR
        SeeMode::Auto => elements.len() < 5 || chromium_page_missing,
        SeeMode::Uia => false,
    };

    if need_ocr {
        if let Ok(mut frame) = capture_target_frame(window_info.as_ref(), clip_rect) {
            // Redact passwords & sensitive windows BEFORE running OCR so secrets never enter OCR text
            let mut all_redacts = redacted_rects.clone();
            if window_info.is_none() {
                all_redacts.extend(collect_sensitive_window_rects(state));
            }
            apply_redactions(&mut frame, &all_redacts);

            if let Ok(mut ocr_els) = run_ocr(&frame, opts.ocr_lang.as_deref()) {
                used_ocr = true;
                // OCR has no notion of password fields: mask likely ones and remember their
                // rects so screenshots of this window black them out too.
                redacted_rects.extend(guard_ocr_passwords(&mut ocr_els));
                if used_uia {
                    merge_ocr_into_uia(&mut elements, ocr_els);
                } else {
                    elements = ocr_els;
                }
            }
        }
    }

    if opts.interactive_only {
        elements.retain(|e| e.is_interactive || e.is_password);
    }

    // Remove duplicate/identical elements at same rect
    deduplicate_elements(&mut elements);
    // Drop labels that only repeat their parent control (e.g. txt "Kaggle" inside lnk "Kaggle")
    prune_redundant_elements(&mut elements);

    // Sort in human reading order (top-to-bottom in 12px row bands, then left-to-right)
    elements.sort_by_key(|e| (e.rect.y / 12, e.rect.x));

    if elements.len() > opts.max_elements {
        elements.truncate(opts.max_elements);
    }

    // Assign sequential 1-based IDs
    for (idx, el) in elements.iter_mut().enumerate() {
        el.id = (idx + 1) as u32;
    }

    let mode_used = match (used_uia, used_ocr) {
        (true, true) => "uia+ocr",
        (true, false) => "uia",
        (false, true) => "ocr",
        (false, false) => "none",
    }
    .to_string();

    let snap = SeeSnapshot {
        window: window_info,
        screen_rect: clip_rect,
        mode_used,
        elements,
        redacted_rects,
        sensitive_blocked: false,
        elapsed_ms: t0.elapsed().as_millis() as u64,
        coords: opts.coords,
        page_not_ready: chromium_page_missing,
    };

    state.remember_snapshot(snap.clone());
    Ok(snap)
}

pub fn see_diff(state: &mut AppState, opts: &SeeOptions) -> Result<(String, SeeSnapshot)> {
    // Baselines are kept per target window, so alternating between windows still yields
    // compact diffs instead of full re-dumps.
    let snapshots_before = state.snapshots.clone();
    let curr = see(state, opts)?;
    let key = curr.window.as_ref().map(|w| w.hwnd);
    let prev = snapshots_before.get(&key);
    let text = match prev {
        Some(p) => diff_snapshots(p, &curr).to_compact_text(&curr),
        None => curr.to_compact_text(),
    };
    Ok((text, curr))
}

#[derive(Debug, Clone)]
pub struct ScreenshotOptions {
    pub window: Option<String>,
    pub diff_only: bool,
    pub crop: Option<Rect>,
    pub som: bool,
    pub max_width: u32,
    pub quality: u8,
    pub format: String,
}

impl Default for ScreenshotOptions {
    fn default() -> Self {
        Self {
            window: None,
            diff_only: false,
            crop: None,
            som: false,
            max_width: 1024,
            quality: 65,
            format: "jpeg".to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ScreenshotResult {
    pub summary: String,
    pub bytes: Vec<u8>,
    pub base64: String,
    pub mime_type: &'static str,
    pub width: u32,
    pub height: u32,
    pub no_change: bool,
}

pub fn screenshot(state: &mut AppState, opts: &ScreenshotOptions) -> Result<ScreenshotResult> {
    let t0 = Instant::now();
    let target = resolve_target(opts.window.as_deref(), state.locked_window, &state.safety)?;

    let (window_info, clip_rect) = match target {
        CaptureTarget::Window(w) => {
            if state.safety.should_redact_window(&w) {
                bail!(
                    "Screenshot blocked: target window \"{}\" ({}) is protected as sensitive.",
                    w.title,
                    w.process
                );
            }
            let r = w.rect;
            (Some(w), r)
        }
        CaptureTarget::Screen(r) => (None, r),
    };

    let mut raw_frame = capture_target_frame(window_info.as_ref(), clip_rect)?;

    // Gather redaction rectangles: visible sensitive windows + known password fields
    let mut redacts = collect_sensitive_window_rects(state);
    if let Some(snap) = &state.last_snapshot {
        let same = match (&snap.window, &window_info) {
            (Some(a), Some(b)) => a.hwnd == b.hwnd,
            (None, None) => true,
            _ => false,
        };
        if same {
            redacts.extend_from_slice(&snap.redacted_rects);
        }
    }
    apply_redactions(&mut raw_frame, &redacts);

    // Compute diff BEFORE modifying raw_frame with overlays
    let mut diff_note = String::new();
    let mut working_frame = if opts.diff_only {
        if let Some(prev) = &state.last_frame {
            if prev.width == raw_frame.width
                && prev.height == raw_frame.height
                && prev.origin_x == raw_frame.origin_x
                && prev.origin_y == raw_frame.origin_y
            {
                let pd = diff_frames(prev, &raw_frame, 18);
                if pd.changed_blocks == 0 {
                    state.last_frame = Some(raw_frame);
                    return Ok(ScreenshotResult {
                        summary: format!(
                            "SCREENSHOT diff=0.0% (no visual change since previous capture) ms={}",
                            t0.elapsed().as_millis()
                        ),
                        bytes: Vec::new(),
                        base64: String::new(),
                        mime_type: "image/jpeg",
                        width: 0,
                        height: 0,
                        no_change: true,
                    });
                }
                if let Some(changed_rect) = pd.changed_rect {
                    diff_note = format!(
                        " diff={:.1}% changed_rect={},{},{},{}",
                        pd.changed_percent,
                        changed_rect.x,
                        changed_rect.y,
                        changed_rect.w,
                        changed_rect.h
                    );
                    if pd.changed_percent < 75.0 {
                        crop_frame(&raw_frame, changed_rect)?
                    } else {
                        raw_frame.clone()
                    }
                } else {
                    raw_frame.clone()
                }
            } else {
                diff_note = " diff=full(size_changed)".to_string();
                raw_frame.clone()
            }
        } else {
            diff_note = " diff=full(initial)".to_string();
            raw_frame.clone()
        }
    } else {
        raw_frame.clone()
    };

    // Save clean baseline frame for subsequent diffs
    state.last_frame = Some(raw_frame);

    // Optional explicit crop (in screen coordinates)
    if let Some(crop_rect) = opts.crop {
        working_frame = crop_frame(&working_frame, crop_rect)?;
    }

    // Optional Set-of-Mark numbered overlay
    if opts.som {
        let elements_for_som = if let Some(snap) = &state.last_snapshot {
            let same = match (&snap.window, &window_info) {
                (Some(a), Some(b)) => a.hwnd == b.hwnd,
                (None, None) => true,
                _ => false,
            };
            if same && !snap.elements.is_empty() {
                snap.elements.clone()
            } else {
                see(
                    state,
                    &SeeOptions {
                        window: opts.window.clone(),
                        ..Default::default()
                    },
                )
                .map(|s| s.elements)
                .unwrap_or_default()
            }
        } else {
            see(
                state,
                &SeeOptions {
                    window: opts.window.clone(),
                    ..Default::default()
                },
            )
            .map(|s| s.elements)
            .unwrap_or_default()
        };
        draw_som_overlay(&mut working_frame, &elements_for_som);
    }

    let (bytes, out_w, out_h, mime_type) =
        encode_frame(&working_frame, opts.max_width, opts.quality, &opts.format)?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);

    let target_label = if let Some(w) = &window_info {
        format!("hwnd={} proc={}", w.hwnd_hex(), w.process)
    } else {
        "screen".to_string()
    };

    let summary = format!(
        "SCREENSHOT {} origin={},{} src={}x{} out={}x{} bytes={} som={}{} ms={}",
        target_label,
        working_frame.origin_x,
        working_frame.origin_y,
        working_frame.width,
        working_frame.height,
        out_w,
        out_h,
        bytes.len(),
        opts.som,
        diff_note,
        t0.elapsed().as_millis()
    );

    Ok(ScreenshotResult {
        summary,
        bytes,
        base64: b64,
        mime_type,
        width: out_w,
        height: out_h,
        no_change: false,
    })
}

pub struct WaitChangeResult {
    pub changed: bool,
    pub changed_percent: f32,
    pub changed_rect: Option<Rect>,
    pub elapsed_ms: u64,
}

/// Fewest changed 16x16 blocks that count as a change in `wait_change`.
const WAIT_CHANGE_MIN_BLOCKS: usize = 3;

pub fn wait_change(
    state: &mut AppState,
    window: Option<&str>,
    timeout_ms: u64,
    poll_ms: u64,
    threshold_pct: f32,
) -> Result<WaitChangeResult> {
    let t0 = Instant::now();
    let target = resolve_target(window, state.locked_window, &state.safety)?;
    let (win_info, clip_rect) = match target {
        CaptureTarget::Window(w) => {
            let r = w.rect;
            (Some(w), r)
        }
        CaptureTarget::Screen(r) => (None, r),
    };

    let baseline = capture_target_frame(win_info.as_ref(), clip_rect)?;
    let poll_dur = Duration::from_millis(poll_ms.clamp(50, 2000));
    let timeout_dur = Duration::from_millis(timeout_ms.clamp(100, 60_000));

    loop {
        if t0.elapsed() >= timeout_dur {
            state.last_frame = Some(baseline);
            return Ok(WaitChangeResult {
                changed: false,
                changed_percent: 0.0,
                changed_rect: None,
                elapsed_ms: t0.elapsed().as_millis() as u64,
            });
        }

        thread::sleep(poll_dur);
        let curr = capture_target_frame(win_info.as_ref(), clip_rect)?;
        let pd = diff_frames(&baseline, &curr, 18);
        // A blinking caret or a hover highlight touches 1-2 blocks; real content changes more
        if pd.changed_percent >= threshold_pct && pd.changed_blocks >= WAIT_CHANGE_MIN_BLOCKS {
            state.last_frame = Some(curr);
            return Ok(WaitChangeResult {
                changed: true,
                changed_percent: pd.changed_percent,
                changed_rect: pd.changed_rect,
                elapsed_ms: t0.elapsed().as_millis() as u64,
            });
        }
    }
}

fn capture_target_frame(window: Option<&WindowInfo>, clip_rect: Rect) -> Result<CapturedFrame> {
    if let Some(w) = window {
        capture_window(w.hwnd, w.rect)
    } else {
        capture_screen(clip_rect)
    }
}

fn collect_sensitive_window_rects(state: &AppState) -> Vec<Rect> {
    if state.safety.config.allow_sensitive {
        return Vec::new();
    }
    let Ok(wins) = list_windows(&state.safety) else {
        return Vec::new();
    };
    wins.into_iter()
        .filter(|w| w.is_sensitive && !w.is_minimized && w.rect.is_valid())
        .map(|w| w.rect)
        .collect()
}

fn merge_ocr_into_uia(uia_elements: &mut Vec<UiElement>, ocr_elements: Vec<UiElement>) {
    for ocr_el in ocr_elements {
        let ocr_low = ocr_el.name.to_ascii_lowercase();
        let overlaps_existing = uia_elements.iter().any(|u| {
            let ratio = ocr_el.rect.overlap_ratio_of_self(&u.rect);
            if ratio >= 0.55 {
                let u_name = u.name.to_ascii_lowercase();
                let u_val = u.value.as_deref().unwrap_or_default().to_ascii_lowercase();
                if u_name.contains(&ocr_low)
                    || ocr_low.contains(&u_name) && !u_name.is_empty()
                    || (!u_val.is_empty() && (u_val.contains(&ocr_low) || ocr_low.contains(&u_val)))
                {
                    return true;
                }
                if ocr_el.rect.iou(&u.rect) >= 0.65 {
                    return true;
                }
            }
            false
        });

        if !overlaps_existing {
            uia_elements.push(ocr_el);
        }
    }
}

/// Roles an agent can act on; a static label fully inside one of these that repeats its
/// name adds tokens but no information.
fn is_actionable_role(role: &str) -> bool {
    matches!(
        role,
        "btn"
            | "lnk"
            | "inp"
            | "chk"
            | "rad"
            | "cmb"
            | "tab"
            | "mnu"
            | "itm"
            | "sld"
            | "hdr"
            | "pwd"
    )
}

fn prune_redundant_elements(elements: &mut Vec<UiElement>) {
    let norm = |s: &str| s.trim().to_lowercase();
    let snapshot: Vec<UiElement> = elements.clone();
    let mut keep = vec![true; elements.len()];
    for (i, el) in snapshot.iter().enumerate() {
        let name = norm(&el.name);
        match el.role.as_str() {
            // Static text / images / generic nodes that just echo an enclosing control's label
            "txt" | "img" | "el" if !name.is_empty() && !el.is_password => {
                let echoed = snapshot.iter().enumerate().any(|(j, p)| {
                    j != i
                        && is_actionable_role(&p.role)
                        && el.rect.overlap_ratio_of_self(&p.rect) >= 0.9
                        && (norm(&p.name).contains(&name)
                            || p.value
                                .as_deref()
                                .map(norm)
                                .is_some_and(|v| v.contains(&name)))
                });
                if echoed {
                    keep[i] = false;
                }
            }
            // List/grid rows whose name just repeats the link or button they wrap
            // (GitHub file lists, nav lists): keep the inner control, which is what gets clicked.
            "itm" if !name.is_empty() => {
                let wrapped = snapshot.iter().enumerate().any(|(j, c)| {
                    if j == i || !matches!(c.role.as_str(), "lnk" | "btn" | "chk" | "rad" | "inp") {
                        return false;
                    }
                    let cname = norm(&c.name);
                    !cname.is_empty()
                        && c.rect.overlap_ratio_of_self(&el.rect) >= 0.9
                        && (cname == name
                            || (name.starts_with(&cname)
                                && cname.chars().count() * 10 >= name.chars().count() * 6))
                });
                if wrapped {
                    keep[i] = false;
                }
            }
            // Unnamed, value-less rows that only group the cells listed right after them
            "itm" if name.is_empty() && el.value.is_none() && el.state.is_none() => {
                let groups_others = snapshot.iter().enumerate().any(|(j, c)| {
                    j != i
                        && !c.name.trim().is_empty()
                        && c.rect.overlap_ratio_of_self(&el.rect) >= 0.9
                });
                if groups_others {
                    keep[i] = false;
                }
            }
            // Unnamed documents nested in / stacked on a named document (iframes, app shells)
            "doc" if name.is_empty() => {
                let covered = snapshot.iter().enumerate().any(|(j, p)| {
                    j != i
                        && p.role == "doc"
                        && !p.name.trim().is_empty()
                        && el.rect.overlap_ratio_of_self(&p.rect) >= 0.9
                });
                if covered {
                    keep[i] = false;
                }
            }
            _ => {}
        }
    }
    let mut idx = 0;
    elements.retain(|_| {
        let k = keep[idx];
        idx += 1;
        k
    });
}

fn deduplicate_elements(elements: &mut Vec<UiElement>) {
    let mut kept: Vec<UiElement> = Vec::with_capacity(elements.len());
    for el in elements.drain(..) {
        let dup = kept.iter().any(|k| {
            k.role == el.role
                && k.name == el.name
                && k.value == el.value
                && k.rect.iou(&el.rect) > 0.85
        });
        if !dup {
            kept.push(el);
        }
    }
    *elements = kept;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ElementSource;

    fn el(role: &str, name: &str, r: Rect) -> UiElement {
        UiElement {
            id: 0,
            role: role.into(),
            name: name.into(),
            value: None,
            state: None,
            rect: r,
            source: ElementSource::Uia,
            is_password: false,
            is_interactive: role != "txt",
            runtime_id: vec![],
        }
    }

    #[test]
    fn labels_echoing_their_control_are_pruned() {
        let mut els = vec![
            el("lnk", "Kaggle", Rect::new(291, 553, 112, 112)),
            el("txt", "Kaggle", Rect::new(327, 632, 40, 17)), // echo inside link -> drop
            el("txt", "Datasets", Rect::new(600, 632, 60, 17)), // standalone text -> keep
            el("btn", "Save", Rect::new(10, 10, 80, 30)),
            el("txt", "Cancel", Rect::new(20, 15, 40, 17)), // inside but different -> keep
            el("doc", "New Tab", Rect::new(0, 121, 1366, 607)),
            el("doc", "", Rect::new(0, 121, 1366, 607)), // unnamed nested doc -> drop
            el("itm", "Pull requests (1.5k)", Rect::new(244, 225, 164, 40)), // wraps link -> drop
            el("lnk", "Pull requests (1.5k)", Rect::new(244, 225, 164, 32)),
            el("itm", "Settings", Rect::new(500, 225, 100, 40)), // no inner control -> keep
            el("itm", "", Rect::new(51, 460, 641, 40)), // unnamed row grouping cells -> drop
            el("itm", ".github, (Directory)", Rect::new(51, 460, 257, 40)),
        ];
        prune_redundant_elements(&mut els);
        let names: Vec<(&str, &str)> = els
            .iter()
            .map(|e| (e.role.as_str(), e.name.as_str()))
            .collect();
        assert_eq!(
            names,
            vec![
                ("lnk", "Kaggle"),
                ("txt", "Datasets"),
                ("btn", "Save"),
                ("txt", "Cancel"),
                ("doc", "New Tab"),
                ("lnk", "Pull requests (1.5k)"),
                ("itm", "Settings"),
                ("itm", ".github, (Directory)")
            ]
        );
    }
}
