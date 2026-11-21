use crate::safety::SafetyGuard;
use crate::types::{Rect, WindowInfo};
use anyhow::{bail, Result};
use std::ffi::c_void;
use std::mem::size_of;
use std::path::Path;
use std::sync::Once;
use windows::core::PWSTR;
use windows::Win32::Foundation::{CloseHandle, BOOL, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS,
};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetForegroundWindow, GetSystemMetrics, GetWindowRect, GetWindowTextLengthW,
    GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible, SM_CXSCREEN,
    SM_CXVIRTUALSCREEN, SM_CYSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

static DPI_INIT: Once = Once::new();

pub fn init_dpi_awareness() {
    DPI_INIT.call_once(|| unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    });
}

pub fn primary_screen_rect() -> Rect {
    unsafe {
        let w = GetSystemMetrics(SM_CXSCREEN);
        let h = GetSystemMetrics(SM_CYSCREEN);
        Rect::new(0, 0, w.max(1), h.max(1))
    }
}

pub fn virtual_screen_rect() -> Rect {
    unsafe {
        let x = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let y = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let w = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let h = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        if w > 0 && h > 0 {
            Rect::new(x, y, w, h)
        } else {
            primary_screen_rect()
        }
    }
}

pub fn is_valid_hwnd(hwnd: usize) -> bool {
    if hwnd == 0 {
        return false;
    }
    unsafe { IsWindow(HWND(hwnd as *mut c_void)).as_bool() }
}

pub fn get_window_info(hwnd: HWND, fg: HWND, safety: &SafetyGuard) -> Option<WindowInfo> {
    unsafe {
        if hwnd.0.is_null() || !IsWindowVisible(hwnd).as_bool() {
            return None;
        }

        // Skip cloaked UWP/background shell windows
        let mut cloaked: u32 = 0;
        if DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut u32 as *mut c_void,
            size_of::<u32>() as u32,
        )
        .is_ok()
            && cloaked != 0
        {
            return None;
        }

        let title = get_window_title(hwnd);
        if title.is_empty() {
            return None;
        }

        let is_minimized = IsIconic(hwnd).as_bool();
        let rect = get_window_rect(hwnd);
        if !is_minimized && (rect.w < 40 || rect.h < 20) {
            return None;
        }

        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        let process = get_process_name(pid).unwrap_or_else(|| "unknown".to_string());
        let is_sensitive = safety.is_sensitive_window(&title, &process);

        Some(WindowInfo {
            hwnd: hwnd.0 as usize,
            title,
            process,
            pid,
            rect,
            is_foreground: hwnd == fg,
            is_minimized,
            is_sensitive,
        })
    }
}

pub fn get_window_info_by_hwnd(hwnd_val: usize, safety: &SafetyGuard) -> Option<WindowInfo> {
    if !is_valid_hwnd(hwnd_val) {
        return None;
    }
    let hwnd = HWND(hwnd_val as *mut c_void);
    let fg = unsafe { GetForegroundWindow() };
    get_window_info(hwnd, fg, safety)
}

pub fn get_foreground_window_info(safety: &SafetyGuard) -> Option<WindowInfo> {
    let fg = unsafe { GetForegroundWindow() };
    get_window_info(fg, fg, safety)
}

struct EnumContext<'a> {
    fg: HWND,
    safety: &'a SafetyGuard,
    windows: Vec<WindowInfo>,
}

unsafe extern "system" fn enum_windows_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let ctx = &mut *(lparam.0 as *mut EnumContext<'_>);
    if let Some(info) = get_window_info(hwnd, ctx.fg, ctx.safety) {
        ctx.windows.push(info);
    }
    BOOL(1)
}

pub fn list_windows(safety: &SafetyGuard) -> Result<Vec<WindowInfo>> {
    init_dpi_awareness();
    let fg = unsafe { GetForegroundWindow() };
    let mut ctx = EnumContext {
        fg,
        safety,
        windows: Vec::new(),
    };
    unsafe {
        let _ = EnumWindows(
            Some(enum_windows_proc),
            LPARAM(&mut ctx as *mut EnumContext<'_> as isize),
        );
    }
    Ok(ctx.windows)
}

struct PopupContext {
    pid: u32,
    popups: Vec<usize>,
}

unsafe extern "system" fn enum_popups_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let ctx = &mut *(lparam.0 as *mut PopupContext);
    if !IsWindowVisible(hwnd).as_bool() {
        return BOOL(1);
    }
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid != ctx.pid {
        return BOOL(1);
    }
    let mut buf = [0u16; 64];
    let len = windows::Win32::UI::WindowsAndMessaging::GetClassNameW(hwnd, &mut buf);
    if len > 0 && String::from_utf16_lossy(&buf[..len as usize]) == "#32768" {
        ctx.popups.push(hwnd.0 as usize);
    }
    BOOL(1)
}

/// Visible Win32 popup menus (`#32768`) owned by the same process as `hwnd`.
/// They are separate top-level windows, so a window-scoped scan would otherwise miss the
/// menu an agent just opened.
pub fn open_popup_menus(hwnd: usize) -> Vec<usize> {
    let mut pid = 0u32;
    unsafe {
        GetWindowThreadProcessId(HWND(hwnd as *mut c_void), Some(&mut pid));
    }
    if pid == 0 {
        return Vec::new();
    }
    let mut ctx = PopupContext {
        pid,
        popups: Vec::new(),
    };
    unsafe {
        let _ = EnumWindows(
            Some(enum_popups_proc),
            LPARAM(&mut ctx as *mut PopupContext as isize),
        );
    }
    ctx.popups
}

