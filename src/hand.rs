use crate::eye::uia::{try_uia_invoke, try_uia_set_value};
use crate::eye::windows::{
    get_foreground_window_info, get_window_info_by_hwnd, init_dpi_awareness, resolve_target,
    virtual_screen_rect, CaptureTarget,
};
use crate::state::AppState;
use crate::types::WindowInfo;
use anyhow::{anyhow, bail, Result};
use std::ffi::c_void;
use std::mem::size_of;
use std::thread;
use std::time::Duration;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
    KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK,
    MOUSEEVENTF_WHEEL, MOUSEINPUT, VIRTUAL_KEY, VK_APPS, VK_BACK, VK_CONTROL, VK_DELETE, VK_DOWN,
    VK_END, VK_ESCAPE, VK_F1, VK_F10, VK_F11, VK_F12, VK_F2, VK_F3, VK_F4, VK_F5, VK_F6, VK_F7,
    VK_F8, VK_F9, VK_HOME, VK_INSERT, VK_LEFT, VK_LWIN, VK_MENU, VK_NEXT, VK_PRIOR, VK_RETURN,
    VK_RIGHT, VK_SHIFT, VK_SNAPSHOT, VK_SPACE, VK_TAB, VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, GetForegroundWindow, GetWindowThreadProcessId, IsIconic, SetForegroundWindow,
    ShowWindow, SW_RESTORE,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

impl MouseButton {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "right" | "r" => Self::Right,
            "middle" | "m" => Self::Middle,
            _ => Self::Left,
        }
    }
}

pub fn focus_window(state: &mut AppState, window_query: &str, lock: bool) -> Result<String> {
    init_dpi_awareness();
    let target = resolve_target(Some(window_query), state.locked_window, &state.safety)?;
    let CaptureTarget::Window(win) = target else {
        bail!("Cannot focus full screen; specify a window title, process name, or hwnd.");
    };

    state.safety.check_write_allowed(Some(&win))?;
    bring_hwnd_to_foreground(win.hwnd)?;

    if lock {
        state.locked_window = Some(win.hwnd);
    }

    Ok(format!(
        "Focused {} ({}) \"{}\"{}",
        win.hwnd_hex(),
        win.process,
        win.title,
        if lock { " [locked]" } else { "" }
    ))
}

