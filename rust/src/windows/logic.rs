//! Platform-independent half of the Windows backend: key tables, the
//! SendInput sequences, UIA role names and the state/action rules of the
//! Python backend. Pure functions only, so the unit tests also run on Linux.

use crate::backend::R;

pub const KEYEVENTF_EXTENDEDKEY: u32 = 0x1;
pub const KEYEVENTF_KEYUP: u32 = 0x2;
pub const KEYEVENTF_UNICODE: u32 = 0x4;
pub const VK_TAB: u16 = 0x09;
pub const VK_RETURN: u16 = 0x0D;
pub const VK_MENU: u16 = 0x12;

/// One KEYBDINPUT: virtual key, scan code (UTF-16 unit for KEYEVENTF_UNICODE)
/// and flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyEv {
    pub vk: u16,
    pub scan: u16,
    pub flags: u32,
}

const VK: &[(&str, u16)] = &[
    ("ctrl", 0x11),
    ("control", 0x11),
    ("shift", 0x10),
    ("alt", 0x12),
    ("altgr", 0xA5),
    ("win", 0x5B),
    ("super", 0x5B),
    ("meta", 0x5B),
    ("enter", 0x0D),
    ("return", 0x0D),
    ("esc", 0x1B),
    ("escape", 0x1B),
    ("tab", 0x09),
    ("space", 0x20),
    ("backspace", 0x08),
    ("delete", 0x2E),
    ("del", 0x2E),
    ("insert", 0x2D),
    ("home", 0x24),
    ("end", 0x23),
    ("pageup", 0x21),
    ("pgup", 0x21),
    ("pagedown", 0x22),
    ("pgdn", 0x22),
    ("left", 0x25),
    ("up", 0x26),
    ("right", 0x27),
    ("down", 0x28),
    ("capslock", 0x14),
    ("printscreen", 0x2C),
    ("sysrq", 0x2C),
    ("menu", 0x5D),
    ("compose", 0x5D),
];
/// Keys that need KEYEVENTF_EXTENDEDKEY (navigation block, Win, Menu,
/// AltGr, PrintScreen).
const EXTENDED: &[u16] = &[
    0x2E, 0x2D, 0x24, 0x23, 0x21, 0x22, 0x25, 0x26, 0x27, 0x28, 0x5B, 0x5D, 0xA5, 0x2C,
];

/// Python `_vk`: alias table, single ASCII letter or digit, or F1-F24.
pub fn vk(name: &str) -> R<u16> {
    let n = name.to_lowercase();
    if let Some((_, code)) = VK.iter().find(|(k, _)| *k == n) {
        return Ok(*code);
    }
    let mut chars = n.chars();
    if let (Some(c), None) = (chars.next(), chars.next())
        && c.is_ascii_alphanumeric()
    {
        return Ok(c.to_ascii_uppercase() as u16);
    }
    if let Some(num) = n.strip_prefix('f')
        && !num.is_empty()
        && num.bytes().all(|b| b.is_ascii_digit())
        && let Ok(i) = num.parse::<u16>()
        && (1..=24).contains(&i)
    {
        return Ok(0x70 + i - 1);
    }
    Err(format!("unknown key '{name}'"))
}

pub fn parse_combo(combo: &str) -> R<Vec<u16>> {
    combo
        .replace(' ', "")
        .split('+')
        .filter(|k| !k.is_empty())
        .map(vk)
        .collect()
}

fn key_flags(vk: u16, up: bool) -> u32 {
    (if EXTENDED.contains(&vk) {
        KEYEVENTF_EXTENDEDKEY
    } else {
        0
    }) | (if up { KEYEVENTF_KEYUP } else { 0 })
}

/// One key's down (or, with `up`, up) event.
pub fn key_event(vk: u16, up: bool) -> KeyEv {
    KeyEv {
        vk,
        scan: 0,
        flags: key_flags(vk, up),
    }
}

/// Key-down events in order, then key-up events in reverse order.
pub fn combo_events(vks: &[u16]) -> (Vec<KeyEv>, Vec<KeyEv>) {
    let downs = vks
        .iter()
        .map(|&vk| KeyEv {
            vk,
            scan: 0,
            flags: key_flags(vk, false),
        })
        .collect();
    let ups = vks
        .iter()
        .rev()
        .map(|&vk| KeyEv {
            vk,
            scan: 0,
            flags: key_flags(vk, true),
        })
        .collect();
    (downs, ups)
}

/// The keystroke fallback of `type_text`: one SendInput batch per
/// character, each UTF-16 unit a KEYEVENTF_UNICODE down/up pair (independent
/// of the keyboard layout; a surrogate pair stays in one batch), newline and
/// tab as real keys. Sent one at a time with a pause in between: Win11
/// Notepad drops and repeats characters of a burst.
pub fn unicode_per_char(text: &str) -> Vec<Vec<KeyEv>> {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut out = Vec::new();
    for ch in text.chars() {
        let mut batch = Vec::new();
        if ch == '\n' || ch == '\t' {
            let vk = if ch == '\n' { VK_RETURN } else { VK_TAB };
            batch.push(KeyEv {
                vk,
                scan: 0,
                flags: 0,
            });
            batch.push(KeyEv {
                vk,
                scan: 0,
                flags: KEYEVENTF_KEYUP,
            });
        } else {
            let mut units = [0u16; 2];
            let units = ch.encode_utf16(&mut units);
            for &unit in units.iter() {
                batch.push(KeyEv {
                    vk: 0,
                    scan: unit,
                    flags: KEYEVENTF_UNICODE,
                });
            }
            for &unit in units.iter() {
                batch.push(KeyEv {
                    vk: 0,
                    scan: unit,
                    flags: KEYEVENTF_UNICODE | KEYEVENTF_KEYUP,
                });
            }
        }
        out.push(batch);
    }
    out
}

// ------------------------------------------------------------ clipboard

pub const CF_TEXT: u32 = 1;
pub const CF_OEMTEXT: u32 = 7;
pub const CF_UNICODETEXT: u32 = 13;
pub const CF_HDROP: u32 = 15;
/// Registered formats that only mark the contents: the password hint of
/// password managers (KeePass, Bitwarden) and the opt-outs of the clipboard
/// history (Win+V) and the cloud clipboard.
pub const HINT_EXCLUDE: &str = "ExcludeClipboardContentFromMonitorProcessing";
pub const HINT_VIEWER_IGNORE: &str = "Clipboard Viewer Ignore";
pub const HINT_HISTORY: &str = "CanIncludeInClipboardHistory";
pub const HINT_CLOUD: &str = "CanUploadToCloudClipboard";
/// Formats that hold no content of their own: those hints, and what OLE
/// adds to everything it puts on the clipboard (alone after
/// `Set-Clipboard -Value $null`).
const NOT_CONTENT: [&str; 6] = [
    HINT_EXCLUDE,
    HINT_VIEWER_IGNORE,
    HINT_HISTORY,
    HINT_CLOUD,
    "DataObject",
    "Ole Private Data",
];

