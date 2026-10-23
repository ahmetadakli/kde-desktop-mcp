//! Virtual keyboard and absolute pointer through /dev/uinput.

use std::str::FromStr;
use std::sync::Mutex;
use std::time::Duration;

use evdev::uinput::VirtualDevice;
use evdev::{AbsInfo, AbsoluteAxisCode, AttributeSet, EventType, InputEvent, KeyCode,
            RelativeAxisCode, UinputAbsSetup};

use crate::atspi::R;

pub const ABS_MAX: i32 = 32767;
/// Time for the compositor to pick up a freshly created device.
const DEVICE_SETTLE: Duration = Duration::from_millis(300);
const KEY_HOLD: Duration = Duration::from_millis(8);
const KEY_GAP: Duration = Duration::from_millis(8);
const POINTER_SETTLE: Duration = Duration::from_millis(25);
const BUTTON_HOLD: Duration = Duration::from_millis(15);
const BUTTON_GAP: Duration = Duration::from_millis(30);

const ALIASES: &[(&str, &str)] = &[
    ("ctrl", "LEFTCTRL"), ("control", "LEFTCTRL"), ("shift", "LEFTSHIFT"),
    ("alt", "LEFTALT"), ("altgr", "RIGHTALT"), ("super", "LEFTMETA"), ("meta", "LEFTMETA"),
    ("win", "LEFTMETA"), ("enter", "ENTER"), ("return", "ENTER"), ("esc", "ESC"),
    ("escape", "ESC"), ("del", "DELETE"), ("pgup", "PAGEUP"), ("pgdn", "PAGEDOWN"),
    ("space", "SPACE"), ("tab", "TAB"), ("backspace", "BACKSPACE"),
];

pub fn keycode(name: &str) -> R<KeyCode> {
    let lower = name.to_lowercase();
    let n = ALIASES.iter().find(|(a, _)| *a == lower).map(|(_, k)| k.to_string())
        .unwrap_or_else(|| name.to_uppercase());
    KeyCode::from_str(&format!("KEY_{n}")).map_err(|_| format!("unknown key '{name}'"))
}

pub fn parse_combo(combo: &str) -> R<Vec<KeyCode>> {
    combo.replace(' ', "").split('+').filter(|k| !k.is_empty()).map(keycode).collect()
}

fn ev(kind: EventType, code: u16, value: i32) -> InputEvent {
    InputEvent::new(kind.0, code, value)
}

#[derive(Default)]
pub struct Input {
    kbd: Mutex<Option<VirtualDevice>>,
    ptr: Mutex<Option<VirtualDevice>>,
}

fn io(e: std::io::Error) -> String {
    format!("uinput: {e} (is /dev/uinput writable?)")
}

impl Input {
    fn with_kbd<T>(&self, f: impl FnOnce(&mut VirtualDevice) -> R<T>) -> R<T> {
        let mut slot = self.kbd.lock().unwrap();
        if slot.is_none() {
            let mut keys = AttributeSet::<KeyCode>::new();
            // Keyboard keys only: buttons (0x100-0x15f) would make it look like a pointer.
            for code in (1u16..0x100).chain(0x160..0x200) {
                keys.insert(KeyCode::new(code));
            }
            let dev = VirtualDevice::builder().map_err(io)?
                .name("desktop-mcp-keyboard")
                .with_keys(&keys).map_err(io)?
                .build().map_err(io)?;
            std::thread::sleep(DEVICE_SETTLE);
            *slot = Some(dev);
        }
        f(slot.as_mut().unwrap())
    }

