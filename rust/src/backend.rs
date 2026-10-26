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

/// The element holding keyboard focus, as `focus_note` reports it.
#[derive(Clone, Debug)]
pub struct Focused<Id> {
    pub id: Id,
    /// Role, name, states and actions only: no children or text.
    pub info: Info<Id>,
    /// Name of the app the element belongs to, read from that app itself
    /// (not from the caller's earlier active-window check), so the caller
    /// can check it against the allowlist and tag the id with it.
    pub app: String,
}

/// What `Backend::prefetch_with` reads, and how far.
// The Windows backend reads everything and looks at the first fields only.
#[cfg_attr(windows, allow(dead_code))]
#[derive(Clone, Copy, Debug)]
pub struct Fetch {
    /// Skip the children of hidden nodes.
    pub only_showing: bool,
    pub text_limit: usize,
    /// Stop after roughly this many new nodes.
    pub limit: usize,
    /// Read action names. Searches match on role, name and text only and
    /// leave them out (on AT-SPI they cost a call or more per node); they
    /// call `fill_actions` for the nodes they return.
    pub actions: bool,
    /// Do not descend below nodes of these roles.
    pub stop_roles: &'static [&'static str],
}

/// Window management the desktop does not offer (only KDE does so far).
pub const NO_WINDOW_CONTROL: &str = "window geometry is only available on KDE Plasma";

/// Clipboard access the desktop does not offer (only Linux so far).
pub const NO_CLIPBOARD: &str = "the clipboard tools are only available on Linux (Wayland)";

/// What `clipboard_get` found on the clipboard.
// Only the Linux backend builds one so far.
#[cfg_attr(windows, allow(dead_code))]
#[derive(Clone, Debug, PartialEq)]
pub enum Clip {
    Empty,
    /// The text, cut to the limit asked for; `truncated` when it was.
    Text {
        text: String,
        truncated: bool,
    },
    /// Marked as a password by the app that copied it; not read.
    Secret,
    /// Not text: the types offered (an image, copied files...).
    Other(Vec<String>),
}

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

/// An app `launch` started: the command, its pid, and the window it opened
/// (None after the wait ran out, or when windows cannot be tracked).
#[derive(Clone, Debug, Default)]
pub struct Launched {
    pub command: String,
    pub pid: u32,
    pub window: Option<WindowGeo>,
    /// Why the window counts as the app's, or why none was waited for.
    pub note: String,
}

/// The window `activate_window` brought to the front: the app's name (""
/// when it has none) and the window's title.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Activated {
    pub app: String,
    pub title: String,
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

/// A screenshot and the screen area it shows, in the coordinates `pointer`
/// takes: (x, y, width, height). None when unknown.
pub struct Shot {
    /// A private file that ends up holding the image sent back.
    pub path: String,
    /// The captured pixels, converted off the event loop; None when an
    /// external tool already wrote a PNG to `path`.
    pub pixels: Option<crate::shot::Convert>,
    pub area: Option<(f64, f64, f64, f64)>,
}

/// File dialogs the server can fill in (only KDE's so far).
pub const NO_FILE_DIALOG: &str = "file dialogs are only supported on KDE Plasma";

/// The active window when it is a file Open/Save dialog.
#[derive(Clone, Debug)]
pub struct FileDialog<Id> {
    pub window: Id,
    /// App name owning the dialog: the app itself (an in-process dialog)
    /// or the desktop portal serving another app.
    pub app: String,
    /// Titles the dialog shows (compositor caption, accessible name).
    pub titles: Vec<String>,
    /// App the compositor says the dialog belongs to (its transient
    /// parent), None when it does not say.
    pub parent: Option<String>,
    /// The file name / location field.
    pub field: Id,
    /// The dialog's default button (Open/Save), when found.
    pub accept: Option<Id>,
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
    /// A mark for `wait_change`, taken before a check so that changes made
    /// during the check still end the wait after it.
    fn change_mark(&self) -> u64 {
        0
    }
    /// Wait until the app whose windows are `near` (none: not running) may
    /// have changed after `mark`, or `timeout` passed. Backends that get
    /// change events return once the app sent some and they settled; the
    /// default just waits one poll interval.
    async fn wait_change(&self, _near: &[Self::Id], _mark: u64, timeout: std::time::Duration) {
        tokio::time::sleep(timeout.min(std::time::Duration::from_millis(50))).await;
    }
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

    /// `prefetch` with the reads of `Fetch`. Backends that cannot skip
    /// anything read everything; `actions` and `stop_roles` only save work.
    async fn prefetch_with(&self, cache: &mut Cache<Self::Id>, roots: Vec<Self::Id>, f: &Fetch) {
        self.prefetch(cache, roots, f.only_showing, f.text_limit, f.limit)
            .await
    }
    /// Read the action names of `ids` into `cache`, for nodes fetched with
    /// `Fetch::actions` false. A backend whose `prefetch_with` always reads
    /// actions keeps this default, which does nothing.
    async fn fill_actions(&self, _cache: &mut Cache<Self::Id>, _ids: &[Self::Id]) {}