/// Names of the predefined clipboard formats (registered ones have their
/// own name).
const STANDARD_FORMATS: &[(u32, &str)] = &[
    (1, "CF_TEXT"),
    (2, "CF_BITMAP"),
    (3, "CF_METAFILEPICT"),
    (4, "CF_SYLK"),
    (5, "CF_DIF"),
    (6, "CF_TIFF"),
    (7, "CF_OEMTEXT"),
    (8, "CF_DIB"),
    (9, "CF_PALETTE"),
    (10, "CF_PENDATA"),
    (11, "CF_RIFF"),
    (12, "CF_WAVE"),
    (13, "CF_UNICODETEXT"),
    (14, "CF_ENHMETAFILE"),
    (15, "CF_HDROP"),
    (16, "CF_LOCALE"),
    (17, "CF_DIBV5"),
    (0x80, "CF_OWNERDISPLAY"),
    (0x81, "CF_DSPTEXT"),
    (0x82, "CF_DSPBITMAP"),
    (0x83, "CF_DSPMETAFILEPICT"),
    (0x8E, "CF_DSPENHMETAFILE"),
];

/// The name of a predefined format; None for a registered one.
pub fn standard_format(id: u32) -> Option<&'static str> {
    STANDARD_FORMATS
        .iter()
        .find(|(i, _)| *i == id)
        .map(|(_, n)| *n)
}

/// What the clipboard holds, from its formats (ids with their names).
#[derive(Debug, PartialEq)]
pub enum ClipKind {
    Empty,
    /// Text; `secret` when marked as a password.
    Text {
        secret: bool,
    },
    /// Copied files (Explorer's copy).
    Files,
    /// Something else (an image...): the formats offered, hints left out.
    Other(Vec<String>),
}

/// The names of the formats that hold content.
pub fn content_names(formats: &[(u32, String)]) -> Vec<String> {
    formats
        .iter()
        .filter(|(_, n)| !NOT_CONTENT.contains(&n.as_str()))
        .map(|(_, n)| n.clone())
        .collect()
}

pub fn classify(formats: &[(u32, String)]) -> ClipKind {
    let has = |id: u32| formats.iter().any(|(i, _)| *i == id);
    let named = |n: &str| formats.iter().any(|(_, x)| x == n);
    let real = content_names(formats);
    if real.is_empty() {
        ClipKind::Empty
    } else if has(CF_HDROP) {
        ClipKind::Files
    } else if has(CF_UNICODETEXT) || has(CF_TEXT) || has(CF_OEMTEXT) {
        ClipKind::Text {
            secret: named(HINT_EXCLUDE) || named(HINT_VIEWER_IGNORE),
        }
    } else {
        ClipKind::Other(real)
    }
}

/// `text` as CF_UNICODETEXT: CRLF line ends, NUL-terminated UTF-16.
pub fn clipboard_units(text: &str) -> Vec<u16> {
    let text = text.replace("\r\n", "\n").replace('\n', "\r\n");
    let mut units: Vec<u16> = text.encode_utf16().collect();
    units.push(0);
    units
}

/// The UTF-16 units of CF_UNICODETEXT data, up to its NUL.
pub fn until_nul(units: &[u16]) -> &[u16] {
    let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
    &units[..end]
}

/// At most `limit` characters of clipboard text with "\n" line ends, and
/// whether any were cut.
pub fn clipboard_text(units: &[u16], limit: usize) -> (String, bool) {
    let text = unix_newlines(&String::from_utf16_lossy(until_nul(units)));
    match text.char_indices().nth(limit) {
        Some((i, _)) => (text[..i].to_string(), true),
        None => (text, false),
    }
}

/// uiautomation's ControlTypeName without "Control", lower-cased, indexed
/// by UIA_*ControlTypeId - 50000.
const CONTROL_TYPES: [&str; 41] = [
    "button",
    "calendar",
    "checkbox",
    "combobox",
    "edit",
    "hyperlink",
    "image",
    "listitem",
    "list",
    "menu",
    "menubar",
    "menuitem",
    "progressbar",
    "radiobutton",
    "scrollbar",
    "slider",
    "spinner",
    "statusbar",
    "tab",
    "tabitem",
    "text",
    "toolbar",
    "tooltip",
    "tree",
    "treeitem",
    "custom",
    "group",
    "thumb",
    "datagrid",
    "dataitem",
    "document",
    "splitbutton",
    "window",
    "pane",
    "header",
    "headeritem",
    "table",
    "titlebar",
    "separator",
    "semanticzoom",
    "appbar",
];

/// Role for a UIA control type id; None for ids uiautomation does not know.
pub fn role_name(control_type: i32) -> Option<&'static str> {
    usize::try_from(control_type - 50000)
        .ok()
        .and_then(|i| CONTROL_TYPES.get(i).copied())
}

/// `Path(image).stem.lower()`: file name without its last extension.
pub fn exe_stem(path: &str) -> String {
    let file = path.rsplit(['\\', '/']).next().unwrap_or("");
    let stem = match file.rfind('.') {
        Some(i) if i > 0 => &file[..i],
        _ => file,
    };
    stem.to_lowercase()
}

/// Text preview: the first `limit` characters, Windows line ends as "\n"
/// (tree/find print it JSON-escaped, so the lines stay visible).
pub fn preview(text: &str, limit: usize) -> String {
    unix_newlines(
        &text
            .chars()
            .take(limit.saturating_add(1))
            .collect::<String>(),
    )
    .chars()
    .take(limit)
    .collect()
}

/// TextPattern text with Windows line ends turned into "\n".
pub fn unix_newlines(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

pub const TOGGLE_ON: i32 = 1;
pub const EXPANDED: i32 = 1;
pub const PARTIALLY_EXPANDED: i32 = 2;
/// ExpandCollapseState_LeafNode: the pattern is there, but nothing opens.
pub const LEAF_NODE: i32 = 3;
/// UIA_MenuItemControlTypeId.
const MENU_ITEM: i32 = 50011;

/// Everything the Python backend reads about one element, as fetched in one
/// UIA cache request.
#[derive(Clone, Debug)]
pub struct Props {
    pub name: String,
    pub control_type: i32,
    pub offscreen: bool,
    pub focused: bool,
    pub enabled: bool,
    pub focusable: bool,
    pub hwnd: i32,
    pub pid: u32,
    pub invoke: bool,
    pub toggle: bool,
    pub expand: bool,
    pub select: bool,
    pub value: bool,
    pub text: bool,
    pub legacy: bool,
    pub toggle_state: i32,
    pub selected: bool,
    pub expand_state: i32,
    pub value_text: String,
    pub read_only: bool,
    pub default_action: String,
    /// UIA IsPassword: a password field, shown as role "password text".
    pub password: bool,
}

impl Default for Props {
    /// What Python's `_safe` defaults amount to for an unreadable element.
    fn default() -> Self {
        Props {
            name: String::new(),
            control_type: 0,
            offscreen: true,
            focused: false,
            enabled: true,
            focusable: false,
            hwnd: 0,
            pid: 0,
            invoke: false,
            toggle: false,
            expand: false,
            select: false,
            value: false,
            text: false,
            legacy: false,
            toggle_state: 0,
            selected: false,
            expand_state: 0,
            value_text: String::new(),
            read_only: true,
            default_action: String::new(),
            password: false,
        }
    }
}

impl Props {
    /// The control type's name; a password field (IsPassword) reads as
    /// AT-SPI's "password text", so its text is hidden like on Linux.
    pub fn role(&self) -> &'static str {
        match role_name(self.control_type) {
            Some("edit") if self.password => "password text",
            r => r.unwrap_or(""),
        }
    }

    fn is_text_role(&self) -> bool {
        matches!(self.role(), "edit" | "password text" | "document")
    }

    /// TextPattern must be read live for the preview (it is not cacheable).
    pub fn wants_document_text(&self) -> bool {
        !self.value && self.is_text_role() && self.text
    }

    /// Preview from the cached ValuePattern value; None when there is none.
    pub fn value_preview(&self, limit: usize) -> Option<String> {
        self.value.then(|| preview(&self.value_text, limit))
    }

    fn editable(&self) -> bool {
        if self.value {
            return !self.read_only;
        }
        // Rich edit controls (e.g. Windows 11 Notepad) expose TextPattern only.
        self.is_text_role() && self.focusable && self.enabled
    }

    /// Python `UiaNode.states()`; `foreground` is the foreground window handle.
    pub fn states(&self, foreground: i32) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.focused {
            out.push("focused");
        }
        if self.toggle && self.toggle_state == TOGGLE_ON {
            out.push("checked");
        }
        if self.select && self.selected {
            out.push("selected");
        }
        if self.expand && matches!(self.expand_state, EXPANDED | PARTIALLY_EXPANDED) {
            out.push("expanded");
        }
        if self.role() == "window" && self.hwnd != 0 && self.hwnd == foreground {
            out.push("active");
        }
        if self.editable() {
            out.push("editable");
        }
        if !self.enabled {
            out.push("disabled");
        }
        out
    }

    /// Python `_action_table` names, in order; but a menu entry that opens
    /// a submenu lists expand first, its default (as Qt's "Show Menu"):
    /// Invoke on a Win11 menu bar entry opens nothing. A leaf node has
    /// nothing to expand.
    pub fn actions(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        let expand = self.expand && self.expand_state != LEAF_NODE;
        let submenu = expand && self.control_type == MENU_ITEM;
        if submenu {
            out.push("expand");
            out.push("collapse");
        }
        if self.invoke {
            out.push("invoke");
        }
        if self.toggle {
            out.push("toggle");
        }
        if expand && !submenu {
            out.push("expand");
            out.push("collapse");
        }
        if self.select {
            out.push("select");
        }
        if !self.invoke && self.legacy && !self.default_action.is_empty() {
            out.push("default");
        }
        out
    }
}

