//! Screenshot-free desktop control MCP server for KDE Plasma (Wayland),
//! Hyprland and Windows.
//!
//! Same tools and output as the former Python server (server.py, removed
//! after 4cd2632), plus `batch`.
//! Every app-scoped operation is restricted to the allowlist (re-read on
//! every call). Platform code lives behind the `Backend` trait.

mod backend;
#[cfg(target_os = "linux")]
mod linux;
mod render;
mod shot;
mod tools;
#[cfg(windows)]
mod windows;
// The Windows backend's pure helpers, so their tests also run on Linux.
#[cfg(all(test, not(windows)))]
#[path = "windows/logic.rs"]
#[allow(dead_code)]
mod windows_logic;

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::{Duration, Instant};

use serde_json::{Value as Json, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

use backend::{
    Backend, Cache, Clip, Fetch, FileDialog, Focused, Info, R, Shot, WindowChange, WindowGeo,
};
use render::{Line, Printed, WalkOpts};

const MAX_REFS: usize = 20000;
const TREE_TEXT_LIMIT: usize = 80;
/// Nodes fetched ahead when rendering reaches an unfetched part of the tree.
const PREFETCH_MORE: usize = 500;

/// How searches (find, locate, the active document) fetch: showing nodes,
/// without action names, which only the nodes they return need.
fn search_fetch(text_limit: usize, limit: usize) -> Fetch {
    Fetch {
        only_showing: true,
        text_limit,
        limit,
        actions: false,
        stop_roles: &[],
    }
}

struct Refs<Id> {
    map: HashMap<u64, Ref<Id>>,
    /// The id last issued for each element (and app), so reading the same
    /// element again prints the same number (see `Server::add_ref`).
    index: HashMap<(Id, String), u64>,
    next: u64,
}

/// An issued id: the element, its app, and its role and name at the time,
/// so a re-rendered page that dropped or reused the object is noticed.
#[derive(Clone)]
struct Ref<Id> {
    id: Id,
    app: String,
    role: String,
    name: String,
    /// Whether `name` must still match on resolve (see `name_is_stable`).
    check_name: bool,
}

/// Roles whose accessible name follows their content or a title that
/// changes while the element stays the same: window, dialog and tab titles,
/// documents (the page title), labels and status text, progress, and fields
/// or pickers some toolkits name after their value. Ids of these are
/// checked by role only.
const VOLATILE_NAME_ROLES: [&str; 31] = [
    // AT-SPI
    "frame",
    "window",
    "dialog",
    "alert",
    "page tab",
    "document web",
    "document frame",
    "document text",
    "document email",
    "document spreadsheet",
    "document presentation",
    "label",
    "static",
    "status bar",
    "progress bar",
    "level bar",
    "tool tip",
    "notification",
    "terminal",
    "text",
    "entry",
    "password text",
    "combo box",
    "spin button",
    "slider",
    // UI Automation
    "document",
    "tabitem",
    "statusbar",
    "progressbar",
    "combobox",
    "spinner",
];

/// Whether an element's name identifies it well enough to compare on
/// resolve: it has one, it is not editable, and its role does not rename it.
fn name_is_stable<Id>(info: &Info<Id>) -> bool {
    !info.name.is_empty()
        && !info.states.contains(&"editable")
        && !VOLATILE_NAME_ROLES.contains(&info.role.as_str())
}

/// `apps` output. Allowed apps list their windows (untitled ones only when
/// active: plasmashell alone has a dozen); blocked apps share one line,
/// since nothing can be done with their windows. The allowlist path and the
/// accessibility note (both in `doctor`) only come when no app is allowed.
fn apps_text(
    list: Vec<backend::AppEntry>,
    allow: &[String],
    allow_file: &std::path::Path,
    note: &str,
) -> String {
    let mut lines = Vec::new();
    // Name, and whether one of its windows is the active one.
    let mut blocked: Vec<(String, bool)> = Vec::new();
    for app in list {
        if app.windows.is_empty() {
            continue;
        }
        if !allow.contains(&app.name.to_lowercase()) {
            let active = app.windows.iter().any(|w| w.active);
            match blocked.iter_mut().find(|(n, _)| *n == app.name) {
                Some((_, a)) => *a |= active,
                None => blocked.push((app.name, active)),
            }
            continue;
        }
        lines.push(format!("{} (allowed)", app.name));
        for w in app.windows {
            if w.title.is_empty() && !w.active {
                continue;
            }
            let act = if w.active { " [active]" } else { "" };
            lines.push(format!(
                "  - {}: {}{act}",
                w.kind,
                render::py_repr(&w.title)
            ));
        }
    }
    let none_allowed = lines.is_empty();
    if !blocked.is_empty() {
        let names: Vec<String> = blocked
            .into_iter()
            .map(|(n, active)| if active { format!("{n} [active]") } else { n })
            .collect();
        lines.push(format!("blocked: {}", names.join(", ")));
    }
    if none_allowed {
        lines.push(format!(
            "no allowed app has a window; allowlist file: {}",
            allow_file.display()
        ));
        if !note.is_empty() {
            lines.push(note.into());
        }
    }
    lines.join("\n")
}

struct Server<B: Backend> {
    b: B,
    refs: Mutex<Refs<B::Id>>,
    allow_file: PathBuf,
    /// Title of the active window as last reported, to mention changes.
    last_active: Mutex<Option<String>>,
    /// Maps pixels of the last screenshot to screen coordinates.
    last_shot: Mutex<Option<ShotMap>>,
    /// Held by tools that send keyboard or pointer input, so two concurrent
    /// calls never interleave their events.
    input: Mutex<()>,
    /// `app_key`s of apps whose accessibility text writes report success
    /// and change nothing (Firefox); set_text types there right away.
    inert_writes: Mutex<HashSet<String>>,
    /// Keys and buttons down now, in the order they were pressed: held by
    /// `hold` across calls, or by an input tool for the length of its
    /// action (a click's modifiers, a drag's button). A cancelled call
    /// leaves its own there, so `release_all` finds them. Never locked
    /// across an await.
    held: std::sync::Mutex<Vec<Held>>,
}

/// A key or mouse button the server pressed and has not released.
#[derive(Clone, Debug, PartialEq)]
enum Held {
    /// Canonical key name (`canon_key`).
    Key(String),
    Button(String),
}

impl Held {
    fn name(&self) -> &str {
        match self {
            Held::Key(k) | Held::Button(k) => k,
        }
    }
}

/// Modifiers click, mouse and scroll take.
const MODIFIERS: [&str; 4] = ["ctrl", "shift", "alt", "meta"];
const BUTTONS: [&str; 3] = ["left", "right", "middle"];

/// One name per key, so "control" held and "ctrl" asked are the same key.
fn canon_key(name: &str) -> String {
    let n = name.trim().to_lowercase();
    match n.as_str() {
        "control" => "ctrl".into(),
        "super" | "win" => "meta".into(),
        "return" => "enter".into(),
        "escape" => "esc".into(),
        _ => n,
    }
}

/// The keys of a chord ("ctrl+shift"), canonical, without repeats.
fn chord_keys(chord: &str) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    for k in chord.split('+').map(canon_key).filter(|k| !k.is_empty()) {
        if !keys.contains(&k) {
            keys.push(k);
        }
    }
    keys
}

/// The `modifiers` argument: a list (["ctrl", "shift"]) or a chord
/// ("ctrl+shift"); only ctrl, shift, alt and meta.
fn modifiers_arg(a: &Json) -> R<Vec<String>> {
    let names: Vec<String> = match a.get("modifiers") {
        None | Some(Json::Null) => return Ok(Vec::new()),
        Some(Json::String(s)) => chord_keys(s),
        Some(Json::Array(list)) => {
            let mut keys: Vec<String> = Vec::new();
            for v in list {
                let s = v
                    .as_str()
                    .ok_or("'modifiers' must be a list of strings, e.g. [\"ctrl\"]")?;
                for k in chord_keys(s) {
                    if !keys.contains(&k) {
                        keys.push(k);
                    }
                }
            }
            keys
        }
        Some(_) => return Err("'modifiers' must be a list of strings, e.g. [\"ctrl\"]".into()),
    };
    match names.iter().find(|k| !MODIFIERS.contains(&k.as_str())) {
        Some(k) => Err(format!("unknown modifier '{k}' (ctrl, shift, alt, meta)")),
        None => Ok(names),
    }
}

/// " with ctrl+shift" for modifiers ["ctrl", "shift"], "" for none: the
/// end of a pointer tool's reply.
fn with_modifiers_note(mods: &[String]) -> String {
    if mods.is_empty() {
        String::new()
    } else {
        format!(" with {}", mods.join("+"))
    }
}

/// Tools that send keyboard or pointer input, run an app's actions (`act`
/// presses buttons and opens menus; `set_text` may retype through the
/// keyboard), or use the clipboard that type_text pastes through; they run
/// one at a time.
const INPUT_TOOLS: [&str; 14] = [
    "act",
    "set_text",
    "focus",
    "click",
    "drag",
    "scroll",
    "key",
    "type_text",
    "mouse",
    "activate_window",
    "clipboard_get",
    "clipboard_set",
    "file_dialog",
    "hold",
];

/// An input tool's `window` argument (tools::WINDOW_TOOLS), trimmed; None
/// when absent, empty, or the tool takes none.
fn window_arg(tool: &str, a: &Json) -> R<Option<String>> {
    if !tools::WINDOW_TOOLS.contains(&tool) {
        return Ok(None);
    }
    match a.get("window") {
        None | Some(Json::Null) => Ok(None),
        Some(Json::String(w)) if w.trim().is_empty() => Ok(None),
        Some(Json::String(w)) => Ok(Some(w.trim().to_string())),
        Some(_) => Err("window must be a string (a title or app name)".into()),
    }
}

tokio::task_local! {
    /// The id of the element the running batch step produced, for `"$N"`
    /// references in later steps (see `Server::batch`). Only set inside a
    /// batch step; tools note it through `note_step_element`.
    static STEP_ELEMENT: std::cell::Cell<Option<u64>>;
}

/// Record `rid` as the element the running batch step produced: the one a
/// target or wait picked, or find's first hit (`replace`), else the focus
/// note's element when the step named none. Does nothing outside a batch.
fn note_step_element(rid: u64, replace: bool) {
    let _ = STEP_ELEMENT.try_with(|e| {
        if replace || e.get().is_none() {
            e.set(Some(rid));
        }
    });
}

/// What an earlier batch step left for `"$N"` references.
enum StepElement {
    Element(u64),
    NoElement,
    Failed,
}

/// Replace `"$N"` in the top-level id arguments (`id`, `*_id`) of step
/// `step` (1-based) with the element step N produced.
fn resolve_step_refs(args: &mut Json, step: usize, done: &[(String, StepElement)]) -> R<()> {
    let Some(map) = args.as_object_mut() else {
        return Ok(());
    };
    for (key, value) in map.iter_mut() {
        if key != "id" && !key.ends_with("_id") {
            continue;
        }
        let Some(text) = value.as_str().and_then(|v| v.strip_prefix('$')) else {
            continue;
        };
        let n: usize = text
            .parse()
            .map_err(|_| format!("`{key}` {value}: use \"$N\" for the element step N produced"))?;
        let (tool, produced) = match n.checked_sub(1).and_then(|i| done.get(i)) {
            Some(d) if n < step => d,
            _ => {
                return Err(format!(
                    "`{key}` \"${n}\": refer to an earlier step (1 to {})",
                    step - 1
                ));
            }
        };
        *value = match produced {
            StepElement::Element(rid) => json!(rid),
            StepElement::Failed => {
                return Err(format!("`{key}` \"${n}\": step {n} ({tool}) failed"));
            }
            StepElement::NoElement => {
                return Err(format!(
                    "`{key}` \"${n}\": step {n} ({tool}) produced no element \
                     (find, wait_for, and tools given an element do)"
                ));
            }
        };
    }
    Ok(())
}

/// Longest `wait_for` / `pause`, in ms (10 minutes).
const MAX_WAIT_MS: i64 = 600_000;
/// `wait_for` re-checks when the app changed (`Backend::wait_change`), and
/// after this long without a change in any case: for apps or events that
/// do not report changes, such as an element scrolled into view.
const WAIT_FALLBACK: Duration = Duration::from_millis(250);
/// Least time between the end of one `wait_for` check and the next, so an
/// app that changes all the time costs no more than polling did.
const WAIT_MIN_GAP: Duration = Duration::from_millis(50);

/// Requested wait in ms, clamped to 0..=MAX_WAIT_MS, and a note when it was cut.
fn wait_ms(a: &Json, k: &str, d: i64) -> (u64, String) {
    let asked = arg_i(a, k, d);
    let ms = asked.clamp(0, MAX_WAIT_MS);
    let note = if asked > MAX_WAIT_MS {
        format!(" ({k} {asked} is above the {MAX_WAIT_MS} ms limit; used {MAX_WAIT_MS})")
    } else {
        String::new()
    };
    (ms as u64, note)
}

/// How long `launch` waits for the app's window by default, and at most.
const LAUNCH_WAIT_MS: i64 = 15_000;
const MAX_LAUNCH_WAIT_MS: i64 = 60_000;

/// Screen point of a screenshot pixel: origin plus pixel times scale.
#[derive(Clone, Copy, Debug, PartialEq)]
struct ShotMap {
    x: f64,
    y: f64,
    sx: f64,
    sy: f64,
}

impl ShotMap {
    /// From the image size in pixels and the screen area it shows.
    fn new((iw, ih): (u32, u32), (x, y, w, h): (f64, f64, f64, f64)) -> Option<Self> {
        (iw > 0 && ih > 0 && w > 0.0 && h > 0.0).then(|| ShotMap {
            x,
            y,
            sx: w / iw as f64,
            sy: h / ih as f64,
        })
    }

    fn to_screen(self, px: i64, py: i64) -> (i64, i64) {
        (
            (self.x + (px as f64 + 0.5) * self.sx).floor() as i64,
            (self.y + (py as f64 + 0.5) * self.sy).floor() as i64,
        )
    }
}

/// A tool's reply: text, plus base64 PNGs sent as MCP image blocks.
struct Out {
    text: String,
    images: Vec<String>,
}

impl From<String> for Out {
    fn from(text: String) -> Self {
        Out {
            text,
            images: Vec::new(),
        }
    }
}

/// Longest side of a screenshot sent back, unless `max_size` says otherwise.
/// Image tokens grow with the area: 1024 costs about 0.64 of 1280 and
/// still shows a 1536 px wide window's text legibly.
const SHOT_MAX_SIZE: i64 = 1024;

/// How long set_text waits for the field to show what was written.
const SET_TEXT_VERIFY: Duration = Duration::from_millis(300);
/// Intermediate pointer moves in a drag, so apps see motion, not a jump.
const DRAG_STEPS: i64 = 12;
const DRAG_STEP_GAP: Duration = Duration::from_millis(12);

/// Whether a field's contents equal what was written, ignoring CRLF vs LF
/// and a trailing newline the widget may add.
fn written_matches(written: &str, now: &str) -> bool {
    let norm = |t: &str| t.replace("\r\n", "\n").trim_end_matches('\n').to_string();
    norm(written) == norm(now)
}

/// What the on-screen indicator says while a tool runs; None for tools that
/// only read.
fn indicator_label(tool: &str, a: &Json) -> Option<String> {
    Some(match tool {
        "click" | "act" => "clicking".into(),
        "focus" => "focusing".into(),
        "key" => format!("pressing {}", arg_s(a, "combo", "a key")),
        "type_text" | "set_text" => "typing".into(),
        "mouse" => "using the mouse".into(),
        "drag" => "dragging".into(),
        "scroll" => "scrolling".into(),
        "activate_window" => "switching windows".into(),
        "move_window" => "arranging windows".into(),
        "clipboard_set" => "copying".into(),
        "launch" => format!("launching {}", arg_s(a, "app", "an app")),
        "file_dialog" => "choosing a file".into(),
        _ => return None,
    })
}

/// How long move_window waits for a window to take its new geometry.
const WINDOW_SETTLE: Duration = Duration::from_millis(1000);

/// Whether a window shows what `change` asked for (within a pixel, as
/// fractional scaling rounds sizes).
fn window_reached(w: &WindowGeo, c: &WindowChange) -> bool {
    let near = |want: Option<f64>, got: f64| want.is_none_or(|v| (v - got).abs() <= 1.0);
    let state = match c.state.as_deref() {
        Some("maximize") => w.maximized && !w.minimized,
        Some("minimize") => w.minimized,
        Some("restore") => !w.maximized && !w.minimized,
        _ => true,
    };
    state && near(c.x, w.x) && near(c.y, w.y) && near(c.width, w.width) && near(c.height, w.height)
}

/// `"title" [app] at (x,y) WxH {active,maximized} on OUTPUT`.
fn describe_window(w: &WindowGeo) -> String {
    let states: Vec<&str> = [
        (w.active, "active"),
        (w.maximized, "maximized"),
        (w.minimized, "minimized"),
    ]
    .iter()
    .filter(|(on, _)| *on)
    .map(|(_, s)| *s)
    .collect();
    let mut line = format!(
        "{} [{}] at ({},{}) {}x{}",
        render::py_repr(&w.title),
        w.app,
        w.x,
        w.y,
        w.width,
        w.height
    );
    if !states.is_empty() {
        line.push_str(&format!(" {{{}}}", states.join(",")));
    }
    if !w.output.is_empty() {
        line.push_str(&format!(" on {}", w.output));
    }
    for t in &w.terminals {
        line.push_str(&format!(
            "\n  {}: {} (in {})",
            t.tty,
            render::py_repr(&t.command),
            t.cwd
        ));
    }
    line
}

/// Apps whose file dialogs serve other apps (the desktop portal): one of
/// theirs is only filled in for the allowed app it belongs to.
const PORTAL_APPS: [&str; 1] = ["xdg-desktop-portal-kde"];
/// How long file_dialog waits for the default button to become enabled
/// after the path was written.
const ACCEPT_ENABLE_WAIT: Duration = Duration::from_millis(1000);

/// The allowed app a file dialog belongs to, or why it may not be filled
/// in. An allowed app's own (in-process) dialog is that app's. A portal
/// dialog belongs to the transient parent KWin reports for it (the window
/// the requesting app exported); without one it is refused, as its title
/// alone could be made to name any window.
fn file_dialog_owner<Id>(d: &FileDialog<Id>, allowed: &[String]) -> R<String> {
    let app = d.app.to_lowercase();
    if allowed.contains(&app) {
        return Ok(app);
    }
    if !PORTAL_APPS.contains(&app.as_str()) {
        return Err(format!(
            "the active file dialog belongs to '{}', which is not allowed",
            d.app
        ));
    }
    if let Some(parent) = &d.parent {
        let parent = parent.to_lowercase();
        return if allowed.contains(&parent) {
            Ok(parent)
        } else {
            Err(format!(
                "the active file dialog was opened by '{}', which is not allowed",
                if parent.is_empty() {
                    "unknown"
                } else {
                    &parent
                }
            ))
        };
    }
    Err(format!(
        "the active file dialog ({}) names no parent window, so it cannot be tied to an allowed app",
        render::py_repr(d.titles.first().map(String::as_str).unwrap_or(""))
    ))
}

