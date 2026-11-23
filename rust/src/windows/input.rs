//! Win32 side of input and windows: SendInput, the cursor, the foreground
//! window, process names and window screenshots.

use windows::Win32::Foundation::{CloseHandle, FILETIME, GetLastError, HWND, POINT, RECT};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleBitmap,
    CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, ReleaseDC,
    SRCCOPY, SelectObject,
};
use windows::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    QueryFullProcessImageNameW,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT, MOUSE_EVENT_FLAGS,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
    MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL, MOUSEINPUT, SendInput,
    VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowExW, GA_ROOT, GetAncestor, GetCursorPos, GetForegroundWindow, GetWindowRect,
    GetWindowTextW, GetWindowThreadProcessId, IsIconic, SW_RESTORE, SetCursorPos,
    SetForegroundWindow, ShowWindow,
};
use windows::core::{PCWSTR, PWSTR};

use super::logic::{self, KeyEv};
use crate::backend::R;
use crate::shot;

const WHEEL_DELTA: i64 = 120;

fn last_error() -> u32 {
    unsafe { GetLastError().0 }
}

fn key(e: &KeyEv) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(e.vk),
                wScan: e.scan,
                dwFlags: KEYBD_EVENT_FLAGS(e.flags),
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn mouse(flags: MOUSE_EVENT_FLAGS, data: i32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: data as u32,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn send(inputs: &[INPUT]) -> R<()> {
    if inputs.is_empty() {
        return Ok(());
    }
    let sent = unsafe { SendInput(inputs, size_of::<INPUT>() as i32) };
    if sent as usize != inputs.len() {
        return Err(format!(
            "[Errno {}] SendInput was blocked (a higher-integrity window may be \
                            in front)",
            last_error()
        ));
    }
    Ok(())
}

pub fn send_keys(events: &[KeyEv]) -> R<()> {
    send(&events.iter().map(key).collect::<Vec<_>>())
}

/// Down and up flags for a mouse button name.
pub fn button_flags(button: &str) -> R<(MOUSE_EVENT_FLAGS, MOUSE_EVENT_FLAGS)> {
    match button {
        "left" => Ok((MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP)),
        "right" => Ok((MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP)),
        "middle" => Ok((MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP)),
        _ => Err(format!("unknown button '{button}' (left, right, middle)")),
    }
}

pub fn button(flags: MOUSE_EVENT_FLAGS) -> R<()> {
    send(&[mouse(flags, 0)])
}

pub fn wheel(scroll: i64) -> R<()> {
    let data = (-scroll)
        .saturating_mul(WHEEL_DELTA)
        .clamp(i32::MIN as i64, i32::MAX as i64);
    send(&[mouse(MOUSEEVENTF_WHEEL, data as i32)])
}

pub fn set_cursor(x: i64, y: i64) -> R<()> {
    let clamp = |v: i64| v.clamp(i32::MIN as i64, i32::MAX as i64) as i32;
    unsafe { SetCursorPos(clamp(x), clamp(y)) }
        .map_err(|_| format!("[Errno {}] SetCursorPos failed", last_error()))
}

/// Where the cursor is, in the physical pixels `set_cursor` takes.
pub fn cursor() -> R<(i64, i64)> {
    let mut p = POINT::default();
    unsafe { GetCursorPos(&mut p) }
        .map_err(|_| format!("[Errno {}] GetCursorPos failed", last_error()))?;
    Ok((p.x as i64, p.y as i64))
}

pub fn foreground() -> HWND {
    unsafe { GetForegroundWindow() }
}

/// Window handle as UIA's NativeWindowHandle reports it (handles only use
/// the low 32 bits).
pub fn hwnd_i32(h: HWND) -> i32 {
    h.0 as usize as u32 as i32
}

pub fn hwnd_from_i32(h: i32) -> HWND {
    HWND(h as isize as *mut core::ffi::c_void)
}

/// Top-level window that owns `h`.
pub fn root(h: HWND) -> HWND {
    unsafe { GetAncestor(h, GA_ROOT) }
}

/// Restore if minimised and ask for the foreground.
pub fn set_foreground(h: HWND) {
    unsafe {
        if IsIconic(h).as_bool() {
            let _ = ShowWindow(h, SW_RESTORE);
        }
        let _ = SetForegroundWindow(h);
    }
}

/// A bare Alt tap: Windows only lets a process take the foreground right
/// after input.
pub fn alt_tap() -> R<()> {
    let alt = logic::VK_MENU;
    send_keys(&[
        KeyEv {
            vk: alt,
            scan: 0,
            flags: 0,
        },
        KeyEv {
            vk: alt,
            scan: 0,
            flags: logic::KEYEVENTF_KEYUP,
        },
    ])
}

/// Executable stem of a process. Not cached: process ids are reused, and a
/// stale name would let input through the allowlist.
pub fn proc_name(pid: u32) -> String {
    if pid == 0 {
        return String::new();
    }
    image_stem(pid)
}

/// The app a window belongs to, as the allowlist names it: see
/// `window_app`.
pub fn hwnd_proc_name(h: HWND) -> String {
    if h.is_invalid() {
        return String::new();
    }
    window_app(h, hwnd_pid(h))
}

pub fn hwnd_pid(h: HWND) -> u32 {
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(h, Some(&mut pid)) };
    pid
}

