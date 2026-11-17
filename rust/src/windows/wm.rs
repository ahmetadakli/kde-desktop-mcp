//! Top-level windows and monitors: listing, moving, resizing and closing
//! windows, in the physical pixels UIA bounds, the cursor and screenshots
//! use (the process is per-monitor DPI aware). A window's position is its
//! visible frame, without the invisible resize borders around it.

use std::collections::BTreeMap;
use std::sync::Mutex;

use windows::Win32::Foundation::{HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITOR_DEFAULTTONEAREST, MONITORINFO,
    MONITORINFOEXW, MonitorFromWindow,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, FindWindowExW, GW_OWNER, GWL_EXSTYLE, GetClassNameW, GetWindow, GetWindowLongW,
    GetWindowPlacement, GetWindowRect, GetWindowTextW, GetWindowThreadProcessId, IsIconic,
    IsWindow, IsWindowVisible, IsZoomed, PostMessageW, SHOW_WINDOW_CMD, SWP_ASYNCWINDOWPOS,
    SWP_NOACTIVATE, SWP_NOZORDER, SetWindowPos, ShowWindowAsync, WINDOWPLACEMENT, WM_CLOSE,
    WS_EX_TOOLWINDOW,
};
use windows::core::{BOOL, PCWSTR};

use super::input;
use super::logic::{self, Rect};
use crate::backend::{R, Screen, WindowGeo};

/// A listed top-level window: its handle, what `windows` reports, and for
/// a Store app's frame the app it hosts ("" otherwise, or while the app is
/// minimized and its CoreWindow detached) and that app's process id.
pub struct Win {
    pub hwnd: HWND,
    pub geo: WindowGeo,
    pub hosted: String,
    pub hosted_pid: u32,
}

fn rect(r: RECT) -> Rect {
    Rect {
        left: r.left,
        top: r.top,
        right: r.right,
        bottom: r.bottom,
    }
}

fn wide(buf: &[u16], n: i32) -> String {
    String::from_utf16_lossy(&buf[..(n.max(0) as usize).min(buf.len())])
}

/// Top-level windows in z-order, topmost first.
fn top_level() -> Vec<HWND> {
    unsafe extern "system" fn add(h: HWND, lp: LPARAM) -> BOOL {
        // SAFETY: lp is the &mut Vec passed below, alive during EnumWindows.
        let list = unsafe { &mut *(lp.0 as *mut Vec<HWND>) };
        list.push(h);
        true.into()
    }
    let mut list: Vec<HWND> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(add), LPARAM(&mut list as *mut Vec<HWND> as isize));
    }
    list
}

fn class_name(h: HWND) -> String {
    let mut buf = [0u16; 256];
    let n = unsafe { GetClassNameW(h, &mut buf) };
    wide(&buf, n)
}

/// The title, read without sending the window a message (a hung app
/// cannot block it).
fn title(h: HWND) -> String {
    let mut buf = [0u16; 512];
    let n = unsafe { GetWindowTextW(h, &mut buf) };
    wide(&buf, n)
}

fn pid(h: HWND) -> u32 {
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(h, Some(&mut pid)) };
    pid
}

fn cloaked(h: HWND) -> bool {
    let mut c = 0u32;
    let ok = unsafe {
        DwmGetWindowAttribute(
            h,
            DWMWA_CLOAKED,
            (&mut c as *mut u32).cast(),
            size_of::<u32>() as u32,
        )
    };
    ok.is_ok() && c != 0
}

fn window_rect(h: HWND) -> Option<Rect> {
    let mut r = RECT::default();
    unsafe { GetWindowRect(h, &mut r) }.ok().map(|_| rect(r))
}

/// The visible frame: the window rectangle without its invisible borders.
fn frame(h: HWND) -> Option<Rect> {
    let mut r = RECT::default();
    let ok = unsafe {
        DwmGetWindowAttribute(
            h,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            (&mut r as *mut RECT).cast(),
            size_of::<RECT>() as u32,
        )
    };
    match ok {
        Ok(()) => Some(rect(r)),
        Err(_) => window_rect(h),
    }
}

