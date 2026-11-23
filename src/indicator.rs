//! Visible "recording" badge: a small always-on-top window in the bottom-right corner that is
//! shown while EYRA captures audio (listen) or runs a watch session, so recording never happens
//! silently. The badge is click-through, never takes focus, and is excluded from screen capture
//! so it doesn't show up in EYRA's own screenshots or keyframes.

use std::ffi::c_void;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint, FillRect, SetBkMode,
    SetTextColor, DT_CENTER, DT_SINGLELINE, DT_VCENTER, PAINTSTRUCT, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect, GetMessageW,
    GetWindowLongPtrW, PostMessageW, PostQuitMessage, RegisterClassW, SetLayeredWindowAttributes,
    SetWindowDisplayAffinity, SetWindowLongPtrW, ShowWindow, SystemParametersInfoW,
    TranslateMessage, GWLP_USERDATA, LWA_ALPHA, MSG, SPI_GETWORKAREA, SW_SHOWNOACTIVATE,
    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, WDA_EXCLUDEFROMCAPTURE, WINDOW_EX_STYLE, WM_CLOSE,
    WM_DESTROY, WM_PAINT, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

const WIDTH: i32 = 340;
const HEIGHT: i32 = 34;

/// Keeps the badge on screen until dropped.
pub struct RecordingIndicator {
    hwnd: usize,
    thread: Option<JoinHandle<()>>,
}

impl RecordingIndicator {
    /// Shows the badge with `label` (e.g. "system audio + microphone"). Returns `None` if the
    /// window can't be created; recording still proceeds, the caller decides.
    pub fn show(label: &str) -> Option<Self> {
        let text: Vec<u16> = format!("\u{25CF}  EYRA is recording: {}", label)
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let (tx, rx) = mpsc::channel::<usize>();
        let thread = thread::spawn(move || unsafe { run_window(text, tx) });
        match rx.recv() {
            Ok(h) if h != 0 => Some(Self {
                hwnd: h,
                thread: Some(thread),
            }),
            _ => {
                let _ = thread.join();
                None
            }
        }
    }
}

impl Drop for RecordingIndicator {
    fn drop(&mut self) {
        unsafe {
            let _ = PostMessageW(
                HWND(self.hwnd as *mut c_void),
                WM_CLOSE,
                WPARAM(0),
                LPARAM(0),
            );
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

unsafe fn run_window(text: Vec<u16>, tx: mpsc::Sender<usize>) {
    let class = w!("EyraRecordingIndicator");
    let instance = GetModuleHandleW(None).map(|m| m.into()).unwrap_or_default();
    let wc = WNDCLASSW {
        lpfnWndProc: Some(wnd_proc),
        hInstance: instance,
        lpszClassName: class,
        ..Default::default()
    };
    // Registering twice (second badge in the same process) fails harmlessly
    RegisterClassW(&wc);

    let mut work = RECT::default();
    let _ = SystemParametersInfoW(
        SPI_GETWORKAREA,
        0,
        Some(&mut work as *mut RECT as *mut c_void),
        SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
    );
    let x = work.right - WIDTH - 16;
    let y = work.bottom - HEIGHT - 16;

    let hwnd = CreateWindowExW(
        WINDOW_EX_STYLE(
            WS_EX_TOPMOST.0
                | WS_EX_TOOLWINDOW.0
                | WS_EX_NOACTIVATE.0
                | WS_EX_LAYERED.0
                | WS_EX_TRANSPARENT.0,
        ),
        class,
        PCWSTR(HSTRING::from("EYRA recording").as_ptr()),
        WS_POPUP,
        x,
        y,
        WIDTH,
        HEIGHT,
        None,
        None,
        instance,
        None,
    );
    let Ok(hwnd) = hwnd else {
        let _ = tx.send(0);
        return;
    };

    // Text buffer lives for the window's lifetime; freed in WM_DESTROY
    let boxed = Box::into_raw(Box::new(text));
    SetWindowLongPtrW(hwnd, GWLP_USERDATA, boxed as isize);
    let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 235, LWA_ALPHA);
    let _ = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    let _ = tx.send(hwnd.0 as usize);

    let mut msg = MSG::default();
    while GetMessageW(&mut msg, None, 0, 0).as_bool() {
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            let brush = CreateSolidBrush(COLORREF(0x0020_20C8)); // BGR: red
            FillRect(hdc, &rc, brush);
            let _ = DeleteObject(brush);
            SetBkMode(hdc, TRANSPARENT);
            SetTextColor(hdc, COLORREF(0x00FF_FFFF));
            let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Vec<u16>;
            if !ptr.is_null() {
                let text = &mut *ptr;
                let len = text.len().saturating_sub(1);
                DrawTextW(
                    hdc,
                    &mut text[..len],
                    &mut rc,
                    DT_CENTER | DT_VCENTER | DT_SINGLELINE,
                );
            }
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Vec<u16>;
            if !ptr.is_null() {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                drop(Box::from_raw(ptr));
            }
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}