pub fn bring_hwnd_to_foreground(hwnd_val: usize) -> Result<()> {
    let hwnd = HWND(hwnd_val as *mut c_void);
    unsafe {
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
            thread::sleep(Duration::from_millis(80));
        }
        if SetForegroundWindow(hwnd).as_bool() {
            thread::sleep(Duration::from_millis(40));
            return Ok(());
        }

        // Foreground-lock workaround via AttachThreadInput
        let fg = GetForegroundWindow();
        let fg_tid = GetWindowThreadProcessId(fg, None);
        let target_tid = GetWindowThreadProcessId(hwnd, None);
        let cur_tid = GetCurrentThreadId();

        if fg_tid != 0 && fg_tid != cur_tid {
            let _ = AttachThreadInput(cur_tid, fg_tid, true);
        }
        if target_tid != 0 && target_tid != cur_tid {
            let _ = AttachThreadInput(cur_tid, target_tid, true);
        }

        let _ = BringWindowToTop(hwnd);
        let _ = SetForegroundWindow(hwnd);

        if target_tid != 0 && target_tid != cur_tid {
            let _ = AttachThreadInput(cur_tid, target_tid, false);
        }
        if fg_tid != 0 && fg_tid != cur_tid {
            let _ = AttachThreadInput(cur_tid, fg_tid, false);
        }

        thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct ClickOptions {
    pub id: Option<u32>,
    pub x: Option<i32>,
    pub y: Option<i32>,
    pub window: Option<String>,
    pub button: MouseButton,
    pub clicks: u32,
    pub background: bool,
}

impl Default for ClickOptions {
    fn default() -> Self {
        Self {
            id: None,
            x: None,
            y: None,
            window: None,
            button: MouseButton::Left,
            clicks: 1,
            background: true,
        }
    }
}

pub fn click(state: &AppState, opts: &ClickOptions) -> Result<String> {
    init_dpi_awareness();

    let (target_x, target_y, target_win, element_desc, bg_uia_ref) = if let Some(id) = opts.id {
        let Some((el, hwnd_opt)) = state.find_element(id) else {
            bail!(
                "Element #{} not found in last snapshot. Call see() first.",
                id
            );
        };
        let win_info = hwnd_opt.and_then(|h| get_window_info_by_hwnd(h, &state.safety));
        let (cx, cy) = el.rect.center();
        let desc = format!("#{} {} \"{}\"", el.id, el.role, el.name);
        let bg_ref = hwnd_opt.map(|h| (h, el.runtime_id.clone()));
        (cx, cy, win_info, desc, bg_ref)
    } else if let (Some(x), Some(y)) = (opts.x, opts.y) {
        let win_info = resolve_optional_window(state, opts.window.as_deref())?;
        (x, y, win_info, format!("({},{})", x, y), None)
    } else {
        bail!("Provide either element `id` or coordinates (`x`, `y`).");
    };

    state.safety.check_write_allowed(target_win.as_ref())?;

    // Try background UIA invocation first (no cursor hijack / no focus steal)
    if opts.background && opts.button == MouseButton::Left && opts.clicks == 1 {
        if let Some((hwnd, ref rid)) = bg_uia_ref {
            if let Ok(Some(pattern)) = try_uia_invoke(hwnd, rid, Some((target_x, target_y))) {
                return Ok(format!(
                    "Clicked {} via background {}",
                    element_desc, pattern
                ));
            }
        }
    }

    // Physical fallback: bring window to foreground if specified/known, then SendInput
    if let Some(win) = &target_win {
        if !win.is_foreground {
            bring_hwnd_to_foreground(win.hwnd)?;
        }
    }

    state.safety.check_failsafe()?;
    send_mouse_click(target_x, target_y, opts.button, opts.clicks.clamp(1, 3))?;

    Ok(format!(
        "Clicked {} at ({},{}) button={:?} clicks={}",
        element_desc, target_x, target_y, opts.button, opts.clicks
    ))
}

#[derive(Debug, Clone)]
pub struct TypeOptions {
    pub text: String,
    pub id: Option<u32>,
    pub window: Option<String>,
    pub clear: bool,
    pub press_enter: bool,
    pub background: bool,
}

pub fn type_text(state: &AppState, opts: &TypeOptions) -> Result<String> {
    init_dpi_awareness();

    let (target_win, click_coords, bg_uia_ref, target_desc) = if let Some(id) = opts.id {
        let Some((el, hwnd_opt)) = state.find_element(id) else {
            bail!(
                "Element #{} not found in last snapshot. Call see() first.",
                id
            );
        };
        let win_info = hwnd_opt.and_then(|h| get_window_info_by_hwnd(h, &state.safety));
        let coords = Some(el.rect.center());
        let bg = hwnd_opt.map(|h| (h, el.runtime_id.clone()));
        (win_info, coords, bg, format!("#{} \"{}\"", el.id, el.name))
    } else {
        let win_info = resolve_optional_window(state, opts.window.as_deref())?;
        (win_info, None, None, "focused element".to_string())
    };

    state.safety.check_write_allowed(target_win.as_ref())?;

    // Background UIA SetValue fast path (when no Enter press needed)
    if opts.background && !opts.press_enter {
        if let Some((hwnd, ref rid)) = bg_uia_ref {
            if try_uia_set_value(hwnd, rid, &opts.text, click_coords).unwrap_or(false) {
                return Ok(format!(
                    "Typed {} chars into {} via background UIA ValuePattern",
                    opts.text.chars().count(),
                    target_desc
                ));
            }
        }
    }

    if let Some(win) = &target_win {
        if !win.is_foreground {
            bring_hwnd_to_foreground(win.hwnd)?;
        }
    }

    if let Some((cx, cy)) = click_coords {
        state.safety.check_failsafe()?;
        send_mouse_click(cx, cy, MouseButton::Left, 1)?;
        thread::sleep(Duration::from_millis(40));
    }

    if opts.clear {
        state.safety.check_failsafe()?;
        send_hotkey_vks(&[VK_CONTROL, VIRTUAL_KEY(b'A' as u16)])?;
        thread::sleep(Duration::from_millis(20));
        send_hotkey_vks(&[VK_BACK])?;
        thread::sleep(Duration::from_millis(20));
    }

    state.safety.check_failsafe()?;
    send_unicode_text(&opts.text)?;

    if opts.press_enter {
        thread::sleep(Duration::from_millis(25));
        send_hotkey_vks(&[VK_RETURN])?;
    }

    Ok(format!(
        "Typed {} chars into {}{}",
        opts.text.chars().count(),
        target_desc,
        if opts.press_enter { " + Enter" } else { "" }
    ))
}

pub fn hotkey(state: &AppState, keys_expr: &str, window: Option<&str>) -> Result<String> {
    init_dpi_awareness();
    let target_win = resolve_optional_window(state, window)?;
    let (normalized, vks) = parse_hotkey(keys_expr)?;
    state
        .safety
        .check_hotkey_allowed(&normalized, target_win.as_ref())?;

    if let Some(win) = &target_win {
        if !win.is_foreground {
            bring_hwnd_to_foreground(win.hwnd)?;
        }
    }

    send_hotkey_vks(&vks)?;
    Ok(format!("Sent hotkey: {}", normalized.join("+")))
}

pub fn scroll(
    state: &AppState,
    delta_y: i32,
    delta_x: i32,
    x: Option<i32>,
    y: Option<i32>,
    window: Option<&str>,
) -> Result<String> {
    init_dpi_awareness();
    let target_win = resolve_optional_window(state, window)?;
    state.safety.check_write_allowed(target_win.as_ref())?;

    if let Some(win) = &target_win {
        if !win.is_foreground {
            bring_hwnd_to_foreground(win.hwnd)?;
        }
    }

    let pos = match (x, y) {
        (Some(px), Some(py)) => Some((px, py)),
        _ => target_win.as_ref().map(|w| w.rect.center()),
    };

    if let Some((px, py)) = pos {
        send_mouse_move(px, py)?;
        thread::sleep(Duration::from_millis(15));
    }

    if delta_y != 0 {
        let mi = MOUSEINPUT {
            dwFlags: MOUSEEVENTF_WHEEL,
            mouseData: (delta_y * 120) as u32,
            ..Default::default()
        };
        send_inputs(&[INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 { mi },
        }])?;
    }

    if delta_x != 0 {
        let mi = MOUSEINPUT {
            dwFlags: MOUSEEVENTF_HWHEEL,
            mouseData: (delta_x * 120) as u32,
            ..Default::default()
        };
        send_inputs(&[INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 { mi },
        }])?;
    }

    Ok(format!(
        "Scrolled delta_y={} delta_x={}{}",
        delta_y,
        delta_x,
        pos.map(|(px, py)| format!(" at ({},{})", px, py))
            .unwrap_or_default()
    ))
}

