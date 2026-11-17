//! The Windows clipboard: CF_UNICODETEXT read and written for the clipboard
//! tools, and the paste of `type_text`, whose text is offered with delayed
//! rendering so the server sees the app in front read it before putting
//! the old clipboard back.
//!
//! A message-only window on a thread of its own owns what the server puts
//! on the clipboard: Windows asks the owner for delayed data with
//! WM_RENDERFORMAT, sent to the owner's thread while the reader waits, so
//! that thread must pump messages, which the tokio thread cannot. The
//! clipboard itself is opened from the tokio thread (with that window as
//! the new owner) and closed again before any await.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use windows::Win32::Foundation::{
    GetLastError, GlobalFree, HANDLE, HGLOBAL, HWND, LPARAM, LRESULT, WPARAM,
};
use windows::Win32::System::DataExchange::{
    CloseClipboard, CountClipboardFormats, EmptyClipboard, EnumClipboardFormats, GetClipboardData,
    GetClipboardFormatNameW, GetClipboardOwner, IsClipboardFormatAvailable, OpenClipboard,
    RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{
    GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, HWND_MESSAGE, MSG,
    RegisterClassExW, WINDOW_EX_STYLE, WINDOW_STYLE, WM_DESTROYCLIPBOARD, WM_RENDERALLFORMATS,
    WM_RENDERFORMAT, WNDCLASSEXW,
};
use windows::core::{HSTRING, PCWSTR, w};

use super::logic::{self, CF_UNICODETEXT, ClipKind};
use crate::backend::{Clip, R};
use crate::delivery::nothing_sent;

/// OpenClipboard fails while another app has the clipboard open (to read
/// or write it, usually for a few ms): tries, and the pause between them.
const OPEN_TRIES: u32 = 25;
const OPEN_RETRY: Duration = Duration::from_millis(10);
/// Largest clipboard text `type_text` puts back, in UTF-16 units (4 MiB);
/// anything bigger stays replaced.
const RESTORE_MAX: usize = 2 << 20;
/// After the app read the paste, how long to leave it before writing the
/// clipboard again: it may still be reading other formats synthesized from
/// the text (CF_TEXT, CF_LOCALE), which do not render again.
const PASTE_QUIET: Duration = Duration::from_millis(20);

/// What happened to the text the owner window offers.
#[derive(Debug)]
enum Ev {
    /// An app asked for it (WM_RENDERFORMAT) and got it.
    Read(Instant),
    /// Another app (or the server) emptied the clipboard: it is gone.
    Replaced(Instant),
}

/// State the owner window's procedure shares with the server: the text to
/// render on demand, and where to report reads.
struct Shared {
    text: Option<Vec<u16>>,
    events: Option<mpsc::UnboundedSender<Ev>>,
}

static SHARED: Mutex<Shared> = Mutex::new(Shared {
    text: None,
    events: None,
});

fn shared() -> std::sync::MutexGuard<'static, Shared> {
    SHARED.lock().unwrap_or_else(|e| e.into_inner())
}

/// The clipboard as it was before a paste: what to put back, or why not.
pub enum Saved {
    /// CF_UNICODETEXT as it was (NUL-terminated); `secret` when marked as
    /// a password.
    Text {
        units: Vec<u16>,
        secret: bool,
    },
    Empty,
    /// Another app held the clipboard open: it cannot be taken either.
    Busy,
    Nothing(&'static str),
}

/// The owner window, started on first use.
struct Owner {
    hwnd: isize,
    /// Reads and replacements of the offered text; locked for a whole
    /// paste, so two pastes do not take each other's reads.
    events: tokio::sync::Mutex<mpsc::UnboundedReceiver<Ev>>,
}

#[derive(Default)]
pub struct Clipboard {
    owner: OnceLock<Option<Owner>>,
}

/// The clipboard, open; closed when dropped (on every path, so a failure
/// never leaves other apps locked out). Never held across an await.
struct Open;

impl Drop for Open {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseClipboard();
        }
    }
}

fn hwnd(h: isize) -> HWND {
    HWND(h as *mut core::ffi::c_void)
}

fn last_error() -> u32 {
    unsafe { GetLastError().0 }
}