/// The app a Store app's frame window hosts and its process id: its
/// CoreWindow child's.
fn hosted_app(h: HWND, class: &str) -> (String, u32) {
    if class != logic::STORE_FRAME_CLASS {
        return (String::new(), 0);
    }
    let core: Vec<u16> = logic::STORE_CORE_CLASS
        .encode_utf16()
        .chain(Some(0))
        .collect();
    match unsafe { FindWindowExW(Some(h), None, PCWSTR(core.as_ptr()), PCWSTR::null()) } {
        Ok(c) if !c.is_invalid() => {
            let p = pid(c);
            (input::proc_name(p), p)
        }
        _ => (String::new(), 0),
    }
}

/// A monitor's device name and its full and work-area rectangles.
fn monitor(m: HMONITOR) -> Option<(String, Rect, Rect)> {
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
    let ok =
        unsafe { GetMonitorInfoW(m, (&mut info as *mut MONITORINFOEXW).cast::<MONITORINFO>()) };
    if !ok.as_bool() {
        return None;
    }
    let n = info.szDevice.iter().position(|c| *c == 0).unwrap_or(32);
    Some((
        logic::display_name(&wide(&info.szDevice, n as i32)),
        rect(info.monitorInfo.rcMonitor),
        rect(info.monitorInfo.rcWork),
    ))
}

/// The invisible borders last seen around each window shown restored, to
/// take off its restored rectangle while it is minimized (DWM has no frame
/// bounds for it then).
static BORDERS: Mutex<BTreeMap<usize, Rect>> = Mutex::new(BTreeMap::new());

/// The window's geometry: its visible frame, or for a minimized window
/// where it goes back to (its restored rectangle, without the borders seen
/// around it last).
fn geometry(h: HWND, minimized: bool, maximized: bool, mon: Option<&(String, Rect, Rect)>) -> Rect {
    let key = h.0 as usize;
    let mut borders = BORDERS.lock().unwrap_or_else(|e| e.into_inner());
    if minimized {
        let mut p = WINDOWPLACEMENT {
            length: size_of::<WINDOWPLACEMENT>() as u32,
            ..Default::default()
        };
        if unsafe { GetWindowPlacement(h, &mut p) }.is_ok() {
            let mut normal = rect(p.rcNormalPosition);
            if let Some((_, full, work)) = mon {
                normal = logic::workspace_to_screen(normal, *work, *full);
            }
            return match borders.get(&key) {
                Some(b) => logic::inset(normal, *b),
                None => normal,
            };
        }
    }
    let Some(fr) = frame(h) else {
        return Rect::default();
    };
    if !minimized
        && !maximized
        && let Some(win) = window_rect(h)
    {
        // Handles are reused: only live windows are kept.
        if borders.len() > 256 {
            borders.retain(|k, _| unsafe { IsWindow(Some(HWND(*k as *mut _))) }.as_bool());
        }
        borders.insert(key, logic::borders(win, fr));
    }
    fr
}

fn describe(h: HWND, title: String, app: String, fg_root: HWND) -> WindowGeo {
    let minimized = unsafe { IsIconic(h) }.as_bool();
    let maximized = unsafe { IsZoomed(h) }.as_bool();
    let mon = monitor(unsafe { MonitorFromWindow(h, MONITOR_DEFAULTTONEAREST) });
    let g = geometry(h, minimized, maximized, mon.as_ref());
    WindowGeo {
        title,
        app,
        x: g.left as f64,
        y: g.top as f64,
        width: g.width() as f64,
        height: g.height() as f64,
        active: h == fg_root,
        minimized,
        maximized,
        output: mon.map(|m| m.0).unwrap_or_default(),
        pid: pid(h),
        id: h.0 as usize as u64,
        terminals: Vec::new(),
        desktops: Vec::new(),
    }
}

/// Windows of allowed apps whose title or app (or hosted Store app)
/// contains `query`, topmost first.
pub fn list(query: &str, allowed: &[String]) -> Vec<Win> {
    let q = query.trim().to_lowercase();
    let fg_root = input::root(input::foreground());
    let mut out = Vec::new();
    for h in top_level() {
        let visible = unsafe { IsWindowVisible(h) }.as_bool();
        if !visible {
            continue;
        }
        let ex = unsafe { GetWindowLongW(h, GWL_EXSTYLE) } as u32;
        let class = class_name(h);
        let t = title(h);
        if !logic::listed_window(
            visible,
            cloaked(h),
            ex & WS_EX_TOOLWINDOW.0 != 0,
            &class,
            &t,
        ) {
            continue;
        }
        let app = input::proc_name(pid(h));
        if app.is_empty() {
            continue;
        }
        let (hosted, hosted_pid) = hosted_app(h, &class);
        if !logic::window_allowed(allowed, &app, &hosted)
            || !logic::window_matches(&q, &t, &app, &hosted)
        {
            continue;
        }
        out.push(Win {
            hwnd: h,
            geo: describe(h, t, app, fg_root),
            hosted,
            hosted_pid,
        });
    }
    out
}