pub fn drag(
    state: &AppState,
    x1: i32,
    y1: i32,
    x2: i32,
    y2: i32,
    steps: u32,
    window: Option<&str>,
) -> Result<String> {
    init_dpi_awareness();
    let target_win = resolve_optional_window(state, window)?;
    state.safety.check_write_allowed(target_win.as_ref())?;

    if let Some(win) = &target_win {
        if !win.is_foreground {
            bring_hwnd_to_foreground(win.hwnd)?;
        }
    }

    send_mouse_move(x1, y1)?;
    thread::sleep(Duration::from_millis(25));

    // Press left button
    send_inputs(&[mouse_button_input(MOUSEEVENTF_LEFTDOWN)])?;

    let n = steps.clamp(2, 60);
    for i in 1..=n {
        state.safety.check_failsafe()?;
        let t = i as f64 / n as f64;
        let cx = (x1 as f64 + (x2 - x1) as f64 * t).round() as i32;
        let cy = (y1 as f64 + (y2 - y1) as f64 * t).round() as i32;
        send_mouse_move(cx, cy)?;
        thread::sleep(Duration::from_millis(8));
    }

    // Release left button
    send_inputs(&[mouse_button_input(MOUSEEVENTF_LEFTUP)])?;
    Ok(format!("Dragged from ({},{}) to ({},{})", x1, y1, x2, y2))
}

