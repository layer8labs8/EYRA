use crate::types::{ElementSource, Rect, UiElement};
use anyhow::{anyhow, Result};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::ffi::c_void;
use std::rc::Rc;
use windows::core::{BSTR, VARIANT};
use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED,
};
use windows::Win32::System::Ole::{
    SafeArrayAccessData, SafeArrayDestroy, SafeArrayGetLBound, SafeArrayGetUBound,
    SafeArrayUnaccessData,
};
use windows::Win32::UI::Accessibility::TreeScope_Subtree;
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationCacheRequest, IUIAutomationElement,
    IUIAutomationInvokePattern, IUIAutomationSelectionItemPattern, IUIAutomationTogglePattern,
    IUIAutomationTreeWalker, IUIAutomationValuePattern, ToggleState_Indeterminate, ToggleState_On,
    UIA_BoundingRectanglePropertyId, UIA_ButtonControlTypeId, UIA_CheckBoxControlTypeId,
    UIA_ComboBoxControlTypeId, UIA_ControlTypePropertyId, UIA_DataItemControlTypeId,
    UIA_DocumentControlTypeId, UIA_EditControlTypeId, UIA_GroupControlTypeId,
    UIA_HasKeyboardFocusPropertyId, UIA_HeaderItemControlTypeId, UIA_HyperlinkControlTypeId,
    UIA_ImageControlTypeId, UIA_InvokePatternId, UIA_IsEnabledPropertyId,
    UIA_IsKeyboardFocusablePropertyId, UIA_IsOffscreenPropertyId, UIA_IsPasswordPropertyId,
    UIA_IsSelectionItemPatternAvailablePropertyId, UIA_IsTogglePatternAvailablePropertyId,
    UIA_IsValuePatternAvailablePropertyId, UIA_ListItemControlTypeId, UIA_MenuBarControlTypeId,
    UIA_MenuControlTypeId, UIA_MenuItemControlTypeId, UIA_NamePropertyId, UIA_PaneControlTypeId,
    UIA_ProgressBarControlTypeId, UIA_RadioButtonControlTypeId, UIA_RuntimeIdPropertyId,
    UIA_ScrollBarControlTypeId, UIA_SelectionItemIsSelectedPropertyId, UIA_SelectionItemPatternId,
    UIA_SliderControlTypeId, UIA_SpinnerControlTypeId, UIA_SplitButtonControlTypeId,
    UIA_StatusBarControlTypeId, UIA_TabControlTypeId, UIA_TabItemControlTypeId,
    UIA_TableControlTypeId, UIA_TextControlTypeId, UIA_TitleBarControlTypeId, UIA_TogglePatternId,
    UIA_ToggleToggleStatePropertyId, UIA_ToolBarControlTypeId, UIA_TreeItemControlTypeId,
    UIA_ValuePatternId, UIA_ValueValuePropertyId, UIA_WindowControlTypeId, UIA_CONTROLTYPE_ID,
};
use windows::Win32::UI::WindowsAndMessaging::GetClassNameW;

pub struct UiaScanResult {
    pub elements: Vec<UiElement>,
    pub redacted_rects: Vec<Rect>,
}