/// Text formats whose data is a DWORD 0: the opt-outs of the clipboard
/// history (Win+V) and the cloud clipboard, which the restore of a text
/// already in the history also uses so it is not listed twice.
const OPT_OUTS: [&str; 2] = [logic::HINT_HISTORY, logic::HINT_CLOUD];

fn register(name: &str) -> u32 {
    unsafe { RegisterClipboardFormatW(&HSTRING::from(name)) }
}

/// A movable global block holding `bytes`, for SetClipboardData.
fn global(bytes: &[u8]) -> Option<HGLOBAL> {
    unsafe {
        let h = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1)).ok()?;
        let p = GlobalLock(h);
        if p.is_null() {
            let _ = GlobalFree(Some(h));
            return None;
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p.cast::<u8>(), bytes.len());
        // Err with no error code means the lock count reached zero.
        let _ = GlobalUnlock(h);
        Some(h)
    }
}

fn unit_bytes(units: &[u16]) -> Vec<u8> {
    units.iter().flat_map(|u| u.to_ne_bytes()).collect()
}

/// Put `bytes` on the open clipboard as `format`. On success the system
/// owns the memory; it is freed here only when that failed.
fn put(format: u32, bytes: &[u8]) -> R<()> {
    let h = global(bytes).ok_or("clipboard: out of memory")?;
    unsafe {
        if SetClipboardData(format, Some(HANDLE(h.0))).is_err() {
            let e = last_error();
            let _ = GlobalFree(Some(h));
            return Err(format!("[Errno {e}] clipboard: SetClipboardData failed"));
        }
    }
    Ok(())
}

/// Mark the open clipboard's contents: out of the history and the cloud
/// clipboard, and with `secret` also as a password for clipboard monitors
/// (and for `clipboard_get`). Best effort: a mark that fails leaves the
/// text usable.
fn mark(secret: bool) {
    for name in OPT_OUTS {
        let _ = put(register(name), &0u32.to_ne_bytes());
    }
    if secret {
        let _ = put(register(logic::HINT_EXCLUDE), &0u32.to_ne_bytes());
    }
}

/// The formats on the clipboard, with names (opened by the caller).
fn formats() -> Vec<(u32, String)> {
    let mut out = Vec::new();
    let mut f = 0;
    loop {
        f = unsafe { EnumClipboardFormats(f) };
        if f == 0 {
            break;
        }
        let name = match logic::standard_format(f) {
            Some(n) => n.to_string(),
            None => {
                let mut buf = [0u16; 256];
                let n = unsafe { GetClipboardFormatNameW(f, &mut buf) };
                match n {
                    1.. => String::from_utf16_lossy(&buf[..n as usize]),
                    _ => format!("format {f}"),
                }
            }
        };
        out.push((f, name));
    }
    out
}

/// CF_UNICODETEXT from the open clipboard: at most `max` units before its
/// NUL, and whether there were more.
fn read_units(max: usize) -> R<(Vec<u16>, bool)> {
    unsafe {
        let h = GetClipboardData(CF_UNICODETEXT)
            .map_err(|_| format!("[Errno {}] clipboard: it holds no text", last_error()))?;
        let g = HGLOBAL(h.0);
        let size = GlobalSize(g) / 2;
        let p = GlobalLock(g).cast::<u16>();
        if p.is_null() {
            return Err(format!(
                "[Errno {}] clipboard: its text could not be read",
                last_error()
            ));
        }
        let all = logic::until_nul(std::slice::from_raw_parts(p, size));
        let more = all.len() > max;
        let units = all[..all.len().min(max)].to_vec();
        let _ = GlobalUnlock(g);
        Ok((units, more))
    }
}

/// Render the offered text on the clipboard, which the reader holds open.
fn render() {
    let (text, events) = {
        let s = shared();
        (s.text.clone(), s.events.clone())
    };
    let Some(text) = text else {
        return;
    };
    let _ = put(CF_UNICODETEXT, &unit_bytes(&text));
    if let Some(tx) = events {
        let _ = tx.send(Ev::Read(Instant::now()));
    }
}