    /// The document a top-level window shows, when the platform says so in
    /// one call (Firefox: the window's EMBEDS relation names the current
    /// tab's page). None to search the window for it instead.
    async fn embedded_document(&self, _window: &Self::Id) -> Option<Self::Id> {
        None
    }

    async fn actions(&self, id: &Self::Id) -> Vec<String>;
    async fn do_action(&self, id: &Self::Id, index: usize) -> R<bool>;
    /// Complete text content (falls back to the name).
    async fn full_text(&self, id: &Self::Id, limit: usize) -> R<String>;
    /// Replace the contents; Err when the element is not editable.
    async fn set_text(&self, id: &Self::Id, text: &str) -> R<bool>;
    /// Names the application `id` belongs to for as long as it runs (Linux:
    /// its AT-SPI bus name), so what was learnt about it (set_text writes
    /// that do nothing) is reused; None when the backend has no such key.
    fn app_key(&self, _id: &Self::Id) -> Option<String> {
        None
    }
    async fn is_focused(&self, id: &Self::Id) -> bool;
    /// The element holding keyboard focus now, when the backend tracks it.
    /// Only an element whose app has the active window; None otherwise.
    async fn focused(&self) -> Option<Focused<Self::Id>> {
        None
    }
    /// Focus without synthesising pointer input. Ok(Some(method)) when it
    /// worked, Ok(None) so the caller can fall back to a click.
    async fn try_focus(&self, id: &Self::Id) -> R<Option<String>>;
    /// Centre of the element in the coordinate space `pointer` accepts.
    async fn element_center(&self, id: &Self::Id) -> R<(i32, i32)>;
    /// `element_center` of several elements at once (a drag's two ends), so
    /// what they share (the active window's origin) is looked up once.
    async fn element_centers(&self, ids: &[Self::Id]) -> Vec<R<(i32, i32)>> {
        futures_util::future::join_all(ids.iter().map(|id| self.element_center(id))).await
    }

    /// Check a chord such as "ctrl+s" without pressing it.
    fn check_combo(&self, combo: &str) -> R<()>;
    async fn press_combo(&self, combo: &str) -> R<()>;
    /// Type `text`; Ok holds a note for the reply ("" when none), e.g. that
    /// the clipboard could not be put back after a paste.
    async fn type_text(&self, text: &str) -> R<String>;
    async fn pointer(&self, x: i64, y: i64, button: &str, clicks: i64, scroll: i64) -> R<()>;
    /// Move the cursor to screen coordinates without clicking.
    async fn move_pointer(&self, x: i64, y: i64) -> R<()>;
    /// Press (`down`) or release a mouse button where the cursor is.
    async fn button(&self, button: &str, down: bool) -> R<()>;
    /// Raise the first window of an allowed app whose title or app name
    /// contains `query`, and wait until it is in front. Some names the
    /// window in front when that wait already found it; None leaves it to
    /// the caller to look.
    async fn activate_window(&self, query: &str, allowed: &[String]) -> R<Option<Activated>>;
    /// Windows of allowed apps whose title or app contains `query` (all when
    /// empty).
    async fn windows(&self, _query: &str, _allowed: &[String]) -> R<Vec<WindowGeo>> {
        Err(NO_WINDOW_CONTROL.into())
    }
    /// `screens` and `windows` together, for backends that read both at once.
    async fn screens_and_windows(
        &self,
        query: &str,
        allowed: &[String],
    ) -> (Vec<Screen>, R<Vec<WindowGeo>>) {
        tokio::join!(self.screens(), self.windows(query, allowed))
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
    /// Read the clipboard as text, at most `limit` characters.
    async fn clipboard_get(&self, _limit: usize) -> R<Clip> {
        Err(NO_CLIPBOARD.into())
    }
    async fn clipboard_set(&self, _text: &str) -> R<()> {
        Err(NO_CLIPBOARD.into())
    }
    /// Start `app` (an allowed name) with `args`, without a shell, and wait
    /// up to `wait` for its window; `allowed` limits which windows are seen.
    async fn launch(
        &self,
        _app: &str,
        _args: &[String],
        _allowed: &[String],
        _wait: std::time::Duration,
    ) -> R<Launched> {
        Err("launch is not supported on this platform yet".into())
    }
    async fn screens(&self) -> Vec<Screen> {
        Vec::new()
    }
    /// Show what the agent is doing on screen, when the desktop has an
    /// indicator.
    async fn indicate(&self, _label: &str) {}
    /// The active window as a file dialog; Err when it is none.
    async fn active_file_dialog(&self) -> R<FileDialog<Self::Id>> {
        Err(NO_FILE_DIALOG.into())
    }
    /// Platform checks for `doctor`.
    async fn doctor(&self) -> Vec<Check> {
        Vec::new()
    }
    /// Capture the active window; `path` is a new private file.
    async fn screenshot(&self) -> Option<Shot>;
}