/// `raw` with a leading `~` expanded to `home`; Err unless the result is an
/// absolute path on one line.
fn expand_path(raw: &str, home: Option<&std::path::Path>) -> R<PathBuf> {
    let raw = raw.trim();
    if raw.is_empty() || raw.contains(['\n', '\r']) {
        return Err("path must be one non-empty line".into());
    }
    let path = match raw.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => home
            .ok_or("cannot expand ~: HOME is not set")?
            .join(rest.trim_start_matches('/')),
        Some(_) => return Err("only ~ and ~/… are expanded, not ~user".into()),
        None => PathBuf::from(raw),
    };
    if !path.is_absolute() {
        return Err(format!(
            "'{raw}' is not absolute; give a full path or one starting with ~/"
        ));
    }
    Ok(path)
}

/// Check `path` can be what an Open or Save dialog returns: an existing
/// file (or folder) to open, a name in an existing folder to save.
fn check_dialog_path(path: &std::path::Path, action: &str) -> R<()> {
    match action {
        "open" if !path.exists() => Err(format!("{} does not exist", path.display())),
        "save" if path.is_dir() => Err(format!("{} is a folder", path.display())),
        "save" if !path.parent().is_some_and(|p| p.is_dir()) => {
            Err(format!("the folder of {} does not exist", path.display()))
        }
        "open" | "save" => Ok(()),
        other => Err(format!("action must be open or save, not '{other}'")),
    }
}

/// Roles that take typed text without the editable state.
const TEXT_SINKS: [&str; 2] = ["terminal", "console"];

/// One end of a drag before the element (if any) is measured.
enum DragEnd<Id> {
    Element(Id),
    Point((i64, i64)),
}

/// An element picked by `id`, or found by `app` + `query`.
struct Target<Id> {
    id: Id,
    /// `[id] role "name"` when found by query, so the reply says what was hit.
    found: Option<String>,
}

impl<Id> Target<Id> {
    /// Prefix a one-line reply with the element a query picked.
    fn reply(&self, text: &str) -> String {
        match &self.found {
            Some(d) => format!("{d}: {text}"),
            None => text.to_string(),
        }
    }
}

fn arg_s(a: &Json, k: &str, d: &str) -> String {
    a.get(k).and_then(Json::as_str).unwrap_or(d).to_string()
}

fn arg_i(a: &Json, k: &str, d: i64) -> i64 {
    a.get(k)
        .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
        .unwrap_or(d)
}

fn arg_b(a: &Json, k: &str, d: bool) -> bool {
    a.get(k).and_then(Json::as_bool).unwrap_or(d)
}

fn req_s(a: &Json, k: &str) -> R<String> {
    a.get(k)
        .and_then(Json::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("missing string argument '{k}'"))
}

fn req_i(a: &Json, k: &str) -> R<i64> {
    a.get(k)
        .and_then(Json::as_i64)
        .ok_or_else(|| format!("missing integer argument '{k}'"))
}

