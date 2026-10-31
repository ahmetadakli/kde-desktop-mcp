//! Takeover: who owns the seat. Input tools pause while the user types or
//! moves the pointer, and stop for good after the user pressed the stop
//! shortcut, until an explicit `activate_window` resumes them.
//!
//! This is the platform-neutral state; the backend feeds it (Linux: real
//! input devices read through evdev, or the compositor's idle
//! notifications, and the shortcut through KGlobalAccel or evdev).
//!
//! Two kinds of activity source:
//! - Exact (evdev): every event comes from a real device; the server's own
//!   virtual devices are left out, so activity is the user's even while
//!   the agent's own input runs.
//! - Idle notifications (ext-idle-notify-v1): the compositor only says
//!   "input happened" / "no input for `IDLE_TIMEOUT`", the agent's own
//!   input included. Activity is the user's when it starts while no input
//!   of ours runs, or lasts longer than ours explains.

// The Windows backend has no takeover detection yet: only the gate is used.
#![cfg_attr(windows, allow(dead_code))]

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How recent user input pauses the input tools, unless
/// DESKTOP_MCP_TAKEOVER_MS says otherwise (0 turns pausing off).
pub const DEFAULT_WINDOW: Duration = Duration::from_millis(1500);
/// The stop shortcut unless DESKTOP_MCP_STOP_KEY says otherwise ("off"
/// turns it off). Plasma 6 binds Meta+Esc to System Monitor, Meta+Shift+Esc
/// and Meta+Ctrl+Esc to KWin actions; Meta+Alt+Esc is free on KDE and
/// Hyprland, and hard to press by accident.
pub const DEFAULT_STOP_KEY: &str = "meta+alt+esc";
/// The idle timeout asked of the compositor: "idled" comes this long after
/// the last input.
pub const IDLE_TIMEOUT: Duration = Duration::from_millis(300);
/// Compositor events arrive a little after the input that caused them.
const OURS_SLACK: Duration = Duration::from_millis(150);

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Device {
    Keyboard,
    Pointer,
    /// Idle notifications do not say which.
    Any,
}

impl Device {
    fn noun(self) -> &'static str {
        match self {
            Device::Keyboard => "keyboard",
            Device::Pointer => "mouse",
            Device::Any => "keyboard or mouse",
        }
    }
}

/// Why an input tool does not run.
#[derive(Clone, Debug, PartialEq)]
pub enum Block {
    /// The stop shortcut (its name) was pressed.
    Stopped(String),
    /// The user's last input, this long ago.
    Paused(Device, Duration),
    /// The user started using the seat after this run of input began.
    Interrupted(Device),
}

impl Block {
    pub fn message(&self, window: Duration) -> String {
        match self {
            Block::Stopped(key) => format!(
                "stopped by the user ({key}): no input is sent until activate_window is \
                 called again. Ask the user before resuming"
            ),
            Block::Paused(dev, ago) => format!(
                "paused: the user is using the {} (last input {} ms ago); nothing was sent. \
                 Retry when they stop (pause {} ms first)",
                dev.noun(),
                ago.as_millis(),
                window.saturating_sub(*ago).as_millis().max(100)
            ),
            Block::Interrupted(dev) => format!(
                "interrupted: the user started using the {}; the rest of the input was not \
                 sent. Retry when they stop",
                dev.noun()
            ),
        }
    }

    /// The on-screen indicator's label.
    pub fn label(&self) -> String {
        match self {
            Block::Stopped(key) => format!("stopped ({key})"),
            Block::Paused(dev, _) | Block::Interrupted(dev) => {
                format!("paused while you use the {}", dev.noun())
            }
        }
    }
}

/// A run of the compositor saying "input happened" (between "resumed" and
/// "idled").
#[derive(Clone, Copy, Debug)]
struct IdleRun {
    since: Instant,
    /// It started while none of our input ran.
    user: bool,
}

#[derive(Default)]
struct State {
    last_user: Option<(Instant, Device)>,
    stopped: bool,
    /// Our input tools running now, and when the last one ended.
    ours_running: u32,
    ours_end: Option<Instant>,
    idle: Option<IdleRun>,
    /// What watches activity and the stop shortcut, or why nothing does
    /// (for `doctor`): (works, text).
    activity_source: Option<(bool, String)>,
    stop_source: Option<(bool, String)>,
}