unsafe extern "system" fn owner_proc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        // An app asks for the delayed text; the clipboard is already open.
        WM_RENDERFORMAT => {
            if wp.0 as u32 == CF_UNICODETEXT {
                render();
            }
            LRESULT(0)
        }
        // The window goes away while its text is still delayed: render it
        // so the clipboard keeps it.
        WM_RENDERALLFORMATS => {
            unsafe {
                if OpenClipboard(Some(h)).is_ok() {
                    if GetClipboardOwner().ok() == Some(h) {
                        render();
                    }
                    let _ = CloseClipboard();
                }
            }
            LRESULT(0)
        }
        WM_DESTROYCLIPBOARD => {
            let mut s = shared();
            s.text = None;
            if let Some(tx) = &s.events {
                let _ = tx.send(Ev::Replaced(Instant::now()));
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(h, msg, wp, lp) },
    }
}

/// Start the owner window's thread; its handle once the window exists.
fn start_owner() -> Option<isize> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("clipboard".into())
        .spawn(move || unsafe {
            let class: PCWSTR = w!("desktop-mcp-clipboard");
            let wc = WNDCLASSEXW {
                cbSize: size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(owner_proc),
                lpszClassName: class,
                ..Default::default()
            };
            RegisterClassExW(&wc);
            let Ok(h) = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                class,
                w!(""),
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                None,
                None,
            ) else {
                let _ = tx.send(None);
                return;
            };
            let _ = tx.send(Some(h.0 as isize));
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                DispatchMessageW(&msg);
            }
        })
        .ok()?;
    rx.recv().ok().flatten()
}

impl Clipboard {
    fn owner(&self) -> R<&Owner> {
        self.owner
            .get_or_init(|| {
                let h = start_owner()?;
                let (tx, rx) = mpsc::unbounded_channel();
                shared().events = Some(tx);
                Some(Owner {
                    hwnd: h,
                    events: tokio::sync::Mutex::new(rx),
                })
            })
            .as_ref()
            .ok_or_else(|| "clipboard: its owner window could not be created".to_string())
    }

    /// Open the clipboard, retrying while another app has it open.
    async fn open(&self) -> R<Open> {
        let owner = self.owner()?.hwnd;
        for i in 0..OPEN_TRIES {
            if i > 0 {
                tokio::time::sleep(OPEN_RETRY).await;
            }
            if unsafe { OpenClipboard(Some(hwnd(owner))) }.is_ok() {
                return Ok(Open);
            }
        }
        Err(format!(
            "[Errno {}] the clipboard is held open by another app",
            last_error()
        ))
    }

    /// Read the clipboard's text, at most `limit` characters.
    pub async fn get(&self, limit: usize) -> R<Clip> {
        let _open = self.open().await?;
        if unsafe { CountClipboardFormats() } == 0 {
            return Ok(Clip::Empty);
        }
        let formats = formats();
        Ok(match logic::classify(&formats) {
            ClipKind::Empty => Clip::Empty,
            ClipKind::Text { secret: true } => Clip::Secret,
            ClipKind::Text { secret: false } => {
                // A character takes at most 2 units ("\r\n" or a surrogate
                // pair); one more shows a cut.
                let (units, more) = read_units(limit.saturating_mul(2).saturating_add(1))?;
                let (text, cut) = logic::clipboard_text(&units, limit);
                Clip::Text {
                    text,
                    truncated: cut || more,
                }
            }
            ClipKind::Files => Clip::Other(logic::content_names(&formats)),
            ClipKind::Other(names) => Clip::Other(names),
        })
    }

    /// Put `text` on the clipboard, as copying it in an app does.
    pub async fn set(&self, text: &str) -> R<()> {
        let units = logic::clipboard_units(text);
        let _open = self.open().await.map_err(nothing_sent)?;
        unsafe { EmptyClipboard() }.map_err(|_| {
            nothing_sent(format!(
                "[Errno {}] clipboard: EmptyClipboard failed",
                last_error()
            ))
        })?;
        put(CF_UNICODETEXT, &unit_bytes(&units))
    }

    /// The clipboard as it is, to put back after a paste.
    pub async fn save(&self) -> Saved {
        let Ok(_open) = self.open().await else {
            return Saved::Busy;
        };
        if unsafe { CountClipboardFormats() } == 0 {
            return Saved::Empty;
        }
        match logic::classify(&formats()) {
            ClipKind::Empty => Saved::Empty,
            ClipKind::Files => Saved::Nothing("it held files"),
            ClipKind::Other(_) => Saved::Nothing("it held no text"),
            ClipKind::Text { secret } => match read_units(RESTORE_MAX) {
                Ok((_, true)) => Saved::Nothing("it held more than 4 MiB"),
                Ok((mut units, false)) => {
                    units.push(0);
                    Saved::Text { units, secret }
                }
                Err(_) => Saved::Nothing("it could not be read"),
            },
        }
    }