/// Poll `check` every few ms until it is true or `limit` passes.
async fn wait_for<F, Fut>(limit: Duration, mut check: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let start = Instant::now();
    loop {
        if check().await {
            return true;
        }
        if start.elapsed() >= limit {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

impl<B: Backend> Server<B> {
    fn new(b: B) -> Self {
        let allow_file = b.allow_file();
        Server {
            b,
            refs: Mutex::new(Refs {
                map: HashMap::new(),
                index: HashMap::new(),
                next: 1,
            }),
            allow_file,
            last_active: Mutex::new(None),
            last_shot: Mutex::new(None),
            input: Mutex::new(()),
            inert_writes: Mutex::new(HashSet::new()),
            held: std::sync::Mutex::new(Vec::new()),
        }
    }

    // ------------------------------------------------------------ held input

    fn held_names(&self) -> Vec<String> {
        let held = self.held.lock().unwrap();
        held.iter().map(|h| h.name().to_string()).collect()
    }

    fn is_held(&self, h: &Held) -> bool {
        self.held.lock().unwrap().contains(h)
    }

    /// "; held: ctrl, left" while anything is held, for input replies.
    fn held_note(&self) -> String {
        let names = self.held_names();
        if names.is_empty() {
            String::new()
        } else {
            format!("; held: {}", names.join(", "))
        }
    }

    /// Press and record `h`. It is recorded first: a call cancelled during
    /// the press still leaves it for `release_all`.
    async fn press_held(&self, h: &Held) -> R<()> {
        self.held.lock().unwrap().push(h.clone());
        let pressed = match h {
            Held::Key(k) => self.b.key(k, true).await,
            Held::Button(b) => self.b.button(b, true).await,
        };
        if pressed.is_err() {
            self.forget(h);
        }
        pressed
    }

    /// Release `h` (even when not recorded: harmless, and it frees a key
    /// stuck some other way). Forgotten only once the release was sent, so
    /// a cancelled release is retried by `release_all`; a failed one is
    /// forgotten too, or every later reply would list it.
    async fn release_held(&self, h: &Held) -> R<()> {
        let released = match h {
            Held::Key(k) => self.b.key(k, false).await,
            Held::Button(b) => self.b.button(b, false).await,
        };
        self.forget(h);
        released
    }

    fn forget(&self, h: &Held) {
        let mut held = self.held.lock().unwrap();
        if let Some(i) = held.iter().rposition(|x| x == h) {
            held.remove(i);
        }
    }

    /// Release everything held, last pressed first, under the input lock
    /// (wait for the input running now). Called after a failed call, a
    /// cancelled one and at exit; returns the names released and the first
    /// error.
    async fn release_all(&self) -> (Vec<String>, R<()>) {
        let _input = self.input.lock().await;
        self.release_all_held().await
    }

    /// `release_all` for a caller holding the input lock.
    async fn release_all_held(&self) -> (Vec<String>, R<()>) {
        let mut names = Vec::new();
        let mut result = Ok(());
        loop {
            let last = self.held.lock().unwrap().last().cloned();
            let Some(h) = last else { break };
            let r = self.release_held(&h).await;
            names.push(h.name().to_string());
            if result.is_ok() {
                result = r;
            }
        }
        (names, result)
    }

    /// Run `action` with `mods` pressed: modifiers go down in order, the
    /// action runs, they come up in reverse order, whatever the action
    /// returned. Modifiers already held (by `hold`) are left as they are.
    async fn with_modifiers<T>(&self, mods: &[String], action: impl Future<Output = R<T>>) -> R<T> {
        let mut pressed: Vec<Held> = Vec::new();
        let mut ready = Ok(());
        for m in mods {
            let h = Held::Key(m.clone());
            if self.is_held(&h) {
                continue;
            }
            if let Err(e) = self.press_held(&h).await {
                ready = Err(e);
                break;
            }
            pressed.push(h);
        }
        let out = match ready {
            Ok(()) => action.await,
            Err(e) => Err(e),
        };
        let mut released = Ok(());
        for h in pressed.iter().rev() {
            let r = self.release_held(h).await;
            if released.is_ok() {
                released = r;
            }
        }
        let out = out?;
        released?;
        Ok(out)
    }

    /// Err when `button` is held: a click would release it.
    fn check_button_free(&self, button: &str) -> R<()> {
        if self.is_held(&Held::Button(button.to_string())) {
            return Err(format!(
                "the {button} button is held; release it with hold (down=false) first"
            ));
        }
        Ok(())
    }

    // ------------------------------------------------------------ allowlist

    fn allowed(&self) -> Vec<String> {
        if !self.allow_file.exists() {
            if let Some(dir) = self.allow_file.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(&self.allow_file, self.b.default_allow());
        }
        let text = std::fs::read_to_string(&self.allow_file).unwrap_or_default();
        let mut names: Vec<String> = text
            .lines()
            .map(|l| l.split('#').next().unwrap_or("").trim().to_lowercase())
            .filter(|l| !l.is_empty())
            .collect();
        names.sort();
        names.dedup();
        names
    }

    fn is_allowed(&self, app: &str) -> bool {
        self.allowed().contains(&app.trim().to_lowercase())
    }

    fn not_allowed(&self, app: &str) -> String {
        format!("app '{app}' is not in {}", self.allow_file.display())
    }

    async fn require_active_allowed(&self) -> R<String> {
        let name = self.b.active_app().await;
        if !self.is_allowed(&name) {
            // Name the window this server last brought forward, so the
            // retry (with `window`) fixes it.
            let hint = match self.last_active.lock().await.as_deref() {
                Some(t) if !t.is_empty() => format!(" (last used: '{t}')"),
                _ => String::new(),
            };
            return Err(format!(
                "active window app '{}' is not in {}; pass window=<title or app> to \
                 activate an allowed window first{hint}",
                if name.is_empty() { "unknown" } else { &name },
                self.allow_file.display()
            ));
        }
        Ok(name)
    }

    // ------------------------------------------------------------ ids

    /// Issue an id for `id`, remembering the role and name in `info`. An element
    /// read again (same backend object, same app) keeps its id while it
    /// still passes the identity check `resolve_with_app` makes; otherwise
    /// (renamed, other role) it gets a new one, and the old id stays stale.
    async fn add_ref(&self, id: &B::Id, info: &Info<B::Id>, app: &str) -> u64 {
        let mut guard = self.refs.lock().await;
        let refs = &mut *guard;
        let key = (id.clone(), app.to_string());
        let check_name = name_is_stable(info);
        if let Some(&rid) = refs.index.get(&key)
            && let Some(r) = refs.map.get_mut(&rid)
            && r.role == info.role
            && r.check_name == check_name
            && (!check_name || r.name == info.name)
        {
            // Unchecked names may change: keep the latest for messages.
            r.name.clone_from(&info.name);
            return rid;
        }
        if refs.map.len() > MAX_REFS {
            // Numbers keep counting up: a cleared id reads as unknown,
            // never as another element.
            refs.map.clear();
            refs.index.clear();
        }
        let rid = refs.next;
        refs.next += 1;
        let r = Ref {
            id: id.clone(),
            app: app.to_string(),
            role: info.role.clone(),
            name: info.name.clone(),
            check_name,
        };
        refs.map.insert(rid, r);
        refs.index.insert(key, rid);
        rid
    }

    /// The element and app behind an id, after checking the allowlist and
    /// that the element still has the role (and stable name) it was issued
    /// with. Every use of an id goes through here, reads included, so a
    /// stale id never acts on, or reads, another element.
    async fn resolve_with_app(&self, rid: i64) -> R<(B::Id, String)> {
        let found = self.refs.lock().await.map.get(&(rid as u64)).cloned();
        let r = found.ok_or_else(|| format!("unknown id {rid}; call tree/find again"))?;
        if !self.is_allowed(&r.app) {
            return Err(self.not_allowed(&r.app));
        }
        let (role, name) = self.b.identity(&r.id).await;
        if role != r.role || (r.check_name && name != r.name) {
            let now = if role.is_empty() {
                "now gone or not answering".to_string()
            } else {
                format!("now {}", render::role_and_name(&role, &name))
            };
            return Err(format!(
                "stale id {rid} (was {}, {now}); call tree/find again",
                render::role_and_name(&r.role, &r.name)
            ));
        }
        Ok((r.id, r.app))
    }

    async fn resolve(&self, rid: i64) -> R<B::Id> {
        Ok(self.resolve_with_app(rid).await?.0)
    }

    async fn app_windows(&self, name: &str) -> R<Vec<B::Id>> {
        if !self.is_allowed(name) {
            return Err(self.not_allowed(name));
        }
        self.b
            .app_windows(&name.to_lowercase())
            .await
            .ok_or_else(|| format!("app '{name}' not found in the accessibility tree"))
    }

    /// Fetch the visible part of `app` and pick the element `query` names;
    /// window and tab titles only with `titles` (see render::TITLE_ROLES).
    /// The cache holds no action names: `fill_actions` before describing.
    async fn locate(
        &self,
        app: &str,
        query: &str,
        role: &str,
        titles: bool,
    ) -> R<Option<(B::Id, Cache<B::Id>)>> {
        let windows = self.app_windows(app).await?;
        Ok(self.locate_in(windows, query, role, titles).await)
    }

    /// `locate` in the app windows `windows`.
    async fn locate_in(
        &self,
        windows: Vec<B::Id>,
        query: &str,
        role: &str,
        titles: bool,
    ) -> Option<(B::Id, Cache<B::Id>)> {
        let mut cache = Cache::new();
        self.b
            .prefetch_with(
                &mut cache,
                windows.clone(),
                &search_fetch(render::FIND_TEXT_LIMIT, render::FIND_MAX_SEEN),
            )
            .await;
        loop {
            match render::locate(&cache, &windows, query, role, titles) {
                Ok(found) => return found.map(|id| (id, cache)),
                Err(missing) => {
                    self.b
                        .prefetch_with(
                            &mut cache,
                            missing,
                            &search_fetch(render::FIND_TEXT_LIMIT, PREFETCH_MORE),
                        )
                        .await;
                }
            }
        }
    }

    /// The element a tool call names: `id`, or `app` + `query` (+ `role`).
    async fn target(&self, a: &Json) -> R<Target<B::Id>> {
        if let Some(rid) = a.get("id").and_then(Json::as_i64) {
            let id = self.resolve(rid).await?;
            note_step_element(rid as u64, true);
            return Ok(Target { id, found: None });
        }
        let (Some(app), Some(query)) = (
            a.get("app").and_then(Json::as_str),
            a.get("query").and_then(Json::as_str),
        ) else {
            return Err("pass `id` (from tree/find), or `app` and `query`".into());
        };
        self.b.enable().await;
        let role = arg_s(a, "role", "");
        let (id, mut cache) = self.locate(app, query, &role, true).await?.ok_or_else(|| {
            let role = if role.is_empty() {
                String::new()
            } else {
                format!(" with role '{role}'")
            };
            format!("no visible element in '{app}' matches '{query}'{role}")
        })?;
        self.b
            .fill_actions(&mut cache, std::slice::from_ref(&id))
            .await;
        let rid = self.add_ref(&id, &cache[&id], &app.to_lowercase()).await;
        note_step_element(rid, true);
        let found = Some(render::describe(&cache[&id], rid));
        Ok(Target { id, found })
    }

    /// Title of the active window, if any.
    async fn active_title(&self) -> Option<String> {
        self.b
            .list_apps()
            .await
            .into_iter()
            .flat_map(|a| a.windows)
            .find(|w| w.active)
            .map(|w| w.title)
    }

    /// Append a note when an action changed the active window (a dialog
    /// opened, a window closed), so no tree call is needed to notice. Only
    /// for `act`: apps run actions before replying, while keys and clicks
    /// take effect after the call returns (use wait_for for those).
    async fn note_active_change(&self, mut reply: String) -> String {
        let now = self.active_title().await;
        let mut last = self.last_active.lock().await;
        if now != *last {
            if let Some(title) = &now {
                reply.push_str(&format!("; active window now {}", render::py_repr(title)));
            }
            *last = now;
        }
        reply
    }

    /// The lines as text, and the id of the first element printed.
    async fn print(
        &self,
        lines: Vec<Line<B::Id>>,
        cache: &Cache<B::Id>,
        app: &str,
    ) -> (String, Option<u64>) {
        let mut out = Vec::with_capacity(lines.len());
        let mut first = None;
        for line in lines {
            out.push(match line {
                Line::Raw(s) => Printed::Raw(s),
                Line::Node { depth, id } => {
                    let rid = self.add_ref(&id, &cache[&id], app).await;
                    first.get_or_insert(rid);
                    Printed::Node {
                        depth,
                        rid,
                        info: &cache[&id],
                    }
                }
            });
        }
        (render::format(&out), first)
    }

    // ------------------------------------------------------------ tools

    async fn apps(&self) -> R<String> {
        self.b.enable().await;
        let list = self.b.list_apps().await;
        Ok(apps_text(
            list,
            &self.allowed(),
            &self.allow_file,
            self.b.apps_note(),
        ))
    }

    async fn windows(&self, a: &Json) -> R<String> {
        let query = arg_s(a, "query", "");
        let allowed = self.allowed();
        let (screens, list) = self.b.screens_and_windows(&query, &allowed).await;
        let mut lines: Vec<String> = screens
            .iter()
            .map(|s| {
                format!(
                    "screen {}: at ({},{}) {}x{}, scale {}",
                    s.name, s.x, s.y, s.width, s.height, s.scale
                )
            })
            .collect();
        let list = list?;
        if list.is_empty() {
            lines.push(if query.is_empty() {
                "no windows of allowed apps".into()
            } else {
                format!("no window of an allowed app matches '{query}'")
            });
        }
        lines.extend(list.iter().map(describe_window));
        Ok(lines.join("\n"))
    }

    async fn move_window(&self, a: &Json) -> R<String> {
        let query = req_s(a, "query")?;
        let num = |k: &str| a.get(k).and_then(Json::as_f64);
        let change = WindowChange {
            x: num("x"),
            y: num("y"),
            width: num("width").map(|w| w.max(1.0)),
            height: num("height").map(|h| h.max(1.0)),
            state: a.get("state").and_then(Json::as_str).map(str::to_string),
        };
        if let Some(st) = &change.state
            && !["maximize", "minimize", "restore"].contains(&st.as_str())
        {
            return Err(format!(
                "state must be maximize, minimize or restore, not '{st}'"
            ));
        }
        if change.state.is_none()
            && [change.x, change.y, change.width, change.height]
                .iter()
                .all(Option::is_none)
        {
            return Err("pass x/y, width/height or state".into());
        }
        let allowed = self.allowed();
        let mut w = self.b.set_window(&query, &allowed, &change).await?;
        // On Wayland the app applies a new size (and KWin a state) a moment
        // later; wait for it so the reply, and the next call, see the result.
        let start = Instant::now();
        while !window_reached(&w, &change) && start.elapsed() < WINDOW_SETTLE {
            tokio::time::sleep(Duration::from_millis(25)).await;
            let now = self.b.windows(&w.title, &allowed).await?;
            match now.into_iter().find(|n| n.app == w.app) {
                Some(n) => w = n,
                None => break,
            }
        }
        let note = if window_reached(&w, &change) {
            ""
        } else {
            " (not what was asked: the app may limit its size or position)"
        };
        Ok(format!("now: {}{note}", describe_window(&w)))
    }

    async fn launch(&self, a: &Json) -> R<String> {
        let app = req_s(a, "app")?.trim().to_lowercase();
        // Refused before anything is resolved or started.
        if !self.is_allowed(&app) {
            return Err(self.not_allowed(&app));
        }
        let bad_args = || "args must be a list of strings".to_string();
        let args = match a.get("args") {
            None | Some(Json::Null) => Vec::new(),
            Some(Json::Array(v)) => v
                .iter()
                .map(|x| x.as_str().map(str::to_string).ok_or_else(bad_args))
                .collect::<R<Vec<_>>>()?,
            Some(_) => return Err(bad_args()),
        };
        let asked = arg_i(a, "wait_ms", LAUNCH_WAIT_MS);
        let ms = asked.clamp(0, MAX_LAUNCH_WAIT_MS);
        let cut = if asked > MAX_LAUNCH_WAIT_MS {
            format!(" (wait_ms {asked} is above the {MAX_LAUNCH_WAIT_MS} ms limit)")
        } else {
            String::new()
        };
        let wait = Duration::from_millis(ms as u64);
        let l = self.b.launch(&app, &args, &self.allowed(), wait).await?;
        let started = format!("started {} (pid {})", l.command, l.pid);
        match l.window {
            Some(w) => {
                if w.active {
                    *self.last_active.lock().await = Some(w.title.clone());
                }
                let how = if l.note.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", l.note)
                };
                Ok(format!("{started}; window{how}: {}", describe_window(&w)))
            }
            None if !l.note.is_empty() => Ok(format!("{started}; {}", l.note)),
            None => Err(format!(
                "{started}, but no window of '{app}' appeared within {ms} ms{cut}; \
                 it may still be starting (see windows)"
            )),
        }
    }

    async fn doctor(&self) -> R<String> {
        self.b.enable().await;
        let allow = self.allowed();
        let mut lines = vec![format!(
            "desktop-mcp {} ({} {})",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH
        )];
        let mut row = |ok: bool, name: &str, detail: &str| {
            lines.push(format!(
                "[{}] {name}: {detail}",
                if ok { "ok" } else { "FAIL" }
            ));
        };
        row(
            !allow.is_empty(),
            "allowlist",
            &format!(
                "{} ({} apps: {})",
                self.allow_file.display(),
                allow.len(),
                allow.join(", ")
            ),
        );
        let (mut allowed, mut blocked) = (Vec::new(), Vec::new());
        for app in self.b.list_apps().await {
            if app.windows.is_empty() {
                continue;
            }
            if allow.contains(&app.name.to_lowercase()) {
                allowed.push(app.name);
            } else {
                blocked.push(app.name);
            }
        }
        row(
            true,
            "running apps with windows",
            &format!(
                "allowed: {}; blocked: {}",
                if allowed.is_empty() {
                    "none".into()
                } else {
                    allowed.join(", ")
                },
                if blocked.is_empty() {
                    "none".into()
                } else {
                    blocked.join(", ")
                }
            ),
        );
        for c in self.b.doctor().await {
            row(c.ok, c.name, &c.detail);
        }
        if !self.b.apps_note().is_empty() {
            lines.push(format!("note: {}", self.b.apps_note()));
        }
        Ok(lines.join("\n"))
    }

    async fn tree(&self, a: &Json) -> R<String> {
        self.b.enable().await;
        let root_id = arg_i(a, "root_id", 0);
        let opts = WalkOpts {
            max_depth: arg_i(a, "max_depth", 12).max(0) as usize,
            budget: arg_i(a, "max_nodes", 300).max(0) as usize,
            only_showing: !arg_b(a, "include_hidden", false),
            boring_roles: self.b.boring_roles(),
        };
        let mut cache = Cache::new();
        let (header, roots, app) = if root_id != 0 {
            let (id, app) = self.resolve_with_app(root_id).await?;
            let info = self.b.info(&id, TREE_TEXT_LIMIT).await;
            let rid = self.add_ref(&id, &info, &app).await;
            let header = render::describe(&info, rid);
            let kids = info.children.clone();
            cache.insert(id, info);
            (header, kids, app)
        } else if arg_b(a, "document", false) {
            let app = arg_s(a, "app", "");
            let windows = self.app_windows(&app).await?;
            let doc = self.active_document(&app, windows, &mut cache).await?;
            let rid = self.add_ref(&doc, &cache[&doc], &app.to_lowercase()).await;
            let header = render::describe(&cache[&doc], rid);
            let kids = cache[&doc].children.clone();
            (header, kids, app.to_lowercase())
        } else {
            let app = arg_s(a, "app", "");
            let windows = self.app_windows(&app).await?;
            (app.clone(), windows, app.to_lowercase())
        };
        let first = (opts.budget * 4).max(1000);
        self.b
            .prefetch(
                &mut cache,
                roots.clone(),
                opts.only_showing,
                TREE_TEXT_LIMIT,
                first,
            )
            .await;
        let lines = loop {
            let mut lines = vec![Line::Raw(header.clone())];
            match render::walk(&cache, &roots, 1, &opts, &mut lines) {
                Ok(()) => break lines,
                Err(missing) => {
                    self.b
                        .prefetch(
                            &mut cache,
                            missing,
                            opts.only_showing,
                            TREE_TEXT_LIMIT,
                            PREFETCH_MORE,
                        )
                        .await;
                }
            }
        };
        Ok(self.print(lines, &cache, &app).await.0)
    }

    /// The page or document of `app`'s active window (else its first visible
    /// window): in a browser, the current tab's web page without the tabs and
    /// toolbars around it. Fills `cache` on the way, which the tree reuses.
    async fn active_document(
        &self,
        app: &str,
        windows: Vec<B::Id>,
        cache: &mut Cache<B::Id>,
    ) -> R<B::Id> {
        // The window around the document only: the tree fetches the
        // document's content itself, with what it needs.
        let fetch = |limit| Fetch {
            stop_roles: &render::DOCUMENT_ROLES,
            ..search_fetch(TREE_TEXT_LIMIT, limit)
        };
        // The windows themselves (limit 1 stops after their level), and the
        // document each says it shows, at once.
        let level = fetch(1);
        let (_, embedded) = tokio::join!(
            self.b.prefetch_with(cache, windows.clone(), &level),
            futures_util::future::join_all(windows.iter().map(|w| self.b.embedded_document(w)))
        );
        let (window, embedded) = windows
            .iter()
            .zip(embedded)
            .filter(|(w, _)| cache.get(*w).is_some_and(|i| i.showing))
            .max_by_key(|(w, _)| cache[*w].states.contains(&"active"))
            .map(|(w, d)| (w.clone(), d))
            .ok_or_else(|| format!("'{app}' has no visible window"))?;
        if let Some(doc) = embedded {
            let info = self.b.info(&doc, TREE_TEXT_LIMIT).await;
            if info.showing && render::DOCUMENT_ROLES.contains(&info.role.as_str()) {
                cache.insert(doc.clone(), info);
                return Ok(doc);
            }
        }
        // Otherwise search the window for it.
        let below = cache[&window].children.clone();
        self.b
            .prefetch_with(cache, below, &fetch(render::FIND_MAX_SEEN))
            .await;
        loop {
            match render::first_with_role(
                cache,
                std::slice::from_ref(&window),
                &render::DOCUMENT_ROLES,
            ) {
                Ok(Some(doc)) => {
                    self.b.fill_actions(cache, std::slice::from_ref(&doc)).await;
                    return Ok(doc);
                }
                Ok(None) => {
                    return Err(format!(
                        "no document in the active window of '{app}'; call tree without `document`"
                    ));
                }
                Err(missing) => {
                    self.b
                        .prefetch_with(cache, missing, &fetch(PREFETCH_MORE))
                        .await;
                }
            }
        }
    }

    async fn find(&self, a: &Json) -> R<String> {
        self.b.enable().await;
        let query = req_s(a, "query")?;
        let app = req_s(a, "app")?;
        let role = arg_s(a, "role", "");
        let max_results = arg_i(a, "max_results", 30).max(0) as usize;
        let windows = self.app_windows(&app).await?;
        let mut cache = Cache::new();
        self.b
            .prefetch_with(
                &mut cache,
                windows.clone(),
                &search_fetch(render::FIND_TEXT_LIMIT, render::FIND_MAX_SEEN),
            )
            .await;
        let hits = loop {
            match render::find(&cache, &windows, &query, &role, max_results) {
                Ok(h) => break h,
                Err(missing) => {
                    self.b
                        .prefetch_with(
                            &mut cache,
                            missing,
                            &search_fetch(render::FIND_TEXT_LIMIT, PREFETCH_MORE),
                        )
                        .await;
                }
            }
        };
        if hits.is_empty() {
            return Ok("no matches".into());
        }
        self.b.fill_actions(&mut cache, &hits).await;
        let lines = hits
            .into_iter()
            .map(|id| Line::Node { depth: 0, id })
            .collect();
        let (text, first) = self.print(lines, &cache, &app.to_lowercase()).await;
        if let Some(rid) = first {
            note_step_element(rid, true);
        }
        Ok(text)
    }

    async fn act(&self, a: &Json) -> R<String> {
        let t = self.target(a).await?;
        let id = &t.id;
        let action = arg_s(a, "action", "");
        let names = self.b.actions(id).await;
        if names.is_empty() {
            return Err("element has no actions; try click, or focus + key".into());
        }
        let mut idx = 0;
        if !action.is_empty() {
            let lowered: Vec<String> = names.iter().map(|n| n.to_lowercase()).collect();
            idx = lowered
                .iter()
                .position(|n| *n == action.to_lowercase())
                .ok_or_else(|| {
                    let list: Vec<String> = names.iter().map(|n| render::py_repr(n)).collect();
                    format!("action '{action}' not in [{}]", list.join(", "))
                })?;
        }
        let ok = self.b.do_action(id, idx).await?;
        let reply = t.reply(&format!(
            "{} -> {}",
            names[idx],
            if ok { "ok" } else { "failed" }
        ));
        Ok(self.note_active_change(reply).await)
    }

    async fn set_text(&self, a: &Json) -> R<String> {
        let text = req_s(a, "text")?;
        let t = self.target(a).await?;
        if !self.b.set_text(&t.id, &text).await? {
            return Ok(t.reply("failed"));
        }
        if self.b.info(&t.id, 0).await.role.contains("password") {
            return Ok(t.reply("ok (password field: contents cannot be read back)"));
        }
        // Firefox reports success for accessibility writes and changes
        // nothing. Retype through the keyboard when the element takes focus;
        // never with a newline, which Enter would turn into a submit.
        let retypable = !text.contains('\n');
        let key = self.b.app_key(&t.id);
        let inert = match &key {
            Some(k) if retypable => self.inert_writes.lock().await.contains(k),
            _ => false,
        };
        // Where writes are known to do nothing, one look instead of waiting
        // SET_TEXT_VERIFY for the write to show.
        let patience = if inert {
            Duration::ZERO
        } else {
            SET_TEXT_VERIFY
        };
        let mut how = "";
        let mut seen = self.read_back(&t.id, &text, patience).await;
        if matches!(&seen, Ok(Some(_))) && retypable && self.retype(&t.id, &text).await {
            how = "; typed, the accessibility write had no effect";
            seen = self.read_back(&t.id, &text, SET_TEXT_VERIFY).await;
            if let (Ok(None), Some(k)) = (&seen, key) {
                self.inert_writes.lock().await.insert(k);
            }
        } else if inert && matches!(&seen, Ok(Some(_))) {
            // Could not retype: give the write its usual time after all.
            seen = self.read_back(&t.id, &text, SET_TEXT_VERIFY).await;
        }
        match seen {
            Ok(None) => Ok(t.reply(&format!("ok (verified{how})"))),
            Ok(Some(now)) => {
                let shown: String = now.chars().take(120).collect();
                let more = if now.chars().count() > 120 { "…" } else { "" };
                Err(t.reply(&format!(
                    "written, but the field now holds {shown:?}{more} ({} chars, {} asked{how}); \
                     it may limit or reformat input. Check with get_text before retrying.",
                    now.chars().count(),
                    text.chars().count()
                )))
            }
            Err(e) => Ok(t.reply(&format!("ok (not verified: {e})"))),
        }
    }

    /// Ok(None) once the element holds `text` (polled for up to `patience`,
    /// as some toolkits apply a change a moment later), else Ok(Some(what
    /// it holds)).
    async fn read_back(&self, id: &B::Id, text: &str, patience: Duration) -> R<Option<String>> {
        let limit = text.chars().count() + 64;
        let start = Instant::now();
        loop {
            let now = self.b.full_text(id, limit).await?;
            if written_matches(text, &now) {
                return Ok(None);
            }
            if start.elapsed() >= patience {
                return Ok(Some(now));
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    }

    /// Focus the element, select all and type `text`; false when the element
    /// would not take focus in an allowed active window (nothing typed).
    /// Runs under the input lock `set_text` (an INPUT_TOOLS entry) holds.
    async fn retype(&self, id: &B::Id, text: &str) -> bool {
        if self.require_active_allowed().await.is_err() {
            return false;
        }
        if !matches!(self.b.try_focus(id).await, Ok(Some(_))) && !self.b.is_focused(id).await {
            return false;
        }
        self.b.press_combo("ctrl+a").await.is_ok() && self.b.type_text(text).await.is_ok()
    }

    async fn get_text(&self, a: &Json) -> R<String> {
        let t = self.target(a).await?;
        let limit = arg_i(a, "limit", 20000).max(0) as usize;
        let mut text = self.b.full_text(&t.id, limit).await?;
        if render::has_embedded_objects(&text) {
            text = self.container_text(&t.id, limit).await;
        }
        Ok(match t.found {
            Some(d) => format!("{d}:\n{text}"),
            None => text,
        })
    }

    /// A web container's text with its descendants' text in place of the
    /// U+FFFC placeholders (see `render::container_text`). Hidden nodes are
    /// read too: text scrolled out of view is part of the page.
    async fn container_text(&self, id: &B::Id, limit: usize) -> String {
        let fetch = |nodes| Fetch {
            only_showing: false,
            ..search_fetch(limit, nodes)
        };
        let mut cache = Cache::new();
        self.b
            .prefetch_with(&mut cache, vec![id.clone()], &fetch(render::FIND_MAX_SEEN))
            .await;
        loop {
            match render::container_text(&cache, id, limit) {
                Ok(text) => return text,
                Err(missing) => {
                    let before = cache.len();
                    self.b
                        .prefetch_with(&mut cache, missing.clone(), &fetch(PREFETCH_MORE))
                        .await;
                    if cache.len() == before {
                        // Nodes that cannot be read (gone meanwhile) read
                        // as nothing.
                        for m in missing {
                            cache.entry(m).or_insert_with(|| Info {
                                role: String::new(),
                                name: String::new(),
                                states: Vec::new(),
                                showing: false,
                                actions: Vec::new(),
                                text: None,
                                children: Vec::new(),
                            });
                        }
                    }
                }
            }
        }
    }

    async fn wait_for(&self, a: &Json) -> R<String> {
        let app = req_s(a, "app")?;
        let query = req_s(a, "query")?;
        let role = arg_s(a, "role", "");
        let gone = arg_b(a, "gone", false);
        let (limit_ms, note) = wait_ms(a, "timeout_ms", 5000);
        let limit = Duration::from_millis(limit_ms);
        if !self.is_allowed(&app) {
            return Err(self.not_allowed(&app));
        }
        self.b.enable().await;
        let start = Instant::now();
        loop {
            // Changes from here on end the wait after this check.
            let mark = self.b.change_mark();
            // An app that is not running (yet) counts as "no match".
            let windows = self.app_windows(&app).await.unwrap_or_default();
            // Titles hold a page's name before the page is there.
            let found = if windows.is_empty() {
                None
            } else {
                self.locate_in(windows.clone(), &query, &role, false).await
            };
            let ms = start.elapsed().as_millis();
            match found {
                Some((id, mut cache)) if !gone => {
                    self.b
                        .fill_actions(&mut cache, std::slice::from_ref(&id))
                        .await;
                    let rid = self.add_ref(&id, &cache[&id], &app.to_lowercase()).await;
                    note_step_element(rid, true);
                    return Ok(format!(
                        "found after {ms} ms{note}: {}",
                        render::describe(&cache[&id], rid)
                    ));
                }
                None if gone => return Ok(format!("gone after {ms} ms{note}")),
                _ => {}
            }
            if start.elapsed() >= limit {
                let what = if gone { "to disappear" } else { "to appear" };
                return Err(format!(
                    "timed out after {ms} ms waiting for '{query}' in '{app}' {what}{note}"
                ));
            }
            let checked = Instant::now();
            let left = limit.saturating_sub(start.elapsed());
            self.b
                .wait_change(&windows, mark, left.min(WAIT_FALLBACK))
                .await;
            // Never more often than the old poll, however busy the app.
            tokio::time::sleep(WAIT_MIN_GAP.saturating_sub(checked.elapsed()).min(left)).await;
        }
    }

    async fn pause(&self, a: &Json) -> R<String> {
        let (ms, note) = wait_ms(a, "ms", 1000);
        tokio::time::sleep(Duration::from_millis(ms)).await;
        Ok(format!("paused {ms} ms{note}"))
    }

    async fn focus(&self, a: &Json) -> R<String> {
        let t = self.target(a).await?;
        let id = &t.id;
        if let Some(method) = self.b.try_focus(id).await? {
            return Ok(t.reply(&format!("ok ({method})")));
        }
        let (x, y) = self.active_center(id).await?;
        self.b.pointer(x as i64, y as i64, "left", 1, 0).await?;
        if wait_for(Duration::from_millis(150), || self.b.is_focused(id)).await {
            return Ok(t.reply(&format!("ok (clicked at {x},{y})")));
        }
        Ok(t.reply(&format!(
            "clicked at {x},{y} but element did not report focus"
        )))
    }

    async fn click(&self, a: &Json) -> R<String> {
        let button = arg_s(a, "button", "left");
        let clicks = arg_i(a, "clicks", 1).clamp(0, 3);
        let mods = modifiers_arg(a)?;
        if clicks > 0 {
            self.check_button_free(&button)?;
        }
        let (t, (x, y)) = self.target_center(a).await?;
        self.with_modifiers(
            &mods,
            self.b.pointer(x as i64, y as i64, &button, clicks, 0),
        )
        .await?;
        let m = with_modifiers_note(&mods);
        Ok(t.reply(&format!("clicked {button} x{clicks} at {x},{y}{m}")))
    }

    /// Centre of `id` for a pointer tool, once the active window is checked
    /// to be an allowed app's. Both lookups are reads and independent, so
    /// they run together; the allowlist error still comes first.
    async fn active_center(&self, id: &B::Id) -> R<(i32, i32)> {
        let (active, center) =
            tokio::join!(self.require_active_allowed(), self.b.element_center(id));
        active?;
        center
    }

    /// The element a pointer tool names and its centre, once the active
    /// window is checked to be an allowed app's. The active-window check
    /// runs alongside finding and measuring the element (all reads); errors
    /// keep the order of doing it step by step: the target, the active
    /// window, then the centre.
    async fn target_center(&self, a: &Json) -> R<(Target<B::Id>, (i32, i32))> {
        let (active, found) = tokio::join!(self.require_active_allowed(), async {
            let t = self.target(a).await?;
            let center = self.b.element_center(&t.id).await;
            Ok::<_, String>((t, center))
        });
        let (t, center) = found?;
        active?;
        Ok((t, center?))
    }

    /// One end of a drag: `<end>_id` (an element, resolved), else
    /// `<end>_x`/`<end>_y`, as screenshot pixels when `image` is true.
    async fn drag_end(&self, a: &Json, end: &str) -> R<DragEnd<B::Id>> {
        if let Some(rid) = a.get(format!("{end}_id")).and_then(Json::as_i64) {
            return Ok(DragEnd::Element(self.resolve_with_app(rid).await?.0));
        }
        let (x, y) = (
            req_i(a, &format!("{end}_x"))?,
            req_i(a, &format!("{end}_y"))?,
        );
        if arg_b(a, "image", false) {
            let map = (*self.last_shot.lock().await)
                .ok_or("image=true needs a screenshot taken first")?;
            return Ok(DragEnd::Point(map.to_screen(x, y)));
        }
        Ok(DragEnd::Point((x, y)))
    }

    /// Both ends of a drag in screen coordinates. The active-window check,
    /// both ends and their element centres are looked up together (the
    /// backend reads the window origin once); errors are reported in the
    /// order they would come one by one: active window, `from`, `to`.
    async fn drag_points(&self, a: &Json) -> R<(String, (i64, i64), (i64, i64))> {
        let (app, from, to) = tokio::join!(
            self.require_active_allowed(),
            self.drag_end(a, "from"),
            self.drag_end(a, "to")
        );
        let app = app?;
        let ids: Vec<B::Id> = [&from, &to]
            .into_iter()
            .filter_map(|e| match e {
                Ok(DragEnd::Element(id)) => Some(id.clone()),
                _ => None,
            })
            .collect();
        let mut centers = if ids.is_empty() {
            Vec::new()
        } else {
            self.b.element_centers(&ids).await
        }
        .into_iter();
        let mut point = |end: R<DragEnd<B::Id>>| -> R<(i64, i64)> {
            match end? {
                DragEnd::Point(p) => Ok(p),
                DragEnd::Element(_) => {
                    let (x, y) = centers.next().ok_or("element centre missing")??;
                    Ok((x as i64, y as i64))
                }
            }
        };
        let from = point(from)?;
        let to = point(to)?;
        Ok((app, from, to))
    }

    async fn drag(&self, a: &Json) -> R<String> {
        let button = arg_s(a, "button", "left");
        self.check_button_free(&button)?;
        let (app, (x0, y0), (x1, y1)) = self.drag_points(a).await?;
        self.b.move_pointer(x0, y0).await?;
        // Recorded as held while it is down: a cancelled drag releases it.
        let held = Held::Button(button.clone());
        self.press_held(&held).await?;
        let mut moved = Ok(());
        for i in 1..=DRAG_STEPS {
            let x = x0 + (x1 - x0) * i / DRAG_STEPS;
            let y = y0 + (y1 - y0) * i / DRAG_STEPS;
            moved = self.b.move_pointer(x, y).await;
            if moved.is_err() {
                break;
            }
            tokio::time::sleep(DRAG_STEP_GAP).await;
        }
        // Never leave the button held, whatever happened on the way.
        let released = self.release_held(&held).await;
        moved?;
        released?;
        Ok(format!(
            "dragged {button} from ({x0},{y0}) to ({x1},{y1}) in {app}"
        ))
    }

    async fn scroll(&self, a: &Json) -> R<String> {
        let amount = arg_i(a, "amount", 3).clamp(-50, 50);
        let mods = modifiers_arg(a)?;
        let (t, (x, y)) = self.target_center(a).await?;
        self.with_modifiers(&mods, self.b.pointer(x as i64, y as i64, "left", 0, amount))
            .await?;
        let m = with_modifiers_note(&mods);
        Ok(t.reply(&format!("scrolled {amount} at {x},{y}{m}")))
    }

    async fn key(&self, a: &Json) -> R<String> {
        let combo = req_s(a, "combo")?;
        let repeat = arg_i(a, "repeat", 1);
        self.b.check_combo(&combo)?;
        // Held keys stay down: pressing one again would release it.
        let keys: Vec<String> = chord_keys(&combo)
            .into_iter()
            .filter(|k| !self.is_held(&Held::Key(k.clone())))
            .collect();
        if keys.is_empty() {
            return Ok(format!(
                "nothing sent: {combo} is held already (hold with down=false releases it)"
            ));
        }
        let send = keys.join("+");
        let app = self.require_active_allowed().await?;
        for _ in 0..repeat.clamp(1, 100) {
            self.b.press_combo(&send).await?;
        }
        let focus = self.focus_note(false).await;
        Ok(format!("sent {combo} x{repeat} to {app}{focus}"))
    }

    async fn type_text(&self, a: &Json) -> R<String> {
        let text = req_s(a, "text")?;
        let app = self.require_active_allowed().await?;
        let note = self.b.type_text(&text).await?;
        let focus = self.focus_note(true).await;
        Ok(format!(
            "typed {} chars into {app}{focus}{note}",
            text.chars().count()
        ))
    }

    /// The clipboard's text. Only with an allowed app in front: the
    /// clipboard may hold anything the user copied anywhere.
    async fn clipboard_get(&self, a: &Json) -> R<String> {
        let limit = arg_i(a, "limit", 20000).max(0) as usize;
        self.require_active_allowed().await?;
        Ok(match self.b.clipboard_get(limit).await? {
            Clip::Empty => "(the clipboard is empty)".into(),
            Clip::Secret => {
                "(the clipboard holds a password, marked so by the app that copied it; not read)"
                    .into()
            }
            Clip::Other(types) => format!(
                "(the clipboard holds no text; types: {})",
                types.iter().take(5).cloned().collect::<Vec<_>>().join(", ")
            ),
            Clip::Text { text, truncated } if truncated => {
                format!("{text}\n(cut at {limit} chars)")
            }
            Clip::Text { text, .. } => text,
        })
    }

    async fn clipboard_set(&self, a: &Json) -> R<String> {
        let text = req_s(a, "text")?;
        self.require_active_allowed().await?;
        self.b.clipboard_set(&text).await?;
        Ok(format!("clipboard set to {} chars", text.chars().count()))
    }

    /// "; focus: [id] role "name"" for the element now holding focus, so a
    /// key or typed text that went somewhere unexpected shows at once. With
    /// `typing`, warns when that element does not take text. The backend
    /// names the app the element belongs to (it may differ from the one the
    /// input was sent to, when the input moved focus); nothing is reported,
    /// and no id issued, unless that app is allowed.
    async fn focus_note(&self, typing: bool) -> String {
        let Some(Focused { id, info, app }) = self.b.focused().await else {
            return String::new();
        };
        if !self.is_allowed(&app) {
            return String::new();
        }
        let rid = self.add_ref(&id, &info, &app.to_lowercase()).await;
        note_step_element(rid, false);
        let mut note = format!("; focus: {}", render::describe(&info, rid));
        if typing && !info.states.contains(&"editable") && !TEXT_SINKS.contains(&info.role.as_str())
        {
            note.push_str(" (not an editable field: check where the text went)");
        }
        note
    }

    async fn mouse(&self, a: &Json) -> R<String> {
        let (x, y) = match (a.get("image_x"), a.get("image_y")) {
            (Some(_), Some(_)) => {
                let map = (*self.last_shot.lock().await)
                    .ok_or("image_x/image_y need a screenshot taken first")?;
                map.to_screen(req_i(a, "image_x")?, req_i(a, "image_y")?)
            }
            _ => (req_i(a, "x")?, req_i(a, "y")?),
        };
        let button = arg_s(a, "button", "left");
        let clicks = arg_i(a, "clicks", 1).clamp(0, 3);
        let mods = modifiers_arg(a)?;
        if clicks > 0 {
            self.check_button_free(&button)?;
        }
        let app = self.require_active_allowed().await?;
        self.with_modifiers(
            &mods,
            self.b.pointer(x, y, &button, clicks, arg_i(a, "scroll", 0)),
        )
        .await?;
        let m = with_modifiers_note(&mods);
        Ok(format!("pointer at ({x},{y}) in {app}{m}"))
    }

    /// Press (`down`, the default) or release a key, chord or mouse button
    /// and leave it so across calls. down=false without key or button
    /// releases everything held.
    async fn hold(&self, a: &Json) -> R<String> {
        let down = arg_b(a, "down", true);
        let key = a.get("key").and_then(Json::as_str).map(str::trim);
        let button = a.get("button").and_then(Json::as_str).map(str::trim);
        let items: Vec<Held> = match (key, button) {
            (Some(_), Some(_)) => return Err("give `key` or `button`, not both".into()),
            (None, None) if !down => {
                let (names, r) = self.release_all_held().await;
                r?;
                return Ok(if names.is_empty() {
                    "nothing was held".into()
                } else {
                    format!("released {}; nothing held", names.join(", "))
                });
            }
            (None, None) => {
                return Err(
                    "hold needs `key` (e.g. \"shift\") or `button` (left, right, \
                            middle); down=false alone releases everything"
                        .into(),
                );
            }
            (Some(k), None) => {
                let keys = chord_keys(k);
                if keys.is_empty() {
                    return Err("empty `key`".into());
                }
                for k in &keys {
                    self.b.check_combo(k)?;
                }
                keys.into_iter().map(Held::Key).collect()
            }
            (None, Some(b)) => {
                if !BUTTONS.contains(&b) {
                    return Err(format!("unknown button '{b}' (left, right, middle)"));
                }
                vec![Held::Button(b.to_string())]
            }
        };
        let what = items.iter().map(|h| h.name()).collect::<Vec<_>>().join("+");
        if down {
            let app = self.require_active_allowed().await?;
            for h in &items {
                if !self.is_held(h) {
                    self.press_held(h).await?;
                }
            }
            return Ok(format!("holding {what} in {app}"));
        }
        let mut result = Ok(());
        for h in items.iter().rev() {
            let r = self.release_held(h).await;
            if result.is_ok() {
                result = r;
            }
        }
        result?;
        let rest = if self.held_names().is_empty() {
            "; nothing held"
        } else {
            ""
        };
        Ok(format!("released {what}{rest}"))
    }

    async fn cursor_position(&self) -> R<String> {
        let (x, y) = self.b.cursor_position().await?;
        Ok(format!("cursor at ({x},{y})"))
    }

    async fn activate_window(&self, a: &Json) -> R<String> {
        let query = match a.get("terminal").and_then(Json::as_str) {
            Some(t) => self.terminal_window(t).await?,
            None => req_s(a, "query")?,
        };
        let (_, app) = self.activate(&query).await?;
        Ok(format!(
            "active app now: {}",
            if app.is_empty() { "unknown" } else { &app }
        ))
    }

    /// Bring the first allowed window matching `query` to the front (the
    /// backend waits until it is), remember its title for
    /// `require_active_allowed`'s hint and `act`'s change note, and return
    /// that title and the active app. Callers hold the input lock.
    async fn activate(&self, query: &str) -> R<(Option<String>, String)> {
        let (title, app) = match self.b.activate_window(query, &self.allowed()).await? {
            Some(a) => (Some(a.title), a.app),
            None => (self.active_title().await, self.b.active_app().await),
        };
        *self.last_active.lock().await = title.clone();
        Ok((title, app))
    }

    /// Whether the active window is an allowed app's and matches `query` as
    /// activate_window would (title or app name contains it). Asks for the
    /// app first, which usually settles it in one read.
    async fn window_is_active(&self, query: &str) -> bool {
        let q = query.to_lowercase();
        let app = self.b.active_app().await;
        if !self.is_allowed(&app) {
            return false;
        }
        if app.to_lowercase().contains(&q) {
            return true;
        }
        match self.b.windows(query, &self.allowed()).await {
            Ok(list) => list.iter().any(|w| w.active),
            Err(_) => self
                .active_title()
                .await
                .is_some_and(|t| t.to_lowercase().contains(&q)),
        }
    }

    /// An input tool's `window`: activate the matching window unless it is
    /// already in front. Some(title) when it activated one. Runs inside
    /// `call`, under the input lock, so nothing else takes the focus
    /// between this and the tool's input.
    async fn ensure_window(&self, query: &str) -> R<Option<String>> {
        if self.window_is_active(query).await {
            return Ok(None);
        }
        let (title, _) = self.activate(query).await?;
        Ok(Some(title.unwrap_or_else(|| query.to_string())))
    }

    /// Title of the allowed window whose terminal runs a command, or is in a
    /// directory, containing `what` (case-insensitive).
    async fn terminal_window(&self, what: &str) -> R<String> {
        let q = what.to_lowercase();
        let list = self.b.windows("", &self.allowed()).await?;
        list.into_iter()
            .find(|w| {
                w.terminals.iter().any(|t| {
                    t.command.to_lowercase().contains(&q) || t.cwd.to_lowercase().contains(&q)
                })
            })
            .map(|w| w.title)
            .ok_or_else(|| {
                format!("no terminal of an allowed app runs or is in '{what}' (see windows)")
            })
    }

    /// Fill in the active file Open/Save dialog of an allowed app: write the
    /// path into its file name field, press its default button and wait for
    /// it to close. Nothing else in the dialog (or the portal) is exposed.
    async fn file_dialog(&self, a: &Json) -> R<String> {
        let action = arg_s(a, "action", "open").to_lowercase();
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let path = expand_path(&req_s(a, "path")?, home.as_deref())?;
        check_dialog_path(&path, &action)?;
        let (limit_ms, note) = wait_ms(a, "timeout_ms", 5000);
        self.b.enable().await;
        let d = self.b.active_file_dialog().await?;
        let allowed = self.allowed();
        let owner = file_dialog_owner(&d, &allowed)
            .map_err(|e| format!("{e} (allowlist: {})", self.allow_file.display()))?;
        let text = path.display().to_string();
        if !self.b.set_text(&d.field, &text).await? {
            return Err("the file name field refused the path".into());
        }
        if let Some(now) = self.read_back(&d.field, &text, SET_TEXT_VERIFY).await? {
            return Err(format!(
                "the file name field holds {now:?} instead of the path; nothing was chosen"
            ));
        }
        // The default button (Open/Save) gets its Press action once the
        // dialog accepts the name; pressing it sends no keyboard input.
        let mut how = "pressed the default button";
        let press = match &d.accept {
            Some(btn) => {
                let start = Instant::now();
                let mut names = self.b.actions(btn).await;
                while names.is_empty() && start.elapsed() < ACCEPT_ENABLE_WAIT {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    names = self.b.actions(btn).await;
                }
                names
                    .iter()
                    .position(|n| n.eq_ignore_ascii_case("press"))
                    .or((!names.is_empty()).then_some(0))
            }
            None => None,
        };
        match (press, &d.accept) {
            (Some(i), Some(btn)) => {
                self.b.do_action(btn, i).await?;
            }
            _ => {
                // Enter in the field accepts too; only while that same
                // dialog is still in front, and the field holds the focus.
                let now = self.b.active_file_dialog().await?;
                if now.window != d.window {
                    return Err("the file dialog is no longer the active window".into());
                }
                if !matches!(self.b.try_focus(&d.field).await, Ok(Some(_)))
                    && !self.b.is_focused(&d.field).await
                {
                    return Err("could not focus the file name field to press Enter".into());
                }
                self.b.press_combo("enter").await?;
                how = "pressed Enter in the file name field";
            }
        }
        let start = Instant::now();
        let closed = wait_for(Duration::from_millis(limit_ms), || async {
            !self.b.info(&d.window, 0).await.showing
        })
        .await;
        let ms = start.elapsed().as_millis();
        if !closed {
            return Err(format!(
                "wrote {text} and {how}, but the dialog is still open after {ms} ms{note}: \
                 it may ask to confirm overwriting a file, or show an error"
            ));
        }
        *self.last_active.lock().await = self.active_title().await;
        Ok(format!(
            "chose {text} in {owner}'s {action} dialog ({how}); dialog closed after {ms} ms{note}"
        ))
    }

    async fn screenshot(&self, a: &Json) -> R<Out> {
        self.require_active_allowed().await?;
        *self.last_shot.lock().await = None;
        let Shot { path, pixels, area } = self.b.screenshot().await.ok_or("screenshot failed")?;
        let max = arg_i(a, "max_size", SHOT_MAX_SIZE).clamp(64, 8192) as u32;
        let inline = arg_b(a, "inline", true);
        // Converting, shrinking and encoding take milliseconds: keep other
        // calls moving.
        let file = path.clone();
        let (size, png) = tokio::task::spawn_blocking(move || shot::finish(&file, pixels, max))
            .await
            .map_err(|e| e.to_string())??;
        let map = area.and_then(|a| ShotMap::new(size, a));
        *self.last_shot.lock().await = map;
        let (iw, ih) = size;
        let mut text = format!("{path}\n{iw}x{ih} px");
        if let (Some((x, y, w, h)), Some(_)) = (area, map) {
            // How to click a point in it is in the server instructions.
            text.push_str(&format!(" of screen area at ({x},{y}), {w}x{h}"));
        }
        Ok(Out {
            text,
            images: if inline {
                vec![shot::base64(&png)]
            } else {
                Vec::new()
            },
        })
    }

    /// Run `steps` in order. A batch with any input step holds the input
    /// lock for its whole run (its steps are called as already holding it),
    /// so two batches' input never interleaves. A step's `id`/`*_id`
    /// argument "$N" names the element step N produced (`STEP_ELEMENT`).
    async fn batch(&self, a: &Json) -> R<Out> {
        let steps = a
            .get("steps")
            .and_then(Json::as_array)
            .ok_or("missing array argument 'steps'")?;
        fn tool_of(step: &Json) -> &str {
            step.get("tool").and_then(Json::as_str).unwrap_or("")
        }
        if steps.iter().any(|s| tool_of(s) == "batch") {
            return Err("batch cannot contain batch".into());
        }
        // A step with `window` activates a window: input too.
        let input = steps.iter().any(|s| {
            let args = s.get("args").unwrap_or(&Json::Null);
            INPUT_TOOLS.contains(&tool_of(s)) || matches!(window_arg(tool_of(s), args), Ok(Some(_)))
        });
        let _input = if input {
            Some(self.input.lock().await)
        } else {
            None
        };
        let mut out = Vec::new();
        let mut images = Vec::new();
        let mut done: Vec<(String, StepElement)> = Vec::new();
        for (i, step) in steps.iter().enumerate() {
            let tool = tool_of(step);
            let mut args = step.get("args").cloned().unwrap_or_else(|| json!({}));
            // An optional step (typically a wait) may fail without costing
            // the steps after it, e.g. the final observation.
            let optional = step.get("optional").and_then(Json::as_bool) == Some(true);
            let result = match resolve_step_refs(&mut args, i + 1, &done) {
                Ok(()) => {
                    STEP_ELEMENT
                        .scope(std::cell::Cell::new(None), async {
                            let r = self.call_inner(tool, &args, input).await;
                            (r, STEP_ELEMENT.with(|e| e.get()))
                        })
                        .await
                }
                Err(e) => (Err(e), None),
            };
            let produced = match (&result.0, result.1) {
                (Err(_), _) => StepElement::Failed,
                (Ok(_), Some(rid)) => StepElement::Element(rid),
                (Ok(_), None) => StepElement::NoElement,
            };
            done.push((tool.to_string(), produced));
            match result.0 {
                Ok(o) => {
                    out.push(format!("[{}] {tool}: {}", i + 1, o.text));
                    images.extend(o.images);
                }
                Err(e) if optional => {
                    out.push(format!("[{}] {tool} failed (optional): {e}", i + 1));
                }
                Err(e) => {
                    out.push(format!("[{}] {tool} failed: {e}", i + 1));
                    if i + 1 < steps.len() {
                        out.push(format!("stopped; {} step(s) not run", steps.len() - i - 1));
                    }
                    return Err(out.join("\n"));
                }
            }
        }
        Ok(Out {
            text: out.join("\n"),
            images,
        })
    }

    /// Run a tool. When it fails, everything held is released (keys and
    /// buttons left down by `hold` would otherwise go on acting on what
    /// the agent does next, after something went wrong).
    fn call<'a>(
        &'a self,
        name: &'a str,
        a: &'a Json,
    ) -> Pin<Box<dyn Future<Output = R<Out>> + 'a>> {
        Box::pin(async move {
            let result = self.call_inner(name, a, false).await;
            match result {
                Err(e) if !self.held_names().is_empty() => {
                    let (names, released) = self.release_all().await;
                    let failed = match released {
                        Ok(()) => String::new(),
                        Err(r) => format!(" (release failed: {r})"),
                    };
                    Err(format!("{e}; released {}{failed}", names.join(", ")))
                }
                r => r,
            }
        })
    }

    /// `call`, for a caller that may already hold the input lock (`held`: a
    /// batch with input steps), which then is not taken again.
    fn call_inner<'a>(
        &'a self,
        name: &'a str,
        a: &'a Json,
        held: bool,
    ) -> Pin<Box<dyn Future<Output = R<Out>> + 'a>> {
        Box::pin(async move {
            let window = window_arg(name, a)?;
            // A screenshot sends no input, but activating its window does. A
            // batch that holds the input lock already runs its steps `held`.
            let _input = if (INPUT_TOOLS.contains(&name) || window.is_some()) && !held {
                Some(self.input.lock().await)
            } else {
                None
            };
            if let Some(label) = indicator_label(name, a) {
                self.b.indicate(&label).await;
            }
            let activated = match &window {
                Some(q) => self.ensure_window(q).await?,
                None => None,
            };
            let result = match name {
                "apps" => self.apps().await.map(Out::from),
                "tree" => self.tree(a).await.map(Out::from),
                "find" => self.find(a).await.map(Out::from),
                "act" => self.act(a).await.map(Out::from),
                "set_text" => self.set_text(a).await.map(Out::from),
                "get_text" => self.get_text(a).await.map(Out::from),
                "wait_for" => self.wait_for(a).await.map(Out::from),
                "pause" => self.pause(a).await.map(Out::from),
                "focus" => self.focus(a).await.map(Out::from),
                "click" => self.click(a).await.map(Out::from),
                "drag" => self.drag(a).await.map(Out::from),
                "windows" => self.windows(a).await.map(Out::from),
                "move_window" => self.move_window(a).await.map(Out::from),
                "launch" => self.launch(a).await.map(Out::from),
                "doctor" => self.doctor().await.map(Out::from),
                "scroll" => self.scroll(a).await.map(Out::from),
                "key" => self.key(a).await.map(Out::from),
                "type_text" => self.type_text(a).await.map(Out::from),
                "clipboard_get" => self.clipboard_get(a).await.map(Out::from),
                "clipboard_set" => self.clipboard_set(a).await.map(Out::from),
                "mouse" => self.mouse(a).await.map(Out::from),
                "hold" => self.hold(a).await.map(Out::from),
                "cursor_position" => self.cursor_position().await.map(Out::from),
                "activate_window" => self.activate_window(a).await.map(Out::from),
                "file_dialog" => self.file_dialog(a).await.map(Out::from),
                "screenshot" => self.screenshot(a).await,
                "batch" => self.batch(a).await,
                _ => Err(format!("Unknown tool: {name}")),
            };
            // What is still held, after every input step but a batch (its
            // steps say it).
            let note = if name == "batch" {
                String::new()
            } else {
                self.held_note()
            };
            let result = result.map(|mut o| {
                o.text.push_str(&note);
                o
            });
            match activated {
                Some(title) => {
                    let done = format!("activated {}; ", render::py_repr(&title));
                    result
                        .map(|mut o| {
                            o.text.insert_str(0, &done);
                            o
                        })
                        .map_err(|e| format!("{done}{e}"))
                }
                None => result,
            }
        })
    }
}

