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
    /// Fetch `roots` and their descendants into `cache`, as fast as the
    /// platform allows. Children of hidden nodes may be skipped when
    /// `only_showing`; stop after roughly `limit` new nodes.
    async fn prefetch(&self, cache: &mut Cache<Self::Id>, roots: Vec<Self::Id>,
                      only_showing: bool, text_limit: usize, limit: usize);

    async fn actions(&self, id: &Self::Id) -> Vec<String>;
    async fn do_action(&self, id: &Self::Id, index: usize) -> R<bool>;
    /// Complete text content (falls back to the name).
    async fn full_text(&self, id: &Self::Id, limit: usize) -> R<String>;
    /// Replace the contents; Err when the element is not editable.
    async fn set_text(&self, id: &Self::Id, text: &str) -> R<bool>;
    async fn is_focused(&self, id: &Self::Id) -> bool;
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
    /// Raise the first window of an allowed app whose title or app name
    /// contains `query`, and wait until it is in front.
    async fn activate_window(&self, query: &str, allowed: &[String]) -> R<()>;
    /// Capture the active window to a private file and return its path.
    async fn screenshot(&self) -> Option<String>;
}