fn ensure_com_init() {
    unsafe {
        // S_OK, S_FALSE, or RPC_E_CHANGED_MODE are all acceptable for COM usage on this thread
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
}

/// Per-thread UIA context. Creating `CUIAutomation` costs several milliseconds and the
/// MCP/bridge servers call `see` hundreds of times per session, so it is built once and reused.
struct UiaCtx {
    uia: IUIAutomation,
    walker: IUIAutomationTreeWalker,
    /// Every property the scanner reads, fetched in ONE cross-process round trip per node
    /// (instead of ~12 separate `Current*` calls per node).
    scan_cache: IUIAutomationCacheRequest,
    /// Same properties, but TreeScope_Subtree: the whole window tree in ONE round trip.
    subtree_cache: IUIAutomationCacheRequest,
}

thread_local! {
    static UIA_CTX: RefCell<Option<Rc<UiaCtx>>> = const { RefCell::new(None) };
}

fn build_ctx() -> Result<UiaCtx> {
    ensure_com_init();
    unsafe {
        let uia: IUIAutomation = CoCreateInstance(&CUIAutomation, None, CLSCTX_ALL)
            .map_err(|e| anyhow!("Failed to create CUIAutomation: {}", e))?;
        let walker = uia
            .ControlViewWalker()
            .map_err(|e| anyhow!("ControlViewWalker failed: {}", e))?;
        let cache = uia
            .CreateCacheRequest()
            .map_err(|e| anyhow!("CreateCacheRequest failed: {}", e))?;
        for prop in [
            UIA_BoundingRectanglePropertyId,
            UIA_ControlTypePropertyId,
            UIA_IsOffscreenPropertyId,
            UIA_IsPasswordPropertyId,
            UIA_IsEnabledPropertyId,
            UIA_IsKeyboardFocusablePropertyId,
            UIA_HasKeyboardFocusPropertyId,
            UIA_NamePropertyId,
            UIA_RuntimeIdPropertyId,
            UIA_ValueValuePropertyId,
            UIA_ToggleToggleStatePropertyId,
            UIA_SelectionItemIsSelectedPropertyId,
            UIA_IsValuePatternAvailablePropertyId,
            UIA_IsTogglePatternAvailablePropertyId,
            UIA_IsSelectionItemPatternAvailablePropertyId,
        ] {
            cache
                .AddProperty(prop)
                .map_err(|e| anyhow!("CacheRequest.AddProperty failed: {}", e))?;
        }
        let subtree_cache = cache
            .Clone()
            .map_err(|e| anyhow!("CacheRequest.Clone failed: {}", e))?;
        subtree_cache
            .SetTreeScope(TreeScope_Subtree)
            .map_err(|e| anyhow!("CacheRequest.SetTreeScope failed: {}", e))?;
        // Skip off-screen nodes inside the provider so long web pages / huge lists stay cheap
        // (their on-screen descendants are still visited: a tree filter skips nodes, not subtrees).
        let on_screen = uia
            .CreatePropertyCondition(UIA_IsOffscreenPropertyId, &VARIANT::from(false))
            .map_err(|e| anyhow!("CreatePropertyCondition failed: {}", e))?;
        let control_view = uia
            .ControlViewCondition()
            .map_err(|e| anyhow!("ControlViewCondition failed: {}", e))?;
        let filter = uia
            .CreateAndCondition(&control_view, &on_screen)
            .map_err(|e| anyhow!("CreateAndCondition failed: {}", e))?;
        subtree_cache
            .SetTreeFilter(&filter)
            .map_err(|e| anyhow!("CacheRequest.SetTreeFilter failed: {}", e))?;
        Ok(UiaCtx {
            uia,
            walker,
            scan_cache: cache,
            subtree_cache,
        })
    }
}

fn uia_ctx() -> Result<Rc<UiaCtx>> {
    UIA_CTX.with(|slot| {
        if let Some(ctx) = slot.borrow().as_ref() {
            return Ok(ctx.clone());
        }
        let ctx = Rc::new(build_ctx()?);
        *slot.borrow_mut() = Some(ctx.clone());
        Ok(ctx)
    })
}

/// True for Chromium-based windows (Chrome, Edge, Brave, Electron apps such as VS Code,
/// Slack, Discord, Claude). Chromium builds its accessibility tree lazily after the first
/// UIA client query, so the very first scan of such a window is often nearly empty.
pub fn is_chromium_window(hwnd: usize) -> bool {
    let mut buf = [0u16; 64];
    let len = unsafe { GetClassNameW(HWND(hwnd as *mut c_void), &mut buf) };
    if len <= 0 {
        return false;
    }
    String::from_utf16_lossy(&buf[..len as usize]).starts_with("Chrome_WidgetWin")
}

/// Asks Chromium to enable accessibility the way screen readers do: an MSAA client request
/// (OBJID_CLIENT) on the browser frame and on every render-widget child window.
pub fn nudge_chromium_accessibility(hwnd: usize) {
    use windows::core::Interface;
    use windows::Win32::Foundation::{BOOL, LPARAM};
    use windows::Win32::UI::Accessibility::{AccessibleObjectFromWindow, IAccessible};
    use windows::Win32::UI::WindowsAndMessaging::EnumChildWindows;
    const OBJID_CLIENT: u32 = 0xFFFF_FFFC; // -4

    unsafe fn poke(h: HWND) {
        let mut obj: *mut c_void = std::ptr::null_mut();
        if AccessibleObjectFromWindow(h, OBJID_CLIENT, &IAccessible::IID, &mut obj).is_ok()
            && !obj.is_null()
        {
            drop(IAccessible::from_raw(obj));
        }
    }
    unsafe extern "system" fn child_proc(h: HWND, _: LPARAM) -> BOOL {
        let mut buf = [0u16; 64];
        let len = GetClassNameW(h, &mut buf);
        if len > 0
            && String::from_utf16_lossy(&buf[..len as usize]) == "Chrome_RenderWidgetHostHWND"
        {
            poke(h);
        }
        BOOL(1)
    }

    ensure_com_init();
    unsafe {
        let top = HWND(hwnd as *mut c_void);
        poke(top);
        let _ = EnumChildWindows(top, Some(child_proc), LPARAM(0));
    }
}

pub fn scan_uia(
    hwnd: Option<usize>,
    clip_rect: Rect,
    max_elements: usize,
    max_depth: usize,
) -> Result<UiaScanResult> {
    let ctx = uia_ctx()?;
    // Window targets: fetch the whole on-screen control tree in a single cross-process call.
    // Desktop-wide scans keep the budgeted walker (the full desktop tree can be enormous).
    // EYRA_UIA_WALKER=1 forces the walker for troubleshooting.
    if let Some(h) = hwnd {
        if std::env::var_os("EYRA_UIA_WALKER").is_none() {
            if let Ok(res) = scan_uia_subtree(&ctx, h, clip_rect, max_elements, max_depth) {
                return Ok(res);
            }
        }
    }
    let root = unsafe {
        if let Some(h) = hwnd {
            ctx.uia
                .ElementFromHandle(HWND(h as *mut c_void))
                .map_err(|e| anyhow!("ElementFromHandle failed: {}", e))?
        } else {
            ctx.uia
                .GetRootElement()
                .map_err(|e| anyhow!("GetRootElement failed: {}", e))?
        }
    };

    let walker = &ctx.walker;
    let cache = &ctx.scan_cache;

    let mut elements = Vec::new();
    let mut redacted_rects = Vec::new();
    let mut queue: VecDeque<(IUIAutomationElement, usize)> = VecDeque::new();
    queue.push_back((root, 0));

    let visit_budget = (max_elements * 20).clamp(800, 4000);
    let mut visited = 0usize;

    while let Some((el, depth)) = queue.pop_front() {
        if elements.len() >= max_elements || visited >= visit_budget {
            break;
        }
        visited += 1;

        if depth > 0 {
            if let Some((ui_el, is_pwd_rect)) = extract_element(&el, clip_rect) {
                if let Some(r) = is_pwd_rect {
                    redacted_rects.push(r);
                }
                elements.push(ui_el);
            }
        }

        if depth >= max_depth {
            continue;
        }

        // Enqueue children; each hop prefetches the child's properties in the same round trip.
        unsafe {
            if let Ok(mut child) = walker.GetFirstChildElementBuildCache(&el, cache) {
                loop {
                    queue.push_back((child.clone(), depth + 1));
                    match walker.GetNextSiblingElementBuildCache(&child, cache) {
                        Ok(next) => child = next,
                        Err(_) => break,
                    }
                }
            }
        }
    }

    Ok(UiaScanResult {
        elements,
        redacted_rects,
    })
}

fn scan_uia_subtree(
    ctx: &UiaCtx,
    hwnd: usize,
    clip_rect: Rect,
    max_elements: usize,
    max_depth: usize,
) -> Result<UiaScanResult> {
    let root = unsafe {
        ctx.uia
            .ElementFromHandleBuildCache(HWND(hwnd as *mut c_void), &ctx.subtree_cache)
            .map_err(|e| anyhow!("ElementFromHandleBuildCache failed: {}", e))?
    };
    let mut elements = Vec::new();
    let mut redacted_rects = Vec::new();
    let mut queue: VecDeque<(IUIAutomationElement, usize)> = VecDeque::new();
    queue.push_back((root, 0));
    while let Some((el, depth)) = queue.pop_front() {
        if elements.len() >= max_elements {
            break;
        }
        if depth > 0 {
            if let Some((ui_el, pwd)) = extract_element(&el, clip_rect) {
                if let Some(r) = pwd {
                    redacted_rects.push(r);
                }
                elements.push(ui_el);
            }
        }
        if depth >= max_depth {
            continue;
        }
        unsafe {
            if let Ok(children) = el.GetCachedChildren() {
                let n = children.Length().unwrap_or(0);
                for i in 0..n {
                    if let Ok(c) = children.GetElement(i) {
                        queue.push_back((c, depth + 1));
                    }
                }
            }
        }
    }
    Ok(UiaScanResult {
        elements,
        redacted_rects,
    })
}

fn cached_bool(
    el: &IUIAutomationElement,
    prop: windows::Win32::UI::Accessibility::UIA_PROPERTY_ID,
) -> Option<bool> {
    unsafe {
        let v: VARIANT = el.GetCachedPropertyValue(prop).ok()?;
        bool::try_from(&v).ok()
    }
}

#[allow(non_upper_case_globals)]
fn extract_element(
    el: &IUIAutomationElement,
    clip_rect: Rect,
) -> Option<(UiElement, Option<Rect>)> {
    unsafe {
        if el
            .CachedIsOffscreen()
            .ok()
            .map(|b| b.as_bool())
            .unwrap_or(false)
        {
            return None;
        }

        let br = el.CachedBoundingRectangle().ok()?;
        let rect = Rect::from_ltrb(br.left, br.top, br.right, br.bottom);
        if !rect.is_valid() || rect.w < 3 || rect.h < 3 {
            return None;
        }
        if clip_rect.is_valid() && !rect.intersects(&clip_rect) {
            return None;
        }

        let ctype = el.CachedControlType().ok()?;
        let is_password = el
            .CachedIsPassword()
            .ok()
            .map(|b| b.as_bool())
            .unwrap_or(false);
        let is_enabled = el
            .CachedIsEnabled()
            .ok()
            .map(|b| b.as_bool())
            .unwrap_or(true);
        let focusable = el
            .CachedIsKeyboardFocusable()
            .ok()
            .map(|b| b.as_bool())
            .unwrap_or(false);
        let focused = el
            .CachedHasKeyboardFocus()
            .ok()
            .map(|b| b.as_bool())
            .unwrap_or(false);

        let name = el
            .CachedName()
            .ok()
            .map(|b| b.to_string().trim().to_string())
            .unwrap_or_default();

        // Pattern properties return a NotSupported sentinel when the pattern is absent, and
        // VARIANT coercion would turn that into garbage, so gate every read on availability.
        let mut value: Option<String> = None;
        let has_value = cached_bool(el, UIA_IsValuePatternAvailablePropertyId).unwrap_or(false);
        let has_toggle = cached_bool(el, UIA_IsTogglePatternAvailablePropertyId).unwrap_or(false);
        let has_selection =
            cached_bool(el, UIA_IsSelectionItemPatternAvailablePropertyId).unwrap_or(false);
        if let Some(v) = has_value
            .then(|| el.GetCachedPropertyValue(UIA_ValueValuePropertyId).ok())
            .flatten()
        {
            if let Ok(s) = BSTR::try_from(&v) {
                let s = s.to_string();
                if !s.is_empty() {
                    value = Some(if is_password {
                        "[REDACTED]".to_string()
                    } else {
                        s
                    });
                }
            }
        }

        let mut states: Vec<&'static str> = Vec::new();
        if !is_enabled {
            states.push("disabled");
        }
        if focused {
            states.push("focused");
        }
        if let Some(v) = has_toggle
            .then(|| {
                el.GetCachedPropertyValue(UIA_ToggleToggleStatePropertyId)
                    .ok()
            })
            .flatten()
        {
            if let Ok(ts) = i32::try_from(&v) {
                if ts == ToggleState_On.0 {
                    states.push("checked");
                } else if ts == ToggleState_Indeterminate.0 {
                    states.push("mixed");
                } else {
                    states.push("unchecked");
                }
            }
        }
        if has_selection && cached_bool(el, UIA_SelectionItemIsSelectedPropertyId).unwrap_or(false)
        {
            states.push("selected");
        }

        let (role_str, interactive_type) = map_control_type(ctype, is_password);
        let is_interactive = interactive_type || focusable || value.is_some();

        // Aggressive clutter filter for ultra-low token footprint:
        // Skip empty non-interactive containers, scrollbars, separators, and unnamed decorative nodes
        let is_container = matches!(
            ctype,
            UIA_PaneControlTypeId
                | UIA_GroupControlTypeId
                | UIA_WindowControlTypeId
                | UIA_TitleBarControlTypeId
                | UIA_ToolBarControlTypeId
                | UIA_MenuBarControlTypeId
                | UIA_StatusBarControlTypeId
                | UIA_TabControlTypeId
                | UIA_TableControlTypeId
                | UIA_ScrollBarControlTypeId
                | UIA_MenuControlTypeId
        );

        if is_container && !is_password {
            return None;
        }

        if ctype == UIA_DocumentControlTypeId && name.is_empty() && value.is_none() {
            return None;
        }

        if !is_interactive && name.is_empty() && value.is_none() {
            return None;
        }

        if ctype == UIA_ImageControlTypeId && name.is_empty() && !focusable {
            return None;
        }

        let runtime_id = cached_runtime_id(el).unwrap_or_else(|| read_runtime_id(el));

        let state = if states.is_empty() {
            None
        } else {
            Some(states.join(","))
        };

        let redacted_rect = if is_password { Some(rect) } else { None };

        let ui_el = UiElement {
            id: 0, // assigned by caller after merging/filtering
            role: role_str.to_string(),
            name,
            value,
            state,
            rect,
            source: ElementSource::Uia,
            is_password,
            is_interactive,
            runtime_id,
        };

        Some((ui_el, redacted_rect))
    }
}

#[allow(non_upper_case_globals)]
fn map_control_type(ctype: UIA_CONTROLTYPE_ID, is_password: bool) -> (&'static str, bool) {
    if is_password {
        return ("pwd", true);
    }
    match ctype {
        UIA_ButtonControlTypeId | UIA_SplitButtonControlTypeId => ("btn", true),
        UIA_EditControlTypeId => ("inp", true),
        UIA_HyperlinkControlTypeId => ("lnk", true),
        UIA_CheckBoxControlTypeId => ("chk", true),
        UIA_RadioButtonControlTypeId => ("rad", true),
        UIA_ComboBoxControlTypeId => ("cmb", true),
        UIA_TabItemControlTypeId => ("tab", true),
        UIA_MenuItemControlTypeId => ("mnu", true),
        UIA_ListItemControlTypeId | UIA_TreeItemControlTypeId | UIA_DataItemControlTypeId => {
            ("itm", true)
        }
        UIA_SliderControlTypeId | UIA_SpinnerControlTypeId => ("sld", true),
        UIA_HeaderItemControlTypeId => ("hdr", true),
        UIA_TextControlTypeId => ("txt", false),
        UIA_ImageControlTypeId => ("img", false),
        UIA_DocumentControlTypeId => ("doc", true),
        UIA_ProgressBarControlTypeId => ("prg", false),
        _ => ("el", false),
    }
}

/// Copies the i32 elements of a one-dimensional SAFEARRAY (does not free it).
unsafe fn safearray_i32(sa: *const windows::Win32::System::Com::SAFEARRAY) -> Vec<i32> {
    let mut result = Vec::new();
    if sa.is_null() {
        return result;
    }
    if let (Ok(lb), Ok(ub)) = (SafeArrayGetLBound(sa, 1), SafeArrayGetUBound(sa, 1)) {
        if ub >= lb {
            let count = (ub - lb + 1) as usize;
            let mut ptr: *mut c_void = std::ptr::null_mut();
            if SafeArrayAccessData(sa, &mut ptr).is_ok() && !ptr.is_null() {
                let slice = std::slice::from_raw_parts(ptr as *const i32, count);
                result.extend_from_slice(slice);
                let _ = SafeArrayUnaccessData(sa);
            }
        }
    }
    result
}

/// RuntimeId from the scan's cache request (VT_ARRAY | VT_I4); no extra round trip.
unsafe fn cached_runtime_id(el: &IUIAutomationElement) -> Option<Vec<i32>> {
    const VT_ARRAY_I4: u16 = 0x2000 | 3;
    let v: VARIANT = el.GetCachedPropertyValue(UIA_RuntimeIdPropertyId).ok()?;
    let raw = v.as_raw();
    if raw.Anonymous.Anonymous.vt != VT_ARRAY_I4 {
        return None;
    }
    let sa =
        raw.Anonymous.Anonymous.Anonymous.parray as *const windows::Win32::System::Com::SAFEARRAY;
    let ids = safearray_i32(sa); // owned by `v`, freed when it drops
    (!ids.is_empty()).then_some(ids)
}

unsafe fn read_runtime_id(el: &IUIAutomationElement) -> Vec<i32> {
    let Ok(sa) = el.GetRuntimeId() else {
        return Vec::new();
    };
    if sa.is_null() {
        return Vec::new();
    }

    let mut result = Vec::new();
    if let (Ok(lb), Ok(ub)) = (SafeArrayGetLBound(sa, 1), SafeArrayGetUBound(sa, 1)) {
        if ub >= lb {
            let count = (ub - lb + 1) as usize;
            let mut ptr: *mut c_void = std::ptr::null_mut();
            if SafeArrayAccessData(sa, &mut ptr).is_ok() && !ptr.is_null() {
                let slice = std::slice::from_raw_parts(ptr as *const i32, count);
                result.extend_from_slice(slice);
                let _ = SafeArrayUnaccessData(sa);
            }
        }
    }
    let _ = SafeArrayDestroy(sa);
    result
}

/// Accessible name of the element under a screen point (for describing coordinate clicks).
pub fn element_name_at(x: i32, y: i32) -> Option<String> {
    let ctx = uia_ctx().ok()?;
    unsafe {
        let el = ctx.uia.ElementFromPoint(POINT { x, y }).ok()?;
        let name = el.CurrentName().ok()?.to_string();
        let name = name.trim().to_string();
        (!name.is_empty()).then_some(name)
    }
}

/// Fast path: hit-test the element's last known center point and walk up a few ancestors.
/// Costs a handful of round trips instead of a full tree walk (thousands on big web pages).
fn find_element_near_point(
    uia: &IUIAutomation,
    walker: &IUIAutomationTreeWalker,
    point: (i32, i32),
    target_id: &[i32],
) -> Option<IUIAutomationElement> {
    unsafe {
        let mut el = uia
            .ElementFromPoint(POINT {
                x: point.0,
                y: point.1,
            })
            .ok()?;
        for _ in 0..6 {
            if read_runtime_id(&el) == target_id {
                return Some(el);
            }
            el = walker.GetParentElement(&el).ok()?;
        }
    }
    None
}

fn find_element_by_runtime_id(
    uia: &IUIAutomation,
    hwnd: usize,
    target_id: &[i32],
    hint_point: Option<(i32, i32)>,
) -> Option<IUIAutomationElement> {
    if target_id.is_empty() {
        return None;
    }
    let walker = unsafe { uia.ControlViewWalker().ok()? };
    if let Some(p) = hint_point {
        if let Some(el) = find_element_near_point(uia, &walker, p, target_id) {
            return Some(el);
        }
    }
    unsafe {
        let root = uia.ElementFromHandle(HWND(hwnd as *mut c_void)).ok()?;
        let mut queue: VecDeque<(IUIAutomationElement, usize)> = VecDeque::new();
        queue.push_back((root, 0));

        let mut visited = 0usize;
        while let Some((el, depth)) = queue.pop_front() {
            if visited >= 3000 {
                break;
            }
            visited += 1;

            if depth > 0 {
                let rid = read_runtime_id(&el);
                if rid == target_id {
                    return Some(el);
                }
            }
            if depth >= 16 {
                continue;
            }

            if let Ok(mut child) = walker.GetFirstChildElement(&el) {
                loop {
                    queue.push_back((child.clone(), depth + 1));
                    match walker.GetNextSiblingElement(&child) {
                        Ok(next) => child = next,
                        Err(_) => break,
                    }
                }
            }
        }
    }
    None
}

/// Attempts background click/activation via UIA patterns (Invoke / Toggle / SelectionItem)
/// without moving the cursor or stealing foreground focus.
pub fn try_uia_invoke(
    hwnd: usize,
    runtime_id: &[i32],
    hint_point: Option<(i32, i32)>,
) -> Result<Option<&'static str>> {
    if runtime_id.is_empty() {
        return Ok(None);
    }
    let ctx = uia_ctx()?;
    let Some(el) = find_element_by_runtime_id(&ctx.uia, hwnd, runtime_id, hint_point) else {
        return Ok(None);
    };

    unsafe {
        if let Ok(tog) = el.GetCurrentPatternAs::<IUIAutomationTogglePattern>(UIA_TogglePatternId) {
            if tog.Toggle().is_ok() {
                return Ok(Some("UIA TogglePattern"));
            }
        }
        if let Ok(inv) = el.GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId) {
            if inv.Invoke().is_ok() {
                return Ok(Some("UIA InvokePattern"));
            }
        }
        if let Ok(sel) =
            el.GetCurrentPatternAs::<IUIAutomationSelectionItemPattern>(UIA_SelectionItemPatternId)
        {
            if sel.Select().is_ok() {
                return Ok(Some("UIA SelectionItemPattern"));
            }
        }
    }
    Ok(None)
}

/// Attempts background text entry via UIA ValuePattern::SetValue without stealing focus.
pub fn try_uia_set_value(
    hwnd: usize,
    runtime_id: &[i32],
    text: &str,
    hint_point: Option<(i32, i32)>,
) -> Result<bool> {
    if runtime_id.is_empty() {
        return Ok(false);
    }
    let ctx = uia_ctx()?;
    let Some(el) = find_element_by_runtime_id(&ctx.uia, hwnd, runtime_id, hint_point) else {
        return Ok(false);
    };

    unsafe {
        if let Ok(val) = el.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId) {
            let is_ro = val
                .CurrentIsReadOnly()
                .ok()
                .map(|b| b.as_bool())
                .unwrap_or(false);
            if !is_ro {
                let bstr = BSTR::from(text);
                if val.SetValue(&bstr).is_ok() {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}