// ---------------------------------------------------------------- MCP

const PROTOCOL_VERSIONS: [&str; 4] = ["2025-06-18", "2025-03-26", "2024-11-05", "2025-11-25"];

async fn handle<B: Backend>(server: &Server<B>, msg: &Json) -> Option<Json> {
    let id = msg.get("id")?.clone();
    let method = msg.get("method").and_then(Json::as_str).unwrap_or("");
    let params = msg.get("params").cloned().unwrap_or(Json::Null);
    let result = match method {
        "initialize" => {
            let asked = params["protocolVersion"].as_str().unwrap_or("");
            let version = if PROTOCOL_VERSIONS.contains(&asked) {
                asked
            } else {
                PROTOCOL_VERSIONS[0]
            };
            json!({
                "protocolVersion": version,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": { "name": "desktop", "version": env!("CARGO_PKG_VERSION") },
                "instructions": tools::INSTRUCTIONS,
            })
        }
        "ping" => json!({}),
        "tools/list" => json!({ "tools": tools::tools() }),
        "tools/call" => {
            let name = params["name"].as_str().unwrap_or("");
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let (out, is_error) = match server.call(name, &args).await {
                Ok(o) => (o, false),
                Err(e) => (Out::from(format!("Error executing tool {name}: {e}")), true),
            };
            let mut content = vec![json!({ "type": "text", "text": out.text })];
            content.extend(
                out.images
                    .into_iter()
                    .map(|data| json!({ "type": "image", "data": data, "mimeType": "image/png" })),
            );
            json!({ "content": content, "isError": is_error })
        }
        _ => {
            return Some(json!({ "jsonrpc": "2.0", "id": id,
                "error": { "code": -32601, "message": format!("Method not found: {method}") } }));
        }
    };
    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

async fn serve<B: Backend + 'static>(backend: B) -> R<()> {
    // Creating virtual input devices takes a moment; do it while the client
    // is still connecting so the first key/click is not delayed.
    backend.warm_up();
    let server = std::rc::Rc::new(Server::new(backend));
    server.b.enable().await;
    server.allowed();
    let result = tokio::select! {
        r = run_loop(
            server.clone(),
            BufReader::new(tokio::io::stdin()),
            tokio::io::stdout(),
        ) => r,
        _ = shutdown_signal() => Ok(()),
    };
    // On a signal the calls still running were dropped with run_loop:
    // release what they or `hold` left pressed before exiting.
    let _ = server.release_all().await;
    result
}

