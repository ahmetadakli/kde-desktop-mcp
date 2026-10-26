//! Platform-neutral contract between the MCP tool layer and a desktop backend.
//!
//! The tool layer (main.rs) owns ids, the allowlist, tree rendering and the
//! order of fallbacks. A backend only knows how to read and drive its
//! platform's accessibility tree and input devices.

use std::collections::HashMap;
use std::fmt::Debug;
use std::hash::Hash;
use std::path::PathBuf;

pub type R<T> = Result<T, String>;

/// Everything tree rendering and search read about one element.
#[derive(Clone, Debug, Default)]
pub struct Info<Id> {
    pub role: String,
    pub name: String,
    /// Subset of: focused, checked, selected, expanded, active, editable,
    /// disabled, in this order.
    pub states: Vec<&'static str>,
    pub showing: bool,
    pub actions: Vec<String>,
    /// Single-line text preview (newlines already turned into spaces), up to
    /// the limit asked for; None when the element has no text.
    pub text: Option<String>,
    pub children: Vec<Id>,
}

pub type Cache<Id> = HashMap<Id, Info<Id>>;

/// Window management the desktop does not offer (only KDE does so far).
pub const NO_WINDOW_CONTROL: &str = "window geometry is only available on KDE Plasma";

/// A top-level window and where it is, in logical screen px.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WindowGeo {
    pub title: String,
    pub app: String,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub active: bool,
    pub minimized: bool,
    pub maximized: bool,
    pub output: String,
    pub pid: u32,
    /// Pseudo-terminals the window's processes hold (terminal emulators).
    pub terminals: Vec<Terminal>,
}

/// A terminal in a window: device, foreground command and its directory.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Terminal {
    pub tty: String,
    pub command: String,
    pub cwd: String,
}

/// What `move_window` changes; None keeps the current value.
#[derive(Clone, Debug, Default)]
pub struct WindowChange {
    pub x: Option<f64>,
    pub y: Option<f64>,
    pub width: Option<f64>,
    pub height: Option<f64>,
    /// "maximize", "minimize" or "restore".
    pub state: Option<String>,
}

/// A display output: name, logical geometry and scale factor.
#[derive(Clone, Debug, PartialEq)]
pub struct Screen {
    pub name: String,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub scale: f64,
}

/// One line of the `doctor` report.
pub struct Check {
    pub name: &'static str,
    pub ok: bool,
    pub detail: String,
}

/// A screenshot file and the screen area it shows, in the coordinates
/// `pointer` takes: (x, y, width, height). None when unknown.
pub struct Shot {
    pub path: String,
    pub area: Option<(f64, f64, f64, f64)>,
}

pub struct WindowEntry {
    pub kind: String,
    pub title: String,
    pub active: bool,
}

pub struct AppEntry {
    pub name: String,
    pub windows: Vec<WindowEntry>,
}

#[allow(async_fn_in_trait)]
pub trait Backend {
    type Id: Clone + Eq + Hash + Debug;

    fn allow_file(&self) -> PathBuf;
    fn default_allow(&self) -> &'static str;
    fn apps_note(&self) -> &'static str;
    /// Roles skipped (children lifted) when unnamed and actionless.
    fn boring_roles(&self) -> &'static [&'static str];

    /// Make sure the accessibility layer is on. Called before reads.
    async fn enable(&self);
    /// Create slow resources (virtual input devices) ahead of first use.
    fn warm_up(&self) {}

    async fn list_apps(&self) -> Vec<AppEntry>;
    /// Top-level windows of every process with this (lower-case) app name;
    /// None when no such app is running.
    async fn app_windows(&self, app: &str) -> Option<Vec<Self::Id>>;
    /// App name owning the foreground window, "" when unknown.
    async fn active_app(&self) -> String;

    async fn info(&self, id: &Self::Id, text_limit: usize) -> Info<Self::Id>;
    /// Role and name only, as cheaply as the platform allows (one round
    /// trip); role "" when the element is gone. Checks an id before use.
    async fn identity(&self, id: &Self::Id) -> (String, String) {
        let info = self.info(id, 0).await;
        (info.role, info.name)
    }
    /// Fetch `roots` and their descendants into `cache`, as fast as the
    /// platform allows. Children of hidden nodes may be skipped when
    /// `only_showing`; stop after roughly `limit` new nodes.
    async fn prefetch(
        &self,
        cache: &mut Cache<Self::Id>,
        roots: Vec<Self::Id>,
        only_showing: bool,
        text_limit: usize,
        limit: usize,
    );

    async fn actions(&self, id: &Self::Id) -> Vec<String>;
    async fn do_action(&self, id: &Self::Id, index: usize) -> R<bool>;
    /// Complete text content (falls back to the name).
    async fn full_text(&self, id: &Self::Id, limit: usize) -> R<String>;
    /// Replace the contents; Err when the element is not editable.
    async fn set_text(&self, id: &Self::Id, text: &str) -> R<bool>;
    async fn is_focused(&self, id: &Self::Id) -> bool;
    /// The element holding keyboard focus now, when the backend tracks it.
    async fn focused(&self) -> Option<Self::Id> {
        None
    }
    /// Focus without synthesising pointer input. Ok(Some(method)) when it
    /// worked, Ok(None) so the caller can fall back to a click.
    async fn try_focus(&self, id: &Self::Id) -> R<Option<String>>;
    /// Centre of the element in the coordinate space `pointer` accepts.
    async fn element_center(&self, id: &Self::Id) -> R<(i32, i32)>;

    /// Check a chord such as "ctrl+s" without pressing it.
    fn check_combo(&self, combo: &str) -> R<()>;
    async fn press_combo(&self, combo: &str) -> R<()>;
    async fn type_text(&self, text: &str) -> R<()>;
    async fn pointer(&self, x: i64, y: i64, button: &str, clicks: i64, scroll: i64) -> R<()>;
    /// Move the cursor to screen coordinates without clicking.
    async fn move_pointer(&self, x: i64, y: i64) -> R<()>;
    /// Press (`down`) or release a mouse button where the cursor is.
    async fn button(&self, button: &str, down: bool) -> R<()>;
    /// Raise the first window of an allowed app whose title or app name
    /// contains `query`, and wait until it is in front.
    async fn activate_window(&self, query: &str, allowed: &[String]) -> R<()>;
    /// Windows of allowed apps whose title or app contains `query` (all when
    /// empty).
    async fn windows(&self, _query: &str, _allowed: &[String]) -> R<Vec<WindowGeo>> {
        Err(NO_WINDOW_CONTROL.into())
    }
    /// Move, resize, maximize, minimize or restore the first such window.
    async fn set_window(
        &self,
        _query: &str,
        _allowed: &[String],
        _change: &WindowChange,
    ) -> R<WindowGeo> {
        Err(NO_WINDOW_CONTROL.into())
    }
    async fn screens(&self) -> Vec<Screen> {
        Vec::new()
    }
    /// Show what the agent is doing on screen, when the desktop has an
    /// indicator.
    async fn indicate(&self, _label: &str) {}
    /// Platform checks for `doctor`.
    async fn doctor(&self) -> Vec<Check> {
        Vec::new()
    }
    /// Capture the active window to a private file and return its path.
    async fn screenshot(&self) -> Option<Shot>;
}