/// Top-down 32-bit BGRA rows (GetDIBits) to packed RGB.
pub fn bgra_to_rgb(bgra: &[u8]) -> Vec<u8> {
    bgra.as_chunks::<4>()
        .0
        .iter()
        .flat_map(|p| [p[2], p[1], p[0]])
        .collect()
}

/// Python's `rect.xcenter()`/`ycenter()`: left + width // 2.
pub fn center(left: i32, top: i32, right: i32, bottom: i32) -> Option<(i32, i32)> {
    let (w, h) = (right - left, bottom - top);
    (w > 0 && h > 0).then(|| (left + w.div_euclid(2), top + h.div_euclid(2)))
}

/// Window classes of the shell itself (desktop, taskbars): explorer's, but
/// not windows to list, move or close.
const SHELL_CLASSES: [&str; 4] = [
    "Progman",
    "WorkerW",
    "Shell_TrayWnd",
    "Shell_SecondaryTrayWnd",
];

/// The frame of a Store app's window; the app runs in its CoreWindow child.
pub const STORE_FRAME_CLASS: &str = "ApplicationFrameWindow";
pub const STORE_CORE_CLASS: &str = "Windows.UI.Core.CoreWindow";
/// The process behind every Store app's frame window.
pub const STORE_FRAME_APP: &str = "applicationframehost";

/// Whether a top-level window is one `windows` lists: shown, not cloaked
/// (a suspended Store app's frame, another virtual desktop), no tool
/// window, titled, and not part of the shell.
pub fn listed_window(
    visible: bool,
    cloaked: bool,
    tool_window: bool,
    class: &str,
    title: &str,
) -> bool {
    visible
        && !cloaked
        && !tool_window
        && !title.trim().is_empty()
        && !SHELL_CLASSES.contains(&class)
}

/// Whether a window of `app` (and, for a Store app's frame, the app it
/// hosts) matches the lower-case query `q`: by title or app; "" matches all.
pub fn window_matches(q: &str, title: &str, app: &str, hosted: &str) -> bool {
    q.is_empty()
        || title.to_lowercase().contains(q)
        || app.contains(q)
        || (!hosted.is_empty() && hosted.contains(q))
}

/// Whether the allowlist lets a window through: its app, or for a Store
/// app's frame only the app it hosts (the frame's own name would get past
/// that app's lower tier).
pub fn window_allowed(allowed: &[String], app: &str, hosted: &str) -> bool {
    let name = if hosted.is_empty() { app } else { hosted };
    allowed.iter().any(|a| a == name)
}

/// The window `move_window` changes among matches in z-order: the active
/// one, else the topmost.
pub fn pick_window(active: &[bool]) -> Option<usize> {
    active
        .iter()
        .position(|a| *a)
        .or((!active.is_empty()).then_some(0))
}

/// A rectangle as Win32 has it: left, top, right, bottom.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Rect {
    pub fn width(&self) -> i32 {
        self.right - self.left
    }
    pub fn height(&self) -> i32 {
        self.bottom - self.top
    }
}

/// The invisible resize borders around a window's visible frame: how far
/// its window rectangle reaches past the frame on each side.
pub fn borders(window: Rect, frame: Rect) -> Rect {
    Rect {
        left: frame.left - window.left,
        top: frame.top - window.top,
        right: window.right - frame.right,
        bottom: window.bottom - frame.bottom,
    }
}

/// The visible frame a change asks for (x, y, width, height), the values
/// not given kept from `frame`.
pub fn wanted_frame(
    frame: Rect,
    x: Option<f64>,
    y: Option<f64>,
    width: Option<f64>,
    height: Option<f64>,
) -> (i32, i32, i32, i32) {
    let px = |v: Option<f64>, keep: i32| v.map_or(keep, |v| v.round() as i32);
    (
        px(x, frame.left),
        px(y, frame.top),
        px(width, frame.width()).max(1),
        px(height, frame.height()).max(1),
    )
}

/// `window` without its invisible `borders`: its visible frame.
pub fn inset(window: Rect, borders: Rect) -> Rect {
    Rect {
        left: window.left + borders.left,
        top: window.top + borders.top,
        right: window.right - borders.right,
        bottom: window.bottom - borders.bottom,
    }
}

/// SetWindowPos arguments (x, y, cx, cy) that give the window the visible
/// frame `(x, y, w, h)`, its invisible borders added around it.
pub fn window_pos(frame: (i32, i32, i32, i32), borders: Rect) -> (i32, i32, i32, i32) {
    let (x, y, w, h) = frame;
    (
        x - borders.left,
        y - borders.top,
        w + borders.left + borders.right,
        h + borders.top + borders.bottom,
    )
}

/// A restored-position rectangle (WINDOWPLACEMENT, workspace coordinates:
/// relative to the work area) in screen coordinates.
pub fn workspace_to_screen(r: Rect, work: Rect, monitor: Rect) -> Rect {
    let (dx, dy) = (work.left - monitor.left, work.top - monitor.top);
    Rect {
        left: r.left + dx,
        top: r.top + dy,
        right: r.right + dx,
        bottom: r.bottom + dy,
    }
}

/// A monitor's name without the device prefix: "DISPLAY1" for `\\.\DISPLAY1`.
pub fn display_name(device: &str) -> String {
    device.trim_start_matches("\\\\.\\").to_string()
}

/// Scale factor of a monitor's effective DPI (96 is 100 %).
pub fn dpi_scale(dpi: u32) -> f64 {
    if dpi == 0 {
        return 1.0;
    }
    (dpi as f64 / 96.0 * 100.0).round() / 100.0
}