/// The window `w` describes, found again.
pub fn find(w: &WindowGeo, allowed: &[String]) -> Option<Win> {
    list(&w.title, allowed)
        .into_iter()
        .find(|n| n.geo.id == w.id)
}

/// `h` as `windows` reports it now.
pub fn reread(h: HWND, app: &str) -> WindowGeo {
    describe(
        h,
        title(h),
        app.to_string(),
        input::root(input::foreground()),
    )
}

pub fn is_minimized(h: HWND) -> bool {
    unsafe { IsIconic(h) }.as_bool()
}

pub fn is_maximized(h: HWND) -> bool {
    unsafe { IsZoomed(h) }.as_bool()
}

/// Post a show command (minimize, maximize, restore) without waiting for
/// the app: a hung one cannot block the server.
pub fn show(h: HWND, cmd: SHOW_WINDOW_CMD) {
    unsafe {
        let _ = ShowWindowAsync(h, cmd);
    }
}

/// Give the window the visible frame (x, y, width, height), the values not
/// given kept; its invisible borders are added for SetWindowPos.
pub fn place(
    h: HWND,
    x: Option<f64>,
    y: Option<f64>,
    width: Option<f64>,
    height: Option<f64>,
) -> R<()> {
    let (Some(win), Some(fr)) = (window_rect(h), frame(h)) else {
        return Err("could not read the window's position".into());
    };
    let want = logic::wanted_frame(fr, x, y, width, height);
    let (px, py, cx, cy) = logic::window_pos(want, logic::borders(win, fr));
    unsafe {
        SetWindowPos(
            h,
            None,
            px,
            py,
            cx,
            cy,
            SWP_NOZORDER | SWP_NOACTIVATE | SWP_ASYNCWINDOWPOS,
        )
    }
    .map_err(|e| format!("SetWindowPos failed: {} (an elevated window?)", e.message()))
}

/// Ask the window to close as its close button does. Posted, not sent: the
/// app may show a save prompt before it answers.
pub fn close(h: HWND) -> R<()> {
    unsafe { PostMessageW(Some(h), WM_CLOSE, WPARAM(0), LPARAM(0)) }
        .map_err(|e| format!("could not ask the window to close: {}", e.message()))
}

/// Shown top-level windows owned by `h`: its dialogs (a Win32 save prompt).
pub fn owned(h: HWND) -> Vec<HWND> {
    top_level()
        .into_iter()
        .filter(|&o| {
            unsafe { GetWindow(o, GW_OWNER) }.is_ok_and(|owner| owner == h)
                && unsafe { IsWindowVisible(o) }.as_bool()
                && !cloaked(o)
        })
        .collect()
}

/// The monitors with their full rectangles and scale.
pub fn screens() -> Vec<Screen> {
    unsafe extern "system" fn add(m: HMONITOR, _: HDC, _: *mut RECT, lp: LPARAM) -> BOOL {
        // SAFETY: lp is the &mut Vec passed below, alive during the call.
        let list = unsafe { &mut *(lp.0 as *mut Vec<HMONITOR>) };
        list.push(m);
        true.into()
    }
    let mut mons: Vec<HMONITOR> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(add),
            LPARAM(&mut mons as *mut Vec<HMONITOR> as isize),
        );
    }
    mons.into_iter()
        .filter_map(|m| {
            let (name, full, _) = monitor(m)?;
            let (mut dx, mut dy) = (0u32, 0u32);
            let dpi = match unsafe { GetDpiForMonitor(m, MDT_EFFECTIVE_DPI, &mut dx, &mut dy) } {
                Ok(()) => dx,
                Err(_) => 0,
            };
            Some(Screen {
                name,
                x: full.left as f64,
                y: full.top as f64,
                width: full.width() as f64,
                height: full.height() as f64,
                scale: logic::dpi_scale(dpi),
            })
        })
        .collect()
}