    /// Own the clipboard with `text` rendered on demand, marked so the
    /// clipboard history, the cloud clipboard and clipboard monitors
    /// leave it alone. Err when the clipboard could not be taken; then it
    /// may have been emptied.
    pub async fn offer(&self, text: &str) -> R<()> {
        let units = logic::clipboard_units(text);
        let owner = self.owner()?;
        let open = self.open().await?;
        unsafe { EmptyClipboard() }
            .map_err(|_| format!("[Errno {}] clipboard: EmptyClipboard failed", last_error()))?;
        // After EmptyClipboard: it told the owner (us, maybe) to drop its text.
        shared().text = Some(units);
        unsafe {
            // NULL data is delayed rendering: a "failed" result is expected.
            let _ = SetClipboardData(CF_UNICODETEXT, None);
            if IsClipboardFormatAvailable(CF_UNICODETEXT).is_err() {
                shared().text = None;
                return Err(format!(
                    "[Errno {}] clipboard: SetClipboardData failed",
                    last_error()
                ));
            }
        }
        mark(true);
        drop(open);
        // What came before (our own EmptyClipboard) is not about this offer.
        let mut events = owner.events.lock().await;
        while events.try_recv().is_ok() {}
        Ok(())
    }

    /// Wait for an app to read the offered text at or after `since` (a
    /// read before it came from a clipboard monitor, not the paste), then
    /// a moment for the rest of its paste. False when none came within
    /// `limit` or another app replaced the clipboard first.
    pub async fn wait_read(&self, since: Instant, limit: Duration) -> bool {
        let Ok(owner) = self.owner() else {
            return false;
        };
        let mut events = owner.events.lock().await;
        let deadline = tokio::time::Instant::from_std(since + limit);
        loop {
            match tokio::time::timeout_at(deadline, events.recv()).await {
                Ok(Some(Ev::Read(at))) if at >= since => break,
                Ok(Some(Ev::Replaced(at))) if at >= since => return false,
                Ok(Some(_)) => {}
                _ => return false,
            }
        }
        drop(events);
        tokio::time::sleep(PASTE_QUIET).await;
        true
    }

    /// Whether the text read before `since` (a clipboard monitor read it
    /// right after the offer, so the paste will not render it again).
    pub async fn read_early(&self) -> bool {
        let Ok(owner) = self.owner() else {
            return false;
        };
        let mut events = owner.events.lock().await;
        let mut read = false;
        while let Ok(e) = events.try_recv() {
            read |= matches!(e, Ev::Read(_));
        }
        read
    }

    /// Put back what `save` found, unless another app took the clipboard
    /// since the paste (then its contents stay). The note for the reply.
    pub async fn restore(&self, saved: Saved) -> String {
        if let Saved::Nothing(_) | Saved::Busy = saved {
            return String::new();
        }
        let owner = match self.owner() {
            Ok(o) => hwnd(o.hwnd),
            Err(e) => return format!("; the clipboard could not be put back ({e})"),
        };
        let open = match self.open().await {
            Ok(o) => o,
            Err(e) => return format!("; the clipboard could not be put back ({e})"),
        };
        if unsafe { GetClipboardOwner() }.ok() != Some(owner) {
            return String::new();
        }
        if unsafe { EmptyClipboard() }.is_err() {
            return format!(
                "; the clipboard could not be put back ([Errno {}] EmptyClipboard failed)",
                last_error()
            );
        }
        let note = match saved {
            Saved::Text { units, secret } => match put(CF_UNICODETEXT, &unit_bytes(&units)) {
                Ok(()) => {
                    mark(secret);
                    String::new()
                }
                Err(e) => format!("; the clipboard could not be put back ({e})"),
            },
            Saved::Empty | Saved::Busy | Saved::Nothing(_) => String::new(),
        };
        drop(open);
        note
    }
}