/// The file `launch` looks for: `app` with ".exe" added unless it has it.
pub fn exe_file(app: &str) -> String {
    if app.to_lowercase().ends_with(".exe") {
        app.to_string()
    } else {
        format!("{app}.exe")
    }
}

/// `%NAME%` references replaced by `env(NAME)`; unknown ones are kept.
pub fn expand_env(s: &str, env: impl Fn(&str) -> Option<String>) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find('%') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        match after.find('%') {
            Some(j) if j > 0 => match env(&after[..j]) {
                Some(v) => {
                    out.push_str(&v);
                    rest = &after[j + 1..];
                }
                None => {
                    out.push('%');
                    rest = after;
                }
            },
            _ => {
                out.push('%');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// The program an App Paths entry names: its default value without the
/// quotes some installers add, environment references expanded.
pub fn app_path_value(raw: &str, env: impl Fn(&str) -> Option<String>) -> Option<String> {
    let v = raw.trim().trim_matches('"').trim();
    (!v.is_empty()).then(|| expand_env(v, env))
}

/// Where `launch` looks for a program after App Paths: PATH (including the
/// Store's execution aliases in WindowsApps), then System32 and the Windows
/// folder, each folder once.
pub fn program_dirs(
    path: &[std::path::PathBuf],
    system_root: Option<&str>,
) -> Vec<std::path::PathBuf> {
    let mut dirs: Vec<std::path::PathBuf> = path.to_vec();
    if let Some(root) = system_root.filter(|r| !r.is_empty()) {
        let root = root.trim_end_matches(['\\', '/']);
        dirs.push(format!("{root}\\System32").into());
        dirs.push(root.into());
    }
    let mut seen = std::collections::HashSet::new();
    dirs.retain(|d| {
        let key = d
            .to_string_lossy()
            .replace('/', "\\")
            .trim_end_matches('\\')
            .to_lowercase();
        !key.is_empty() && seen.insert(key)
    });
    dirs
}

/// The program to start for `file` ("notepad.exe"): the App Paths entry
/// when it exists, else the first folder of `dirs` holding it. `exists`
/// must accept the Store's 0-byte execution aliases.
pub fn find_program(
    file: &str,
    app_path: Option<String>,
    dirs: &[std::path::PathBuf],
    exists: impl Fn(&std::path::Path) -> bool,
) -> Option<std::path::PathBuf> {
    if let Some(p) = app_path.map(std::path::PathBuf::from)
        && p.extension().is_some_and(|e| e.eq_ignore_ascii_case("exe"))
        && exists(&p)
    {
        return Some(p);
    }
    dirs.iter().map(|d| d.join(file)).find(|p| exists(p))
}

/// The value of attribute `name` in an XML start tag, quoted either way.
fn xml_attr(tag: &str, name: &str) -> Option<String> {
    let mut from = 0;
    while let Some(i) = tag[from..].find(name) {
        let at = from + i;
        from = at + name.len();
        let before = tag[..at].chars().next_back();
        if !before.is_some_and(char::is_whitespace) {
            continue;
        }
        let rest = tag[at + name.len()..].trim_start();
        let Some(rest) = rest.strip_prefix('=') else {
            continue;
        };
        let rest = rest.trim_start();
        let q = rest.chars().next()?;
        if q != '"' && q != '\'' {
            continue;
        }
        let end = rest[1..].find(q)?;
        return Some(rest[1..1 + end].to_string());
    }
    None
}

/// The start tags named `name` in `xml` (`<Application ...>`), not the
/// ones whose name only begins with it (`<Applications>`).
fn xml_tags<'a>(xml: &'a str, name: &str) -> Vec<&'a str> {
    let open = format!("<{name}");
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(i) = xml[from..].find(&open) {
        let at = from + i;
        let rest = &xml[at + open.len()..];
        from = at + open.len();
        if !rest.starts_with(|c: char| c.is_whitespace() || c == '>' || c == '/') {
            continue;
        }
        let end = rest.find('>').unwrap_or(rest.len());
        out.push(&rest[..end]);
    }
    out
}

/// A package's identity name and its applications (Id, Executable) from
/// its AppxManifest.xml.
pub fn manifest_apps(xml: &str) -> (String, Vec<(String, String)>) {
    let name = xml_tags(xml, "Identity")
        .first()
        .and_then(|t| xml_attr(t, "Name"))
        .unwrap_or_default();
    let apps = xml_tags(xml, "Application")
        .into_iter()
        .filter_map(|t| {
            Some((
                xml_attr(t, "Id")?,
                xml_attr(t, "Executable").unwrap_or_default(),
            ))
        })
        .collect();
    (name, apps)
}

/// The executables (lower-case stems) of a package's apps that run inside
/// an ApplicationFrameHost frame: UWP apps listed in the Start menu, not
/// full-trust (Win32) ones that open windows of their own, nor the hidden
/// system ones (AppListEntry="none": shell parts, brokers, pickers).
pub fn manifest_hosted(xml: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut rest = xml;
    while let Some(i) = rest.find("<Application") {
        let after = &rest[i + "<Application".len()..];
        rest = after;
        if !after.starts_with(char::is_whitespace) {
            continue;
        }
        let end = [after.find("</Application>"), after.find("<Application")]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(after.len());
        let body = &after[..end];
        let tag = &body[..body.find('>').unwrap_or(body.len())];
        let win32 = xml_attr(tag, "EntryPoint").as_deref() == Some("Windows.FullTrustApplication")
            || ["packagedClassicApp", "win32App"]
                .iter()
                .any(|v| tag.contains(&format!("RuntimeBehavior=\"{v}\"")));
        let hidden = xml_tags(body, "uap:VisualElements")
            .iter()
            .chain(&xml_tags(body, "VisualElements"))
            .any(|t| xml_attr(t, "AppListEntry").as_deref() == Some("none"));
        let exe = xml_attr(tag, "Executable").map(|e| exe_stem(&e));
        if let Some(exe) = exe.filter(|e| !win32 && !hidden && !e.is_empty() && !e.contains('$'))
            && !out.contains(&exe)
        {
            out.push(exe);
        }
    }
    out
}

/// The family name of a package full name
/// (Name_Version_Arch_ResourceId_PublisherId): Name_PublisherId.
pub fn package_family(full: &str) -> Option<String> {
    let parts: Vec<&str> = full.split('_').collect();
    match parts.as_slice() {
        [name, .., publisher] if parts.len() >= 3 && !name.is_empty() && !publisher.is_empty() => {
            Some(format!("{name}_{publisher}"))
        }
        _ => None,
    }
}

/// How well a Store app matches `app`, lower is better: its executable
/// ("CalculatorApp.exe" for "calculatorapp"), then the last part of the
/// package name ("Microsoft.Paint" for "paint"), then a package name
/// holding it ("Microsoft.WindowsCalculator" for "calculator").
pub fn store_rank(app: &str, package: &str, executable: &str) -> Option<u8> {
    let app = app.to_lowercase();
    let package = package.to_lowercase();
    if !executable.is_empty() && exe_stem(executable) == app {
        Some(0)
    } else if package.rsplit('.').next() == Some(app.as_str()) {
        Some(1)
    } else if app.chars().count() >= 4 && package.contains(&app) {
        Some(2)
    } else {
        None
    }
}

/// A Store app `store_rank` matched: its rank, package name, executable
/// and AppUserModelID.
pub struct StoreMatch {
    pub rank: u8,
    pub package: String,
    pub executable: String,
    pub aumid: String,
}