/// Target resolution result: either a specific window or full screen.
#[derive(Debug, Clone)]
pub enum CaptureTarget {
    Window(WindowInfo),
    Screen(Rect),
}

pub fn resolve_target(
    query: Option<&str>,
    locked_hwnd: Option<usize>,
    safety: &SafetyGuard,
) -> Result<CaptureTarget> {
    init_dpi_awareness();

    if let Some(q) = query.map(str::trim).filter(|s| !s.is_empty()) {
        let low = q.to_ascii_lowercase();
        if low == "screen" || low == "desktop" || low == "primary" {
            return Ok(CaptureTarget::Screen(primary_screen_rect()));
        }
        if low == "all" || low == "virtual" {
            return Ok(CaptureTarget::Screen(virtual_screen_rect()));
        }
        if low == "active" || low == "foreground" || low == "fg" {
            if let Some(fg) = get_foreground_window_info(safety) {
                return Ok(CaptureTarget::Window(fg));
            }
            return Ok(CaptureTarget::Screen(primary_screen_rect()));
        }

        // Try parsing as hex or decimal HWND
        if let Some(hwnd) = parse_hwnd(q) {
            if let Some(info) = get_window_info_by_hwnd(hwnd, safety) {
                return Ok(CaptureTarget::Window(info));
            }
            bail!("Window with hwnd {} not found or not visible.", q);
        }

        // Match against open windows (exact title > exact process > substring title > substring process)
        let wins = list_windows(safety)?;
        if let Some(w) = wins.iter().find(|w| w.title.eq_ignore_ascii_case(q)) {
            return Ok(CaptureTarget::Window(w.clone()));
        }
        if let Some(w) = wins.iter().find(|w| {
            w.process.eq_ignore_ascii_case(q)
                || w.process.trim_end_matches(".exe").eq_ignore_ascii_case(q)
        }) {
            return Ok(CaptureTarget::Window(w.clone()));
        }
        if let Some(w) = wins
            .iter()
            .find(|w| w.title.to_ascii_lowercase().contains(&low))
        {
            return Ok(CaptureTarget::Window(w.clone()));
        }
        if let Some(w) = wins
            .iter()
            .find(|w| w.process.to_ascii_lowercase().contains(&low))
        {
            return Ok(CaptureTarget::Window(w.clone()));
        }

        bail!(
            "No visible window matching \"{}\". Use list_windows to inspect available windows.",
            q
        );
    }

    if let Some(hwnd) = locked_hwnd {
        if let Some(info) = get_window_info_by_hwnd(hwnd, safety) {
            return Ok(CaptureTarget::Window(info));
        }
    }

    if let Some(fg) = get_foreground_window_info(safety) {
        return Ok(CaptureTarget::Window(fg));
    }

    Ok(CaptureTarget::Screen(primary_screen_rect()))
}

pub fn parse_hwnd(s: &str) -> Option<usize> {
    let trimmed = s.trim();
    if let Some(hex) = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
    {
        usize::from_str_radix(hex, 16).ok()
    } else if trimmed.chars().all(|c| c.is_ascii_digit()) && trimmed.len() >= 3 {
        trimmed.parse::<usize>().ok()
    } else {
        None
    }
}

pub fn get_window_rect(hwnd: HWND) -> Rect {
    unsafe {
        let mut dwm_rect = RECT::default();
        if DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut dwm_rect as *mut RECT as *mut c_void,
            size_of::<RECT>() as u32,
        )
        .is_ok()
            && dwm_rect.right > dwm_rect.left
            && dwm_rect.bottom > dwm_rect.top
        {
            return Rect::from_ltrb(dwm_rect.left, dwm_rect.top, dwm_rect.right, dwm_rect.bottom);
        }

        let mut wr = RECT::default();
        if GetWindowRect(hwnd, &mut wr).is_ok() {
            Rect::from_ltrb(wr.left, wr.top, wr.right, wr.bottom)
        } else {
            Rect::default()
        }
    }
}

fn get_window_title(hwnd: HWND) -> String {
    unsafe {
        let len = GetWindowTextLengthW(hwnd);
        if len <= 0 {
            return String::new();
        }
        let mut buf = vec![0u16; (len as usize) + 2];
        let copied = GetWindowTextW(hwnd, &mut buf);
        if copied <= 0 {
            return String::new();
        }
        String::from_utf16_lossy(&buf[..copied as usize])
            .trim()
            .to_string()
    }
}

fn get_process_name(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = vec![0u16; 512];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
        .is_ok();
        let _ = CloseHandle(handle);
        if !ok || len == 0 {
            return None;
        }
        let full = String::from_utf16_lossy(&buf[..len as usize]);
        let name = Path::new(&full)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(&full)
            .to_string();
        Some(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hwnd_hex_and_decimal() {
        assert_eq!(parse_hwnd("0x1A0B2C"), Some(0x1A0B2C));
        assert_eq!(parse_hwnd("0X10"), Some(16));
        assert_eq!(parse_hwnd("123456"), Some(123456));
        assert_eq!(parse_hwnd("Notepad"), None);
    }
}