fn wide_z(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// The app of the top-level window `h` of process `pid`: the process's
/// executable stem, or for a Store app's frame (ApplicationFrameHost's) the
/// app it hosts, when it hosts one.
pub fn window_app(h: HWND, pid: u32) -> String {
    let name = proc_name(pid);
    // A null parent would make FindWindowExW search the top-level windows.
    if name != logic::STORE_FRAME_APP || h.is_invalid() {
        return name;
    }
    match hosted(h, pid) {
        Some((app, _)) => app,
        None => name,
    }
}

/// The Store app the frame `h` (of ApplicationFrameHost, process
/// `frame_pid`) hosts, and its process id: its CoreWindow child's. While
/// the app is minimized Windows takes its CoreWindow out of the frame and
/// parks it as a top-level window of the same title; that one counts too.
pub fn hosted(h: HWND, frame_pid: u32) -> Option<(String, u32)> {
    let class = wide_z(logic::STORE_CORE_CLASS);
    let of = |c: HWND| {
        let p = hwnd_pid(c);
        let name = proc_name(p);
        (p != frame_pid && !name.is_empty()).then_some((name, p))
    };
    if let Ok(c) = unsafe { FindWindowExW(Some(h), None, PCWSTR(class.as_ptr()), PCWSTR::null()) }
        && !c.is_invalid()
    {
        return of(c);
    }
    let mut buf = [0u16; 512];
    let n = unsafe { GetWindowTextW(h, &mut buf) };
    if n <= 0 {
        return None;
    }
    let title: Vec<u16> = buf[..n as usize].iter().copied().chain(Some(0)).collect();
    match unsafe { FindWindowExW(None, None, PCWSTR(class.as_ptr()), PCWSTR(title.as_ptr())) } {
        Ok(c) if !c.is_invalid() => of(c),
        _ => None,
    }
}

/// The apps the Store frames now open host, for the allowlist alias.
/// Win32 only: cheap enough for every allowlist read.
pub fn hosted_apps() -> Vec<String> {
    let class = wide_z(logic::STORE_FRAME_CLASS);
    let mut out: Vec<String> = Vec::new();
    let mut prev: Option<HWND> = None;
    // A bound in case frames come and go while walking them.
    for _ in 0..256 {
        let Ok(h) = (unsafe { FindWindowExW(None, prev, PCWSTR(class.as_ptr()), PCWSTR::null()) })
        else {
            break;
        };
        if h.is_invalid() {
            break;
        }
        // Only ApplicationFrameHost's frames: explorer has a window of
        // this class too, which made explorer an alias of the frame.
        let pid = hwnd_pid(h);
        if proc_name(pid) == logic::STORE_FRAME_APP
            && let Some((app, _)) = hosted(h, pid)
            && !out.contains(&app)
        {
            out.push(app);
        }
        prev = Some(h);
    }
    out
}

/// When process `pid` started (100 ns units since 1601); None when it
/// cannot be opened.
pub fn process_created(pid: u32) -> Option<u64> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let (mut created, mut exited, mut kernel, mut user) = (
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
        );
        let ok = GetProcessTimes(h, &mut created, &mut exited, &mut kernel, &mut user);
        let _ = CloseHandle(h);
        ok.ok()?;
        Some(u64::from(created.dwHighDateTime) << 32 | u64::from(created.dwLowDateTime))
    }
}

