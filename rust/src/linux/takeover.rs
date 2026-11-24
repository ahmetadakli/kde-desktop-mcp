//! Feeds `crate::takeover::Takeover` on Linux.
//!
//! User input: the real keyboards and pointers under /dev/input, read-only
//! and never grabbed (other readers, the compositor included, see every
//! event as before); the server's own uinput devices (desktop-mcp-*) are
//! left out. Reading them needs the `input` group. Without it, the
//! compositor's idle notifications (ext-idle-notify-v1, version 2: input
//! only, inhibitors ignored) say when there was input, ours included.
//!
//! Stop shortcut: on KDE, a KGlobalAccel shortcut (component "desktop-mcp",
//! listed in System Settings > Shortcuts, where it can be changed); with
//! /dev/input readable, also the chord pressed on a real keyboard (the only
//! way on Hyprland).

use std::collections::HashSet;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use evdev::{
    AbsoluteAxisCode, Device as EvDevice, EventSummary, KeyCode, PropType, RelativeAxisCode,
};

use super::input;
use crate::takeover::{Device, IDLE_TIMEOUT, Takeover, pretty_key};

/// How often /dev/input is scanned for new devices (a plugged-in mouse).
const RESCAN: Duration = Duration::from_secs(3);

/// Start watching; `kde` also registers the KGlobalAccel shortcut. Needs a
/// tokio runtime (the KGlobalAccel listener is a task).
pub fn start(t: Arc<Takeover>, kde: bool, session: zbus::Connection) {
    let chord = t.stop_key().and_then(|k| Chord::parse(k).ok());
    let seen = Arc::new(Mutex::new(HashSet::new()));
    let first = scan(&t, chord.as_ref(), &seen);
    if first.readable > 0 {
        t.set_activity_source(
            true,
            format!("/dev/input (evdev, read-only; {} devices)", first.readable),
        );
        let (t2, chord2) = (t.clone(), chord.clone());
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(RESCAN);
                scan(&t2, chord2.as_ref(), &seen);
            }
        });
    } else {
        let why = first.why_not();
        let t2 = t.clone();
        std::thread::spawn(move || {
            if let Err(e) = idle_watch(&t2, &why) {
                t2.set_activity_source(false, format!("{why}; and {e}"));
            }
        });
    }
    match (t.stop_key(), kde) {
        (None, _) => {}
        (Some(_), true) => {
            let (t2, evdev) = (t.clone(), first.keyboards > 0 && chord.is_some());
            tokio::spawn(async move {
                if let Err(e) = kglobalaccel(&t2, &session, evdev).await {
                    match evdev {
                        true => t2.set_stop_source(
                            true,
                            format!("the keyboards in /dev/input (KGlobalAccel: {e})"),
                        ),
                        false => t2.set_stop_source(false, format!("KGlobalAccel: {e}")),
                    }
                }
            });
        }
        (Some(k), false) => match (&chord, first.keyboards) {
            (Some(_), n) if n > 0 => t.set_stop_source(true, "the keyboards in /dev/input".into()),
            (None, _) => t.set_stop_source(
                false,
                format!("'{k}' is not a key chord (see the key tool for names)"),
            ),
            _ => t.set_stop_source(
                false,
                format!("needs read access to /dev/input ({})", first.why_not()),
            ),
        },
    }
}

// ------------------------------------------------------------------ evdev

/// The stop chord as evdev keys: modifiers (left variants) and the key.
#[derive(Clone, Debug, PartialEq)]
struct Chord {
    mods: Vec<KeyCode>,
    key: KeyCode,
}

/// Right-hand modifiers count as the left ones.
fn left(k: KeyCode) -> KeyCode {
    match k {
        KeyCode::KEY_RIGHTMETA => KeyCode::KEY_LEFTMETA,
        KeyCode::KEY_RIGHTALT => KeyCode::KEY_LEFTALT,
        KeyCode::KEY_RIGHTCTRL => KeyCode::KEY_LEFTCTRL,
        KeyCode::KEY_RIGHTSHIFT => KeyCode::KEY_LEFTSHIFT,
        k => k,
    }
}