/// The AppUserModelID to start for `app` among the matches: the best
/// ranked one. A match by its executable is exact; one by package name is
/// taken only when no other app matches as well, so a short name ("xbox",
/// "store") does not start whichever of several packages comes first.
pub fn pick_store(app: &str, matches: &[StoreMatch]) -> Result<Option<String>, String> {
    let Some(best) = matches.iter().map(|m| m.rank).min() else {
        return Ok(None);
    };
    let top: Vec<&StoreMatch> = matches.iter().filter(|m| m.rank == best).collect();
    if best == 0 || top.len() == 1 {
        return Ok(Some(top[0].aumid.clone()));
    }
    let names: Vec<String> = top
        .iter()
        .map(|m| match exe_stem(&m.executable) {
            e if e.is_empty() => m.package.clone(),
            e => format!("{e} ({})", m.package),
        })
        .collect();
    Err(format!(
        "'{app}' matches several Store apps: {}; launch one by its executable name",
        names.join(", ")
    ))
}

/// Arguments as one command line, quoted the way programs split it
/// (CommandLineToArgvW): words with spaces or quotes are quoted, quotes
/// and the backslashes before them escaped.
pub fn command_args(args: &[String]) -> String {
    let quote = |a: &String| {
        if !a.is_empty() && !a.contains([' ', '\t', '"']) {
            return a.clone();
        }
        let mut out = String::from('"');
        let mut slashes = 0;
        for c in a.chars() {
            match c {
                '\\' => slashes += 1,
                '"' => {
                    out.push_str(&"\\".repeat(slashes * 2 + 1));
                    slashes = 0;
                }
                _ => {
                    out.push_str(&"\\".repeat(slashes));
                    slashes = 0;
                }
            }
            if c != '\\' {
                out.push(c);
            }
        }
        out.push_str(&"\\".repeat(slashes * 2));
        out.push('"');
        out
    };
    args.iter().map(quote).collect::<Vec<_>>().join(" ")
}

/// Whether a started program's exit is a hand-off rather than a failure.
/// Starters such as System32's notepad.exe, the Store's execution aliases,
/// `explorer <folder>` (exit code 1) and `msedge <url>` pass the request to
/// another process and end; any other code is a failure.
pub fn handoff_exit(code: Option<i32>) -> bool {
    matches!(code, Some(0 | 1))
}

/// An exit code as Windows tools show it: decimal, or hex for an NTSTATUS
/// (a crash such as 0xC0000005).
pub fn exit_code(code: u32) -> String {
    if code >= 0x8000_0000 {
        format!("{code:#010X}").replace("0X", "0x")
    } else {
        code.to_string()
    }
}

/// Window class of a Win32 dialog box: the common item dialog (Open, Save
/// As) is one, in the process of the app that opened it.
pub const DIALOG_CLASS: &str = "#32770";
pub const NOT_A_FILE_DIALOG: &str = "the active window is not a file Open/Save dialog";

/// A part of the common file dialog that file_dialog uses.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DialogPart {
    /// The file name field.
    FileName,
    /// The default button (Open, Save).
    Accept,
}

/// Which part of a common file dialog an element is, by its AutomationId
/// (the Win32 control id) and window class, both the same in every
/// language: the Save dialog's file name Edit is 1001, the Open dialog's
/// 1148 (in a ComboBox of that id); the default button is IDOK (1). The
/// address bar's toolbar is also 1001 and the folder view's items reuse
/// small ids, hence the class.
pub fn dialog_part(automation_id: &str, class: &str) -> Option<DialogPart> {
    match (automation_id, class) {
        ("1001" | "1148", "Edit") => Some(DialogPart::FileName),
        ("1", "Button") => Some(DialogPart::Accept),
        _ => None,
    }
}

pub const EM_SETSEL: u32 = 0x00B1;
pub const EM_REPLACESEL: u32 = 0x00C2;
const ES_MULTILINE: u32 = 0x0004;

/// Whether a window of class `class` and style `style` is a single-line
/// Win32 Edit control. set_text writes those with EM_REPLACESEL, as a user
/// edit: the common file dialog ignores a name set with WM_SETTEXT (what
/// ValuePattern.SetValue sends) and saves under the name it had.
pub fn single_line_edit(class: &str, style: u32) -> bool {
    class.eq_ignore_ascii_case("Edit") && style & ES_MULTILINE == 0
}

/// MSAA's STATE_SYSTEM_BUSY: a browser's document still loading.
pub const STATE_BUSY: u32 = 0x800;