pub struct Takeover {
    window: Duration,
    /// Lower-case chord ("meta+alt+esc"); None: no stop shortcut.
    stop_key: Option<String>,
    state: Mutex<State>,
}

/// "meta+alt+esc" -> "Meta+Alt+Esc".
pub fn pretty_key(combo: &str) -> String {
    combo
        .split('+')
        .map(|k| {
            let mut c = k.chars();
            match c.next() {
                Some(f) => f.to_uppercase().chain(c).collect(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join("+")
}

impl Takeover {
    pub fn new(window: Duration, stop_key: Option<&str>) -> Self {
        Takeover {
            window,
            stop_key: stop_key
                .map(|k| k.replace(' ', "").to_lowercase())
                .filter(|k| !k.is_empty()),
            state: Mutex::new(State::default()),
        }
    }

    /// From DESKTOP_MCP_TAKEOVER_MS and DESKTOP_MCP_STOP_KEY.
    pub fn from_env() -> Self {
        let window = std::env::var("DESKTOP_MCP_TAKEOVER_MS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map_or(DEFAULT_WINDOW, Duration::from_millis);
        let key = std::env::var("DESKTOP_MCP_STOP_KEY").unwrap_or_else(|_| DEFAULT_STOP_KEY.into());
        let key = key.trim();
        let off = key.is_empty() || ["off", "0", "none"].contains(&key.to_lowercase().as_str());
        Takeover::new(window, (!off).then_some(key))
    }

    pub fn window(&self) -> Duration {
        self.window
    }

    /// The stop chord, lower case ("meta+alt+esc").
    pub fn stop_key(&self) -> Option<&str> {
        self.stop_key.as_deref()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Input from a real device of the user's.
    pub fn user_input(&self, at: Instant, dev: Device) {
        let mut s = self.lock();
        if s.last_user.is_none_or(|(t, _)| at >= t) {
            s.last_user = Some((at, dev));
        }
    }

    /// The stop shortcut was pressed.
    pub fn stop(&self) {
        self.lock().stopped = true;
    }

    /// Clear a stop; whether there was one.
    pub fn resume(&self) -> bool {
        std::mem::take(&mut self.lock().stopped)
    }

    /// Mark our own input as running until the guard drops, so idle
    /// notifications it causes are not taken for the user's.
    pub fn ours(&self) -> Ours<'_> {
        self.lock().ours_running += 1;
        Ours(self)
    }

    fn ours_at(s: &State, at: Instant) -> bool {
        s.ours_running > 0 || s.ours_end.is_some_and(|e| at <= e + OURS_SLACK)
    }

    /// The compositor saw input after `IDLE_TIMEOUT` without any.
    pub fn idle_resumed(&self, at: Instant) {
        let mut s = self.lock();
        let user = !Self::ours_at(&s, at);
        s.idle = Some(IdleRun { since: at, user });
        if user {
            drop(s);
            self.user_input(at, Device::Any);
        }
    }

    /// The compositor saw no input for `IDLE_TIMEOUT`.
    pub fn idle_idled(&self, at: Instant) {
        let mut s = self.lock();
        let Some(run) = s.idle.take() else {
            return;
        };
        let last = at.checked_sub(IDLE_TIMEOUT).unwrap_or(at);
        let mut user = run.user.then_some(run.since);
        if !Self::ours_at(&s, last) && last > run.since {
            user = Some(last);
        }
        drop(s);
        if let Some(t) = user {
            self.user_input(t, Device::Any);
        }
    }

    /// The user's latest input as of `now`, including input the compositor
    /// still reports as going on.
    fn last_user(&self, now: Instant) -> Option<(Instant, Device)> {
        let s = self.lock();
        let mut last = s.last_user;
        if let Some(run) = s.idle {
            // Input within the last IDLE_TIMEOUT that ours does not explain.
            let recent = now.checked_sub(IDLE_TIMEOUT).unwrap_or(now);
            let derived = if run.user {
                Some(run.since.max(recent))
            } else if s.ours_running == 0
                && s.ours_end
                    .is_none_or(|e| now > e + IDLE_TIMEOUT + OURS_SLACK)
            {
                Some(recent.max(run.since))
            } else {
                None
            };
            if let Some(t) = derived
                && last.is_none_or(|(l, _)| t > l)
            {
                last = Some((t, Device::Any));
            }
        }
        last
    }

    /// Whether the user used the keyboard or mouse after `at`, as far as
    /// it is watched.
    pub fn user_input_since(&self, at: Instant) -> bool {
        self.last_user(Instant::now()).is_some_and(|(t, _)| t > at)
    }

    /// Whether an input tool may start now.
    #[cfg(test)]
    pub fn check(&self, now: Instant) -> Result<(), Block> {
        self.check_since(None, now)
    }

    /// `check`, and for a run of input that began at `start` (a batch, a
    /// drag), whether the user has used the seat since.
    pub fn check_since(&self, start: Option<Instant>, now: Instant) -> Result<(), Block> {
        if self.lock().stopped {
            return Err(Block::Stopped(pretty_key(
                self.stop_key.as_deref().unwrap_or("stop shortcut"),
            )));
        }
        if self.window.is_zero() {
            return Ok(());
        }
        let Some((t, dev)) = self.last_user(now) else {
            return Ok(());
        };
        if start.is_some_and(|s| t > s) {
            return Err(Block::Interrupted(dev));
        }
        let ago = now.saturating_duration_since(t);
        if ago < self.window {
            return Err(Block::Paused(dev, ago));
        }
        Ok(())
    }

    pub fn set_activity_source(&self, ok: bool, text: String) {
        self.lock().activity_source = Some((ok, text));
    }

    pub fn set_stop_source(&self, ok: bool, text: String) {
        self.lock().stop_source = Some((ok, text));
    }

    /// The `doctor` line: whether detection works, and how.
    pub fn report(&self) -> (bool, String) {
        let s = self.lock();
        let (act_ok, act) = match (&s.activity_source, self.window.is_zero()) {
            (_, true) => (true, "pausing off (DESKTOP_MCP_TAKEOVER_MS=0)".to_string()),
            (Some((ok, text)), _) => (
                *ok,
                if *ok {
                    format!(
                        "pauses input for {} ms after the user's input, seen through {text}",
                        self.window.as_millis()
                    )
                } else {
                    format!("not watching user input: {text}")
                },
            ),
            (None, _) => (false, "not watching user input (not started)".to_string()),
        };
        let (stop_ok, stop) = match (&self.stop_key, &s.stop_source) {
            (None, _) => (
                true,
                "no stop shortcut (DESKTOP_MCP_STOP_KEY=off)".to_string(),
            ),
            (Some(k), Some((ok, text))) if *ok => (
                true,
                format!("stop shortcut {} through {text}", pretty_key(k)),
            ),
            (Some(k), Some((_, text))) => (
                false,
                format!("stop shortcut {} unavailable: {text}", pretty_key(k)),
            ),
            (Some(k), None) => (
                false,
                format!("stop shortcut {} not registered (yet)", pretty_key(k)),
            ),
        };
        let state = if s.stopped {
            "; now stopped by the user (activate_window resumes)"
        } else {
            ""
        };
        (act_ok && stop_ok, format!("{act}; {stop}{state}"))
    }
}

/// Our input running; see `Takeover::ours`.
pub struct Ours<'a>(&'a Takeover);

impl Drop for Ours<'_> {
    fn drop(&mut self) {
        let mut s = self.0.lock();
        s.ours_running = s.ours_running.saturating_sub(1);
        s.ours_end = Some(Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn recent_user_input_pauses_until_the_window_passes() {
        let t = Takeover::new(ms(1500), Some("meta+alt+esc"));
        let t0 = Instant::now();
        assert_eq!(t.check(t0), Ok(()));
        t.user_input(t0, Device::Pointer);
        assert_eq!(
            t.check(t0 + ms(400)),
            Err(Block::Paused(Device::Pointer, ms(400)))
        );
        let msg = Block::Paused(Device::Pointer, ms(400)).message(ms(1500));
        assert!(msg.contains("mouse") && msg.contains("1100 ms"), "{msg}");
        assert_eq!(t.check(t0 + ms(1500)), Ok(()));
        // An older event does not move the last input back.
        t.user_input(t0 - ms(5000), Device::Keyboard);
        assert_eq!(
            t.check(t0 + ms(10)),
            Err(Block::Paused(Device::Pointer, ms(10)))
        );
    }

    #[test]
    fn input_after_a_run_began_interrupts_it() {
        let t = Takeover::new(ms(1500), None);
        let t0 = Instant::now();
        t.user_input(t0, Device::Keyboard);
        // Input before the run: only the window applies.
        assert_eq!(t.check_since(Some(t0 + ms(1)), t0 + ms(2000)), Ok(()));
        assert_eq!(
            t.check_since(Some(t0 - ms(1)), t0 + ms(2000)),
            Err(Block::Interrupted(Device::Keyboard))
        );
    }

    #[test]
    fn a_stop_holds_until_resumed() {
        let t = Takeover::new(ms(1500), Some("Meta + Alt + Esc"));
        let t0 = Instant::now();
        t.stop();
        let err = t.check(t0).unwrap_err();
        assert_eq!(err, Block::Stopped("Meta+Alt+Esc".into()));
        assert!(err.message(t.window()).contains("activate_window"));
        // Long after: still stopped.
        assert!(t.check(t0 + ms(60_000)).is_err());
        assert!(t.resume());
        assert!(!t.resume());
        assert_eq!(t.check(t0), Ok(()));
    }

    #[test]
    fn a_zero_window_only_keeps_the_stop() {
        let t = Takeover::new(Duration::ZERO, Some("meta+alt+esc"));
        let t0 = Instant::now();
        t.user_input(t0, Device::Keyboard);
        assert_eq!(t.check_since(Some(t0 - ms(1)), t0), Ok(()));
        t.stop();
        assert!(matches!(t.check(t0), Err(Block::Stopped(_))));
    }

    #[test]
    fn idle_notifications_during_our_input_are_ours() {
        let t = Takeover::new(ms(1500), None);
        {
            let _ours = t.ours();
            let now = Instant::now();
            t.idle_resumed(now);
            assert_eq!(t.check(now), Ok(()));
        }
        // Just after our input: the compositor still reports it as going on.
        let end = Instant::now();
        assert_eq!(t.check(end + ms(100)), Ok(()));
        // Our input ended long enough ago that "idled" should have come:
        // someone else keeps it going.
        let late = end + IDLE_TIMEOUT + OURS_SLACK + ms(50);
        assert!(matches!(t.check(late), Err(Block::Paused(Device::Any, _))));
        // It went idle soon after ours: nothing of the user's.
        let t = Takeover::new(ms(1500), None);
        {
            let _ours = t.ours();
            t.idle_resumed(Instant::now());
        }
        let end = Instant::now();
        t.idle_idled(end + IDLE_TIMEOUT);
        assert_eq!(t.check(end + IDLE_TIMEOUT + ms(10)), Ok(()));
        assert_eq!(t.check(end + ms(5000)), Ok(()));
    }

    #[test]
    fn idle_notifications_while_we_are_quiet_are_the_users() {
        let t = Takeover::new(ms(1500), None);
        let t0 = Instant::now();
        t.idle_resumed(t0);
        // Still going on: paused, and it interrupts a run that began before.
        assert!(matches!(t.check(t0 + ms(5000)), Err(Block::Paused(..))));
        assert_eq!(
            t.check_since(Some(t0 - ms(1)), t0 + ms(10)),
            Err(Block::Interrupted(Device::Any))
        );
        // Idle after the user stopped: paused until the window passes after
        // the last input (idled time minus the timeout).
        let idled = t0 + ms(2000);
        t.idle_idled(idled);
        let last = idled - IDLE_TIMEOUT;
        assert!(matches!(t.check(last + ms(1400)), Err(Block::Paused(..))));
        assert_eq!(t.check(last + ms(1500)), Ok(()));
    }

    #[test]
    fn report_says_what_watches() {
        let t = Takeover::new(ms(1500), Some("meta+alt+esc"));
        let (ok, text) = t.report();
        assert!(!ok, "{text}");
        t.set_activity_source(true, "evdev (2 devices)".into());
        t.set_stop_source(true, "KGlobalAccel".into());
        let (ok, text) = t.report();
        assert!(ok, "{text}");
        assert!(
            text.contains("1500 ms") && text.contains("Meta+Alt+Esc through KGlobalAccel"),
            "{text}"
        );
        t.stop();
        assert!(t.report().1.contains("now stopped"));
    }
}