fn resolve_optional_window(
    state: &AppState,
    window_query: Option<&str>,
) -> Result<Option<WindowInfo>> {
    if let Some(q) = window_query.map(str::trim).filter(|s| !s.is_empty()) {
        match resolve_target(Some(q), state.locked_window, &state.safety)? {
            CaptureTarget::Window(w) => Ok(Some(w)),
            CaptureTarget::Screen(_) => Ok(None),
        }
    } else if let Some(hwnd) = state.locked_window {
        Ok(get_window_info_by_hwnd(hwnd, &state.safety))
    } else {
        Ok(get_foreground_window_info(&state.safety))
    }
}

fn normalize_virtual_coords(x: i32, y: i32) -> (i32, i32) {
    let vs = virtual_screen_rect();
    let denom_x = (vs.w - 1).max(1) as f64;
    let denom_y = (vs.h - 1).max(1) as f64;
    let nx = (((x - vs.x) as f64) * 65535.0 / denom_x).round() as i32;
    let ny = (((y - vs.y) as f64) * 65535.0 / denom_y).round() as i32;
    (nx.clamp(0, 65535), ny.clamp(0, 65535))
}

fn send_mouse_move(x: i32, y: i32) -> Result<()> {
    let (nx, ny) = normalize_virtual_coords(x, y);
    let mi = MOUSEINPUT {
        dx: nx,
        dy: ny,
        dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
        ..Default::default()
    };
    send_inputs(&[INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi },
    }])
}

fn mouse_button_input(
    flags: windows::Win32::UI::Input::KeyboardAndMouse::MOUSE_EVENT_FLAGS,
) -> INPUT {
    let mi = MOUSEINPUT {
        dwFlags: flags,
        ..Default::default()
    };
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi },
    }
}

fn send_mouse_click(x: i32, y: i32, button: MouseButton, clicks: u32) -> Result<()> {
    send_mouse_move(x, y)?;
    thread::sleep(Duration::from_millis(20));

    let (down, up) = match button {
        MouseButton::Left => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
        MouseButton::Right => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
        MouseButton::Middle => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP),
    };

    for i in 0..clicks {
        send_inputs(&[mouse_button_input(down), mouse_button_input(up)])?;
        if i + 1 < clicks {
            thread::sleep(Duration::from_millis(55));
        }
    }
    Ok(())
}

fn send_unicode_text(text: &str) -> Result<()> {
    let mut inputs: Vec<INPUT> = Vec::with_capacity(text.len() * 2);
    for ch in text.chars() {
        if ch == '\n' {
            inputs.push(vk_input(VK_RETURN, false));
            inputs.push(vk_input(VK_RETURN, true));
            continue;
        }
        if ch == '\r' {
            continue;
        }
        let mut buf = [0u16; 2];
        for &mut unit in ch.encode_utf16(&mut buf) {
            let down = KEYBDINPUT {
                wScan: unit,
                dwFlags: KEYEVENTF_UNICODE,
                ..Default::default()
            };
            let up = KEYBDINPUT {
                wScan: unit,
                dwFlags: KEYEVENTF_UNICODE | KEYEVENTF_KEYUP,
                ..Default::default()
            };

            inputs.push(INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 { ki: down },
            });
            inputs.push(INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 { ki: up },
            });
        }
    }

    // Send in chunks so large pastes do not hit input queue limits
    for chunk in inputs.chunks(64) {
        send_inputs(chunk)?;
        if inputs.len() > 64 {
            thread::sleep(Duration::from_millis(5));
        }
    }
    Ok(())
}

fn vk_input(vk: VIRTUAL_KEY, key_up: bool) -> INPUT {
    let mut flags = if key_up {
        KEYEVENTF_KEYUP
    } else {
        Default::default()
    };
    if matches!(
        vk,
        VK_INSERT
            | VK_DELETE
            | VK_HOME
            | VK_END
            | VK_PRIOR
            | VK_NEXT
            | VK_LEFT
            | VK_UP
            | VK_RIGHT
            | VK_DOWN
            | VK_LWIN
    ) {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    let ki = KEYBDINPUT {
        wVk: vk,
        dwFlags: flags,
        ..Default::default()
    };
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki },
    }
}

