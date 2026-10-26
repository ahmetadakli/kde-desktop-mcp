//! Win32 side of input and windows: SendInput, the cursor, the foreground
//! window, process names and window screenshots.

use std::path::PathBuf;

use windows::Win32::Foundation::{CloseHandle, GetLastError, HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleBitmap,
    CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, ReleaseDC,
    SRCCOPY, SelectObject,
};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT, MOUSE_EVENT_FLAGS,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
    MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL, MOUSEINPUT, SendInput,
    VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GA_ROOT, GetAncestor, GetForegroundWindow, GetWindowRect, GetWindowThreadProcessId, IsIconic,
    SW_RESTORE, SetCursorPos, SetForegroundWindow, ShowWindow,
};
use windows::core::PWSTR;

use super::logic::{self, KeyEv};
use crate::backend::R;

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

pub fn hwnd_proc_name(h: HWND) -> String {
    if h.is_invalid() {
        return String::new();
    }
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(h, Some(&mut pid)) };
    proc_name(pid)
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

/// Grab the screen rectangle of the foreground window as 24-bit RGB rows.
fn grab_foreground() -> Option<(u32, u32, Vec<u8>)> {
    let h = foreground();
    if h.is_invalid() {
        return None;
    }
    let mut r = RECT::default();
    unsafe { GetWindowRect(h, &mut r) }.ok()?;
    let (w, ht) = (r.right - r.left, r.bottom - r.top);
    if w <= 0 || ht <= 0 {
        return None;
    }
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
        (lines == ht).then(|| (w as u32, ht as u32, logic::bgra_to_rgb(&bgra)))
    }
}

/// Capture the foreground window to a new private PNG in %TEMP%.
pub fn screenshot() -> Option<String> {
    let (w, h, rgb) = grab_foreground()?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path: PathBuf = std::env::temp_dir().join(format!("desktop-mcp-{nanos}.png"));
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .ok()?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    enc.set_compression(png::Compression::Fast);
    let ok = enc
        .write_header()
        .and_then(|mut wr| wr.write_image_data(&rgb))
        .is_ok();
    if !ok {
        let _ = std::fs::remove_file(&path);
        return None;
    }
    Some(path.display().to_string())
}