fn image_stem(pid: u32) -> String {
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return String::new();
        };
        let mut buf = [0u16; 1024];
        let mut size = buf.len() as u32;
        let ok =
            QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut size);
        let _ = CloseHandle(h);
        if ok.is_err() {
            return String::new();
        }
        logic::exe_stem(&String::from_utf16_lossy(&buf[..size as usize]))
    }
}

/// Grab the screen rectangle of window `h` as 24-bit RGB rows. The screen
/// shows whatever is in front there: `h` must be the foreground window
/// before and after, or the grab fails.
fn grab_foreground(h: HWND) -> R<(u32, u32, Vec<u8>, RECT)> {
    let changed = || {
        "the foreground window changed during the screenshot; nothing captured, \
                      take a new one"
            .to_string()
    };
    if h.is_invalid() || foreground() != h {
        return Err(changed());
    }
    let mut r = RECT::default();
    unsafe { GetWindowRect(h, &mut r) }
        .map_err(|_| format!("[Errno {}] GetWindowRect failed", last_error()))?;
    let (w, ht) = (r.right - r.left, r.bottom - r.top);
    if w <= 0 || ht <= 0 {
        return Err(format!("the window has no area ({w}x{ht})"));
    }
    let grabbed = grab_rect(w, ht, r).ok_or("the screen could not be copied (BitBlt)")?;
    if foreground() != h {
        return Err(changed());
    }
    Ok(grabbed)
}

/// The screen rectangle `r` (`w` x `ht`) as 24-bit RGB rows.
fn grab_rect(w: i32, ht: i32, r: RECT) -> Option<(u32, u32, Vec<u8>, RECT)> {
    unsafe {
        let screen = GetDC(None);
        let mem = CreateCompatibleDC(Some(screen));
        let bmp = CreateCompatibleBitmap(screen, w, ht);
        let old = SelectObject(mem, bmp.into());
        let copied = BitBlt(
            mem,
            0,
            0,
            w,
            ht,
            Some(screen),
            r.left,
            r.top,
            SRCCOPY | CAPTUREBLT,
        );
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -ht, // top-down rows
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bgra = vec![0u8; w as usize * ht as usize * 4];
        let lines = if copied.is_ok() {
            GetDIBits(
                mem,
                bmp,
                0,
                ht as u32,
                Some(bgra.as_mut_ptr().cast()),
                &mut info,
                DIB_RGB_COLORS,
            )
        } else {
            0
        };
        SelectObject(mem, old);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
        (lines == ht).then(|| (w as u32, ht as u32, logic::bgra_to_rgb(&bgra), r))
    }
}

/// Capture window `h`, which must be the foreground window: its RGB pixels
/// and screen rectangle, plus a new private file (`spill::SHOTS`) for the PNG made
/// from them.
pub fn screenshot(h: HWND) -> R<(String, shot::Pixels, RECT)> {
    let (width, height, data, rect) = grab_foreground(h)?;
    let dir = crate::spill::dir(&crate::spill::SHOTS).ok_or("screenshot: no image directory")?;
    let (path, _) = crate::spill::create(&dir, &crate::spill::SHOTS)
        .map_err(|e| format!("screenshot: cannot create a file in {}: {e}", dir.display()))?;
    let pixels = shot::Pixels {
        width,
        height,
        channels: 3,
        data,
    };
    Ok((path.display().to_string(), pixels, rect))
}