/// The toolkit (`Backend::toolkit_of`) of an element from its UIA
/// FrameworkId and its top-level window's class: "chromium" for Chromium
/// browsers and Electron apps, "gecko" for Firefox, else "".
pub fn web_toolkit(framework: &str, root_class: &str) -> &'static str {
    if framework.eq_ignore_ascii_case("chrome") || root_class.starts_with("Chrome_WidgetWin") {
        "chromium"
    } else if framework.eq_ignore_ascii_case("gecko") || root_class == "MozillaWindowClass" {
        "gecko"
    } else {
        ""
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_single_line_edits_get_their_text_replaced() {
        assert!(single_line_edit("Edit", 0x5001_0080));
        assert!(single_line_edit("EDIT", 0));
        assert!(!single_line_edit("Edit", 0x5001_0084));
        assert!(!single_line_edit("RichEditD2DPT", 0));
        assert!(!single_line_edit("ComboBox", 0));
    }

    #[test]
    fn file_dialog_parts_go_by_id_and_class() {
        assert_eq!(dialog_part("1001", "Edit"), Some(DialogPart::FileName));
        assert_eq!(dialog_part("1148", "Edit"), Some(DialogPart::FileName));
        assert_eq!(dialog_part("1", "Button"), Some(DialogPart::Accept));
        // The address bar, the Open dialog's ComboBox, Cancel and the
        // folder view's items are not.
        assert_eq!(dialog_part("1001", "ToolbarWindow32"), None);
        assert_eq!(dialog_part("1148", "ComboBox"), None);
        assert_eq!(dialog_part("2", "Button"), None);
        assert_eq!(dialog_part("1", "UIItem"), None);
    }

    #[test]
    fn parses_keys_like_python() {
        assert_eq!(parse_combo("ctrl+s").unwrap(), vec![0x11, b'S' as u16]);
        assert_eq!(
            parse_combo("Ctrl + Shift + F5").unwrap(),
            vec![0x11, 0x10, 0x74]
        );
        assert_eq!(parse_combo("f24").unwrap(), vec![0x87]);
        assert_eq!(parse_combo("alt+4").unwrap(), vec![0x12, b'4' as u16]);
        assert_eq!(parse_combo("pgdn").unwrap(), vec![0x22]);
        assert_eq!(parse_combo("ctrl+F25").unwrap_err(), "unknown key 'F25'");
        assert_eq!(parse_combo("f").unwrap(), vec![b'F' as u16]);
        assert_eq!(parse_combo("f0").unwrap_err(), "unknown key 'f0'");
        assert_eq!(parse_combo("ctrl+ö").unwrap_err(), "unknown key 'ö'");
        assert!(parse_combo("ctrl+nosuchkey").is_err());
    }

    #[test]
    fn combo_releases_in_reverse_with_extended_flags() {
        let (downs, ups) = combo_events(&parse_combo("ctrl+home").unwrap());
        assert_eq!(
            downs,
            vec![
                KeyEv {
                    vk: 0x11,
                    scan: 0,
                    flags: 0
                },
                KeyEv {
                    vk: 0x24,
                    scan: 0,
                    flags: KEYEVENTF_EXTENDEDKEY
                }
            ]
        );
        assert_eq!(
            ups,
            vec![
                KeyEv {
                    vk: 0x24,
                    scan: 0,
                    flags: KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP
                },
                KeyEv {
                    vk: 0x11,
                    scan: 0,
                    flags: KEYEVENTF_KEYUP
                }
            ]
        );
    }

    #[test]
    fn unicode_typing_sends_one_char_at_a_time() {
        let b = unicode_per_char("ğ😀\r\n\t");
        assert_eq!(b.len(), 4);
        assert_eq!(
            b[0],
            vec![
                KeyEv {
                    vk: 0,
                    scan: 0x011F,
                    flags: KEYEVENTF_UNICODE
                },
                KeyEv {
                    vk: 0,
                    scan: 0x011F,
                    flags: KEYEVENTF_UNICODE | KEYEVENTF_KEYUP
                }
            ]
        );
        // A surrogate pair goes down together, then up.
        let scans: Vec<(u16, u32)> = b[1].iter().map(|e| (e.scan, e.flags)).collect();
        assert_eq!(
            scans,
            vec![
                (0xD83D, KEYEVENTF_UNICODE),
                (0xDE00, KEYEVENTF_UNICODE),
                (0xD83D, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP),
                (0xDE00, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP)
            ]
        );
        assert_eq!(
            b[2],
            vec![
                KeyEv {
                    vk: VK_RETURN,
                    scan: 0,
                    flags: 0
                },
                KeyEv {
                    vk: VK_RETURN,
                    scan: 0,
                    flags: KEYEVENTF_KEYUP
                }
            ]
        );
        assert_eq!(b[3][0].vk, VK_TAB);
        assert!(unicode_per_char("").is_empty());
    }

    #[test]
    fn clipboard_formats_classify_like_linux_types() {
        let f = |v: &[(u32, &str)]| -> Vec<(u32, String)> {
            v.iter().map(|(i, n)| (*i, n.to_string())).collect()
        };
        assert_eq!(classify(&[]), ClipKind::Empty);
        // Only the history opt-outs left: nothing to read.
        assert_eq!(
            classify(&f(&[(0xC100, HINT_HISTORY), (0xC101, HINT_CLOUD)])),
            ClipKind::Empty
        );
        assert_eq!(
            classify(&f(&[
                (13, "CF_UNICODETEXT"),
                (16, "CF_LOCALE"),
                (1, "CF_TEXT")
            ])),
            ClipKind::Text { secret: false }
        );
        assert_eq!(
            classify(&f(&[(0xC0AA, HINT_EXCLUDE), (13, "CF_UNICODETEXT")])),
            ClipKind::Text { secret: true }
        );
        assert_eq!(
            classify(&f(&[(0xC0AA, HINT_VIEWER_IGNORE), (1, "CF_TEXT")])),
            ClipKind::Text { secret: true }
        );
        // Explorer's copy also offers names as text: files win.
        assert_eq!(
            classify(&f(&[
                (15, "CF_HDROP"),
                (0xC00F, "FileNameW"),
                (13, "CF_UNICODETEXT")
            ])),
            ClipKind::Files
        );
        assert_eq!(
            classify(&f(&[(8, "CF_DIB"), (0xC101, HINT_CLOUD), (17, "CF_DIBV5")])),
            ClipKind::Other(vec!["CF_DIB".into(), "CF_DIBV5".into()])
        );
        // OLE's own formats alone: emptied through OLE.
        assert_eq!(
            classify(&f(&[(0xC009, "DataObject"), (0xC013, "Ole Private Data")])),
            ClipKind::Empty
        );
        assert_eq!(
            content_names(&f(&[(0xC009, "DataObject"), (15, "CF_HDROP")])),
            vec!["CF_HDROP".to_string()]
        );
        assert_eq!(standard_format(17), Some("CF_DIBV5"));
        assert_eq!(standard_format(0xC000), None);
    }

    #[test]
    fn clipboard_text_round_trips_with_crlf() {
        let units = clipboard_units("a\nb\r\nç😀");
        assert_eq!(units.last(), Some(&0));
        assert_eq!(String::from_utf16_lossy(until_nul(&units)), "a\r\nb\r\nç😀");
        assert_eq!(clipboard_text(&units, 100), ("a\nb\nç😀".into(), false));
        assert_eq!(clipboard_text(&units, 3), ("a\nb".into(), true));
        // Data past the NUL (GlobalSize rounds up) is not text.
        assert_eq!(
            clipboard_text(&[104, 105, 0, 120, 120], 10),
            ("hi".into(), false)
        );
        assert_eq!(clipboard_text(&[], 10), (String::new(), false));
    }

    #[test]
    fn roles_match_uiautomation_names() {
        assert_eq!(role_name(50000), Some("button"));
        assert_eq!(role_name(50004), Some("edit"));
        assert_eq!(role_name(50011), Some("menuitem"));
        assert_eq!(role_name(50030), Some("document"));
        assert_eq!(role_name(50032), Some("window"));
        assert_eq!(role_name(50033), Some("pane"));
        assert_eq!(role_name(50040), Some("appbar"));
        assert_eq!(role_name(50041), None);
        assert_eq!(role_name(0), None);
    }

    #[test]
    fn exe_stem_matches_pathlib() {
        assert_eq!(exe_stem(r"C:\Windows\System32\Notepad.EXE"), "notepad");
        assert_eq!(
            exe_stem(r"C:\Program Files\Mozilla Firefox\firefox.exe"),
            "firefox"
        );
        assert_eq!(exe_stem(r"C:\x\archive.tar.exe"), "archive.tar");
        assert_eq!(exe_stem(r"C:\x\noext"), "noext");
        assert_eq!(exe_stem(""), "");
    }

    #[test]
    fn previews_are_single_line() {
        assert_eq!(preview("a\r\nb\nc\rd", 80), "a\nb\nc\nd");
        assert_eq!(preview("a\r\nb", 2), "a\n");
        assert_eq!(preview("çğüşöı", 3), "çğü");
        assert_eq!(unix_newlines("a\r\nb\rc\n"), "a\nb\nc\n");
    }

    fn edit() -> Props {
        Props {
            control_type: 50004,
            offscreen: false,
            focusable: true,
            text: true,
            ..Default::default()
        }
    }

    #[test]
    fn states_follow_python_order() {
        let mut p = edit();
        p.focused = true;
        assert_eq!(p.states(0), vec!["focused", "editable"]);
        p.enabled = false;
        assert_eq!(p.states(0), vec!["focused", "disabled"]);

        let mut w = Props {
            control_type: 50032,
            hwnd: 42,
            ..Default::default()
        };
        assert_eq!(w.states(42), vec!["active"]);
        assert!(w.states(7).is_empty());
        w.hwnd = 0;
        assert!(w.states(0).is_empty());

        let mut c = Props {
            control_type: 50002,
            toggle: true,
            toggle_state: TOGGLE_ON,
            select: true,
            selected: true,
            expand: true,
            expand_state: PARTIALLY_EXPANDED,
            value: true,
            read_only: false,
            ..Default::default()
        };
        assert_eq!(
            c.states(0),
            vec!["checked", "selected", "expanded", "editable"]
        );
        c.toggle = false;
        c.read_only = true;
        assert_eq!(c.states(0), vec!["selected", "expanded"]);
    }

    #[test]
    fn password_edits_are_password_text() {
        let mut p = edit();
        p.password = true;
        assert_eq!(p.role(), "password text");
        assert_eq!(p.states(0), vec!["editable"]);
        let doc = Props {
            control_type: 50030,
            password: true,
            ..Default::default()
        };
        assert_eq!(doc.role(), "document");
    }

    #[test]
    fn value_pattern_wins_over_text_pattern() {
        let mut p = edit();
        assert!(p.wants_document_text());
        assert_eq!(p.value_preview(80), None);
        p.value = true;
        p.value_text = "x\ny".into();
        assert!(!p.wants_document_text());
        assert_eq!(p.value_preview(80).as_deref(), Some("x\ny"));
        let label = Props {
            control_type: 50020,
            text: true,
            ..Default::default()
        };
        assert!(!label.wants_document_text());
    }

    #[test]
    fn actions_follow_python_table() {
        let p = Props {
            invoke: true,
            toggle: true,
            expand: true,
            select: true,
            legacy: true,
            default_action: "Press".into(),
            ..Default::default()
        };
        assert_eq!(
            p.actions(),
            vec!["invoke", "toggle", "expand", "collapse", "select"]
        );
        let p = Props {
            legacy: true,
            default_action: "Press".into(),
            ..Default::default()
        };
        assert_eq!(p.actions(), vec!["default"]);
        let p = Props {
            legacy: true,
            ..Default::default()
        };
        assert!(p.actions().is_empty());
        // A menu entry opening a submenu expands by default; a leaf node
        // has nothing to expand.
        let entry = Props {
            control_type: 50011,
            invoke: true,
            expand: true,
            ..Default::default()
        };
        assert_eq!(entry.actions(), vec!["expand", "collapse", "invoke"]);
        let leaf = Props {
            expand_state: LEAF_NODE,
            ..entry.clone()
        };
        assert_eq!(leaf.actions(), vec!["invoke"]);
        let button = Props {
            control_type: 50000,
            ..entry
        };
        assert_eq!(button.actions(), vec!["invoke", "expand", "collapse"]);
    }

    #[test]
    fn center_floors_like_python() {
        assert_eq!(center(0, 0, 5, 3), Some((2, 1)));
        assert_eq!(center(-10, -10, -5, -7), Some((-8, -9)));
        assert_eq!(center(0, 0, 0, 3), None);
    }

    #[test]
    fn bgra_to_rgb_drops_alpha() {
        assert_eq!(
            bgra_to_rgb(&[1, 2, 3, 255, 4, 5, 6, 0]),
            vec![3, 2, 1, 6, 5, 4]
        );
    }

    #[test]
    fn shell_and_hidden_windows_are_not_listed() {
        assert!(listed_window(true, false, false, "Notepad", "a - Notepad"));
        assert!(!listed_window(
            true,
            false,
            false,
            "Progman",
            "Program Manager"
        ));
        assert!(!listed_window(true, false, false, "Shell_TrayWnd", "x"));
        assert!(!listed_window(
            true,
            true,
            false,
            "ApplicationFrameWindow",
            "Calc"
        ));
        assert!(!listed_window(false, false, false, "Notepad", "a"));
        assert!(!listed_window(true, false, true, "Notepad", "a"));
        assert!(!listed_window(true, false, false, "Notepad", "  "));
    }

    #[test]
    fn windows_match_title_app_or_hosted_app() {
        assert!(window_matches("", "x", "notepad", ""));
        assert!(window_matches(
            "hesap",
            "Hesap Makinesi",
            "applicationframehost",
            ""
        ));
        assert!(window_matches(
            "calc",
            "Hesap",
            "applicationframehost",
            "calculatorapp"
        ));
        assert!(!window_matches("calc", "Hesap", "applicationframehost", ""));
        let allowed = vec!["calculatorapp".to_string()];
        assert!(window_allowed(
            &allowed,
            "applicationframehost",
            "calculatorapp"
        ));
        assert!(!window_allowed(&allowed, "applicationframehost", ""));
        assert!(!window_allowed(&[String::new()], "notepad", ""));
        // The frame's name reaches only frames that host no app.
        let frame = vec!["applicationframehost".to_string()];
        assert!(!window_allowed(
            &frame,
            "applicationframehost",
            "calculatorapp"
        ));
        assert!(window_allowed(&frame, "applicationframehost", ""));
    }

    #[test]
    fn the_active_match_is_picked_else_the_topmost() {
        assert_eq!(pick_window(&[false, true]), Some(1));
        assert_eq!(pick_window(&[false, false]), Some(0));
        assert_eq!(pick_window(&[]), None);
    }

    #[test]
    fn invisible_borders_are_added_to_the_frame_asked_for() {
        let r = |left, top, right, bottom| Rect {
            left,
            top,
            right,
            bottom,
        };
        let window = r(93, 80, 907, 587);
        let frame = r(100, 80, 900, 580);
        let b = borders(window, frame);
        assert_eq!(b, r(7, 0, 7, 7));
        let want = wanted_frame(frame, Some(200.0), None, None, Some(300.0));
        assert_eq!(want, (200, 80, 800, 300));
        assert_eq!(window_pos(want, b), (193, 80, 814, 307));
        assert_eq!(inset(window, b), frame);
        assert_eq!(wanted_frame(frame, None, None, Some(0.0), None).2, 1);
    }

    #[test]
    fn workspace_rects_move_by_the_work_area_offset() {
        let r = |left, top, right, bottom| Rect {
            left,
            top,
            right,
            bottom,
        };
        let mon = r(0, 0, 1920, 1080);
        let work = r(0, 40, 1920, 1080); // taskbar on top
        assert_eq!(
            workspace_to_screen(r(10, 10, 110, 60), work, mon),
            r(10, 50, 110, 100)
        );
    }

    #[test]
    fn monitors_are_named_and_scaled() {
        assert_eq!(display_name("\\\\.\\DISPLAY1"), "DISPLAY1");
        assert_eq!(display_name("DISPLAY2"), "DISPLAY2");
        assert_eq!(dpi_scale(96), 1.0);
        assert_eq!(dpi_scale(144), 1.5);
        assert_eq!(dpi_scale(120), 1.25);
        assert_eq!(dpi_scale(0), 1.0);
    }
    #[test]
    fn launch_finds_app_paths_then_path_then_the_windows_folders() {
        use std::path::{Path, PathBuf};
        assert_eq!(exe_file("notepad"), "notepad.exe");
        assert_eq!(exe_file("Foo.EXE"), "Foo.EXE");
        let env = |k: &str| match k {
            "ProgramFiles" => Some(r"C:\Program Files".to_string()),
            _ => None,
        };
        assert_eq!(
            app_path_value(r#" "%ProgramFiles%\Edge\msedge.exe" "#, env).as_deref(),
            Some(r"C:\Program Files\Edge\msedge.exe")
        );
        assert_eq!(app_path_value("\"\"", env), None);
        assert_eq!(expand_env("%NOPE%\\a%%b%", env), "%NOPE%\\a%%b%");
        assert_eq!(expand_env("x%ProgramFiles%y", env), r"xC:\Program Filesy");
        let path = [
            PathBuf::from(r"C:\WINDOWS\system32"),
            PathBuf::from(r"C:\Users\a\AppData\Local\Microsoft\WindowsApps"),
            PathBuf::from(""),
        ];
        let dirs = program_dirs(&path, Some(r"C:\WINDOWS"));
        // System32 is on PATH already (other case): listed once.
        assert_eq!(
            dirs,
            [
                PathBuf::from(r"C:\WINDOWS\system32"),
                PathBuf::from(r"C:\Users\a\AppData\Local\Microsoft\WindowsApps"),
                PathBuf::from(r"C:\WINDOWS"),
            ]
        );
        let have = |p: &Path| {
            let s = p.to_string_lossy();
            s.ends_with("WindowsApps/mspaint.exe")
                || s.ends_with("WindowsApps\\mspaint.exe")
                || s.contains("Edge")
        };
        let edge = Some(r"C:\Program Files\Edge\msedge.exe".to_string());
        assert_eq!(
            find_program("msedge.exe", edge, &dirs, have),
            Some(PathBuf::from(r"C:\Program Files\Edge\msedge.exe"))
        );
        // An App Paths entry naming a missing file or a script is skipped.
        let script = Some(r"C:\Program Files\Edge\run.cmd".to_string());
        assert_eq!(find_program("x.exe", script, &dirs, have), None);
        let paint = find_program("mspaint.exe", None, &dirs, have).unwrap();
        assert!(paint.starts_with(&dirs[1]), "{paint:?}");
        assert_eq!(find_program("nosuch.exe", None, &dirs, have), None);
    }

    #[test]
    fn store_apps_come_from_their_manifest() {
        let xml = r#"<?xml version="1.0"?><Package><Identity Name="Microsoft.WindowsCalculator"
            Publisher="CN=Microsoft" Version="11.2607.0.0" ProcessorArchitecture="x64"/>
            <Applications>
              <Application Id="App" Executable="CalculatorApp.exe" EntryPoint="Calculator.App"
                desktop4:SupportsMultipleInstances="true">
              </Application>
              <Application Id='Second' Executable='Tools\Other.exe'/>
            </Applications></Package>"#;
        let (name, apps) = manifest_apps(xml);
        assert_eq!(name, "Microsoft.WindowsCalculator");
        assert_eq!(
            apps,
            [
                ("App".to_string(), "CalculatorApp.exe".to_string()),
                ("Second".to_string(), r"Tools\Other.exe".to_string())
            ]
        );
        assert_eq!(manifest_apps("<Package/>"), (String::new(), vec![]));
    }

    #[test]
    fn hosted_apps_are_the_uwp_ones() {
        let xml = r#"<Applications>
            <Application Id="App" Executable="CalculatorApp.exe" EntryPoint="Calculator.App">
              <uap:VisualElements DisplayName="Calc" AppListEntry="default"/>
            </Application>
            <Application Id="N" Executable="Notepad\Notepad.exe"
                EntryPoint="Windows.FullTrustApplication"></Application>
            <Application Id="P" Executable="P.exe" uap10:RuntimeBehavior="packagedClassicApp">
            </Application>
            <Application Id="Again" Executable="calculatorapp.exe" EntryPoint="X"/>
            <Application Id="NoExe" EntryPoint="Y"></Application>
            <Application Id="Hidden" Executable="Broker.exe" EntryPoint="Z">
              <uap:VisualElements AppListEntry="none" DisplayName="B"/>
            </Application>
            <Application Id="T" Executable="$targetnametoken$.exe" EntryPoint="Z"/>
        </Applications>"#;
        assert_eq!(manifest_hosted(xml), vec!["calculatorapp"]);
        assert_eq!(
            package_family("Microsoft.WindowsCalculator_11.2607.0.0_x64__8wekyb3d8bbwe").as_deref(),
            Some("Microsoft.WindowsCalculator_8wekyb3d8bbwe")
        );
        assert_eq!(package_family("NoUnderscore"), None);
        let calc = "Microsoft.WindowsCalculator";
        assert_eq!(
            store_rank("calculatorapp", calc, "CalculatorApp.exe"),
            Some(0)
        );
        assert_eq!(
            store_rank("mspaint", "Microsoft.Paint", r"PaintApp\mspaint.exe"),
            Some(0)
        );
        assert_eq!(store_rank("paint", "Microsoft.Paint", "x.exe"), Some(1));
        assert_eq!(store_rank("calculator", calc, ""), Some(2));
        assert_eq!(store_rank("calc", calc, "CalculatorApp.exe"), Some(2));
        assert_eq!(store_rank("app", calc, "CalculatorApp.exe"), None);
        assert_eq!(store_rank("paint", calc, "CalculatorApp.exe"), None);
    }

    #[test]
    fn a_store_app_matched_by_package_name_must_be_the_only_one() {
        let m = |rank, package: &str, executable: &str, aumid: &str| StoreMatch {
            rank,
            package: package.into(),
            executable: executable.into(),
            aumid: aumid.into(),
        };
        assert_eq!(pick_store("x", &[]), Ok(None));
        let calc = [m(
            2,
            "Microsoft.WindowsCalculator",
            "CalculatorApp.exe",
            "C!App",
        )];
        assert_eq!(pick_store("calculator", &calc), Ok(Some("C!App".into())));
        let xbox = [
            m(2, "Microsoft.Xbox.TCUI", "TCUI-App.exe", "T!App"),
            m(2, "Microsoft.XboxGamingOverlay", "GameBar.exe", "G!App"),
        ];
        let e = pick_store("xbox", &xbox).unwrap_err();
        assert!(e.contains("several Store apps"), "{e}");
        assert!(e.contains("gamebar (Microsoft.XboxGamingOverlay)"), "{e}");
        // A better rank decides; an exact executable always does.
        let mixed = [
            m(2, "Microsoft.Xbox.TCUI", "TCUI-App.exe", "T!App"),
            m(1, "Microsoft.Xbox", "", "X!App"),
        ];
        assert_eq!(pick_store("xbox", &mixed), Ok(Some("X!App".into())));
        let exact = [
            m(0, "A", "gamebar.exe", "A!1"),
            m(0, "B", "gamebar.exe", "B!1"),
        ];
        assert_eq!(pick_store("gamebar", &exact), Ok(Some("A!1".into())));
    }

    #[test]
    fn launch_args_are_quoted_for_the_command_line() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(
            command_args(&s(&[r"C:\mcp\a.txt", "-x"])),
            r"C:\mcp\a.txt -x"
        );
        assert_eq!(
            command_args(&s(&[r"C:\My Files\", "", r#"say "hi""#, r"a\\"])),
            r#""C:\My Files\\" "" "say \"hi\"" a\\"#
        );
        assert_eq!(command_args(&s(&[r#"a\"b"#])), r#""a\\\"b""#);
    }

    #[test]
    fn quick_exits_with_0_or_1_are_hand_offs() {
        assert!(handoff_exit(Some(0)));
        // explorer.exe <folder> exits with 1 after the shell opened it.
        assert!(handoff_exit(Some(1)));
        assert!(!handoff_exit(Some(2)));
        assert!(!handoff_exit(Some(-1073741819)));
        assert!(!handoff_exit(None));
        assert_eq!(exit_code(2), "2");
        assert_eq!(exit_code(0xC000_0005), "0xC0000005");
    }

    #[test]
    fn web_toolkit_by_framework_or_window_class() {
        assert_eq!(web_toolkit("Chrome", "Chrome_WidgetWin_1"), "chromium");
        // The top-level window says Win32; its class says Chromium.
        assert_eq!(web_toolkit("Win32", "Chrome_WidgetWin_1"), "chromium");
        assert_eq!(web_toolkit("Gecko", ""), "gecko");
        assert_eq!(web_toolkit("Win32", "MozillaWindowClass"), "gecko");
        assert_eq!(web_toolkit("XAML", "ApplicationFrameWindow"), "");
        assert_eq!(web_toolkit("Win32", "Notepad"), "");
    }
}