    fn with_ptr<T>(&self, f: impl FnOnce(&mut VirtualDevice) -> R<T>) -> R<T> {
        let mut slot = self.ptr.lock().unwrap();
        if slot.is_none() {
            let mut buttons = AttributeSet::<KeyCode>::new();
            for b in [KeyCode::BTN_LEFT, KeyCode::BTN_RIGHT, KeyCode::BTN_MIDDLE] {
                buttons.insert(b);
            }
            let mut rel = AttributeSet::<RelativeAxisCode>::new();
            rel.insert(RelativeAxisCode::REL_WHEEL);
            let axis = |code| UinputAbsSetup::new(code, AbsInfo::new(0, 0, ABS_MAX, 0, 0, 0));
            let dev = VirtualDevice::builder().map_err(io)?
                .name("desktop-mcp-pointer")
                .with_keys(&buttons).map_err(io)?
                .with_absolute_axis(&axis(AbsoluteAxisCode::ABS_X)).map_err(io)?
                .with_absolute_axis(&axis(AbsoluteAxisCode::ABS_Y)).map_err(io)?
                .with_relative_axes(&rel).map_err(io)?
                .build().map_err(io)?;
            std::thread::sleep(DEVICE_SETTLE);
            *slot = Some(dev);
        }
        f(slot.as_mut().unwrap())
    }

    /// Create both devices ahead of first use (blocking).
    pub fn warm_up(&self) {
        let _ = self.with_kbd(|_| Ok(()));
        let _ = self.with_ptr(|_| Ok(()));
    }

    pub async fn press_combo(&self, combo: &str) -> R<()> {
        let codes = parse_combo(combo)?;
        let down: Vec<InputEvent> = codes.iter().map(|c| ev(EventType::KEY, c.0, 1)).collect();
        let up: Vec<InputEvent> = codes.iter().rev().map(|c| ev(EventType::KEY, c.0, 0)).collect();
        self.with_kbd(|d| d.emit(&down).map_err(io))?;
        tokio::time::sleep(KEY_HOLD).await;
        self.with_kbd(|d| d.emit(&up).map_err(io))?;
        tokio::time::sleep(KEY_GAP).await;
        Ok(())
    }

    /// Move to absolute device coordinates, then click and/or scroll.
    pub async fn pointer(&self, ax: i32, ay: i32, button: &str, clicks: i64,
                         scroll: i64) -> R<()> {
        let btn = match button {
            "left" => KeyCode::BTN_LEFT,
            "right" => KeyCode::BTN_RIGHT,
            "middle" => KeyCode::BTN_MIDDLE,
            _ => return Err(format!("unknown button '{button}' (left, right, middle)")),
        };
        self.with_ptr(|d| d.emit(&[
            ev(EventType::ABSOLUTE, AbsoluteAxisCode::ABS_X.0, ax),
            ev(EventType::ABSOLUTE, AbsoluteAxisCode::ABS_Y.0, ay),
        ]).map_err(io))?;
        tokio::time::sleep(POINTER_SETTLE).await;
        for _ in 0..clicks.clamp(0, 3) {
            self.with_ptr(|d| d.emit(&[ev(EventType::KEY, btn.0, 1)]).map_err(io))?;
            tokio::time::sleep(BUTTON_HOLD).await;
            self.with_ptr(|d| d.emit(&[ev(EventType::KEY, btn.0, 0)]).map_err(io))?;
            tokio::time::sleep(BUTTON_GAP).await;
        }
        if scroll != 0 {
            let v = (-scroll).clamp(i32::MIN as i64, i32::MAX as i64) as i32;
            self.with_ptr(|d| d.emit(&[
                ev(EventType::RELATIVE, RelativeAxisCode::REL_WHEEL.0, v)]).map_err(io))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_aliases_and_names() {
        assert_eq!(parse_combo("ctrl+s").unwrap(), vec![KeyCode::KEY_LEFTCTRL, KeyCode::KEY_S]);
        assert_eq!(parse_combo("Ctrl + Shift + F5").unwrap(),
                   vec![KeyCode::KEY_LEFTCTRL, KeyCode::KEY_LEFTSHIFT, KeyCode::KEY_F5]);
        assert_eq!(parse_combo("enter").unwrap(), vec![KeyCode::KEY_ENTER]);
        assert_eq!(parse_combo("pgdn").unwrap(), vec![KeyCode::KEY_PAGEDOWN]);
        assert!(parse_combo("ctrl+nosuchkey").is_err());
    }
}