impl Chord {
    fn parse(combo: &str) -> Result<Chord, String> {
        let mut keys = input::parse_combo(combo)?;
        let key = keys.pop().ok_or("empty chord")?;
        Ok(Chord {
            mods: keys.into_iter().map(left).collect(),
            key,
        })
    }

    /// `code` went down while `held` (left variants) were held.
    fn hit(&self, code: KeyCode, held: &HashSet<KeyCode>) -> bool {
        code == self.key && self.mods.iter().all(|m| held.contains(m))
    }
}

#[derive(Default)]
struct Scan {
    readable: usize,
    keyboards: usize,
    /// Real input devices that could not be opened, and the last error.
    denied: usize,
    error: Option<String>,
}

impl Scan {
    fn why_not(&self) -> String {
        match &self.error {
            Some(e) if self.denied > 0 => format!(
                "/dev/input not readable ({e}; add the user to the 'input' group for \
                 exact detection and the stop chord everywhere)"
            ),
            _ => "no keyboard or pointer found in /dev/input".into(),
        }
    }
}

/// What a device is to us: a real keyboard and/or pointer, or nothing.
fn kinds(dev: &EvDevice) -> (bool, bool) {
    if dev.name().unwrap_or("").starts_with("desktop-mcp") {
        return (false, false);
    }
    let keys = dev.supported_keys();
    let has = |k| keys.is_some_and(|s| s.contains(k));
    let keyboard = has(KeyCode::KEY_A) && has(KeyCode::KEY_ESC);
    let rel = dev
        .supported_relative_axes()
        .is_some_and(|r| r.contains(RelativeAxisCode::REL_X));
    let abs = dev
        .supported_absolute_axes()
        .is_some_and(|a| a.contains(AbsoluteAxisCode::ABS_X));
    let pointer = (rel || abs)
        && [
            KeyCode::BTN_LEFT,
            KeyCode::BTN_TOUCH,
            KeyCode::BTN_TOOL_FINGER,
            KeyCode::BTN_TOOL_PEN,
        ]
        .into_iter()
        .any(has);
    (keyboard, pointer)
}

/// Open the event devices not looked at yet and start a reader for each
/// real keyboard or pointer; counts what this scan found.
fn scan(t: &Arc<Takeover>, chord: Option<&Chord>, seen: &Arc<Mutex<HashSet<PathBuf>>>) -> Scan {
    let mut out = Scan::default();
    let Ok(dir) = std::fs::read_dir("/dev/input") else {
        out.error = Some("/dev/input missing".into());
        return out;
    };
    let mut paths: Vec<PathBuf> = dir
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("event"))
        })
        .collect();
    paths.sort();
    // Forget devices that went away, so a reused node is looked at again.
    seen.lock().unwrap().retain(|p| paths.contains(p));
    for path in paths {
        // Every node is looked at once: watched, irrelevant or not readable.
        if !seen.lock().unwrap().insert(path.clone()) {
            continue;
        }
        let dev = match open(&path) {
            Ok(d) => d,
            Err(e) => {
                out.denied += 1;
                out.error = Some(format!("{}: {e}", path.display()));
                continue;
            }
        };
        let (keyboard, pointer) = kinds(&dev);
        if !keyboard && !pointer {
            continue;
        }
        out.readable += 1;
        out.keyboards += usize::from(keyboard);
        let (t, chord, seen) = (t.clone(), chord.cloned(), seen.clone());
        std::thread::spawn(move || {
            read(dev, &t, chord.as_ref());
            // Unplugged (or failing): a later scan may open it again.
            seen.lock().unwrap().remove(&path);
        });
    }
    out
}

/// Read-only: the device is never grabbed and nothing is written to it.
fn open(path: &Path) -> std::io::Result<EvDevice> {
    let file = std::fs::File::open(path)?;
    EvDevice::from_fd(OwnedFd::from(file))
}

