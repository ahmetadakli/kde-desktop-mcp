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

    /// Python `_action_table` names, in order.
    pub fn actions(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.invoke {
            out.push("invoke");
        }
        if self.toggle {
            out.push("toggle");
        }
        if self.expand {
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