fn send_hotkey_vks(vks: &[VIRTUAL_KEY]) -> Result<()> {
    let mut down: Vec<INPUT> = vks.iter().map(|&vk| vk_input(vk, false)).collect();
    let mut up: Vec<INPUT> = vks.iter().rev().map(|&vk| vk_input(vk, true)).collect();
    send_inputs(&down)?;
    thread::sleep(Duration::from_millis(20));
    send_inputs(&up)?;
    down.clear();
    up.clear();
    Ok(())
}

fn send_inputs(inputs: &[INPUT]) -> Result<()> {
    if inputs.is_empty() {
        return Ok(());
    }
    let sent = unsafe { SendInput(inputs, size_of::<INPUT>() as i32) };
    if sent == 0 {
        return Err(anyhow!(
            "SendInput sent 0 events (target window may be elevated/UIPI-protected)."
        ));
    }
    Ok(())
}

pub fn parse_hotkey(expr: &str) -> Result<(Vec<String>, Vec<VIRTUAL_KEY>)> {
    let mut names = Vec::new();
    let mut vks = Vec::new();

    for raw in expr.split('+') {
        let k = raw.trim().to_ascii_lowercase();
        if k.is_empty() {
            continue;
        }
        let (norm, vk) = match k.as_str() {
            "ctrl" | "control" => ("ctrl", VK_CONTROL),
            "alt" | "menu" => ("alt", VK_MENU),
            "shift" => ("shift", VK_SHIFT),
            "win" | "meta" | "super" | "cmd" => ("win", VK_LWIN),
            "enter" | "return" => ("enter", VK_RETURN),
            "tab" => ("tab", VK_TAB),
            "esc" | "escape" => ("esc", VK_ESCAPE),
            "space" => ("space", VK_SPACE),
            "backspace" | "bs" => ("backspace", VK_BACK),
            "del" | "delete" => ("delete", VK_DELETE),
            "ins" | "insert" => ("insert", VK_INSERT),
            "home" => ("home", VK_HOME),
            "end" => ("end", VK_END),
            "pgup" | "pageup" => ("pageup", VK_PRIOR),
            "pgdn" | "pagedown" => ("pagedown", VK_NEXT),
            "up" => ("up", VK_UP),
            "down" => ("down", VK_DOWN),
            "left" => ("left", VK_LEFT),
            "right" => ("right", VK_RIGHT),
            "printscreen" | "prtsc" => ("printscreen", VK_SNAPSHOT),
            "apps" | "context" => ("apps", VK_APPS),
            "f1" => ("f1", VK_F1),
            "f2" => ("f2", VK_F2),
            "f3" => ("f3", VK_F3),
            "f4" => ("f4", VK_F4),
            "f5" => ("f5", VK_F5),
            "f6" => ("f6", VK_F6),
            "f7" => ("f7", VK_F7),
            "f8" => ("f8", VK_F8),
            "f9" => ("f9", VK_F9),
            "f10" => ("f10", VK_F10),
            "f11" => ("f11", VK_F11),
            "f12" => ("f12", VK_F12),
            s if s.len() == 1 => {
                let b = s.as_bytes()[0];
                if b.is_ascii_alphanumeric() {
                    let upper = b.to_ascii_uppercase();
                    names.push(s.to_string());
                    vks.push(VIRTUAL_KEY(upper as u16));
                    continue;
                } else {
                    bail!("Unsupported key token: \"{}\"", raw);
                }
            }
            _ => bail!("Unknown key token: \"{}\"", raw),
        };
        names.push(norm.to_string());
        vks.push(vk);
    }

    if vks.is_empty() {
        bail!("Empty hotkey expression.");
    }
    Ok((names, vks))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hotkey_parser_handles_combos() {
        let (names, vks) = parse_hotkey("Ctrl + Shift + T").unwrap();
        assert_eq!(names, vec!["ctrl", "shift", "t"]);
        assert_eq!(vks, vec![VK_CONTROL, VK_SHIFT, VIRTUAL_KEY(b'T' as u16)]);

        let (names2, _) = parse_hotkey("alt+f4").unwrap();
        assert_eq!(names2, vec!["alt", "f4"]);

        assert!(parse_hotkey("unknownkey").is_err());
    }
}