/// Note every key, button and motion event of `dev` as the user's, and
/// stop on the chord. Returns when the device goes away.
fn read(mut dev: EvDevice, t: &Takeover, chord: Option<&Chord>) {
    let mut sort = Sorter::new(&dev);
    loop {
        let Ok(events) = dev.fetch_events() else {
            return;
        };
        let now = Instant::now();
        for e in events {
            let e = e.destructure();
            if let EventSummary::Key(_, code, 1) = e
                && chord.is_some_and(|c| c.hit(code, &sort.held))
            {
                t.stop();
            }
            sort.feed(&e, now);
        }
        match sort.take() {
            Some((_, false)) => t.user_motion(now),
            Some((k, true)) => t.user_input(now, k),
            None => {}
        }
    }
}

/// The longest touch of a touchpad that may be a tap (a click: libinput
/// sends no button for it).
const TAP_MAX: Duration = Duration::from_millis(300);

/// Tells a device's plain pointer motion from input that may change what
/// apps show (`Takeover::user_motion`, `Takeover::user_input`): keys,
/// buttons, wheels, a drag, a touchpad tap or multi-finger scroll, any
/// touch of a touchscreen.
struct Sorter {
    /// A touchpad: one finger on it only moves the pointer.
    touchpad: bool,
    /// A touchscreen or tablet surface: touching it presses.
    direct: bool,
    /// Keys and buttons held now (left variants).
    held: HashSet<KeyCode>,
    /// When the finger now on the touchpad touched it.
    touched: Option<Instant>,
    /// What the events fed since the last `take` were.
    seen: Option<(Device, bool)>,
}

impl Sorter {
    fn new(dev: &EvDevice) -> Self {
        let keys = dev.supported_keys();
        let has = |k| keys.is_some_and(|s| s.contains(k));
        let direct = dev.properties().contains(PropType::DIRECT);
        Sorter::of(
            !direct && has(KeyCode::BTN_TOOL_FINGER) && !has(KeyCode::BTN_TOOL_PEN),
            direct,
        )
    }

    fn of(touchpad: bool, direct: bool) -> Self {
        Sorter {
            touchpad,
            direct,
            held: HashSet::new(),
            touched: None,
            seen: None,
        }
    }

    /// Whether holding `code` turns motion into more than motion (a drag,
    /// a multi-finger scroll): all but a tool near the surface (pen,
    /// finger: 0x140..=0x147) and a touchpad's one-finger touch.
    fn drags(&self, code: KeyCode) -> bool {
        !(0x140..=0x147).contains(&code.0) && !(self.touchpad && code == KeyCode::BTN_TOUCH)
    }

    fn note(&mut self, dev: Device, view: bool) {
        let (d, v) = self.seen.get_or_insert((dev, false));
        if dev == Device::Keyboard {
            *d = dev;
        }
        *v |= view;
    }

    fn feed(&mut self, e: &EventSummary, now: Instant) {
        match *e {
            EventSummary::Key(_, code, value) => {
                // Below BTN_MISC: keyboard keys; above: buttons.
                if code.0 < 0x100 {
                    self.note(Device::Keyboard, true);
                } else {
                    let view = if self.touchpad && code == KeyCode::BTN_TOUCH {
                        // A short touch may be a tap: a click.
                        match value {
                            1 => {
                                self.touched = Some(now);
                                false
                            }
                            0 => self
                                .touched
                                .take()
                                .is_some_and(|t| now.duration_since(t) <= TAP_MAX),
                            _ => false,
                        }
                    } else {
                        self.drags(code) || self.direct
                    };
                    self.note(Device::Pointer, view);
                }
                match value {
                    0 => {
                        self.held.remove(&left(code));
                    }
                    1 => {
                        self.held.insert(left(code));
                    }
                    _ => {}
                }
            }
            EventSummary::RelativeAxis(_, axis, _) => {
                let motion = matches!(axis, RelativeAxisCode::REL_X | RelativeAxisCode::REL_Y);
                let drag = self.held.iter().any(|&k| self.drags(k));
                self.note(Device::Pointer, !motion || drag);
            }
            EventSummary::AbsoluteAxis(..) => {
                let drag = self.held.iter().any(|&k| self.drags(k));
                self.note(Device::Pointer, self.direct || drag);
            }
            _ => {}
        }
    }