/// SIGTERM, SIGINT or SIGHUP (Ctrl+C on Windows): the client is shutting
/// the server down. Never resolves when the handlers cannot be installed.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let kinds = [
            SignalKind::terminate(),
            SignalKind::interrupt(),
            SignalKind::hangup(),
        ];
        let mut streams: Vec<_> = kinds.into_iter().filter_map(|k| signal(k).ok()).collect();
        if streams.is_empty() {
            std::future::pending::<()>().await;
        }
        let waits = streams.iter_mut().map(|s| Box::pin(s.recv()));
        futures_util::future::select_all(waits).await;
    }
    #[cfg(windows)]
    {
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// The JSON-RPC loop over newline-delimited messages, apart from the
/// process' stdin/stdout so tests can drive it over an in-memory pipe.
///
/// A `tools/call` request runs on its own local task, so a call that waits
/// (`wait_for`, a slow app) does not hold up the messages behind it: `ping`,
/// `initialize` and `tools/list` stay inline because they never wait. Replies
/// are matched by `id`, so they may go out in any order. Tools that send
/// input take `Server::input`, so their events still do not interleave.
/// `notifications/cancelled` aborts the call it names, and no response is
/// written for it. Every reply goes through one writer task. At end of input
/// the loop waits for the calls still running and writes their replies.
///
/// Needs a `LocalSet`: backends are not `Send` (COM objects on Windows).
async fn run_loop<B, Rd, Wr>(server: std::rc::Rc<Server<B>>, reader: Rd, stdout: Wr) -> R<()>
where
    B: Backend + 'static,
    Rd: tokio::io::AsyncBufRead + Unpin,
    Wr: tokio::io::AsyncWrite + Unpin + 'static,
{
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Json>();
    let writer = tokio::task::spawn_local(async move {
        let mut stdout = stdout;
        while let Some(reply) = rx.recv().await {
            let mut out = reply.to_string();
            out.push('\n');
            stdout
                .write_all(out.as_bytes())
                .await
                .map_err(|e| e.to_string())?;
            stdout.flush().await.map_err(|e| e.to_string())?;
        }
        Ok::<(), String>(())
    });
    let in_flight = std::rc::Rc::new(std::cell::RefCell::new(InFlight::default()));
    let mut calls = tokio::task::JoinSet::new();
    let mut lines = reader.lines();
    while let Some(line) = lines.next_line().await.map_err(|e| e.to_string())? {
        // Reap finished calls so a long session does not accumulate them.
        while calls.try_join_next().is_some() {}
        if line.trim().is_empty() {
            continue;
        }
        let msg = match serde_json::from_str::<Json>(&line) {
            Ok(msg) => msg,
            Err(e) => {
                let reply = json!({ "jsonrpc": "2.0", "id": null,
                    "error": { "code": -32700, "message": format!("Parse error: {e}") } });
                if tx.send(reply).is_err() {
                    break;
                }
                continue;
            }
        };
        let method = msg.get("method").and_then(Json::as_str).unwrap_or("");
        if method == "notifications/cancelled" {
            if let Some(id) = msg.pointer("/params/requestId")
                && in_flight.borrow_mut().cancel(id)
            {
                // Whatever the call held (a drag's button, a click's
                // modifiers, keys `hold` pressed) comes up; the release
                // takes the input lock, so it waits until the aborted call
                // has let go of it.
                let server = server.clone();
                calls.spawn_local(async move {
                    let _ = server.release_all().await;
                });
            }
            continue;
        }
        if method == "tools/call" && msg.get("id").is_some_and(|id| !id.is_null()) {
            let key = msg["id"].to_string();
            let (server, tx, registry) = (server.clone(), tx.clone(), in_flight.clone());
            let seq = in_flight.borrow_mut().next_seq();
            let handle = calls.spawn_local(async move {
                let reply = handle(&server, &msg).await;
                // A cancellation that got here first means: say nothing.
                if registry.borrow_mut().finish(&msg["id"].to_string(), seq)
                    && let Some(reply) = reply
                {
                    let _ = tx.send(reply);
                }
            });
            // The task cannot run before this: spawn_local only queues it.
            in_flight.borrow_mut().register(key, seq, handle);
            continue;
        }
        if let Some(reply) = handle(&server, &msg).await
            && tx.send(reply).is_err()
        {
            break; // the writer stopped: its error is returned below
        }
    }
    while calls.join_next().await.is_some() {}
    // The client went away: nothing may stay pressed after the server.
    let _ = server.release_all().await;
    drop(tx);
    writer.await.map_err(|e| e.to_string())?
}

/// The `tools/call` requests still running, by request id (its JSON text:
/// `2` and `"2"` are different ids), so `notifications/cancelled` can abort
/// the right one. The sequence number keeps a reused id from finishing or
/// cancelling another call.
#[derive(Default)]
struct InFlight {
    next: u64,
    by_id: HashMap<String, (u64, tokio::task::AbortHandle)>,
}

impl InFlight {
    fn next_seq(&mut self) -> u64 {
        self.next += 1;
        self.next
    }

    fn register(&mut self, key: String, seq: u64, handle: tokio::task::AbortHandle) {
        self.by_id.insert(key, (seq, handle));
    }

    /// The call is done: `true` if it should reply, `false` if it was
    /// cancelled in the meantime.
    fn finish(&mut self, key: &str, seq: u64) -> bool {
        match self.by_id.get(key) {
            Some((s, _)) if *s == seq => {
                self.by_id.remove(key);
                true
            }
            _ => false,
        }
    }

    /// Abort the running call with this id, if any; true when there was
    /// one. An unknown or finished id is ignored, as the MCP spec asks.
    fn cancel(&mut self, id: &Json) -> bool {
        match self.by_id.remove(&id.to_string()) {
            Some((_, handle)) => {
                handle.abort();
                true
            }
            None => false,
        }
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
compile_error!("desktop-mcp supports Linux (KDE Plasma, Hyprland) and Windows only");

#[tokio::main(flavor = "current_thread")]
async fn main() {
    #[cfg(target_os = "linux")]
    if std::env::args().nth(1).as_deref() == Some("--indicator") {
        if let Err(e) = linux::indicator::run() {
            eprintln!("desktop-mcp indicator: {e}");
            std::process::exit(1);
        }
        return;
    }
    // Tool calls run as local tasks: the backends are not `Send`.
    let local = tokio::task::LocalSet::new();
    #[cfg(target_os = "linux")]
    let result = local
        .run_until(async {
            match linux::LinuxBackend::new().await {
                Ok(b) => serve(b).await,
                Err(e) => Err(e),
            }
        })
        .await;
    #[cfg(windows)]
    let result = local
        .run_until(async {
            match windows::WindowsBackend::new() {
                Ok(b) => serve(b).await,
                Err(e) => Err(e),
            }
        })
        .await;
    if let Err(e) = result {
        eprintln!("desktop-mcp: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    use backend::{AppEntry, Info};
    use tokio::io::DuplexStream;

    /// An app "fake" whose one window is the focused element `focus` (a
    /// push button "OK" unless a test changes it between calls), so finds
    /// for it hit and `wait_for` anything else keeps polling until its
    /// timeout; key presses and actions are logged.
    struct FakeBackend {
        allow_file: PathBuf,
        keys: Rc<RefCell<Vec<String>>>,
        focus: Rc<RefCell<Info<u32>>>,
        /// App the focused element belongs to.
        focus_app: Rc<RefCell<String>>,
        clipboard: RefCell<String>,
        /// A text field like Firefox's when Some: accessibility writes do
        /// nothing, typing replaces it, and it always has focus.
        field: Rc<RefCell<Option<String>>>,
        /// The active window (app, title); activate_window picks one of
        /// FAKE_WINDOWS and logs "activate <title>" in `keys`.
        active: Rc<RefCell<(String, String)>>,
    }

    /// Windows on the fake desktop: (app, title).
    const FAKE_WINDOWS: [(&str, &str); 3] = [
        ("konsole", "~ : bash — Konsole"),
        ("fake", "notes.txt — Fake"),
        ("fake", "todo.txt — Fake"),
    ];

    impl Backend for FakeBackend {
        type Id = u32;
        fn allow_file(&self) -> PathBuf {
            self.allow_file.clone()
        }
        fn default_allow(&self) -> &'static str {
            "fake\n"
        }
        fn apps_note(&self) -> &'static str {
            ""
        }
        fn boring_roles(&self) -> &'static [&'static str] {
            &[]
        }
        async fn enable(&self) {}
        async fn list_apps(&self) -> Vec<AppEntry> {
            Vec::new()
        }
        async fn app_windows(&self, app: &str) -> Option<Vec<u32>> {
            (app == "fake").then(|| vec![FAKE_FOCUS])
        }
        async fn active_app(&self) -> String {
            self.active.borrow().0.clone()
        }
        async fn info(&self, id: &u32, _: usize) -> Info<u32> {
            match *id {
                FAKE_FOCUS => self.focus.borrow().clone(),
                _ => Info::default(),
            }
        }
        async fn focused(&self) -> Option<Focused<u32>> {
            Some(Focused {
                id: FAKE_FOCUS,
                info: self.focus.borrow().clone(),
                app: self.focus_app.borrow().clone(),
            })
        }
        async fn prefetch(
            &self,
            cache: &mut Cache<u32>,
            roots: Vec<u32>,
            _: bool,
            _: usize,
            _: usize,
        ) {
            for id in roots {
                let info = self.info(&id, 0).await;
                cache.insert(id, info);
            }
        }
        async fn actions(&self, id: &u32) -> Vec<String> {
            self.info(id, 0).await.actions
        }
        async fn do_action(&self, id: &u32, index: usize) -> R<bool> {
            self.keys
                .borrow_mut()
                .push(format!("action {index} on {id}"));
            Ok(true)
        }
        async fn full_text(&self, _: &u32, _: usize) -> R<String> {
            let field = self.field.borrow().clone();
            if field.is_some() {
                self.keys.borrow_mut().push("read".into());
            }
            Ok(field.unwrap_or_default())
        }
        async fn set_text(&self, _: &u32, _: &str) -> R<bool> {
            Ok(true)
        }
        fn app_key(&self, _: &u32) -> Option<String> {
            Some("fake".into())
        }
        async fn is_focused(&self, _: &u32) -> bool {
            self.field.borrow().is_some()
        }
        async fn try_focus(&self, _: &u32) -> R<Option<String>> {
            Ok(None)
        }
        async fn element_center(&self, _: &u32) -> R<(i32, i32)> {
            Ok((0, 0))
        }
        fn check_combo(&self, _: &str) -> R<()> {
            Ok(())
        }
        async fn press_combo(&self, combo: &str) -> R<()> {
            self.keys.borrow_mut().push(format!("down {combo}"));
            tokio::time::sleep(Duration::from_millis(50)).await;
            self.keys.borrow_mut().push(format!("up {combo}"));
            Ok(())
        }
        async fn type_text(&self, text: &str) -> R<String> {
            if let Some(f) = self.field.borrow_mut().as_mut() {
                *f = text.to_string();
            }
            Ok(String::new())
        }
        async fn clipboard_get(&self, limit: usize) -> R<Clip> {
            let text = self.clipboard.borrow().clone();
            let cut: String = text.chars().take(limit).collect();
            Ok(match text.is_empty() {
                true => Clip::Empty,
                false => Clip::Text {
                    truncated: cut.len() < text.len(),
                    text: cut,
                },
            })
        }
        async fn clipboard_set(&self, text: &str) -> R<()> {
            *self.clipboard.borrow_mut() = text.to_string();
            Ok(())
        }
        /// Logs clicks and scrolls; fails at x = FAIL_X, and waits at
        /// x = SLOW_X (for a call to cancel).
        async fn pointer(&self, x: i64, _: i64, button: &str, clicks: i64, scroll: i64) -> R<()> {
            if x == FAIL_X {
                return Err("pointer failed".into());
            }
            if x == SLOW_X {
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            if clicks > 0 {
                self.keys
                    .borrow_mut()
                    .push(format!("click {button} x{clicks}"));
            }
            if scroll != 0 {
                self.keys.borrow_mut().push(format!("scroll {scroll}"));
            }
            Ok(())
        }
        async fn key(&self, key: &str, down: bool) -> R<()> {
            let what = if down { "keydown" } else { "keyup" };
            self.keys.borrow_mut().push(format!("{what} {key}"));
            Ok(())
        }
        async fn cursor_position(&self) -> R<(i64, i64)> {
            Ok((12, 34))
        }
        async fn move_pointer(&self, x: i64, y: i64) -> R<()> {
            self.keys.borrow_mut().push(format!("move {x},{y}"));
            Ok(())
        }
        async fn button(&self, button: &str, down: bool) -> R<()> {
            let what = if down { "press" } else { "release" };
            self.keys.borrow_mut().push(format!("{what} {button}"));
            Ok(())
        }
        async fn activate_window(
            &self,
            query: &str,
            allowed: &[String],
        ) -> R<Option<backend::Activated>> {
            let q = query.to_lowercase();
            let (app, title) = FAKE_WINDOWS
                .into_iter()
                .find(|(app, title)| {
                    allowed.iter().any(|a| a == app)
                        && (title.to_lowercase().contains(&q) || app.contains(&q))
                })
                .ok_or_else(|| format!("no window of an allowed app matches '{query}'"))?;
            self.keys.borrow_mut().push(format!("activate {title}"));
            *self.active.borrow_mut() = (app.into(), title.into());
            Ok(Some(backend::Activated {
                app: app.into(),
                title: title.into(),
            }))
        }
        async fn windows(&self, query: &str, allowed: &[String]) -> R<Vec<WindowGeo>> {
            let q = query.to_lowercase();
            let active = self.active.borrow().clone();
            Ok(FAKE_WINDOWS
                .into_iter()
                .filter(|(app, title)| {
                    allowed.iter().any(|a| a == app)
                        && (title.to_lowercase().contains(&q) || app.contains(&q))
                })
                .map(|(app, title)| WindowGeo {
                    title: title.into(),
                    app: app.into(),
                    active: (app, title) == (active.0.as_str(), active.1.as_str()),
                    ..WindowGeo::default()
                })
                .collect())
        }
        async fn screenshot(&self) -> Option<Shot> {
            // A 200x100 image of a 400x200 area at (10, 20): 2x scaling.
            let path = self.allow_file.with_extension("png");
            std::fs::write(&path, b"").ok()?;
            Some(Shot {
                path: path.display().to_string(),
                pixels: Some(Box::new(|| {
                    Ok(shot::Pixels {
                        width: 200,
                        height: 100,
                        channels: 3,
                        data: vec![128; 200 * 100 * 3],
                    })
                })),
                area: Some((10.0, 20.0, 400.0, 200.0)),
            })
        }
    }

    const FAKE_FOCUS: u32 = 7;
    /// `pointer` at this x fails.
    const FAIL_X: i64 = 666;
    /// `pointer` at this x takes 5 s.
    const SLOW_X: i64 = 777;

    struct Session {
        stdin: DuplexStream,
        stdout: tokio::io::Lines<BufReader<DuplexStream>>,
        keys: Rc<RefCell<Vec<String>>>,
        focus: Rc<RefCell<Info<u32>>>,
        focus_app: Rc<RefCell<String>>,
        field: Rc<RefCell<Option<String>>>,
        active: Rc<RefCell<(String, String)>>,
    }

    /// `run_loop` over in-memory pipes, on the current LocalSet.
    fn session(name: &str) -> Session {
        let allow_file = std::env::temp_dir().join(format!(
            "desktop-mcp-test-{}-{name}.txt",
            std::process::id()
        ));
        std::fs::write(&allow_file, "fake\n").unwrap();
        let keys = Rc::new(RefCell::new(Vec::new()));
        let focus = Rc::new(RefCell::new(Info {
            role: "push button".into(),
            name: "OK".into(),
            showing: true,
            actions: vec!["Press".into()],
            ..Info::default()
        }));
        let focus_app = Rc::new(RefCell::new("fake".to_string()));
        let field = Rc::new(RefCell::new(None));
        let active = Rc::new(RefCell::new((
            FAKE_WINDOWS[1].0.to_string(),
            FAKE_WINDOWS[1].1.to_string(),
        )));
        let server = Rc::new(Server::new(FakeBackend {
            allow_file,
            keys: keys.clone(),
            focus: focus.clone(),
            focus_app: focus_app.clone(),
            clipboard: RefCell::new(String::new()),
            field: field.clone(),
            active: active.clone(),
        }));
        let (stdin, server_in) = tokio::io::duplex(64 * 1024);
        let (server_out, client_out) = tokio::io::duplex(64 * 1024);
        tokio::task::spawn_local(run_loop(server, BufReader::new(server_in), server_out));
        Session {
            stdin,
            stdout: BufReader::new(client_out).lines(),
            keys,
            focus,
            focus_app,
            field,
            active,
        }
    }

    async fn send(s: &mut Session, msg: Json) {
        s.stdin
            .write_all(format!("{msg}\n").as_bytes())
            .await
            .unwrap();
    }

    fn waiting_call(id: u64, timeout_ms: u64) -> Json {
        json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {
            "name": "wait_for",
            "arguments": {"app": "fake", "query": "never there", "timeout_ms": timeout_ms}}})
    }

    fn ping(id: u64) -> Json {
        json!({"jsonrpc": "2.0", "id": id, "method": "ping"})
    }

    /// Every reply until stdout closes or `limit` passes, by id.
    async fn replies_within(s: &mut Session, limit: Duration) -> Vec<Json> {
        let mut out = Vec::new();
        let _ = tokio::time::timeout(limit, async {
            while let Ok(Some(line)) = s.stdout.next_line().await {
                out.push(serde_json::from_str::<Json>(&line).unwrap());
            }
        })
        .await;
        out
    }

    fn ids(replies: &[Json]) -> Vec<Json> {
        replies.iter().map(|r| r["id"].clone()).collect()
    }

    fn local<F: Future<Output = ()>>(f: F) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(tokio::task::LocalSet::new().run_until(f));
    }

    #[test]
    fn ping_is_answered_while_a_tool_call_waits() {
        local(async {
            let mut s = session("ping");
            send(&mut s, waiting_call(2, 3000)).await;
            tokio::time::sleep(Duration::from_millis(100)).await;
            send(&mut s, ping(3)).await;
            let line = tokio::time::timeout(Duration::from_millis(500), s.stdout.next_line())
                .await
                .expect("no reply within 500 ms: ping queued behind the waiting tool call")
                .unwrap()
                .unwrap();
            let reply: Json = serde_json::from_str(&line).unwrap();
            assert_eq!(reply["id"], 3, "{reply}");
        });
    }

    #[test]
    fn a_cancelled_tool_call_gets_no_response() {
        local(async {
            let mut s = session("cancel");
            send(&mut s, waiting_call(2, 800)).await;
            tokio::time::sleep(Duration::from_millis(100)).await;
            send(
                &mut s,
                json!({"jsonrpc": "2.0", "method": "notifications/cancelled",
                                "params": {"requestId": 2, "reason": "test"}}),
            )
            .await;
            send(&mut s, ping(3)).await;
            let got = ids(&replies_within(&mut s, Duration::from_millis(1500)).await);
            assert!(got.contains(&json!(3)), "ping was not answered: {got:?}");
            assert!(
                !got.contains(&json!(2)),
                "the cancelled call was answered: {got:?}"
            );
        });
    }

    #[test]
    fn end_of_input_still_answers_running_calls() {
        local(async {
            let mut s = session("eof");
            send(&mut s, waiting_call(2, 200)).await;
            drop(std::mem::replace(&mut s.stdin, tokio::io::duplex(1).0));
            let replies = replies_within(&mut s, Duration::from_secs(3)).await;
            assert_eq!(ids(&replies), vec![json!(2)], "{replies:?}");
            assert_eq!(replies[0]["result"]["isError"], true, "{replies:?}");
        });
    }

    #[test]
    fn concurrent_key_calls_do_not_interleave() {
        local(async {
            let mut s = session("keys");
            for (id, combo) in [(2, "a"), (3, "b")] {
                send(
                    &mut s,
                    json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
                    "params": {"name": "key", "arguments": {"combo": combo}}}),
                )
                .await;
            }
            let replies = replies_within(&mut s, Duration::from_millis(500)).await;
            assert_eq!(replies.len(), 2, "{replies:?}");
            assert_eq!(*s.keys.borrow(), vec!["down a", "up a", "down b", "up b"]);
        });
    }
    #[test]
    fn waits_above_the_limit_are_cut_and_reported() {
        assert_eq!(wait_ms(&json!({"ms": 250}), "ms", 0), (250, String::new()));
        assert_eq!(wait_ms(&json!({"ms": -5}), "ms", 0), (0, String::new()));
        let (ms, note) = wait_ms(&json!({"timeout_ms": 900_000}), "timeout_ms", 5000);
        assert_eq!(ms, 600_000);
        assert!(note.contains("900000") && note.contains("600000"), "{note}");
    }

    #[test]
    fn pause_sleeps_and_does_not_block_other_calls() {
        local(async {
            let mut s = session("pause");
            send(
                &mut s,
                json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                "params": {"name": "pause", "arguments": {"ms": 300}}}),
            )
            .await;
            send(&mut s, ping(3)).await;
            let replies = replies_within(&mut s, Duration::from_millis(600)).await;
            assert_eq!(ids(&replies), vec![json!(3), json!(2)], "{replies:?}");
            let text = replies[1]["result"]["content"][0]["text"].as_str().unwrap();
            assert_eq!(text, "paused 300 ms");
        });
    }

    #[test]
    fn screenshot_pixels_map_to_scaled_screen_points() {
        // 125 % scaling: a 1920x1080 image of a window at (0,0), 1536x864.
        let map = ShotMap::new((1920, 1080), (0.0, 0.0, 1536.0, 864.0)).unwrap();
        assert_eq!(map.to_screen(867, 655), (694, 524));
        assert_eq!(map.to_screen(0, 0), (0, 0));
        // Unscaled window away from the origin.
        let map = ShotMap::new((800, 600), (100.0, 50.0, 800.0, 600.0)).unwrap();
        assert_eq!(map.to_screen(10, 20), (110, 70));
        assert_eq!(ShotMap::new((0, 600), (0.0, 0.0, 800.0, 600.0)), None);
    }

    async fn call_tool(s: &mut Session, id: u64, name: &str, args: Json) -> Json {
        send(
            s,
            json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": {"name": name, "arguments": args}}),
        )
        .await;
        let line = tokio::time::timeout(Duration::from_secs(5), s.stdout.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        serde_json::from_str(&line).unwrap()
    }

    #[test]
    fn launch_checks_the_allowlist_before_starting_anything() {
        local(async {
            let mut s = session("launch");
            let text = |r: &Json| {
                r["result"]["content"][0]["text"]
                    .as_str()
                    .unwrap()
                    .to_string()
            };
            let r = call_tool(&mut s, 2, "launch", json!({"app": "Evil"})).await;
            assert_eq!(r["result"]["isError"], true, "{r}");
            assert!(text(&r).contains("app 'evil' is not in"), "{r}");
            let r = call_tool(&mut s, 3, "launch", json!({"app": "fake", "args": [1]})).await;
            assert!(text(&r).contains("args must be a list of strings"), "{r}");
            // Allowed: it reaches the backend, which cannot launch here.
            let r = call_tool(&mut s, 4, "launch", json!({"app": " Fake ", "args": ["x"]})).await;
            assert!(text(&r).contains("not supported"), "{r}");
        });
        assert_eq!(
            indicator_label("launch", &json!({"app": "kate"})).as_deref(),
            Some("launching kate")
        );
    }

    fn set_active(s: &Session, i: usize) {
        let (app, title) = FAKE_WINDOWS[i];
        *s.active.borrow_mut() = (app.into(), title.into());
    }

    #[test]
    fn window_activates_the_matching_window_before_the_input() {
        local(async {
            let mut s = session("window-activate");
            set_active(&s, 0); // the user's Konsole, not allowed
            let r = call_tool(&mut s, 2, "key", json!({"combo": "a", "window": "todo"})).await;
            assert_eq!(r["result"]["isError"], false, "{r}");
            assert!(
                reply_text(&r).starts_with("activated 'todo.txt — Fake'; sent a x1 to fake"),
                "{r}"
            );
            assert_eq!(
                *s.keys.borrow(),
                vec!["activate todo.txt — Fake", "down a", "up a"]
            );
            // Another window of the same app is activated too.
            s.keys.borrow_mut().clear();
            let r = call_tool(
                &mut s,
                3,
                "type_text",
                json!({"text": "x", "window": "Notes"}),
            )
            .await;
            assert!(
                reply_text(&r).starts_with("activated 'notes.txt — Fake'; typed 1 chars"),
                "{r}"
            );
            assert_eq!(*s.keys.borrow(), vec!["activate notes.txt — Fake"]);
            // A screenshot takes it as well.
            set_active(&s, 0);
            let r = call_tool(
                &mut s,
                4,
                "screenshot",
                json!({"window": "fake", "inline": false}),
            )
            .await;
            assert_eq!(r["result"]["isError"], false, "{r}");
            assert!(
                reply_text(&r).starts_with("activated 'notes.txt — Fake'; "),
                "{r}"
            );
        });
    }

    #[test]
    fn window_already_in_front_is_not_activated_again() {
        local(async {
            let mut s = session("window-skip");
            // notes.txt is active: matched by title and by app name.
            for (id, w) in [(2, "notes.TXT"), (3, "Fake")] {
                let r = call_tool(&mut s, id, "key", json!({"combo": "a", "window": w})).await;
                assert!(reply_text(&r).starts_with("sent a x1 to fake"), "{r}");
            }
            assert_eq!(*s.keys.borrow(), vec!["down a", "up a", "down a", "up a"]);
        });
    }

    #[test]
    fn window_without_a_match_fails_before_any_input() {
        local(async {
            let mut s = session("window-none");
            set_active(&s, 0);
            // No such window; Konsole's own does not count, it is not allowed.
            for (id, w) in [(2, "nothing"), (3, "konsole")] {
                let r = call_tool(&mut s, id, "key", json!({"combo": "a", "window": w})).await;
                assert_eq!(r["result"]["isError"], true, "{r}");
                let want = format!("no window of an allowed app matches '{w}'");
                assert!(reply_text(&r).contains(&want), "{r}");
            }
            let r = call_tool(&mut s, 4, "key", json!({"combo": "a", "window": 5})).await;
            assert!(reply_text(&r).contains("window must be a string"), "{r}");
            assert!(s.keys.borrow().is_empty(), "{:?}", s.keys.borrow());
        });
    }

    #[test]
    fn no_window_means_no_activation() {
        local(async {
            let mut s = session("window-absent");
            set_active(&s, 0);
            let r = call_tool(&mut s, 2, "key", json!({"combo": "a"})).await;
            assert_eq!(r["result"]["isError"], true, "{r}");
            assert!(
                reply_text(&r).contains("active window app 'konsole' is not in"),
                "{r}"
            );
            assert!(reply_text(&r).contains("pass window="), "{r}");
            assert!(s.keys.borrow().is_empty(), "{:?}", s.keys.borrow());
            assert_eq!(s.active.borrow().0, "konsole");
        });
    }

    #[test]
    fn activation_and_input_hold_the_input_lock_together() {
        local(async {
            let mut s = session("window-lock");
            set_active(&s, 0);
            let calls = [
                (2, json!({"combo": "a", "window": "todo"})),
                (3, json!({"combo": "b"})),
            ];
            for (id, args) in calls {
                send(
                    &mut s,
                    json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
                    "params": {"name": "key", "arguments": args}}),
                )
                .await;
            }
            let replies = replies_within(&mut s, Duration::from_millis(500)).await;
            assert_eq!(replies.len(), 2, "{replies:?}");
            assert_eq!(
                *s.keys.borrow(),
                vec![
                    "activate todo.txt — Fake",
                    "down a",
                    "up a",
                    "down b",
                    "up b"
                ]
            );
            // Inside a batch too (each step takes the lock itself).
            s.keys.borrow_mut().clear();
            set_active(&s, 0);
            let steps = json!({"steps": [
                {"tool": "key", "args": {"combo": "c", "window": "todo"}},
                {"tool": "key", "args": {"combo": "d"}},
            ]});
            let r = call_tool(&mut s, 4, "batch", steps).await;
            assert_eq!(r["result"]["isError"], false, "{r}");
            assert!(
                reply_text(&r).contains("[1] key: activated 'todo.txt — Fake'; sent c"),
                "{r}"
            );
            assert_eq!(
                *s.keys.borrow(),
                vec![
                    "activate todo.txt — Fake",
                    "down c",
                    "up c",
                    "down d",
                    "up d"
                ]
            );
        });
    }

    fn batch_steps(wait_optional: bool) -> Json {
        json!({"steps": [
            {"tool": "key", "args": {"combo": "a"}},
            {"tool": "wait_for", "args": {"app": "fake", "query": "never there", "timeout_ms": 60},
             "optional": wait_optional},
            {"tool": "key", "args": {"combo": "b"}},
        ]})
    }

    #[test]
    fn an_optional_batch_step_fails_and_the_batch_goes_on() {
        local(async {
            let mut s = session("batch-optional");
            let r = call_tool(&mut s, 2, "batch", batch_steps(true)).await;
            assert_eq!(r["result"]["isError"], false, "{r}");
            let lines: Vec<&str> = reply_text(&r).lines().collect();
            assert_eq!(lines.len(), 3, "{r}");
            assert!(lines[0].starts_with("[1] key: "), "{r}");
            assert!(
                lines[1].starts_with("[2] wait_for failed (optional): "),
                "{r}"
            );
            assert!(lines[2].starts_with("[3] key: "), "{r}");
            assert_eq!(*s.keys.borrow(), ["down a", "up a", "down b", "up b"]);
        });
    }

    #[test]
    fn a_required_batch_step_failing_stops_the_batch() {
        local(async {
            let mut s = session("batch-required");
            let r = call_tool(&mut s, 2, "batch", batch_steps(false)).await;
            assert_eq!(r["result"]["isError"], true, "{r}");
            let lines: Vec<&str> = reply_text(&r).lines().collect();
            assert_eq!(lines.len(), 3, "{r}");
            assert!(
                lines[0].starts_with("Error executing tool batch: [1] key: "),
                "{r}"
            );
            assert!(lines[1].starts_with("[2] wait_for failed: "), "{r}");
            assert_eq!(lines[2], "stopped; 1 step(s) not run");
            assert_eq!(*s.keys.borrow(), ["down a", "up a"]);
        });
    }

    #[test]
    fn initialize_carries_short_instructions() {
        local(async {
            let mut s = session("init");
            send(
                &mut s,
                json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {"protocolVersion": "2025-06-18"}}),
            )
            .await;
            let line = s.stdout.next_line().await.unwrap().unwrap();
            let r: Json = serde_json::from_str(&line).unwrap();
            let text = r["result"]["instructions"].as_str().unwrap();
            // Claude Code shows at most 4096 characters of it.
            assert!(text.chars().count() < 3000, "{}", text.len());
            let names: Vec<Json> = tools::tools()
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t["name"].clone())
                .collect();
            let common = text.rsplit("one search: ").next().unwrap();
            for name in common.trim_end_matches('.').split(", ") {
                assert!(names.contains(&json!(name)), "{name}");
            }
        });
    }

    #[test]
    fn screenshot_comes_back_inline_shrunk_and_clickable() {
        local(async {
            let mut s = session("shot");
            let r = call_tool(&mut s, 2, "screenshot", json!({"max_size": 100})).await;
            let content = &r["result"]["content"];
            let text = content[0]["text"].as_str().unwrap();
            assert!(
                text.contains("100x50 px of screen area at (10,20), 400x200"),
                "{text}"
            );
            assert_eq!(content[1]["type"], "image", "{r}");
            assert_eq!(content[1]["mimeType"], "image/png");
            assert!(
                content[1]["data"]
                    .as_str()
                    .unwrap()
                    .starts_with("iVBORw0KGgo")
            );
            // The file on disk is the shrunk image the coordinates refer to.
            let path = text.lines().next().unwrap();
            let img = shot::read_png(path).unwrap();
            assert_eq!((img.width, img.height), (100, 50));
            std::fs::remove_file(path).unwrap();
            // Pixel (50, 25) of a 4x-scaled image of an area at (10, 20).
            let r = call_tool(&mut s, 3, "mouse", json!({"image_x": 50, "image_y": 25})).await;
            let text = r["result"]["content"][0]["text"].as_str().unwrap();
            assert_eq!(text, "pointer at (212,122) in fake");
            let r = call_tool(&mut s, 4, "screenshot", json!({"inline": false})).await;
            assert_eq!(r["result"]["content"].as_array().unwrap().len(), 1, "{r}");
            let path = r["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .lines()
                .next()
                .unwrap();
            std::fs::remove_file(path).unwrap();
        });
    }

    #[test]
    fn clipboard_round_trips_and_needs_an_allowed_window() {
        local(async {
            let mut s = session("clipboard");
            let text = |r: &Json| {
                r["result"]["content"][0]["text"]
                    .as_str()
                    .unwrap()
                    .to_string()
            };
            let r = call_tool(&mut s, 2, "clipboard_get", json!({})).await;
            assert_eq!(text(&r), "(the clipboard is empty)");
            let r = call_tool(&mut s, 3, "clipboard_set", json!({"text": "çağ €\n😀"})).await;
            assert_eq!(text(&r), "clipboard set to 7 chars");
            let r = call_tool(&mut s, 4, "clipboard_get", json!({})).await;
            assert_eq!(text(&r), "çağ €\n😀");
            let r = call_tool(&mut s, 5, "clipboard_get", json!({"limit": 2})).await;
            assert_eq!(text(&r), "ça\n(cut at 2 chars)");
            // Nothing is read or written while the app in front is not allowed.
            let allow = std::env::temp_dir().join(format!(
                "desktop-mcp-test-{}-clipboard.txt",
                std::process::id()
            ));
            std::fs::write(&allow, "other\n").unwrap();
            for (id, tool, args) in [
                (6, "clipboard_get", json!({})),
                (7, "clipboard_set", json!({"text": "x"})),
            ] {
                let r = call_tool(&mut s, id, tool, args).await;
                assert_eq!(r["result"]["isError"], true, "{r}");
                assert!(text(&r).contains("not in"), "{r}");
            }
            std::fs::write(&allow, "fake\n").unwrap();
            let r = call_tool(&mut s, 8, "clipboard_get", json!({})).await;
            assert_eq!(text(&r), "çağ €\n😀");
        });
    }

    #[test]
    fn typing_reports_focus_and_warns_off_a_text_field() {
        local(async {
            let mut s = session("focusnote");
            let r = call_tool(&mut s, 2, "type_text", json!({"text": "hi"})).await;
            let text = r["result"]["content"][0]["text"].as_str().unwrap();
            assert!(
                text.starts_with("typed 2 chars into fake; focus: [1] push button \"OK\""),
                "{text}"
            );
            assert!(
                text.ends_with("(not an editable field: check where the text went)"),
                "{text}"
            );
            let r = call_tool(&mut s, 3, "key", json!({"combo": "a"})).await;
            let text = r["result"]["content"][0]["text"].as_str().unwrap();
            // The same element again: the same id.
            assert!(text.contains("; focus: [1] push button \"OK\""), "{text}");
            assert!(!text.contains("not an editable"), "{text}");
        });
    }

    #[test]
    fn set_text_readback_tolerates_line_endings_only() {
        assert!(written_matches("a\nb", "a\r\nb\n"));
        assert!(written_matches("", ""));
        assert!(!written_matches("hello world", "hello"));
        assert!(!written_matches("Ahmet", "ahmet"));
    }

    fn log(s: &Session) -> Vec<String> {
        s.keys.borrow().clone()
    }

    #[test]
    fn modifiers_go_down_before_the_click_and_up_after_it_in_reverse() {
        local(async {
            let mut s = session("modifiers");
            let r = call_tool(
                &mut s,
                2,
                "mouse",
                json!({"x": 5, "y": 5, "button": "middle", "clicks": 3,
                       "modifiers": ["ctrl", "Shift"]}),
            )
            .await;
            assert_eq!(reply_text(&r), "pointer at (5,5) in fake with ctrl+shift");
            assert_eq!(
                log(&s),
                [
                    "keydown ctrl",
                    "keydown shift",
                    "click middle x3",
                    "keyup shift",
                    "keyup ctrl"
                ]
            );
            s.keys.borrow_mut().clear();
            // A chord string works too; scroll and click take them as well.
            let r = call_tool(
                &mut s,
                3,
                "scroll",
                json!({"app": "fake", "query": "OK", "amount": 2, "modifiers": "control"}),
            )
            .await;
            assert!(
                reply_text(&r).ends_with("scrolled 2 at 0,0 with ctrl"),
                "{r}"
            );
            let r = call_tool(
                &mut s,
                4,
                "click",
                json!({"app": "fake", "query": "OK", "modifiers": ["alt"]}),
            )
            .await;
            assert!(
                reply_text(&r).ends_with("clicked left x1 at 0,0 with alt"),
                "{r}"
            );
            assert_eq!(
                log(&s),
                [
                    "keydown ctrl",
                    "scroll 2",
                    "keyup ctrl",
                    "keydown alt",
                    "click left x1",
                    "keyup alt"
                ]
            );
            // Anything but ctrl/shift/alt/meta fails before any input.
            s.keys.borrow_mut().clear();
            let r = call_tool(
                &mut s,
                5,
                "mouse",
                json!({"x": 5, "y": 5, "modifiers": ["ctrl", "a"]}),
            )
            .await;
            assert!(reply_text(&r).contains("unknown modifier 'a'"), "{r}");
            assert!(log(&s).is_empty(), "{:?}", log(&s));
        });
    }

    #[test]
    fn modifiers_are_parsed_from_lists_and_chords() {
        assert_eq!(modifiers_arg(&json!({})), Ok(vec![]));
        assert_eq!(
            modifiers_arg(&json!({"modifiers": ["Control", "win", "ctrl"]})),
            Ok(vec!["ctrl".to_string(), "meta".to_string()])
        );
        assert_eq!(
            modifiers_arg(&json!({"modifiers": "shift+alt"})),
            Ok(vec!["shift".to_string(), "alt".to_string()])
        );
        assert!(modifiers_arg(&json!({"modifiers": [1]})).is_err());
        assert!(modifiers_arg(&json!({"modifiers": ["enter"]})).is_err());
    }

    #[test]
    fn held_keys_and_buttons_are_listed_and_left_alone() {
        local(async {
            let mut s = session("hold");
            let r = call_tool(&mut s, 2, "hold", json!({"key": "shift"})).await;
            assert_eq!(reply_text(&r), "holding shift in fake; held: shift");
            let r = call_tool(&mut s, 3, "hold", json!({"button": "left"})).await;
            assert_eq!(reply_text(&r), "holding left in fake; held: shift, left");
            // A held modifier is not pressed or released again by a click,
            // nor a held key by a chord.
            let r = call_tool(
                &mut s,
                4,
                "mouse",
                json!({"x": 1, "y": 1, "button": "right", "modifiers": ["shift", "ctrl"]}),
            )
            .await;
            assert_eq!(
                reply_text(&r),
                "pointer at (1,1) in fake with shift+ctrl; held: shift, left"
            );
            let r = call_tool(&mut s, 5, "key", json!({"combo": "shift+a"})).await;
            assert!(reply_text(&r).starts_with("sent shift+a x1 to fake"), "{r}");
            assert!(reply_text(&r).ends_with("; held: shift, left"), "{r}");
            // Moving with the button held drags; read-only tools say it too.
            let r = call_tool(&mut s, 6, "mouse", json!({"x": 9, "y": 9, "clicks": 0})).await;
            assert_eq!(
                reply_text(&r),
                "pointer at (9,9) in fake; held: shift, left"
            );
            let r = call_tool(&mut s, 7, "cursor_position", json!({})).await;
            assert_eq!(reply_text(&r), "cursor at (12,34); held: shift, left");
            let r = call_tool(&mut s, 8, "hold", json!({"key": "shift", "down": false})).await;
            assert_eq!(reply_text(&r), "released shift; held: left");
            let r = call_tool(&mut s, 9, "hold", json!({"down": false})).await;
            assert_eq!(reply_text(&r), "released left; nothing held");
            let r = call_tool(&mut s, 10, "hold", json!({"down": false})).await;
            assert_eq!(reply_text(&r), "nothing was held");
            assert_eq!(
                log(&s),
                [
                    "keydown shift",
                    "press left",
                    "keydown ctrl",
                    "click right x1",
                    "keyup ctrl",
                    "down a",
                    "up a",
                    "keyup shift",
                    "release left"
                ]
            );
        });
    }

    #[test]
    fn hold_presses_only_into_an_allowed_window_but_always_releases() {
        local(async {
            let mut s = session("hold-allow");
            let r = call_tool(&mut s, 2, "hold", json!({"key": "ctrl+alt"})).await;
            assert_eq!(reply_text(&r), "holding ctrl+alt in fake; held: ctrl, alt");
            set_active(&s, 0);
            let r = call_tool(&mut s, 3, "hold", json!({"key": "meta"})).await;
            assert_eq!(r["result"]["isError"], true, "{r}");
            // The failure released what was held.
            assert!(reply_text(&r).ends_with("; released alt, ctrl"), "{r}");
            let r = call_tool(&mut s, 4, "hold", json!({"key": "alt", "down": false})).await;
            assert_eq!(reply_text(&r), "released alt; nothing held");
            assert_eq!(
                log(&s),
                [
                    "keydown ctrl",
                    "keydown alt",
                    "keyup alt",
                    "keyup ctrl",
                    "keyup alt"
                ]
            );
            for bad in [
                json!({}),
                json!({"key": "a", "button": "left"}),
                json!({"button": "x"}),
            ] {
                let r = call_tool(&mut s, 5, "hold", bad).await;
                assert_eq!(r["result"]["isError"], true, "{r}");
            }
        });
    }

    #[test]
    fn a_failed_call_releases_its_modifiers_and_everything_held() {
        local(async {
            let mut s = session("hold-fail");
            call_tool(&mut s, 2, "hold", json!({"key": "ctrl"})).await;
            let r = call_tool(
                &mut s,
                3,
                "mouse",
                json!({"x": FAIL_X, "y": 0, "modifiers": ["alt"]}),
            )
            .await;
            assert_eq!(
                reply_text(&r),
                "Error executing tool mouse: pointer failed; released ctrl"
            );
            assert_eq!(
                log(&s),
                ["keydown ctrl", "keydown alt", "keyup alt", "keyup ctrl"]
            );
            // Clicking a held button would release it: refused, and the
            // failure releases it.
            call_tool(&mut s, 4, "hold", json!({"button": "left"})).await;
            let r = call_tool(&mut s, 5, "mouse", json!({"x": 1, "y": 1})).await;
            assert!(
                reply_text(&r).contains("the left button is held; release it with hold"),
                "{r}"
            );
            assert!(reply_text(&r).ends_with("; released left"), "{r}");
            let r = call_tool(&mut s, 6, "cursor_position", json!({})).await;
            assert_eq!(reply_text(&r), "cursor at (12,34)");
        });
    }

    #[test]
    fn a_cancelled_call_leaves_nothing_held() {
        local(async {
            let mut s = session("hold-cancel");
            call_tool(&mut s, 2, "hold", json!({"key": "shift"})).await;
            send(
                &mut s,
                json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {
                    "name": "mouse",
                    "arguments": {"x": SLOW_X, "y": 0, "modifiers": ["ctrl"]}}}),
            )
            .await;
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert_eq!(log(&s), ["keydown shift", "keydown ctrl"]);
            send(
                &mut s,
                json!({"jsonrpc": "2.0", "method": "notifications/cancelled",
                       "params": {"requestId": 3}}),
            )
            .await;
            let r = call_tool(&mut s, 4, "cursor_position", json!({})).await;
            assert_eq!(r["id"], 4, "{r}");
            assert_eq!(reply_text(&r), "cursor at (12,34)");
            assert_eq!(
                log(&s),
                ["keydown shift", "keydown ctrl", "keyup ctrl", "keyup shift"]
            );
        });
    }

    #[test]
    fn end_of_input_releases_what_is_held() {
        local(async {
            let mut s = session("hold-eof");
            call_tool(&mut s, 2, "hold", json!({"button": "right"})).await;
            drop(std::mem::replace(&mut s.stdin, tokio::io::duplex(1).0));
            replies_within(&mut s, Duration::from_secs(1)).await;
            assert_eq!(log(&s), ["press right", "release right"]);
        });
    }

    #[test]
    fn drag_moves_in_steps_and_releases() {
        local(async {
            let mut s = session("drag");
            let r = call_tool(
                &mut s,
                2,
                "drag",
                json!({"from_x": 0, "from_y": 0, "to_x": 120, "to_y": 60}),
            )
            .await;
            let text = r["result"]["content"][0]["text"].as_str().unwrap();
            assert_eq!(text, "dragged left from (0,0) to (120,60) in fake");
            let log = s.keys.borrow().clone();
            assert_eq!(log[..2], ["move 0,0", "press left"]);
            assert_eq!(log[log.len() - 2..], ["move 120,60", "release left"]);
            assert_eq!(log.len(), 2 + DRAG_STEPS as usize + 1);
            // Screenshot pixels: (50, 25) of the fake's 2x image at (10, 20).
            call_tool(&mut s, 3, "screenshot", json!({"inline": false})).await;
            let r = call_tool(
                &mut s,
                4,
                "drag",
                json!({"from_x": 0, "from_y": 0, "to_x": 50, "to_y": 25, "image": true}),
            )
            .await;
            let text = r["result"]["content"][0]["text"].as_str().unwrap();
            assert_eq!(text, "dragged left from (11,21) to (111,71) in fake");
            let _ = std::fs::remove_file(
                std::env::temp_dir()
                    .join(format!("desktop-mcp-test-{}-drag.png", std::process::id())),
            );
        });
    }

    fn dialog(app: &str, title: &str, parent: Option<&str>) -> FileDialog<u32> {
        FileDialog {
            window: 1,
            app: app.into(),
            titles: vec![title.into()],
            parent: parent.map(str::to_string),
            field: 2,
            accept: None,
        }
    }

    #[test]
    fn file_dialogs_are_filled_only_for_allowed_apps() {
        let allowed = vec!["firefox".to_string(), "kate".to_string()];
        let owner = |d: &FileDialog<u32>| file_dialog_owner(d, &allowed);
        let portal = "xdg-desktop-portal-kde";
        let title = "Dosya gönder - Google Gemini — Mozilla Firefox";
        // The compositor's transient parent decides when it is known.
        assert_eq!(
            owner(&dialog(portal, "x", Some("Firefox"))).unwrap(),
            "firefox"
        );
        assert!(owner(&dialog(portal, title, Some("telegram-desktop"))).is_err());
        assert!(owner(&dialog(portal, title, Some(""))).is_err());
        // Without a parent the title is not trusted, even when it names an
        // allowed window.
        assert!(owner(&dialog(portal, title, None)).is_err());
        // An allowed app's own dialog; any other app's is refused.
        assert_eq!(owner(&dialog("Kate", "Aç", None)).unwrap(), "kate");
        assert!(owner(&dialog("telegram-desktop", title, Some("firefox"))).is_err());
    }

    #[test]
    fn dialog_paths_expand_home_and_must_be_absolute() {
        let home = std::path::Path::new("/home/u");
        let exp = |p: &str| expand_path(p, Some(home));
        assert_eq!(
            exp("~/İndirilenler/deneme3.mp4").unwrap(),
            PathBuf::from("/home/u/İndirilenler/deneme3.mp4")
        );
        assert_eq!(exp(" ~ ").unwrap(), PathBuf::from("/home/u"));
        assert_eq!(exp("/tmp/a b.txt").unwrap(), PathBuf::from("/tmp/a b.txt"));
        assert!(exp("~root/x").is_err());
        assert!(exp("relative/x").is_err());
        assert!(exp("/tmp/a\nb").is_err());
        assert!(exp("").is_err());
        assert!(expand_path("~/x", None).is_err());
    }

    #[test]
    fn open_needs_an_existing_path_and_save_a_folder() {
        let dir = std::env::temp_dir().join(format!("desktop-mcp-fd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("there.txt");
        std::fs::write(&file, "x").unwrap();
        let missing = dir.join("missing.txt");
        assert!(check_dialog_path(&file, "open").is_ok());
        assert!(check_dialog_path(&missing, "open").is_err());
        assert!(check_dialog_path(&missing, "save").is_ok());
        assert!(check_dialog_path(&file, "save").is_ok());
        assert!(check_dialog_path(&dir, "save").is_err());
        assert!(check_dialog_path(&dir.join("no/such.txt"), "save").is_err());
        assert!(check_dialog_path(&file, "delete").is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn file_dialog_needs_a_backend_that_has_them() {
        local(async {
            let mut s = session("filedialog");
            let r = call_tool(&mut s, 2, "file_dialog", json!({"path": "/"})).await;
            assert_eq!(r["result"]["isError"], true, "{r}");
            let text = r["result"]["content"][0]["text"].as_str().unwrap();
            assert!(text.contains(backend::NO_FILE_DIALOG), "{text}");
        });
    }

    #[test]
    fn apps_lists_allowed_windows_and_blocked_names_on_one_line() {
        let w = |title: &str, active: bool| backend::WindowEntry {
            kind: "frame".into(),
            title: title.into(),
            active,
        };
        let app = |name: &str, windows| AppEntry {
            name: name.into(),
            windows,
        };
        let list = || {
            vec![
                app("kwin", vec![w("", false)]),
                app("konsole", vec![w("~ : claude", false)]),
                app(
                    "Firefox",
                    vec![w("Page — Mozilla Firefox", true), w("", false)],
                ),
                app("konsole", vec![w("other", false)]),
                app("plasmashell", vec![w("Desktop", false), w("", false)]),
                app("idle", vec![]),
            ]
        };
        let file = PathBuf::from("/home/u/.config/desktop-mcp/allow.txt");
        let allow = vec!["firefox".to_string()];
        assert_eq!(
            apps_text(list(), &allow, &file, "restart note"),
            "Firefox (allowed)\n  - frame: 'Page — Mozilla Firefox' [active]\n\
             blocked: kwin, konsole, plasmashell"
        );
        // An untitled window is listed when it is the active one; so is
        // a blocked app's.
        let mut l = list();
        l[1].windows[0].active = true;
        l[2].windows[0].active = false;
        l[2].windows[1].active = true;
        assert_eq!(
            apps_text(l, &allow, &file, "restart note"),
            "Firefox (allowed)\n  - frame: 'Page — Mozilla Firefox'\n  - frame: '' [active]\n\
             blocked: kwin, konsole [active], plasmashell"
        );
        // Nothing allowed: where the allowlist is, and the note.
        assert_eq!(
            apps_text(list(), &[], &file, "restart note"),
            "blocked: kwin, konsole, Firefox [active], plasmashell\n\
             no allowed app has a window; allowlist file: \
             /home/u/.config/desktop-mcp/allow.txt\nrestart note"
        );
    }

    #[test]
    fn window_reached_checks_asked_fields_only() {
        let w = WindowGeo {
            x: 100.0,
            y: 80.0,
            width: 700.0,
            height: 450.4,
            ..WindowGeo::default()
        };
        let ask = |x, width, state: Option<&str>| WindowChange {
            x,
            width,
            state: state.map(str::to_string),
            ..WindowChange::default()
        };
        assert!(window_reached(&w, &ask(Some(100.0), Some(700.0), None)));
        assert!(window_reached(&w, &ask(None, None, Some("restore"))));
        assert!(!window_reached(&w, &ask(Some(300.0), None, None)));
        assert!(!window_reached(&w, &ask(None, None, Some("maximize"))));
        let max = WindowGeo {
            maximized: true,
            ..w.clone()
        };
        assert!(window_reached(&max, &ask(None, None, Some("maximize"))));
        assert!(!window_reached(&max, &ask(None, None, Some("restore"))));
    }

    fn reply_text(r: &Json) -> &str {
        r["result"]["content"][0]["text"].as_str().unwrap()
    }

    /// Issue an id for the fake's focused element (through the focus note of
    /// `key`) and return it.
    async fn focus_id(s: &mut Session, call: u64) -> i64 {
        let r = call_tool(s, call, "key", json!({"combo": "a"})).await;
        let text = reply_text(&r);
        let at = text.find("focus: [").expect(text) + "focus: [".len();
        text[at..].split(']').next().unwrap().parse().unwrap()
    }

    fn actions_run(s: &Session) -> usize {
        s.keys
            .borrow()
            .iter()
            .filter(|k| k.starts_with("action"))
            .count()
    }

    #[test]
    fn an_unchanged_id_acts() {
        local(async {
            let mut s = session("fresh");
            let id = focus_id(&mut s, 2).await;
            let r = call_tool(&mut s, 3, "act", json!({"id": id})).await;
            assert_eq!(reply_text(&r), "Press -> ok", "{r}");
            assert_eq!(actions_run(&s), 1);
        });
    }

    #[test]
    fn a_renamed_or_changed_element_is_stale_and_not_acted_on() {
        local(async {
            let mut s = session("stale");
            let id = focus_id(&mut s, 2).await;
            s.focus.borrow_mut().name = "Cancel".into();
            let r = call_tool(&mut s, 3, "act", json!({"id": id})).await;
            assert_eq!(r["result"]["isError"], true, "{r}");
            assert_eq!(
                reply_text(&r),
                format!(
                    "Error executing tool act: stale id {id} (was push button \"OK\", \
                     now push button \"Cancel\"); call tree/find again"
                )
            );
            // Reads and pointer tools check too.
            for (n, tool) in [(4, "get_text"), (5, "click"), (6, "focus")] {
                let r = call_tool(&mut s, n, tool, json!({"id": id})).await;
                assert!(reply_text(&r).contains(": stale id"), "{tool}: {r}");
            }
            // Same name, other role: a reused object path.
            s.focus.borrow_mut().name = "OK".into();
            s.focus.borrow_mut().role = "check box".into();
            let r = call_tool(&mut s, 7, "act", json!({"id": id})).await;
            assert!(reply_text(&r).contains("now check box \"OK\""), "{r}");
            // Gone: the backend no longer knows the object.
            *s.focus.borrow_mut() = Info::default();
            let r = call_tool(&mut s, 8, "act", json!({"id": id})).await;
            assert!(reply_text(&r).contains("now gone"), "{r}");
            assert_eq!(actions_run(&s), 0);
            assert!(!s.keys.borrow().iter().any(|k| k.contains("move")));
        });
    }

    #[test]
    fn names_that_follow_content_are_not_compared() {
        local(async {
            let mut s = session("volatile");
            *s.focus.borrow_mut() = Info {
                role: "entry".into(),
                name: "Search".into(),
                actions: vec!["Activate".into()],
                ..Info::default()
            };
            let entry = focus_id(&mut s, 2).await;
            s.focus.borrow_mut().name = "Search: rust".into();
            let r = call_tool(&mut s, 3, "act", json!({"id": entry})).await;
            assert_eq!(reply_text(&r), "Activate -> ok", "{r}");
            // An unnamed element may gain a name; its role still counts.
            *s.focus.borrow_mut() = Info {
                role: "push button".into(),
                actions: vec!["Press".into()],
                ..Info::default()
            };
            let unnamed = focus_id(&mut s, 4).await;
            s.focus.borrow_mut().name = "Play".into();
            let r = call_tool(&mut s, 5, "act", json!({"id": unnamed})).await;
            assert_eq!(reply_text(&r), "Press -> ok", "{r}");
            assert_eq!(actions_run(&s), 2);
        });
    }

    #[test]
    fn stable_names_exclude_editable_and_titled_roles() {
        let info = |role: &str, name: &str, states: Vec<&'static str>| Info::<u32> {
            role: role.into(),
            name: name.into(),
            states,
            ..Info::default()
        };
        assert!(name_is_stable(&info("push button", "OK", vec![])));
        assert!(name_is_stable(&info("list item", "a.txt", vec![])));
        assert!(!name_is_stable(&info("push button", "", vec![])));
        assert!(!name_is_stable(&info("frame", "Untitled - Kate", vec![])));
        assert!(!name_is_stable(&info("page tab", "Loading…", vec![])));
        assert!(!name_is_stable(&info("section", "x", vec!["editable"])));
    }
    #[test]
    fn focus_in_an_app_not_allowed_is_not_reported() {
        local(async {
            let mut s = session("focusapp");
            *s.focus_app.borrow_mut() = "secret".into();
            let r = call_tool(&mut s, 2, "key", json!({"combo": "a"})).await;
            assert_eq!(reply_text(&r), "sent a x1 to fake", "{r}");
            // Focus moved to another allowed app: the id is tagged with it.
            *s.focus_app.borrow_mut() = "Fake".into();
            let id = focus_id(&mut s, 3).await;
            let r = call_tool(&mut s, 4, "act", json!({"id": id})).await;
            assert_eq!(reply_text(&r), "Press -> ok", "{r}");
        });
    }

    #[test]
    fn set_text_types_right_away_where_writes_did_nothing_before() {
        local(async {
            let mut s = session("inert");
            *s.field.borrow_mut() = Some("old".into());
            let id = focus_id(&mut s, 2).await;
            let reads = |s: &Session| s.keys.borrow().iter().filter(|k| *k == "read").count();
            let start = Instant::now();
            let r = call_tool(&mut s, 3, "set_text", json!({"id": id, "text": "one"})).await;
            let first = start.elapsed();
            assert!(reply_text(&r).contains("ok (verified; typed"), "{r}");
            assert!(first >= SET_TEXT_VERIFY, "{first:?}");
            assert_eq!(s.field.borrow().as_deref(), Some("one"));
            // Known now: one look at the field, then typing, then verified.
            s.keys.borrow_mut().clear();
            let start = Instant::now();
            let r = call_tool(&mut s, 4, "set_text", json!({"id": id, "text": "two"})).await;
            assert!(reply_text(&r).contains("ok (verified; typed"), "{r}");
            assert!(start.elapsed() < SET_TEXT_VERIFY, "{:?}", start.elapsed());
            assert_eq!(s.field.borrow().as_deref(), Some("two"));
            assert_eq!(reads(&s), 2, "{:?}", s.keys.borrow());
            // Text with a newline is never retyped: the usual wait, unchanged.
            let start = Instant::now();
            let r = call_tool(&mut s, 5, "set_text", json!({"id": id, "text": "a\nb"})).await;
            assert!(r.to_string().contains("the field now holds"), "{r}");
            assert!(start.elapsed() >= SET_TEXT_VERIFY);
            assert_eq!(s.field.borrow().as_deref(), Some("two"));
        });
    }

    #[test]
    fn act_and_set_text_wait_for_running_input() {
        local(async {
            let mut s = session("actlock");
            let id = focus_id(&mut s, 2).await;
            s.keys.borrow_mut().clear();
            for (n, tool, args) in [
                (3, "key", json!({"combo": "a"})),
                (4, "act", json!({"id": id})),
                (5, "key", json!({"combo": "b"})),
                (6, "set_text", json!({"id": id, "text": "x"})),
            ] {
                send(
                    &mut s,
                    json!({"jsonrpc": "2.0", "id": n, "method": "tools/call",
                    "params": {"name": tool, "arguments": args}}),
                )
                .await;
            }
            // set_text's read-back keeps polling for SET_TEXT_VERIFY, then
            // its retype must not wait for the lock it already holds.
            let replies = replies_within(&mut s, Duration::from_secs(4)).await;
            assert_eq!(replies.len(), 4, "{replies:?}");
            assert_eq!(
                *s.keys.borrow(),
                vec!["down a", "up a", "action 0 on 7", "down b", "up b"]
            );
        });
    }
    /// The `[N]` ids in a reply, in order.
    fn printed_ids(text: &str) -> Vec<i64> {
        text.split('[')
            .filter_map(|p| p.split(']').next()?.parse().ok())
            .collect()
    }

    #[test]
    fn batch_steps_refer_to_elements_earlier_steps_found() {
        local(async {
            let mut s = session("batch-refs");
            let r = call_tool(
                &mut s,
                2,
                "batch",
                json!({"steps": [
                    {"tool": "find", "args": {"app": "fake", "query": "OK"}},
                    {"tool": "act", "args": {"id": "$1"}},
                    {"tool": "pause", "args": {"ms": 0}},
                    {"tool": "act", "args": {"app": "fake", "query": "ok"}},
                    {"tool": "get_text", "args": {"id": "$4"}},
                ]}),
            )
            .await;
            assert_eq!(r["result"]["isError"], false, "{r}");
            let lines: Vec<&str> = reply_text(&r).lines().collect();
            assert_eq!(lines.len(), 5, "{r}");
            let found = printed_ids(lines[0].strip_prefix("[1] find: ").unwrap())[0];
            assert_eq!(lines[1], "[2] act: Press -> ok", "{r}");
            // The query picked the same element, under the same id.
            assert_eq!(
                lines[3],
                format!("[4] act: [{found}] push button \"OK\": Press -> ok"),
                "{r}"
            );
            assert!(lines[4].starts_with("[5] get_text: "), "{r}");
            assert_eq!(*s.keys.borrow(), ["action 0 on 7", "action 0 on 7"], "{r}");
        });
    }

    #[test]
    fn a_bad_batch_reference_fails_its_step() {
        local(async {
            let mut s = session("batch-badref");
            let r = call_tool(
                &mut s,
                2,
                "batch",
                json!({"steps": [
                    {"tool": "pause", "args": {"ms": 0}},
                    {"tool": "wait_for", "optional": true,
                     "args": {"app": "fake", "query": "never there", "timeout_ms": 0}},
                    {"tool": "act", "args": {"id": "$1"}, "optional": true},
                    {"tool": "act", "args": {"id": "$2"}, "optional": true},
                    {"tool": "act", "args": {"id": "$5"}, "optional": true},
                    {"tool": "act", "args": {"id": "$x"}, "optional": true},
                    // Text arguments are never references.
                    {"tool": "clipboard_set", "args": {"text": "$1"}},
                    {"tool": "act", "args": {"id": "$0"}},
                    {"tool": "key", "args": {"combo": "a"}},
                ]}),
            )
            .await;
            assert_eq!(r["result"]["isError"], true, "{r}");
            let lines: Vec<&str> = reply_text(&r).lines().collect();
            assert_eq!(
                lines[2],
                "[3] act failed (optional): `id` \"$1\": step 1 (pause) produced no element \
                 (find, wait_for, and tools given an element do)"
            );
            assert_eq!(
                lines[3],
                "[4] act failed (optional): `id` \"$2\": step 2 (wait_for) failed"
            );
            assert_eq!(
                lines[4],
                "[5] act failed (optional): `id` \"$5\": refer to an earlier step (1 to 4)"
            );
            assert_eq!(
                lines[5],
                "[6] act failed (optional): `id` \"$x\": use \"$N\" for the element step N produced"
            );
            assert!(lines[6].starts_with("[7] clipboard_set: "), "{r}");
            assert_eq!(
                lines[7],
                "[8] act failed: `id` \"$0\": refer to an earlier step (1 to 7)"
            );
            assert_eq!(lines[8], "stopped; 1 step(s) not run");
            assert_eq!(actions_run(&s), 0);
            assert!(s.keys.borrow().is_empty());
        });
    }

    #[test]
    fn reading_an_element_again_keeps_its_id_until_it_changes() {
        local(async {
            let mut s = session("stable-ids");
            let find = json!({"app": "fake", "query": "OK"});
            let first = printed_ids(reply_text(
                &call_tool(&mut s, 2, "find", find.clone()).await,
            ));
            let again = printed_ids(reply_text(&call_tool(&mut s, 3, "find", find).await));
            assert_eq!(first.len(), 1);
            assert_eq!(first, again);
            assert_eq!(focus_id(&mut s, 4).await, first[0]);
            // Renamed: a new id, and the old one stays stale.
            s.focus.borrow_mut().name = "Cancel".into();
            let r = call_tool(&mut s, 5, "find", json!({"app": "fake", "query": "Cancel"})).await;
            let renamed = printed_ids(reply_text(&r));
            assert_ne!(renamed, first, "{r}");
            let r = call_tool(&mut s, 6, "act", json!({"id": first[0]})).await;
            assert!(reply_text(&r).contains(": stale id"), "{r}");
            let r = call_tool(&mut s, 7, "act", json!({"id": renamed[0]})).await;
            assert_eq!(reply_text(&r), "Press -> ok", "{r}");
            // A volatile name may change and keep the id.
            *s.focus.borrow_mut() = Info {
                role: "entry".into(),
                name: "Search".into(),
                showing: true,
                ..Info::default()
            };
            let entry = focus_id(&mut s, 8).await;
            s.focus.borrow_mut().name = "Search: rust".into();
            assert_eq!(focus_id(&mut s, 9).await, entry);
        });
    }

    #[test]
    fn concurrent_batches_do_not_interleave_their_input() {
        local(async {
            let mut s = session("batch-lock");
            for (id, a, b) in [(2, "a", "b"), (3, "c", "d")] {
                send(
                    &mut s,
                    json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
                    "params": {"name": "batch", "arguments": {"steps": [
                        {"tool": "key", "args": {"combo": a}},
                        {"tool": "find", "args": {"app": "fake", "query": "OK"}},
                        {"tool": "key", "args": {"combo": b}},
                    ]}}}),
                )
                .await;
            }
            let replies = replies_within(&mut s, Duration::from_millis(800)).await;
            assert_eq!(replies.len(), 2, "{replies:?}");
            assert_eq!(
                *s.keys.borrow(),
                [
                    "down a", "up a", "down b", "up b", "down c", "up c", "down d", "up d"
                ]
            );
        });
    }

    #[test]
    fn a_cancelled_batch_lets_go_of_the_input_lock() {
        local(async {
            let mut s = session("batch-cancel");
            send(
                &mut s,
                json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                "params": {"name": "batch", "arguments": {"steps": [
                    {"tool": "key", "args": {"combo": "a"}},
                    {"tool": "pause", "args": {"ms": 5000}},
                    {"tool": "key", "args": {"combo": "b"}},
                ]}}}),
            )
            .await;
            tokio::time::sleep(Duration::from_millis(150)).await;
            send(
                &mut s,
                json!({"jsonrpc": "2.0", "method": "notifications/cancelled",
                "params": {"requestId": 2}}),
            )
            .await;
            let r = call_tool(&mut s, 3, "key", json!({"combo": "c"})).await;
            assert_eq!(r["id"], 3, "{r}");
            assert_eq!(*s.keys.borrow(), ["down a", "up a", "down c", "up c"]);
        });
    }
}