    /// What the events fed since the last call were: the device, and
    /// whether they may have changed what apps show. None: none counted.
    fn take(&mut self) -> Option<(Device, bool)> {
        self.seen.take()
    }
}

// ------------------------------------------------------- idle notifications

mod idle {
    use smithay_client_toolkit::reexports::client::{
        Connection, Dispatch, QueueHandle,
        globals::GlobalListContents,
        protocol::{wl_registry, wl_seat},
    };
    use smithay_client_toolkit::reexports::protocols::ext::idle_notify::v1::client::{
        ext_idle_notification_v1::{self, ExtIdleNotificationV1},
        ext_idle_notifier_v1::{self, ExtIdleNotifierV1},
    };
    use std::sync::Arc;
    use std::time::Instant;

    use crate::takeover::Takeover;

    pub struct Watch(pub Arc<Takeover>);

    impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Watch {
        fn event(
            _: &mut Self,
            _: &wl_registry::WlRegistry,
            _: wl_registry::Event,
            _: &GlobalListContents,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    impl Dispatch<wl_seat::WlSeat, ()> for Watch {
        fn event(
            _: &mut Self,
            _: &wl_seat::WlSeat,
            _: wl_seat::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    impl Dispatch<ExtIdleNotifierV1, ()> for Watch {
        fn event(
            _: &mut Self,
            _: &ExtIdleNotifierV1,
            _: ext_idle_notifier_v1::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    impl Dispatch<ExtIdleNotificationV1, ()> for Watch {
        fn event(
            state: &mut Self,
            _: &ExtIdleNotificationV1,
            event: ext_idle_notification_v1::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            match event {
                ext_idle_notification_v1::Event::Idled => state.0.idle_idled(Instant::now()),
                ext_idle_notification_v1::Event::Resumed => state.0.idle_resumed(Instant::now()),
                _ => {}
            }
        }
    }
}

/// Follow the compositor's input idle notifications until the connection
/// ends. `why` says why evdev is not used, for `doctor`.
fn idle_watch(t: &Arc<Takeover>, why: &str) -> Result<(), String> {
    use smithay_client_toolkit::reexports::client::{
        Connection, globals::registry_queue_init, protocol::wl_seat::WlSeat,
    };
    use smithay_client_toolkit::reexports::protocols::ext::idle_notify::v1::client::ext_idle_notifier_v1::ExtIdleNotifierV1;
    let conn = Connection::connect_to_env().map_err(|e| format!("Wayland: {e}"))?;
    let (globals, mut queue) =
        registry_queue_init::<idle::Watch>(&conn).map_err(|e| format!("Wayland: {e}"))?;
    let qh = queue.handle();
    let seat: WlSeat = globals
        .bind(&qh, 1..=1, ())
        .map_err(|e| format!("no Wayland seat: {e}"))?;
    // Version 2: input only, whatever inhibits idling (a playing video).
    let notifier: ExtIdleNotifierV1 = globals
        .bind(&qh, 2..=2, ())
        .map_err(|e| format!("no ext-idle-notify-v1 version 2: {e}"))?;
    let _notification =
        notifier.get_input_idle_notification(IDLE_TIMEOUT.as_millis() as u32, &seat, &qh, ());
    t.set_activity_source(
        true,
        format!(
            "the compositor's idle notifications (coarse: user input during the \
             agent's own input goes unseen; {why})"
        ),
    );
    let mut state = idle::Watch(t.clone());
    loop {
        queue
            .blocking_dispatch(&mut state)
            .map_err(|e| format!("Wayland: {e}"))?;
    }
}

// ------------------------------------------------------------ KGlobalAccel

/// Qt key code of a chord ("meta+alt+esc" -> Qt::META|Qt::ALT|Key_Escape),
/// for the keys a stop shortcut would use.
fn qt_key(combo: &str) -> Result<i32, String> {
    let parts: Vec<String> = combo
        .split('+')
        .map(|p| p.trim().to_lowercase())
        .filter(|p| !p.is_empty())
        .collect();
    let (key, mods) = parts.split_last().ok_or("empty chord")?;
    let mut code: i32 = 0;
    for m in mods {
        code |= match m.as_str() {
            "shift" => 0x0200_0000,
            "ctrl" | "control" => 0x0400_0000,
            "alt" => 0x0800_0000,
            "meta" | "super" | "win" => 0x1000_0000,
            _ => return Err(format!("unknown modifier '{m}'")),
        };
    }
    let k = key.as_str();
    let base = match k {
        "esc" | "escape" => 0x0100_0000,
        "tab" => 0x0100_0001,
        "backspace" => 0x0100_0003,
        "enter" | "return" => 0x0100_0004,
        "pause" => 0x0100_0008,
        "scrolllock" => 0x0100_0026,
        "space" => 0x20,
        _ if k.len() == 1 && k.chars().all(|c| c.is_ascii_alphanumeric()) => {
            k.to_ascii_uppercase().as_bytes()[0] as i32
        }
        _ => match k.strip_prefix('f').and_then(|n| n.parse::<i32>().ok()) {
            Some(n @ 1..=24) => 0x0100_0030 + n - 1,
            _ => return Err(format!("'{key}' cannot be a global shortcut key here")),
        },
    };
    Ok(code | base)
}

/// Register the stop shortcut with KGlobalAccel and stop on each press,
/// for as long as the session bus connection lasts.
async fn kglobalaccel(t: &Takeover, conn: &zbus::Connection, evdev: bool) -> Result<(), String> {
    use futures_util::StreamExt;
    let combo = t.stop_key().ok_or("no stop key")?;
    let key = qt_key(combo)?;
    let err = |e: zbus::Error| e.to_string();
    let call = |method: &'static str, body| {
        let conn = conn.clone();
        async move {
            conn.call_method(
                Some("org.kde.kglobalaccel"),
                "/kglobalaccel",
                Some("org.kde.KGlobalAccel"),
                method,
                &body,
            )
            .await
        }
    };
    let id: Vec<String> = [
        "desktop-mcp",
        "stop",
        "Desktop MCP",
        "Stop the desktop agent",
    ]
    .map(String::from)
    .to_vec();
    conn.call_method(
        Some("org.kde.kglobalaccel"),
        "/kglobalaccel",
        Some("org.kde.KGlobalAccel"),
        "doRegister",
        &(id.clone(),),
    )
    .await
    .map_err(err)?;
    let path: zbus::zvariant::OwnedObjectPath = conn
        .call_method(
            Some("org.kde.kglobalaccel"),
            "/kglobalaccel",
            Some("org.kde.KGlobalAccel"),
            "getComponent",
            &("desktop-mcp",),
        )
        .await
        .map_err(err)?
        .body()
        .deserialize()
        .map_err(|e| e.to_string())?;
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface("org.kde.kglobalaccel.Component")
        .map_err(err)?
        .member("globalShortcutPressed")
        .map_err(err)?
        .path(path)
        .map_err(err)?
        .build();
    let mut stream = zbus::MessageStream::for_match_rule(rule, conn, Some(16))
        .await
        .map_err(err)?;
    // As KGlobalAccel::setGlobalShortcut does: the active keys (SetPresent
    // = 2, keeping keys the user saved in System Settings), then the
    // default (IsDefault = 8). Each QKeySequence is an int array.
    let keys = vec![(vec![key],)];
    let active: Vec<(Vec<i32>,)> = call("setShortcutKeys", (id.clone(), keys.clone(), 2u32))
        .await
        .map_err(err)?
        .body()
        .deserialize()
        .map_err(|e| e.to_string())?;
    let _ = call("setShortcutKeys", (id.clone(), keys, 8u32)).await;
    let active_keys: Vec<i32> = active.into_iter().flat_map(|(k,)| k).collect();
    if active_keys.is_empty() {
        let taken = format!(
            "{} is taken by another global shortcut; set another in System Settings > \
             Shortcuts > Desktop MCP or with DESKTOP_MCP_STOP_KEY",
            pretty_key(combo)
        );
        return Err(taken);
    }
    let how = match (active_keys.contains(&key), evdev) {
        (true, true) => "KGlobalAccel and the keyboards in /dev/input".to_string(),
        (true, false) => "KGlobalAccel".to_string(),
        (false, _) => format!(
            "KGlobalAccel, set to another key in System Settings > Shortcuts > Desktop MCP \
             (Qt key code {:#x})",
            active_keys[0]
        ),
    };
    t.set_stop_source(true, how);
    while let Some(Ok(msg)) = stream.next().await {
        if let Ok((_, action, _)) = msg.body().deserialize::<(String, String, i64)>()
            && action == "stop"
        {
            t.stop();
        }
    }
    Err("the session bus connection ended".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chords_match_either_side_modifiers() {
        let c = Chord::parse("meta+alt+esc").unwrap();
        let held: HashSet<_> = [KeyCode::KEY_LEFTMETA, left(KeyCode::KEY_RIGHTALT)].into();
        assert!(c.hit(KeyCode::KEY_ESC, &held));
        assert!(!c.hit(KeyCode::KEY_ENTER, &held));
        let only_meta: HashSet<_> = [KeyCode::KEY_LEFTMETA].into();
        assert!(!c.hit(KeyCode::KEY_ESC, &only_meta));
        assert!(Chord::parse("meta+nosuchkey").is_err());
    }

    #[test]
    fn qt_key_codes_match_kglobalaccel() {
        // As globalShortcutsByKey lists Meta+Esc (System Monitor on Plasma).
        assert_eq!(qt_key("meta+esc"), Ok(285_212_672));
        assert_eq!(qt_key("meta+alt+esc"), Ok(0x1900_0000));
        assert_eq!(qt_key("Ctrl+Shift+F5"), Ok(0x0700_0034));
        assert_eq!(qt_key("meta+s"), Ok(0x1000_0053));
        assert!(qt_key("meta+hyper+esc").is_err());
        assert!(qt_key("meta+volumeup").is_err());
    }

    fn ev(kind: evdev::EventType, code: u16, value: i32) -> EventSummary {
        evdev::InputEvent::new(kind.0, code, value).destructure()
    }

    fn key(code: KeyCode, value: i32) -> EventSummary {
        ev(evdev::EventType::KEY, code.0, value)
    }

    fn rel(axis: RelativeAxisCode) -> EventSummary {
        ev(evdev::EventType::RELATIVE, axis.0, 1)
    }

    fn abs() -> EventSummary {
        ev(evdev::EventType::ABSOLUTE, AbsoluteAxisCode::ABS_X.0, 100)
    }

    /// What `events`, fed at `now`, were.
    fn sort(s: &mut Sorter, events: &[EventSummary], now: Instant) -> Option<(Device, bool)> {
        for e in events {
            s.feed(e, now);
        }
        s.take()
    }

    #[test]
    fn plain_pointer_motion_is_told_from_input_that_changes_the_view() {
        let now = Instant::now();
        let mut mouse = Sorter::of(false, false);
        assert_eq!(sort(&mut mouse, &[], now), None);
        let moved = [rel(RelativeAxisCode::REL_X), rel(RelativeAxisCode::REL_Y)];
        assert_eq!(
            sort(&mut mouse, &moved, now),
            Some((Device::Pointer, false))
        );
        let wheel = rel(RelativeAxisCode::REL_WHEEL);
        assert_eq!(
            sort(&mut mouse, &[wheel], now),
            Some((Device::Pointer, true))
        );
        let hi_res = rel(RelativeAxisCode::REL_HWHEEL_HI_RES);
        assert_eq!(
            sort(&mut mouse, &[hi_res], now),
            Some((Device::Pointer, true))
        );
        // A button, and motion while it is held (a drag); not after.
        let press = key(KeyCode::BTN_LEFT, 1);
        assert_eq!(
            sort(&mut mouse, &[press], now),
            Some((Device::Pointer, true))
        );
        assert_eq!(sort(&mut mouse, &moved, now), Some((Device::Pointer, true)));
        sort(&mut mouse, &[key(KeyCode::BTN_LEFT, 0)], now);
        assert_eq!(
            sort(&mut mouse, &moved, now),
            Some((Device::Pointer, false))
        );
        // Keys, a release and a repeat too.
        let mut kbd = Sorter::of(false, false);
        for v in [1, 2, 0] {
            let k = [key(KeyCode::KEY_A, v)];
            assert_eq!(sort(&mut kbd, &k, now), Some((Device::Keyboard, true)));
        }
    }

    #[test]
    fn a_touchpad_stroke_is_motion_but_a_tap_or_scroll_is_not() {
        let t0 = Instant::now();
        let mut pad = Sorter::of(true, false);
        let down = [
            key(KeyCode::BTN_TOUCH, 1),
            key(KeyCode::BTN_TOOL_FINGER, 1),
            abs(),
        ];
        let up = [key(KeyCode::BTN_TOUCH, 0), key(KeyCode::BTN_TOOL_FINGER, 0)];
        // A stroke: one finger moving the pointer for a while.
        assert_eq!(sort(&mut pad, &down, t0), Some((Device::Pointer, false)));
        assert_eq!(sort(&mut pad, &[abs()], t0), Some((Device::Pointer, false)));
        let late = t0 + TAP_MAX + Duration::from_millis(50);
        assert_eq!(sort(&mut pad, &up, late), Some((Device::Pointer, false)));
        // A tap clicks.
        sort(&mut pad, &down, t0);
        let soon = t0 + Duration::from_millis(80);
        assert_eq!(sort(&mut pad, &up, soon), Some((Device::Pointer, true)));
        // Two fingers scroll, for as long as they stay.
        sort(&mut pad, &down, t0);
        let two = [
            key(KeyCode::BTN_TOOL_FINGER, 0),
            key(KeyCode::BTN_TOOL_DOUBLETAP, 1),
        ];
        assert_eq!(sort(&mut pad, &two, t0), Some((Device::Pointer, true)));
        assert_eq!(sort(&mut pad, &[abs()], t0), Some((Device::Pointer, true)));
        sort(&mut pad, &[key(KeyCode::BTN_TOOL_DOUBLETAP, 0)], late);
        sort(&mut pad, &up, late);
        assert_eq!(
            sort(&mut pad, &[abs()], late),
            Some((Device::Pointer, false))
        );
        // A touchscreen presses where it is touched.
        let mut screen = Sorter::of(false, true);
        assert_eq!(sort(&mut screen, &down, t0), Some((Device::Pointer, true)));
        assert_eq!(
            sort(&mut screen, &[abs()], t0),
            Some((Device::Pointer, true))
        );
        // A pen near a tablet moves the pointer; touching it presses.
        let mut tablet = Sorter::of(false, false);
        let near = [key(KeyCode::BTN_TOOL_PEN, 1), abs()];
        assert_eq!(sort(&mut tablet, &near, t0), Some((Device::Pointer, false)));
        let touch = [key(KeyCode::BTN_TOUCH, 1)];
        assert_eq!(sort(&mut tablet, &touch, t0), Some((Device::Pointer, true)));
    }

    /// Watches the real devices (read-only) for a few seconds and prints
    /// what it saw: `cargo test watch_live -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn watch_live() {
        let t = Arc::new(Takeover::from_env());
        let seen = Arc::new(Mutex::new(HashSet::new()));
        let chord = t.stop_key().and_then(|k| Chord::parse(k).ok());
        let s = scan(&t, chord.as_ref(), &seen);
        println!(
            "evdev: {} readable, {} keyboards; {}",
            s.readable,
            s.keyboards,
            s.why_not()
        );
        if s.readable == 0 {
            let t2 = t.clone();
            std::thread::spawn(move || println!("idle watch: {:?}", idle_watch(&t2, "test")));
        }
        for _ in 0..10 {
            std::thread::sleep(Duration::from_millis(500));
            println!("{:?} | {}", t.check(Instant::now()), t.report().1);
        }
    }
}
