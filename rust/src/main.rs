//! Screenshot-free desktop control MCP server for KDE Plasma (Wayland),
//! Hyprland and Windows.
//!
//! Same tools and output as the former Python server (server.py, removed
//! after 4cd2632), plus `batch`.
//! Every app-scoped operation is restricted to the allowlist (re-read on
//! every call). Platform code lives behind the `Backend` trait.

mod backend;
mod delivery;
mod flow;
mod launch_pick;
#[cfg(target_os = "linux")]
mod linux;
mod policy;
mod render;
mod semantic;
mod shot;
mod takeover;
mod tools;
mod trace;
mod update;
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
    Backend, Cache, Clip, Fetch, FileDialog, Focused, Info, R, Shot, Toggles, WindowChange,
    WindowGeo,
};
use policy::{Policy, Tier};
use render::{Line, Printed, WalkOpts};
use semantic::Verb;

const MAX_REFS: usize = 20000;
/// Text read per element for tree lines: one more character than they
/// show, so a cut is visible.
const TREE_TEXT_LIMIT: usize = render::PREVIEW_CHARS + 1;
/// Longest `tree` reply by default, in characters (about 5k tokens).
const TREE_MAX_CHARS: i64 = 20_000;
/// Characters kept free in a capped tree for the line saying how to go on.
const CONTINUATION_CHARS: usize = 150;
/// Nodes fetched ahead when rendering reaches an unfetched part of the tree.
const PREFETCH_MORE: usize = 500;
/// Candidates an ambiguous target lists (`Server::untie`).
const AMBIGUOUS_SHOWN: usize = 6;
/// Candidates of an ambiguous target measured for an on-screen area at
/// most (`Server::untie`): more are refused as they are, as dropping the
/// ones without would rarely leave one.
const AREA_CHECK_MAX: usize = 2 * AMBIGUOUS_SHOWN;
/// Off-screen matches or close names a missed search offers.
const MISS_SUGGESTIONS: usize = 5;
/// Nodes of a web page fetched when looking for matches scrolled out of
/// view (see `Server::miss_report`).
const OFFSCREEN_FETCH: usize = 3000;

/// What `Server::locate` found: the matches, or the windows searched; with
/// the nodes fetched.
enum Located<Id> {
    /// The matches, the nodes read, and whether they came from a kept read.
    Found(render::Matches<Id>, Cache<Id>, bool),
    /// The windows searched and the nodes read there.
    Missed(Vec<Id>, Cache<Id>),
}

/// How searches (find, locate, the active document) fetch: showing nodes,
/// without action names, which only the nodes they return need.
fn search_fetch(text_limit: usize, limit: usize) -> Fetch {
    Fetch {
        only_showing: true,
        text_limit,
        limit,
        actions: false,
        stop_roles: &[],
        offscreen: false,
    }
}

/// `render::locate_all`, and when nothing of `role` matches, again with its
/// synonyms (`render::with_role_synonyms`; `by_synonym` set then).
fn locate_with_synonyms<Id: Clone + Eq + std::hash::Hash>(
    cache: &Cache<Id>,
    windows: &[Id],
    query: &str,
    role: &str,
    titles: bool,
    window: &str,
) -> Result<Option<render::Matches<Id>>, Vec<Id>> {
    let (m, by_synonym) = render::with_role_synonyms(role, Option::is_some, |r| {
        render::locate_all(cache, windows, query, r, titles, window)
    })?;
    Ok(m.map(|mut m| {
        m.by_synonym = by_synonym;
        m
    }))
}

/// How long a read of an app's tree is reused at most, even when the app
/// reported no change: a safety net for changes no event announces.
const READ_MAX_AGE: Duration = Duration::from_secs(2);
/// Reads kept at once (one per app and `ReadKey`).
const READS_KEPT: usize = 4;

/// How a read of an app's tree fetched: kept reads are reused only by
/// reads that fetch the same way. Reads with `stop_roles` are not kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ReadKey {
    only_showing: bool,
    text_limit: usize,
    limit: usize,
    actions: bool,
}

impl ReadKey {
    fn of(f: &Fetch) -> Option<Self> {
        f.stop_roles.is_empty().then_some(ReadKey {
            only_showing: f.only_showing,
            text_limit: f.text_limit,
            limit: f.limit,
            actions: f.actions,
        })
    }
}

/// Taken before a read that may be kept: the backend's change mark, the
/// input generation (`Reads::generation`) and the time.
#[derive(Clone, Copy, Debug)]
struct ReadStart {
    mark: u64,
    generation: u64,
    at: Instant,
}

/// A read of an app's windows kept for reuse (`Server::kept_read`).
struct KeptRead<Id> {
    app: String,
    key: ReadKey,
    windows: Vec<Id>,
    cache: Cache<Id>,
    start: ReadStart,
}

/// Reads of apps' trees kept for the next calls, and what invalidates them.
struct Reads<Id> {
    /// Bumped when a call that may change what apps show (input) starts
    /// and when it ends: reads that began before are never reused.
    generation: u64,
    /// Such calls running now: meanwhile no read is kept or reused.
    inputs: u32,
    kept: Vec<KeptRead<Id>>,
}

impl<Id> Reads<Id> {
    fn new() -> Self {
        Reads {
            generation: 0,
            inputs: 0,
            kept: Vec::new(),
        }
    }

    fn invalidate(&mut self) {
        self.generation += 1;
        self.kept.clear();
    }
}

/// Marks a call that may change what apps show as running, for as long as
/// it lives: kept reads are dropped when it starts and when it ends.
struct InputRun<'a, Id>(&'a std::sync::Mutex<Reads<Id>>);

impl<'a, Id> InputRun<'a, Id> {
    fn start(reads: &'a std::sync::Mutex<Reads<Id>>) -> Self {
        let mut r = reads.lock().unwrap_or_else(|e| e.into_inner());
        r.inputs += 1;
        r.invalidate();
        InputRun(reads)
    }
}

impl<Id> Drop for InputRun<'_, Id> {
    fn drop(&mut self) {
        let mut r = self.0.lock().unwrap_or_else(|e| e.into_inner());
        r.inputs = r.inputs.saturating_sub(1);
        r.invalidate();
    }
}

/// Tools that change nothing an app shows (without `window`, which may
/// activate one): they keep and reuse reads. A batch's steps count one by
/// one. Every other tool drops the kept reads.
const READ_ONLY_TOOLS: [&str; 12] = [
    "apps",
    "tree",
    "find",
    "get_text",
    "wait_for",
    "wait_idle",
    "pause",
    "windows",
    "doctor",
    "cursor_position",
    "screenshot",
    "batch",
];

/// The nearest frame, window or dialog at or above `id` in `cache` (what
/// the backend would walk up to); None when the way up is not known for
/// sure (a node listed by two parents).
fn frame_in<Id: Clone + Eq + std::hash::Hash>(cache: &Cache<Id>, id: &Id) -> Option<Id> {
    let mut parents: HashMap<&Id, Option<&Id>> = HashMap::new();
    for (p, info) in cache {
        for c in &info.children {
            parents
                .entry(c)
                .and_modify(|seen| {
                    if *seen != Some(p) {
                        *seen = None;
                    }
                })
                .or_insert(Some(p));
        }
    }
    let mut cur = id;
    for _ in 0..=cache.len() {
        let info = cache.get(cur)?;
        if matches!(info.role.as_str(), "frame" | "window" | "dialog") {
            return Some(cur.clone());
        }
        cur = (*parents.get(cur)?)?;
    }
    None
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
    allow: &Policy,
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
        let Some(tier) = allow.tier(&app.name) else {
            let active = app.windows.iter().any(|w| w.active);
            match blocked.iter_mut().find(|(n, _)| *n == app.name) {
                Some((_, a)) => *a |= active,
                None => blocked.push((app.name, active)),
            }
            continue;
        };
        lines.push(match tier {
            Tier::Full => format!("{} (allowed)", app.name),
            t => format!("{} (allowed: {t} tier)", app.name),
        });
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
    /// The backend; what writes to the desktop is noted (`delivery`).
    b: delivery::Sends<B>,
    refs: Mutex<Refs<B::Id>>,
    allow_file: PathBuf,
    /// Title of the active window as last reported, to mention changes.
    last_active: Mutex<Option<String>>,
    /// The window in front after the last input, for `window_change_note`.
    seen_active: Mutex<Option<String>>,
    /// The surfaces (menus, dialogs; `id_hash`) last listed without change
    /// events (`Server::surfaces_note` scans then), so one still open is
    /// not listed again.
    seen_surfaces: Mutex<Vec<u64>>,
    /// The change mark taken when the last input's reply listed what it
    /// opened, and the surfaces it listed (`id_hash`): what showed after
    /// it (a dialog that opened after that reply) is listed by the next
    /// input's reply (`Server::opened_before`).
    shown_pending: Mutex<Option<(u64, Vec<u64>)>>,
    /// Maps pixels of the last screenshot to screen coordinates.
    last_shot: Mutex<Option<ShotMap>>,
    /// Held by tools that send keyboard or pointer input, so two concurrent
    /// calls never interleave their events.
    input: Mutex<()>,
    /// `app_key`s of apps whose accessibility text writes report success
    /// and change nothing (Firefox); set_text types there right away.
    inert_writes: Mutex<HashSet<String>>,
    /// The user's input and the stop shortcut, when the backend watches them.
    takeover: Option<std::sync::Arc<takeover::Takeover>>,
    /// When the current holder of `input` took it, then when its first
    /// gated input went ahead: user input after this interrupts the rest of
    /// its run (a batch's later steps, a drag).
    run_start: std::sync::Mutex<Option<Instant>>,
    /// A gated input of the current run went ahead (`Server::gate_wait`):
    /// until then, the user's input only pauses the run.
    run_acted: std::sync::atomic::AtomicBool,
    /// Set by an input that likely changes a web page in place (a press,
    /// a click, Enter), for `call_inner`'s content wait
    /// (`Server::content_wait`).
    content: std::sync::Mutex<Option<ContentNote<B::Id>>>,
    /// Change mark taken when the current input call started (with its
    /// window in front): an app that changed between it and the input was
    /// already changing (`Server::pre_content`). None after an activation,
    /// whose own events would count.
    content_base: std::sync::Mutex<Option<u64>>,
    /// Keys and buttons down now, in the order they were pressed: held by
    /// `hold` across calls, or by an input tool for the length of its
    /// action (a click's modifiers, a drag's button). A cancelled call
    /// leaves its own there, so `release_all` finds them. Never locked
    /// across an await.
    held: std::sync::Mutex<Vec<Held>>,
    /// The way back to the client, for requests the server sends it
    /// (elicitation); see `Server::request`.
    link: std::cell::RefCell<ClientLink>,
    /// Reads of apps' trees kept for the next calls (`Server::read_app`).
    /// Never locked across an await.
    reads: std::sync::Mutex<Reads<B::Id>>,
    /// Per app, the window `Server::page_of` last looked in and the web
    /// page document it found there (None: none), read again while that
    /// window is the one and the document shows (None: for up to
    /// NO_PAGE_RECHECK after that look). Never locked across an await.
    #[allow(clippy::type_complexity)]
    page_docs: std::sync::Mutex<HashMap<String, (B::Id, Option<B::Id>, Instant)>>,
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

/// The server's side of requests it sends the client. `run_loop` sets `out`
/// and hands responses to `pending`; `initialize` notes what the client can
/// do.
#[derive(Default)]
struct ClientLink {
    /// The client declared `elicitation` in initialize: it can ask the user.
    elicitation: bool,
    out: Option<tokio::sync::mpsc::UnboundedSender<Json>>,
    /// Requests waiting for their response, by id (its JSON text).
    pending: HashMap<String, tokio::sync::oneshot::Sender<Json>>,
    next: u64,
}

/// How long a confirmation waits for the user's answer.
const CONFIRM_WAIT: Duration = Duration::from_secs(300);

/// The tool call running on this task, for the allowlist checks deep in
/// the tools (`Server::permit`): its name, its arguments, and whether the
/// user already confirmed it. Set by `call_inner` (per batch step too).
struct Gate {
    tool: String,
    args: Json,
    confirmed: bool,
    /// The change mark taken before the input of a call whose reply a
    /// batch tests, until `Server::settle` waited for it.
    settle: Option<u64>,
    /// The input is likely to open a menu, list or dialog (a click on a
    /// combo box or menu entry): its reply waits for one to show.
    opener: bool,
    /// The reply lists what opened already (`menu` listing a submenu).
    quiet: bool,
    /// The surfaces (`id_hash`) this call's reply listed already.
    reported: Vec<u64>,
    /// "before it, opened: …" lines for what showed after the previous
    /// input's reply (`Server::opened_before`), said with this call's.
    before: String,
}

tokio::task_local! {
    static GATE: std::cell::RefCell<Gate>;
}

/// `tool` and its arguments, briefly, for a confirmation question: long
/// texts cut, `confirm` and `window` left out.
fn describe_call(tool: &str, app: Option<&str>, a: &Json) -> String {
    let mut args = a.clone();
    if let Some(map) = args.as_object_mut() {
        map.remove("confirm");
        for v in map.values_mut() {
            if let Some(t) = v.as_str()
                && t.chars().count() > 200
            {
                let cut: String = t.chars().take(200).collect();
                *v = Json::String(format!("{cut}… ({} chars)", t.chars().count()));
            }
        }
    }
    let on = app.map(|a| format!(" on {a}")).unwrap_or_default();
    match args.as_object() {
        Some(m) if !m.is_empty() => format!("{tool}{on} {args}"),
        _ => format!("{tool}{on}"),
    }
}

/// Tools that send keyboard or pointer input, run an app's actions (`act`
/// presses buttons and opens menus; `set_text` may retype through the
/// keyboard), or use the clipboard that type_text pastes through; they run
/// one at a time.
const INPUT_TOOLS: [&str; 15] = [
    "act",
    "menu",
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

/// Tools that act on the seat or what the user sees: they pause while the
/// user uses the keyboard or mouse, and stop after the stop shortcut. The
/// clipboard tools send no input and are not held up.
const GATED_TOOLS: [&str; 16] = [
    "act",
    "menu",
    "close_window",
    "hold",
    "set_text",
    "focus",
    "click",
    "drag",
    "scroll",
    "key",
    "type_text",
    "mouse",
    "activate_window",
    "file_dialog",
    "move_window",
    "launch",
];

/// Tools whose replies and errors say whether input was sent (`delivery`):
/// the gated ones and clipboard_set. A batch says it per step and when it
/// stops.
const DELIVERY_TOOLS: [&str; 17] = [
    "act",
    "menu",
    "close_window",
    "hold",
    "set_text",
    "focus",
    "click",
    "drag",
    "scroll",
    "key",
    "type_text",
    "mouse",
    "activate_window",
    "file_dialog",
    "move_window",
    "launch",
    "clipboard_set",
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
    /// The focus a step of the running batch reported last, so later steps
    /// with the same focus say "(unchanged)" instead of describing it again.
    static BATCH_FOCUS: std::cell::RefCell<Option<String>>;
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

tokio::task_local! {
    /// The refusals made while the running call (or batch step) ran: see
    /// `refusal` and `with_refusals`.
    static REFUSALS: std::cell::RefCell<Vec<String>>;
}

/// Note `e` as a refusal (by the allowlist, a tier, a confirmation the user
/// declined or was not asked for, a password field, or the user taking
/// over), for the batch (`with_refusals`); returns it. On_fail steps never
/// run after one, and it ends a repeat.
fn refusal(e: String) -> String {
    let _ = REFUSALS.try_with(|r| r.borrow_mut().push(e.clone()));
    e
}

/// Run `f`; its result, and whether its error is (or carries) a refusal
/// made while it ran. A refusal a tool got past (one candidate refused,
/// another taken) does not count: only one its error names.
async fn with_refusals<T>(f: impl Future<Output = R<T>>) -> (R<T>, bool) {
    REFUSALS
        .scope(std::cell::RefCell::new(Vec::new()), async {
            let r = f.await;
            let refused = match &r {
                Ok(_) => false,
                Err(e) => REFUSALS.with(|v| v.borrow().iter().any(|m| e.contains(m.as_str()))),
            };
            (r, refused)
        })
        .await
}

/// What one tool call of a batch step left.
struct StepResult {
    result: R<Out>,
    /// The element it produced (`STEP_ELEMENT`).
    element: Option<u64>,
    /// It failed by a refusal (`with_refusals`).
    refused: bool,
    /// It sent input (`delivery::track`).
    wrote: bool,
}

/// A running batch: its reply so far and what its steps left.
#[derive(Default)]
struct BatchRun {
    out: Vec<String>,
    images: Vec<(String, &'static str)>,
    /// Records of the top-level steps run so far, for `"$N"` and conditions.
    done: Vec<flow::Record>,
    /// A printed step failed.
    failed: bool,
    /// Top-level steps that sent input, and those that failed after
    /// sending some (`delivery::stopped`).
    sent: Vec<usize>,
    maybe: Vec<usize>,
}

impl BatchRun {
    /// Note what a call of top-level step `n` sent: maybe once one of its
    /// calls failed after writing, else sent once one wrote.
    fn deliver(&mut self, n: usize, r: &StepResult) {
        if !r.wrote {
            return;
        }
        if r.result.is_err() {
            self.sent.retain(|&k| k != n);
            if !self.maybe.contains(&n) {
                self.maybe.push(n);
            }
        } else if !self.sent.contains(&n) && !self.maybe.contains(&n) {
            self.sent.push(n);
        }
    }

    /// The reply line of a call labelled `label` ("3", "2.1", "4.on_fail").
    fn print(&mut self, label: &str, call: &flow::Call, r: &StepResult, optional: bool) {
        let tool = &call.tool;
        match &r.result {
            Ok(o) => {
                self.out.push(format!("[{label}] {tool}: {}", o.text));
                self.images.extend(o.images.iter().cloned());
            }
            Err(e) => {
                self.failed = true;
                let opt = if optional { " (optional)" } else { "" };
                self.out.push(format!("[{label}] {tool} failed{opt}: {e}"));
            }
        }
    }

    fn record(&mut self, tool: String, ok: bool, r: StepResult) {
        let text = match r.result {
            Ok(o) => o.text,
            Err(e) => e,
        };
        let element = r.element;
        self.done.push(flow::Record {
            tool,
            outcome: flow::Outcome::Ran { ok, element, text },
        });
    }

    fn skip(&mut self, tool: &str) {
        self.done.push(flow::Record {
            tool: tool.into(),
            outcome: flow::Outcome::Skipped,
        });
    }
}

/// Longest run of a batch's repeat: it starts no new run after this.
const REPEAT_TIME: Duration = Duration::from_millis(MAX_WAIT_MS as u64);

/// Longest `wait_for` / `pause`, in ms (10 minutes).
const MAX_WAIT_MS: i64 = 600_000;
/// `wait_for` re-checks when the app changed (`Backend::wait_change`), and
/// after this long without a change in any case: for apps or events that
/// do not report changes, such as an element scrolled into view.
const WAIT_FALLBACK: Duration = Duration::from_millis(250);
/// Least time between the end of one `wait_for` check and the next, so an
/// app that changes all the time costs no more than polling did.
const WAIT_MIN_GAP: Duration = Duration::from_millis(50);

/// How often `wait_idle` reads the tree where no change events come.
const IDLE_POLL: Duration = Duration::from_millis(100);

/// How often the effect line reads an app's content again while it waits
/// for a change, where the backend gets no change events.
const CONTENT_POLL: Duration = Duration::from_millis(50);

/// What `wait_for` `state` asks of the element.
#[derive(Clone, Copy, Debug, PartialEq)]
enum WaitState {
    Enabled,
    Focused,
    Checked,
    Selected,
}

impl WaitState {
    fn parse(s: &str) -> R<Self> {
        Ok(match s.trim().to_lowercase().as_str() {
            "enabled" => WaitState::Enabled,
            "focused" => WaitState::Focused,
            "checked" => WaitState::Checked,
            "selected" => WaitState::Selected,
            other => {
                return Err(format!(
                    "unknown state '{other}'; use enabled, focused, checked or selected"
                ));
            }
        })
    }

    fn name(self) -> &'static str {
        match self {
            WaitState::Enabled => "enabled",
            WaitState::Focused => "focused",
            WaitState::Checked => "checked",
            WaitState::Selected => "selected",
        }
    }

    /// Whether an element with `states` (as `Info::states`) has it.
    fn holds(self, states: &[&str]) -> bool {
        match self {
            WaitState::Enabled => !states.contains(&"disabled"),
            s => states.contains(&s.name()),
        }
    }
}

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

    /// The screen area of the image pixels (x, y, w, h).
    fn area_of(self, (x, y, w, h): (f64, f64, f64, f64)) -> (f64, f64, f64, f64) {
        (
            self.x + x * self.sx,
            self.y + y * self.sy,
            w * self.sx,
            h * self.sy,
        )
    }
}

/// The part of a capture showing screen area `area` that screen area `want`
/// covers, as fractions of the capture; None when they do not overlap.
fn region_frac(
    (ax, ay, aw, ah): (f64, f64, f64, f64),
    (x, y, w, h): (f64, f64, f64, f64),
) -> Option<shot::Frac> {
    let (x0, y0) = (x.max(ax), y.max(ay));
    let (x1, y1) = ((x + w).min(ax + aw), (y + h).min(ay + ah));
    (aw > 0.0 && ah > 0.0 && x1 > x0 && y1 > y0).then(|| shot::Frac {
        x: (x0 - ax) / aw,
        y: (y0 - ay) / ah,
        w: (x1 - x0) / aw,
        h: (y1 - y0) / ah,
    })
}

/// The screenshot's `region` argument: [x, y, w, h], None when absent.
fn region_arg(a: &Json) -> R<Option<(f64, f64, f64, f64)>> {
    let bad =
        || "region must be [x, y, width, height] in pixels of the last screenshot".to_string();
    match a.get("region") {
        None | Some(Json::Null) => Ok(None),
        Some(Json::Array(v)) if v.len() == 4 => {
            let n: Vec<f64> = v
                .iter()
                .map(Json::as_f64)
                .collect::<Option<_>>()
                .ok_or_else(bad)?;
            if n[2] <= 0.0 || n[3] <= 0.0 {
                return Err(bad());
            }
            Ok(Some((n[0], n[1], n[2], n[3])))
        }
        Some(_) => Err(bad()),
    }
}

/// What an input tool's `observe` asks to see after the input settled.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Observe {
    Nothing,
    Focus,
    Tree,
    Screenshot,
}

/// A tool's `observe` argument; None when the tool reports no effect
/// (tools::OBSERVE_TOOLS).
fn observe_arg(tool: &str, a: &Json) -> R<Option<Observe>> {
    if !tools::OBSERVE_TOOLS.contains(&tool) {
        return Ok(None);
    }
    // Without `observe` nothing is measured, so plain input stays as fast
    // as before; act, menu and close_window keep their effect line (it
    // replaced their note that the active window changed).
    let default = match tool {
        "act" | "close_window" => Some(Observe::Nothing),
        // Listing the menu bar changes nothing.
        "menu" if a.get("path").is_some_and(|p| !p.is_null()) => Some(Observe::Nothing),
        _ => None,
    };
    Ok(match a.get("observe") {
        None | Some(Json::Null) => default,
        Some(Json::String(s)) => Some(match s.trim().to_lowercase().as_str() {
            "" | "none" => return Ok(default),
            "effect" => Observe::Nothing,
            "focus" => Observe::Focus,
            "tree" => Observe::Tree,
            "screenshot" => Observe::Screenshot,
            other => {
                return Err(format!(
                    "observe '{other}': use one of {}",
                    tools::OBSERVE_KINDS.join(", ")
                ));
            }
        }),
        Some(_) => {
            return Err("observe must be a string (none, effect, focus, tree, screenshot)".into());
        }
    })
}

/// Default and longest settle before the effect line and observation.
const SETTLE_MS: i64 = 150;
const MAX_SETTLE_MS: i64 = 5000;
/// Default (unless `settle_ms` says otherwise) and longest wait for a web
/// page to change after an input that likely changes it in place
/// (`Server::content_wait`); once it changed, the wait ends when its events
/// went quiet and no page of the app is loading.
const CONTENT_MS: i64 = 1500;
/// Quiet that ends a content wait.
const CONTENT_QUIET: Duration = Duration::from_millis(150);
/// A first change this soon after the input may be a placeholder or a
/// spinner put up before a network round trip: the wait then needs
/// CONTENT_QUIET_EARLY of quiet, as when the app was already changing
/// before the input.
const CONTENT_EARLY: Duration = Duration::from_millis(100);
/// 600 ms: a page that answers a press after a short delay (a script's
/// timer, a fast fetch, 400 ms in a test page) was read stale with 350.
const CONTENT_QUIET_EARLY: Duration = Duration::from_millis(600);
/// Longest wait past the first change (or past the end of loading) while
/// no page of the app loads: events of other tabs or a live page (a clock,
/// a video) never go quiet.
const CONTENT_BURST: Duration = Duration::from_millis(500);
/// Toolkits (`toolkit_of`) of apps that show web pages, whose content
/// changes in place after a press, a click or Enter, often only after a
/// network round trip.
const WEB_TOOLKITS: [&str; 2] = ["gecko", "chromium"];
/// Keys (`chord_keys`, joined) that submit a form or move to another page.
const CONTENT_KEYS: [&str; 5] = ["enter", "kpenter", "alt+left", "alt+right", "f5"];
/// Roles a press or click on rarely changes a page's content: text fields
/// (a click there is for typing; editable elements of other roles are
/// skipped by their state), lists and menus that open (listed with
/// `opened:` instead), controls that only toggle (their state change is no
/// change event: the wait would always run out), sliders, and whole
/// documents and windows.
const NO_CONTENT_ROLES: [&str; 22] = [
    "entry",
    "text",
    "password text",
    "editbar",
    "terminal",
    "document text",
    "spin button",
    "combo box",
    "push button menu",
    "menu",
    "check box",
    "radio button",
    "toggle button",
    "switch",
    "check menu item",
    "radio menu item",
    "slider",
    "scroll bar",
    "document web",
    "document frame",
    "frame",
    "window",
];
/// Roles of a focused element where Enter types (a new line, a command),
/// besides multi-line text fields: no content wait.
const NO_CONTENT_KEY_ROLES: [&str; 2] = ["terminal", "document text"];
/// Size of the tree `observe: "tree"` returns.
const OBSERVE_TREE_NODES: i64 = 150;
const OBSERVE_TREE_CHARS: usize = 6000;

/// Taken just before an input that may change a web page in place
/// (`Server::pre_content`).
#[derive(Clone, Copy)]
struct PreContent {
    /// Change mark before the input.
    mark: u64,
    /// The element's app sent change events since the call started.
    busy: bool,
}

/// An input whose page change `Server::content_wait` waits for.
#[derive(Clone)]
struct ContentNote<Id> {
    pre: PreContent,
    /// The element the input went to.
    id: Id,
    /// When the input was sent.
    at: Instant,
}

/// What an input is compared on: the active window (app, title), the
/// focused element (id, role, name, states) and, for the allowed app the
/// input goes to, the web page its window shows. Two reads, about 1 ms,
/// and three more for the page.
#[derive(Clone, Debug, PartialEq)]
struct Fingerprint<Id> {
    window: Option<(String, String)>,
    focus: Option<(Id, String, String, Vec<&'static str>)>,
    /// The allowed app the input goes to (lower case) and its windows,
    /// whose change events count for the effect; None when not read.
    app: Option<(String, Vec<Id>)>,
    /// The page the app's window shows (`Backend::page`).
    page: Option<backend::Page>,
    /// A hash of what the app's windows show (`content_hash`), read only
    /// when the backend gets no change events: an input that changes a
    /// display but neither focus nor title still shows.
    content: Option<u64>,
}

/// After an input to a window showing a web page, how much longer than
/// the settle the effect line waits for the app to report a change: a
/// link or a web app's row starts loading the next page only after a
/// round trip of its own.
const PAGE_SETTLE: Duration = Duration::from_millis(400);

/// How long `page_of` trusts that a browser's window shows no web page (a
/// dialog, a mail app's window without a web view) before searching it
/// again: the search reads up to FIND_MAX_SEEN nodes.
const NO_PAGE_RECHECK: Duration = Duration::from_secs(5);

/// How the effect line names a page: its title, else its address.
fn page_name(p: &backend::Page) -> String {
    match (p.title.is_empty(), p.url.is_empty()) {
        (false, _) => render::py_repr(&p.title),
        (true, false) => p.url.clone(),
        (true, true) => "an untitled page".into(),
    }
}

/// The effect line: how `after` differs from `before`. `events`: whether
/// the app reported changes meanwhile (None: the backend cannot tell).
/// Titles of windows of apps not `allowed` are not named. A page that is
/// loading after the input is said so, as reading it now reads the old
/// or a half page; one that was loading already makes no change of its
/// own (its events are the load's).
fn effect_text<Id: PartialEq>(
    before: &Fingerprint<Id>,
    after: &Fingerprint<Id>,
    events: Option<bool>,
    allowed: impl Fn(&str) -> bool,
) -> String {
    let loading = if after.page.as_ref().is_some_and(|p| p.busy) {
        " (still loading: wait_idle before reading it)"
    } else {
        ""
    };
    let page = match (&before.page, &after.page) {
        (Some(b), Some(a)) if b.differs(a) => {
            let mut out = format!("page changed to {}", page_name(a));
            // The same title: the address tells the pages apart.
            if a.title == b.title && !a.title.is_empty() && !a.url.is_empty() {
                out.push_str(&format!(" ({})", a.url));
            }
            Some(out)
        }
        _ => None,
    };
    let same_app = matches!((&before.window, &after.window),
        (Some((a, _)), Some((b, _))) if a == b);
    if before.window != after.window && !(same_app && page.is_some()) {
        let text = match &after.window {
            Some((app, title)) if allowed(app) && same_app => {
                format!("changed (window title now {})", render::py_repr(title))
            }
            Some((app, title)) if allowed(app) => {
                format!("window changed to {}", render::py_repr(title))
            }
            Some((app, _)) => return format!("window changed to one of '{app}' (not allowed)"),
            None => return "window changed (none active now)".into(),
        };
        return format!("{text}{loading}");
    }
    if let Some(page) = page {
        return format!("{page}{loading}");
    }
    let was_busy = before.page.as_ref().is_some_and(|p| p.busy);
    if before.focus != after.focus {
        return match loading.is_empty() {
            true => "changed".into(),
            false => "changed (the page is still loading: wait_idle before reading it)".into(),
        };
    }
    match (was_busy, !loading.is_empty()) {
        // The input started it.
        (false, true) => "changed: the page started loading (wait_idle before reading it)".into(),
        // Loading before the input: its own changes tell nothing.
        (true, true) if events == Some(true) => {
            "not known: the page was already loading (wait_idle before reading it)".into()
        }
        (true, true) => {
            "no visible change (the page was already loading: wait_idle before reading it)".into()
        }
        _ if events == Some(true) => "changed".into(),
        // No events to go by: what the app shows differs.
        _ if events.is_none()
            && before.content.is_some()
            && after.content.is_some()
            && before.content != after.content =>
        {
            "changed".into()
        }
        _ => "no visible change".into(),
    }
}

/// `text` cut to at most `max` characters at a line end, with a note.
fn cap_lines(text: String, max: usize) -> String {
    if text.chars().count() <= max {
        return text;
    }
    let mut out = String::new();
    for line in text.lines() {
        if out.chars().count() + line.chars().count() + 1 > max {
            break;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(&format!("… (cut at {max} chars; call tree for the rest)"));
    out
}

/// A tool's reply: text, plus base64 PNGs sent as MCP image blocks.
struct Out {
    text: String,
    /// Inline images: base64 data and media type.
    images: Vec<(String, &'static str)>,
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

/// Characters `get_text` returns unless `limit` says otherwise. A whole
/// web page read 20,000 (5,000 tokens, re-read on every later turn) where
/// the part wanted was near the top; a cut reply says where to go on.
const GET_TEXT_LIMIT: i64 = 10000;

/// How long set_text waits for the field to show what was written.
const SET_TEXT_VERIFY: Duration = Duration::from_millis(300);
/// How long `act`'s verified actions wait for the state they asked for to
/// show, and the longest pause between reads meanwhile.
const VERIFY_WAIT: Duration = Duration::from_millis(500);
const VERIFY_POLL: Duration = Duration::from_millis(50);
/// Nodes `act(action=select)` reads under a control for its entries.
const OPTIONS_FETCH: usize = 2000;
/// scroll_to's wheel fallback: notches per round, rounds, and how long
/// each round waits for the element to show.
const WHEEL_STEP: i64 = 5;
const WHEEL_ROUNDS: usize = 10;
const WHEEL_WAIT: Duration = Duration::from_millis(200);
/// Why `act` opens no menu of Firefox's (see `Backend::may_open_menu`).
const NO_MENU_OPEN: &str = "opening its menu through accessibility can hang or crash this app \
     (Firefox); nothing sent: click it (or focus it and key alt+down), then act on the entry";

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
        "act" => match semantic::Verb::parse(&arg_s(a, "action", "")) {
            Some(v) => v.label().into(),
            None if a.get("option").is_some_and(Json::is_string) => "selecting".into(),
            None => "clicking".into(),
        },
        "click" => "clicking".into(),
        "focus" => "focusing".into(),
        "key" => format!("pressing {}", arg_s(a, "combo", "a key")),
        "type_text" | "set_text" => "typing".into(),
        "mouse" => "using the mouse".into(),
        "drag" => "dragging".into(),
        "scroll" => "scrolling".into(),
        "activate_window" => "switching windows".into(),
        "move_window" => "arranging windows".into(),
        "close_window" => "closing a window".into(),
        "menu" => "using a menu".into(),
        "clipboard_set" => "copying".into(),
        "launch" => format!("launching {}", arg_s(a, "app", "an app")),
        "file_dialog" => "choosing a file".into(),
        _ => return None,
    })
}

/// Rounds of fetching what a menu walk misses before giving up on a part
/// of the tree that does not answer.
const MENU_FETCH_ROUNDS: usize = 50;
/// How long `menu` waits for a submenu it opened to list its entries.
const MENU_OPEN_WAIT: Duration = Duration::from_millis(2000);
/// Between looks for a menu that is no window of its own (`open_menus`).
const MENU_POLL: Duration = Duration::from_millis(30);
/// After this long, a menu that showed before a submenu was opened may be
/// that submenu (opened again) rather than one about to close.
const MENU_OLD_AFTER: Duration = Duration::from_millis(400);

/// The action that runs a menu entry: press, click and the like, else its
/// only (or first) action; None without any.
fn menu_action(actions: &[String]) -> Option<usize> {
    const RUN: [&str; 7] = [
        "press", "click", "activate", "trigger", "toggle", "select", "check",
    ];
    actions
        .iter()
        .position(|a| RUN.contains(&a.to_lowercase().as_str()))
        .or((!actions.is_empty()).then_some(0))
}

/// How long close_window waits for the window to go before saying it is
/// still open (an app asking to save changes keeps it).
const CLOSE_SETTLE: Duration = Duration::from_millis(1500);

/// `desktops: 1 "Main" (current), 2 "Web"`; Hyprland: `workspaces: …`
/// with the monitor each is on.
fn desktops_line(list: &[backend::Desktop]) -> String {
    let hypr = list.iter().any(|d| !d.output.is_empty());
    let items: Vec<String> = list
        .iter()
        .map(|d| {
            // Hyprland's named workspaces have no number (0).
            let mut s = if d.number == 0 {
                render::py_repr(&d.name)
            } else {
                d.number.to_string()
            };
            if d.number != 0 && d.name != s {
                s.push_str(&format!(" {}", render::py_repr(&d.name)));
            }
            match (d.current, d.output.is_empty()) {
                (true, true) => s.push_str(" (current)"),
                (true, false) => s.push_str(&format!(" (shown on {})", d.output)),
                (false, false) => s.push_str(&format!(" (on {})", d.output)),
                (false, true) => {}
            }
            s
        })
        .collect();
    format!(
        "{}: {}",
        if hypr { "workspaces" } else { "desktops" },
        items.join(", ")
    )
}

/// move_window's `desktop`: a number or a name; None when absent.
fn desktop_arg(a: &Json) -> R<Option<String>> {
    match a.get("desktop") {
        None | Some(Json::Null) => Ok(None),
        Some(Json::Number(n)) => Ok(Some(n.to_string())),
        Some(Json::String(s)) if !s.trim().is_empty() => Ok(Some(s.trim().to_string())),
        Some(_) => Err("desktop must be a desktop number or name".into()),
    }
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

/// Whether two reads of windows can be the same window by their handles
/// (`id`, 0 where the backend has none): two windows of a process can
/// share a title.
fn same_id(a: &WindowGeo, b: &WindowGeo) -> bool {
    a.id == 0 || b.id == 0 || a.id == b.id
}

/// `"title" [app] at (x,y) WxH {active,maximized} on OUTPUT, desktop 2`.
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
    match w.desktops.as_slice() {
        [] => {}
        [all] if all == "all" => line.push_str(", all desktops"),
        [one] => line.push_str(&format!(", desktop {one}")),
        many => line.push_str(&format!(", desktops {}", many.join(","))),
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

/// What separates `~` from the rest of a path: also `\\` on Windows.
const HOME_SEPS: &[char] = if cfg!(windows) { &['/', '\\'] } else { &['/'] };

/// The home folder `~` stands for: HOME, else (Windows, where it is
/// usually unset) USERPROFILE.
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| {
            cfg!(windows)
                .then(|| std::env::var_os("USERPROFILE"))
                .flatten()
        })
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

/// `raw` with a leading `~` expanded to `home`; Err unless the result is an
/// absolute path on one line.
fn expand_path(raw: &str, home: Option<&std::path::Path>) -> R<PathBuf> {
    expand_path_with(raw, home, HOME_SEPS)
}

/// `expand_path` with `seps` as the separators after `~`.
fn expand_path_with(raw: &str, home: Option<&std::path::Path>, seps: &[char]) -> R<PathBuf> {
    let raw = raw.trim();
    if raw.is_empty() || raw.contains(['\n', '\r']) {
        return Err("path must be one non-empty line".into());
    }
    let path = match raw.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with(seps) => home
            .ok_or("cannot expand ~: the home folder is not set")?
            .join(rest.trim_start_matches(seps)),
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
    /// The app it belongs to (lower case) and its role.
    app: String,
    role: String,
}

impl<Id> Target<Id> {
    /// A password field: its text is never read, and written only where
    /// the allowlist (or DESKTOP_MCP_ALLOW_PASSWORDS) allows it.
    fn protected(&self) -> bool {
        render::is_protected_role(&self.role)
    }
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

/// The last line of a cut tree: how much is left and the call that lists
/// it, `after` the last element printed. Ids are stable, so the next page
/// starts right after that element while it is still there.
fn continuation(a: &Json, app: &str, last: Option<u64>, left: usize, truncated: bool) -> String {
    let Some(last) = last else {
        return render::TRUNCATED.to_string();
    };
    let left = match (left, truncated) {
        (0, _) => "more nodes".to_string(),
        (n, true) => format!("{n}+ more nodes"),
        (n, false) => format!("{n} more nodes"),
    };
    let mut call = Vec::new();
    match arg_i(a, "root_id", 0) {
        0 => {
            call.push(format!("app={}", render::json_str(app)));
            if arg_b(a, "document", false) {
                call.push("document=true".into());
            }
        }
        root => call.push(format!("root_id={root}")),
    }
    for flag in ["interactive", "include_hidden"] {
        if arg_b(a, flag, false) {
            call.push(format!("{flag}=true"));
        }
    }
    for (k, d) in [("max_depth", 12), ("max_nodes", 300)] {
        let v = arg_i(a, k, d);
        if v != d {
            call.push(format!("{k}={v}"));
        }
    }
    if a.get("max_chars").is_some() {
        call.push(format!(
            "max_chars={}",
            arg_i(a, "max_chars", TREE_MAX_CHARS)
        ));
    }
    call.push(format!("after={last}"));
    format!("… {left}; continue with tree({})", call.join(", "))
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

/// For a verified action on an element not of the kind it needs: Ok(None)
/// when the element still has an action named `action` (it then runs as a
/// plain action), else the error, with nothing sent.
fn not_of_kind<Id>(info: &Info<Id>, action: &str, kind: &str) -> R<Option<String>> {
    let want = semantic::norm(action);
    if info.actions.iter().any(|a| semantic::norm(a) == want) {
        return Ok(None);
    }
    Err(format!(
        "it is not {kind} ({}); nothing sent{}",
        info.role,
        actions_hint(info)
    ))
}

/// The error for a verified action that found no action to run.
fn no_action<Id>(info: &Info<Id>, what: &str) -> String {
    format!(
        "it has no action to {what} it; nothing sent{}",
        actions_hint(info)
    )
}

/// ": its actions are [...]; act with one, or click" (or ": use click").
fn actions_hint<Id>(info: &Info<Id>) -> String {
    let list: Vec<String> = render::shown_actions(info)
        .into_iter()
        .map(render::py_repr)
        .collect();
    if list.is_empty() {
        ": use click".into()
    } else {
        format!(
            ": its actions are [{}]; act with one, or click",
            list.join(", ")
        )
    }
}

impl<B: Backend> Server<B> {
    fn new(b: B) -> Self {
        let allow_file = b.allow_file();
        let b_takeover = b.takeover();
        Server {
            b: delivery::Sends(b),
            refs: Mutex::new(Refs {
                map: HashMap::new(),
                index: HashMap::new(),
                next: 1,
            }),
            allow_file,
            last_active: Mutex::new(None),
            seen_active: Mutex::new(None),
            seen_surfaces: Mutex::new(Vec::new()),
            shown_pending: Mutex::new(None),
            last_shot: Mutex::new(None),
            input: Mutex::new(()),
            inert_writes: Mutex::new(HashSet::new()),
            held: std::sync::Mutex::new(Vec::new()),
            takeover: b_takeover,
            run_start: std::sync::Mutex::new(None),
            run_acted: std::sync::atomic::AtomicBool::new(false),
            content: std::sync::Mutex::new(None),
            content_base: std::sync::Mutex::new(None),
            link: Default::default(),
            reads: std::sync::Mutex::new(Reads::new()),
            page_docs: std::sync::Mutex::new(HashMap::new()),
        }
    }

    // ------------------------------------------------------------ takeover

    /// For a call whose reply a batch tests (`call_inner`'s `settle`), once
    /// per call: wait up to SETTLE_MS for the app whose elements are `near`
    /// to report a change since the input, and its burst of events to end.
    /// True when it waited.
    async fn settle(&self, near: &[B::Id]) -> bool {
        let Some(mark) = GATE
            .try_with(|g| g.borrow_mut().settle.take())
            .ok()
            .flatten()
        else {
            return false;
        };
        self.b
            .wait_change(near, mark, Duration::from_millis(SETTLE_MS as u64))
            .await;
        true
    }

    /// Note that a run of input starts now (the input lock was just taken).
    fn start_run(&self) {
        *self.run_start.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
        self.run_acted
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }

    /// `gate` at the start of a gated call. While the user's input is
    /// recent but the pause would end within takeover::WAIT_LIMIT all told,
    /// it waits (holding the input lock, sending nothing) and lets the call
    /// go on; the time waited, if any. The stop shortcut and an
    /// interruption of a run that already sent input (a batch step after
    /// the user started using the seat) are refused at once: the run's
    /// later steps were planned for what the screen showed before. Until
    /// a run's first gated input went ahead, the user's input since its
    /// start (during a batch's reads) only pauses it, and the run starts
    /// over once the wait is done. Cancelling the call ends the wait.
    async fn gate_wait(&self, in_run: bool) -> R<Option<Duration>> {
        let Some(t) = &self.takeover else {
            return Ok(None);
        };
        let began = Instant::now();
        let mut shown = false;
        loop {
            let acted = self.run_acted.load(std::sync::atomic::Ordering::SeqCst);
            let start = match in_run && acted {
                true => *self.run_start.lock().unwrap_or_else(|e| e.into_inner()),
                false => None,
            };
            let now = Instant::now();
            match t.plan(start, now.saturating_duration_since(began), now) {
                Ok(None) => {
                    if !acted {
                        // The run's input starts now: user input before
                        // (waited out) does not interrupt it.
                        *self.run_start.lock().unwrap_or_else(|e| e.into_inner()) = Some(now);
                        self.run_acted
                            .store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                    return Ok(shown.then(|| began.elapsed()));
                }
                Ok(Some(next)) => {
                    if !shown {
                        // Shown while it waits, so the user sees why.
                        if let Err(block) = t.check_since(start, now) {
                            self.b.indicate(&block.label()).await;
                        }
                        shown = true;
                    }
                    tokio::time::sleep(next).await;
                }
                Err(block) => {
                    self.b.indicate(&block.label()).await;
                    return Err(refusal(block.message(t.window())));
                }
            }
        }
    }

    /// Whether a gated tool may act now: not after the stop shortcut, not
    /// while the user uses the seat, and in a run of input (a batch, a
    /// drag), not once the user started using it after the run began. A
    /// refusal shows on the indicator.
    /// `in_run`: called inside a run that holds the input lock (a batch
    /// step, a drag's moves), whose start counts.
    async fn gate(&self, in_run: bool) -> R<()> {
        let Some(t) = &self.takeover else {
            return Ok(());
        };
        let start = match in_run {
            true => *self.run_start.lock().unwrap_or_else(|e| e.into_inner()),
            false => None,
        };
        match t.check_since(start, Instant::now()) {
            Ok(()) => Ok(()),
            Err(block) => {
                self.b.indicate(&block.label()).await;
                Err(refusal(block.message(t.window())))
            }
        }
    }

    // ------------------------------------------------------------ content wait

    /// Just before an input that may change a web page in place: the
    /// change mark, and whether the app of `id` was already changing since
    /// the call started (`content_base`).
    fn pre_content(&self, id: Option<&B::Id>) -> PreContent {
        let mark = self.b.change_mark();
        let base = *self.content_base.lock().unwrap_or_else(|e| e.into_inner());
        let busy = match (id, base) {
            (Some(id), Some(base)) if base < mark => {
                self.b.changed_since(std::slice::from_ref(id), base) == Some(true)
            }
            _ => false,
        };
        PreContent { mark, busy }
    }

    /// After an input to `id` (of `role`) that likely changes a web page in
    /// place (a press, a plain left click, or with `key` Enter to the
    /// focused element): when its app shows web pages and change events
    /// are heard, note it for `call_inner`'s content wait. Not after a
    /// click or press on an editable element (for typing), nor after Enter
    /// in a multi-line one or a terminal (it types a line). Cheap: the
    /// toolkit is read once per app, the states once.
    async fn note_content(&self, pre: PreContent, id: &B::Id, role: &str, key: bool) {
        let at = Instant::now();
        let skip = match key {
            true => NO_CONTENT_KEY_ROLES.contains(&role),
            false => NO_CONTENT_ROLES.contains(&role),
        };
        if !self.b.changes_live() || skip {
            return;
        }
        let toolkit = self.b.toolkit_of(id).await;
        if !WEB_TOOLKITS.contains(&toolkit.as_str()) {
            return;
        }
        let typing = match self.b.edit_states(id).await {
            Some((_, true)) => true,
            Some((editable, false)) => editable && !key,
            None => false,
        };
        if !typing {
            *self.content.lock().unwrap_or_else(|e| e.into_inner()) = Some(ContentNote {
                pre,
                id: id.clone(),
                at,
            });
        }
    }

    /// Wait up to `limit` after the input `c` (`note_content`) for the app
    /// of its element to report a change; once it did, until its events
    /// were quiet for CONTENT_QUIET (CONTENT_QUIET_EARLY when the change
    /// came at once, or the app was already changing) and no page of it is
    /// loading. While none loads, at most CONTENT_BURST past the first
    /// change: other tabs and live pages keep sending events. Reads made
    /// after this see the new content. The reply's note.
    async fn content_wait(&self, c: &ContentNote<B::Id>, limit: Duration) -> String {
        let near = std::slice::from_ref(&c.id);
        let left = || limit.saturating_sub(c.at.elapsed());
        let ms = || c.at.elapsed().as_millis();
        if !c.pre.busy && !self.b.wait_change(near, c.pre.mark, left()).await {
            return format!("; no page change seen in {} ms", ms());
        }
        let quiet = match c.pre.busy || c.at.elapsed() < CONTENT_EARLY {
            true => CONTENT_QUIET_EARLY,
            false => CONTENT_QUIET,
        };
        let mut cap = Instant::now() + CONTENT_BURST;
        let mut windows: Option<Vec<B::Id>> = None;
        let mut loading = false;
        loop {
            let wait = quiet.min(left());
            if wait.is_zero() {
                break;
            }
            let m = self.b.change_mark();
            let changed = self.b.wait_change(near, m, wait).await;
            if changed && Instant::now() < cap {
                continue;
            }
            // Quiet (or changing past the cap): unless a page waits for
            // the network.
            if windows.is_none() {
                windows = Some(match self.b.app_name(&c.id).await {
                    Some(app) => self
                        .b
                        .app_windows(&app.to_lowercase())
                        .await
                        .unwrap_or_default(),
                    None => Vec::new(),
                });
            }
            loading = self.b.loading(windows.as_deref().unwrap_or_default()).await;
            if loading {
                // Once it loaded, its page renders: another burst's time.
                tokio::time::sleep(IDLE_POLL.min(left())).await;
                cap = Instant::now() + CONTENT_BURST;
                continue;
            }
            // A quiet cut short by the limit is no quiet.
            if changed || wait < quiet {
                break;
            }
            return match c.pre.busy {
                true => format!("; app was already changing; quiet after {} ms", ms()),
                false => format!("; page changed (settled after {} ms)", ms()),
            };
        }
        let still = if loading { "loading" } else { "changing" };
        match c.pre.busy {
            true => format!(
                "; app was already changing; still {still} after {} ms",
                ms()
            ),
            false => format!("; page changed, still {still} after {} ms", ms()),
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

    /// The allowlist file, parsed (see `policy`); created with the
    /// backend's default list when missing.
    fn policy(&self) -> Policy {
        let mut p = self.policy_file();
        p.add_aliases(&self.b.app_aliases());
        p
    }

    /// The allowlist file as written, without the apps allowed through an
    /// alias.
    fn policy_file(&self) -> Policy {
        if !self.allow_file.exists() {
            if let Some(dir) = self.allow_file.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(&self.allow_file, self.b.default_allow());
        }
        Policy::parse(&std::fs::read_to_string(&self.allow_file).unwrap_or_default())
    }

    /// Allowed app names (any tier), sorted.
    fn allowed(&self) -> Vec<String> {
        self.policy().names()
    }

    fn is_allowed(&self, app: &str) -> bool {
        self.policy().tier(app).is_some()
    }

    /// Check that the running tool call may use `app`: the app is allowed
    /// with the tier the tool needs (`policy::needed_tier`), and, when the
    /// allowlist's `confirm` list names the tool (for every app or this
    /// one), the user confirmed it (`confirm`). Every tool that reads from or
    /// sends input to an app comes through here, with the app it reaches:
    /// the target element's app, or the active window's
    /// (`require_active_allowed`). A new tool that acts on an app (e.g.
    /// close_window) only needs to call `self.permit(&app).await?` before
    /// acting; its tier comes from `policy::needed_tier` (unknown tools need
    /// full). One confirmation covers the whole call.
    async fn permit(&self, app: &str) -> R<()> {
        let (tool, args, confirmed) = GATE
            .try_with(|g| {
                let g = g.borrow();
                (g.tool.clone(), g.args.clone(), g.confirmed)
            })
            .unwrap_or_else(|_| (String::new(), Json::Null, false));
        let policy = self.policy();
        let app = app.trim().to_lowercase();
        let tier = policy.tier(&app).ok_or_else(|| self.not_allowed(&app))?;
        let need = policy::needed_tier(&tool, &args);
        if tier < need {
            return Err(refusal(format!(
                "{tool} needs the {need} tier, but '{app}' is allowed with the {tier} tier \
                 ('{app}:{tier}' in {}), which allows {}; changing it is the user's decision",
                self.allow_file.display(),
                tier.allows()
            )));
        }
        if !confirmed && policy.needs_confirm(&tool, Some(&app)) {
            self.confirm(&tool, Some(&app), &args).await?;
        }
        Ok(())
    }

    /// Ask the user to confirm `tool` (on `app`): through MCP elicitation
    /// when the client declared it, else the call must carry `confirm: true`
    /// (the agent asked the user itself). Notes the confirmation in `GATE`.
    async fn confirm(&self, tool: &str, app: Option<&str>, a: &Json) -> R<()> {
        let what = describe_call(tool, app, a);
        if self.link.borrow().elicitation {
            let message = format!("The agent wants to run: {what}\nAllow it?");
            let params = json!({
                "message": message,
                "requestedSchema": {
                    "type": "object",
                    "properties": {
                        "confirm": {
                            "type": "boolean",
                            "title": "Allow",
                            "description": "Allow this action once",
                            "default": true,
                        }
                    },
                },
            });
            let answer = self
                .request("elicitation/create", params, CONFIRM_WAIT)
                .await
                .map_err(|e| {
                    refusal(format!(
                        "{what} needs the user's confirmation, but asking failed: {e}"
                    ))
                })?;
            let accepted = answer["action"] == "accept"
                && answer.pointer("/content/confirm") != Some(&json!(false));
            if !accepted {
                let how = match answer["action"].as_str() {
                    Some("accept" | "decline") => "declined",
                    _ => "dismissed",
                };
                return Err(refusal(format!("the user {how} {what}; nothing was done")));
            }
        } else if !arg_b(a, "confirm", false) {
            let why = match self.policy().needs_confirm(tool, app) {
                true => format!("confirm list in {}", self.allow_file.display()),
                false => "it may lose unsaved work".into(),
            };
            return Err(refusal(format!(
                "{what} needs the user's confirmation ({why}), and this client cannot ask \
                 (no elicitation): ask the user, and only if they agree call again with \
                 confirm: true"
            )));
        }
        let _ = GATE.try_with(|g| g.borrow_mut().confirmed = true);
        Ok(())
    }

    /// Send the client a request and wait up to `limit` for its response
    /// (its `result`). Other messages keep flowing meanwhile: `run_loop`
    /// hands the response over through `ClientLink::pending`.
    async fn request(&self, method: &str, params: Json, limit: Duration) -> R<Json> {
        let (key, rx) = {
            let mut link = self.link.borrow_mut();
            let out = link.out.clone().ok_or("the client connection is closed")?;
            link.next += 1;
            let id = json!(format!("desktop-mcp-{}", link.next));
            let (tx, rx) = tokio::sync::oneshot::channel();
            link.pending.insert(id.to_string(), tx);
            out.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
                .map_err(|_| "the client connection is closed")?;
            (id.to_string(), rx)
        };
        // Forget the request however this ends (answer, timeout, the call
        // cancelled while waiting).
        struct Forget<'a>(&'a std::cell::RefCell<ClientLink>, String);
        impl Drop for Forget<'_> {
            fn drop(&mut self) {
                self.0.borrow_mut().pending.remove(&self.1);
            }
        }
        let _forget = Forget(&self.link, key);
        let reply = match tokio::time::timeout(limit, rx).await {
            Err(_) => return Err(format!("no answer within {} s", limit.as_secs())),
            Ok(Err(_)) => return Err("the client connection closed before it answered".into()),
            Ok(Ok(reply)) => reply,
        };
        match reply.get("error") {
            Some(e) => Err(format!(
                "the client answered with an error: {}",
                e["message"].as_str().unwrap_or("unknown")
            )),
            None => Ok(reply["result"].clone()),
        }
    }

    /// A response from the client to one of `request`'s requests.
    fn deliver(&self, msg: Json) {
        let tx = self
            .link
            .borrow_mut()
            .pending
            .remove(&msg["id"].to_string());
        if let Some(tx) = tx {
            let _ = tx.send(msg);
        }
    }

    /// Whether set_text and type_text may fill in a password field of `app`.
    fn passwords_allowed(&self, app: &str) -> bool {
        self.policy()
            .passwords_allowed(app, std::env::var(policy::PASSWORDS_ENV).ok().as_deref())
    }

    fn not_allowed(&self, app: &str) -> String {
        refusal(format!(
            "app '{app}' is not in {}",
            self.allow_file.display()
        ))
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
            return Err(refusal(format!(
                "active window app '{}' is not in {}; pass window=<title or app> to \
                 activate an allowed window first{hint}",
                if name.is_empty() { "unknown" } else { &name },
                self.allow_file.display()
            )));
        }
        self.permit(&name).await?;
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
        let r = self.resolve_ref(rid).await?;
        Ok((r.id, r.app))
    }

    /// `resolve_with_app`, with the role the element (still) has.
    async fn resolve_ref(&self, rid: i64) -> R<Ref<B::Id>> {
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
        Ok(r)
    }

    /// The allowlist name of `w`'s app, found as the window list allows
    /// windows: the compositor's app id when the allowlist names it, else
    /// the accessibility name of its process, else the longest entry its
    /// app id contains (Hyprland's `org.kde.konsole` for `konsole` when the
    /// app is not in the accessibility tree).
    async fn window_app(&self, w: &WindowGeo) -> String {
        let policy = self.policy();
        if policy.tier(&w.app.to_lowercase()).is_some() {
            return w.app.clone();
        }
        let name = self.b.window_app(w).await;
        if policy.tier(&name.to_lowercase()).is_some() {
            return name;
        }
        let class = w.app.to_lowercase();
        policy
            .names()
            .into_iter()
            .filter(|n| class.contains(n.as_str()))
            .max_by_key(String::len)
            .unwrap_or(name)
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

    // ------------------------------------------------------------ kept reads

    fn reads(&self) -> std::sync::MutexGuard<'_, Reads<B::Id>> {
        self.reads.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Taken before a read that may be kept; None when it may not be: the
    /// backend cannot tell that an app did not change, or input runs.
    fn read_start(&self) -> Option<ReadStart> {
        let mark = self.b.read_mark()?;
        let r = self.reads();
        (r.inputs == 0).then(|| ReadStart {
            mark,
            generation: r.generation,
            at: Instant::now(),
        })
    }

    /// Whether a read of `windows` that began at `start` still shows them
    /// as they are: no input of ours since, the app provably sent no change
    /// (`Backend::unchanged_since`), the user did not use the keyboard or
    /// mouse (scrolling shows other elements without an event), and it is
    /// younger than READ_MAX_AGE.
    fn read_current(&self, r: &Reads<B::Id>, windows: &[B::Id], start: &ReadStart) -> bool {
        r.inputs == 0
            && r.generation == start.generation
            && start.at.elapsed() < READ_MAX_AGE
            && self.b.unchanged_since(windows, start.mark)
            && !self
                .takeover
                .as_ref()
                .is_some_and(|t| t.user_input_since(start.at))
    }

    /// The kept read of `app`'s `windows` fetched as `key`, while it is
    /// current (`read_current`), and when it began; else it is dropped.
    fn kept_read(
        &self,
        app: &str,
        key: ReadKey,
        windows: &[B::Id],
    ) -> Option<(Cache<B::Id>, ReadStart)> {
        let mut r = self.reads();
        let i = r.kept.iter().position(|k| k.app == app && k.key == key)?;
        let k = &r.kept[i];
        if k.windows == windows && self.read_current(&r, windows, &k.start) {
            return Some((k.cache.clone(), k.start));
        }
        if trace::on() {
            let why = if k.windows != windows {
                "windows differ"
            } else if r.inputs > 0 || r.generation != k.start.generation {
                "input since"
            } else if k.start.at.elapsed() >= READ_MAX_AGE {
                "too old"
            } else if !self.b.unchanged_since(windows, k.start.mark) {
                "app changed"
            } else {
                "user input"
            };
            eprintln!("trace          kept read dropped: {why}");
        }
        r.kept.remove(i);
        None
    }

    /// Keep a read of `app`'s `windows` that began at `start`, unless
    /// something changed meanwhile.
    fn keep_read(
        &self,
        app: &str,
        key: ReadKey,
        windows: &[B::Id],
        cache: &Cache<B::Id>,
        start: ReadStart,
    ) {
        let mut r = self.reads();
        r.kept.retain(|k| !(k.app == app && k.key == key));
        if !self.read_current(&r, windows, &start) {
            return;
        }
        if r.kept.len() >= READS_KEPT {
            r.kept.remove(0);
        }
        r.kept.push(KeptRead {
            app: app.to_string(),
            key,
            windows: windows.to_vec(),
            cache: cache.clone(),
            start,
        });
    }

    /// Drop the kept reads of `app`.
    fn forget_reads(&self, app: &str) {
        self.reads().kept.retain(|k| k.app != app);
    }

    /// Fetch below `roots` into `cache` with `first`, then with `more`
    /// until `pick` has all it needs; `pick`'s result and the nodes read.
    /// With `app` (lower case; `roots` are its windows, `cache` empty), a
    /// read of the same windows kept from an earlier call is used instead
    /// while it is current (`read_current`), and this read is kept for the
    /// next calls. The flag tells whether the nodes came from a kept read.
    async fn read_app<T>(
        &self,
        app: Option<&str>,
        roots: &[B::Id],
        mut cache: Cache<B::Id>,
        first: Fetch,
        more: Fetch,
        mut pick: impl FnMut(&Cache<B::Id>) -> Result<T, Vec<B::Id>>,
    ) -> (T, Cache<B::Id>, bool) {
        let keep = match (app.filter(|_| cache.is_empty()), ReadKey::of(&first)) {
            (Some(app), Some(key)) => Some((app, key)),
            _ => None,
        };
        let kept = keep.and_then(|(app, key)| self.kept_read(app, key, roots));
        let cached = kept.is_some();
        let start = match kept {
            Some((nodes, start)) => {
                if trace::on() {
                    eprintln!("trace          kept read of {} nodes", nodes.len());
                }
                cache = nodes;
                Some(start)
            }
            None => {
                let start = keep.and_then(|_| self.read_start());
                let (_, may_keep) = trace::time("prefetch", async {
                    tokio::join!(
                        self.b.prefetch_with(&mut cache, roots.to_vec(), &first),
                        async { start.is_some() && self.b.may_keep(roots).await }
                    )
                })
                .await;
                if trace::on() {
                    eprintln!("trace          prefetched {} nodes", cache.len());
                }
                start.filter(|_| may_keep)
            }
        };
        let read = cache.len();
        let out = loop {
            match pick(&cache) {
                Ok(out) => break out,
                Err(missing) => {
                    trace::time(
                        "prefetch more",
                        self.b.prefetch_with(&mut cache, missing, &more),
                    )
                    .await;
                }
            }
        };
        if let (Some((app, key)), Some(start)) = (keep, start)
            && (!cached || cache.len() > read)
        {
            self.keep_read(app, key, roots, &cache, start);
        }
        (out, cache, cached)
    }

    /// Fetch the visible part of `app` and find the elements `query` names
    /// (render::locate_all; `window` the tool's window argument or "");
    /// window and tab titles only with `titles` (see render::TITLE_ROLES).
    /// `kept`: a kept read of `app` may serve. The cache holds no action
    /// names: `fill_actions` before describing.
    async fn locate(
        &self,
        app: &str,
        query: &str,
        role: &str,
        titles: bool,
        kept: bool,
        window: &str,
    ) -> R<Located<B::Id>> {
        let windows = trace::time("app_windows", self.app_windows(app)).await?;
        let app = app.to_lowercase();
        let (found, cache, cached) = self
            .read_app(
                kept.then_some(app.as_str()),
                &windows,
                Cache::new(),
                search_fetch(render::FIND_TEXT_LIMIT, render::FIND_MAX_SEEN),
                search_fetch(render::FIND_TEXT_LIMIT, PREFETCH_MORE),
                |c| locate_with_synonyms(c, &windows, query, role, titles, window),
            )
            .await;
        Ok(match found {
            // Matched through the synonyms of `role` while an element of
            // `role` itself is only scrolled out of view: that one is
            // meant, and the miss offers it.
            Some(m)
                if m.by_synonym && self.offscreen_of_role(&windows, &cache, query, role).await =>
            {
                Located::Missed(windows, cache)
            }
            Some(m) => Located::Found(m, cache, cached),
            None => Located::Missed(windows, cache),
        })
    }

    /// Whether an element of `role` exactly (no synonyms) that `query`
    /// names is scrolled out of view in a web page of `windows` (whose
    /// visible part `cache` holds).
    async fn offscreen_of_role(
        &self,
        windows: &[B::Id],
        cache: &Cache<B::Id>,
        query: &str,
        role: &str,
    ) -> bool {
        let docs = render::showing_documents(cache, windows);
        if docs.is_empty() {
            return false;
        }
        let mut off = Cache::new();
        let fetch = Fetch {
            offscreen: true,
            ..search_fetch(render::FIND_TEXT_LIMIT, OFFSCREEN_FETCH)
        };
        self.b.prefetch_with(&mut off, docs.clone(), &fetch).await;
        !render::find_offscreen(&off, &docs, query, role, 1).is_empty()
    }

    /// `locate` in the app windows `windows`, read afresh: the first
    /// element picked (a read takes the first of a tie), and whether it
    /// matched the synonyms of `role` rather than `role` itself.
    async fn locate_in(
        &self,
        windows: Vec<B::Id>,
        query: &str,
        role: &str,
        titles: bool,
    ) -> Option<(B::Id, Cache<B::Id>, bool)> {
        let (found, cache, _) = self
            .read_app(
                None,
                &windows,
                Cache::new(),
                search_fetch(render::FIND_TEXT_LIMIT, render::FIND_MAX_SEEN),
                search_fetch(render::FIND_TEXT_LIMIT, PREFETCH_MORE),
                |c| locate_with_synonyms(c, &windows, query, role, titles, ""),
            )
            .await;
        found.map(|m| (m.first().clone(), cache, m.by_synonym))
    }

    /// What to offer when nothing visible in `windows` matches `query`
    /// (and `role`, nor its synonyms), first found first: matches of `role`
    /// (or its synonyms) scrolled out of view in the page (web documents
    /// only); with `role` given, the visible elements of other roles that
    /// match, then off-screen ones of other roles (with their roles); else
    /// the visible elements with the closest names, with their ids. Empty
    /// when there are none. `cache` holds the visible part of `windows`.
    async fn miss_report(
        &self,
        windows: &[B::Id],
        cache: &mut Cache<B::Id>,
        app: &str,
        query: &str,
        role: &str,
    ) -> String {
        let as_lines = |ids: Vec<B::Id>| -> Vec<Line<B::Id>> {
            ids.into_iter()
                .map(|id| Line::Node { depth: 0, id })
                .collect()
        };
        let roles_of = |c: &Cache<B::Id>, ids: &[B::Id]| -> String {
            let mut roles: Vec<String> = Vec::new();
            for id in ids {
                let r = render::py_repr(&c[id].role);
                if !roles.contains(&r) {
                    roles.push(r);
                }
            }
            roles.join(", ")
        };
        let with_role = !role.trim().is_empty() && !query.trim().is_empty();
        let docs = render::showing_documents(cache, windows);
        let mut off = Cache::new();
        if !docs.is_empty() {
            let fetch = Fetch {
                offscreen: true,
                ..search_fetch(render::FIND_TEXT_LIMIT, OFFSCREEN_FETCH)
            };
            trace::time(
                "prefetch offscreen",
                self.b.prefetch_with(&mut off, docs.clone(), &fetch),
            )
            .await;
            // The role asked for, else its synonyms: the element wanted
            // may just be scrolled out of view.
            let (hits, by_synonym) = render::with_role_synonyms(
                role,
                |h: &Vec<B::Id>| !h.is_empty(),
                |r| {
                    Ok::<_, Vec<B::Id>>(render::find_offscreen(
                        &off,
                        &docs,
                        query,
                        r,
                        MISS_SUGGESTIONS,
                    ))
                },
            )
            .unwrap_or_default();
            if !hits.is_empty() {
                let roles: Vec<&str> = hits.iter().map(|id| off[id].role.as_str()).collect();
                let note = render::synonym_note(role, by_synonym, &roles);
                let note = if note.is_empty() {
                    note
                } else {
                    format!(" ({note})")
                };
                self.b.fill_actions(&mut off, &hits).await;
                let (text, _) = self.print(as_lines(hits), &off, app).await;
                return format!(
                    "\noff-screen, scrolled out of view{note} (act and get_text work on \
                     them; act action=scroll_to before click):\n{text}"
                );
            }
        }
        if with_role {
            // Visible elements the query names in other roles: the role
            // asked for (and its synonyms) was the miss.
            if let Ok(other) = render::find(cache, windows, query, "", MISS_SUGGESTIONS)
                && !other.is_empty()
            {
                let roles = roles_of(cache, &other);
                self.b.fill_actions(cache, &other).await;
                let (text, _) = self.print(as_lines(other), cache, app).await;
                return format!(
                    "; it matches elements of role {roles} (pass that role, or none):\n{text}"
                );
            }
            let hits = render::find_offscreen(&off, &docs, query, "", MISS_SUGGESTIONS);
            if !hits.is_empty() {
                let roles = roles_of(&off, &hits);
                self.b.fill_actions(&mut off, &hits).await;
                let (text, _) = self.print(as_lines(hits), &off, app).await;
                return format!(
                    "\noff-screen, scrolled out of view, of role {roles} (pass that role, or \
                     none; act action=scroll_to before click):\n{text}"
                );
            }
        }
        let near = render::closest(cache, windows, query, MISS_SUGGESTIONS);
        if near.is_empty() {
            return String::new();
        }
        self.b.fill_actions(cache, &near).await;
        let (text, _) = self.print(as_lines(near), cache, app).await;
        format!("; closest:\n{text}")
    }

    /// For an input tool's query that names several elements (`m`, from
    /// one fresh read in `cache`): when some have an on-screen area and
    /// others none (a collapsed menu's entries), the ones without are left
    /// out (up to AREA_CHECK_MAX candidates, in front or not), and one left
    /// is the target. Else the refusal, listing the candidates with ids,
    /// actions and (when their window is in front) where they are: nothing
    /// is sent.
    async fn untie(
        &self,
        m: &mut render::Matches<B::Id>,
        cache: &mut Cache<B::Id>,
        app: &str,
        query: &str,
        role: &str,
    ) -> R<()> {
        if m.picked.len() <= AREA_CHECK_MAX {
            let ids: Vec<B::Id> = m.picked.iter().map(|(id, _)| id.clone()).collect();
            let area = trace::time("has_area", self.b.has_area(&ids)).await;
            let no_area = |a: &Option<bool>| *a == Some(false);
            if area.iter().any(|a| !no_area(a)) {
                let mut keep = area.iter().map(|a| !no_area(a));
                m.picked.retain(|_| keep.next().unwrap_or(true));
            }
        }
        if m.picked.len() == 1 {
            return Ok(());
        }
        // Only the lines shown: their actions (the read has none) and
        // positions, together.
        let shown: Vec<(B::Id, B::Id)> = m.picked.iter().take(AMBIGUOUS_SHOWN).cloned().collect();
        let shown_ids: Vec<B::Id> = shown.iter().map(|(id, _)| id.clone()).collect();
        let ((), centers) = tokio::join!(
            trace::time("fill_actions", self.b.fill_actions(cache, &shown_ids)),
            trace::time("element_centers", self.b.element_centers_in(&shown))
        );
        let app_key = app.to_lowercase();
        let mut windows: Vec<&B::Id> = m.picked.iter().map(|(_, w)| w).collect();
        windows.dedup();
        let several = windows.len() > 1;
        let mut lines = Vec::new();
        for ((id, w), c) in shown.iter().zip(&centers) {
            let rid = self.add_ref(id, &cache[id], &app_key).await;
            let mut line = render::describe(&cache[id], rid);
            if let Ok((x, y)) = c {
                line.push_str(&format!(" at {x},{y}"));
            }
            if several && let Some(info) = cache.get(w) {
                line.push_str(&format!(" in {}", render::py_repr(&info.name)));
                if render::is_dialog(info) {
                    line.push_str(" (dialog)");
                }
            }
            lines.push(line);
        }
        let n = m.picked.len();
        if n > AMBIGUOUS_SHOWN {
            lines.push(format!("… {} more", n - AMBIGUOUS_SHOWN));
        }
        let with_role = if role.is_empty() {
            String::new()
        } else {
            format!(" with role '{role}'")
        };
        let first_role = &cache[&m.picked[0].0].role;
        let roles_differ = m.picked.iter().any(|(id, _)| &cache[id].role != first_role);
        let narrow = match (several, roles_differ) {
            (true, _) => "pass one id, or window= its title",
            (false, true) => "pass one id, or role",
            (false, false) => "pass one id",
        };
        // A refusal: nothing was sent, and the candidates say what to pass
        // (no on_fail, no "now:" line).
        Err(refusal(format!(
            "ambiguous: {n} elements in '{app}' match '{query}'{with_role}; nothing sent. \
             {narrow}:\n{}",
            lines.join("\n")
        )))
    }

    /// The element an input tool names: `id`, or `app` + `query` (+
    /// `role`). The tool's tier (and confirmation, `permit`) is checked
    /// against the element's app before anything is looked up there. A
    /// query that names several elements equally well, after the
    /// tie-breaks (render::locate_all), is refused with the candidates.
    async fn target(&self, a: &Json) -> R<Target<B::Id>> {
        Ok(self.target_and_center(a, false, true).await?.0)
    }

    /// `target` for a read tool: of several elements a query names equally
    /// well it takes the first, and says so.
    async fn read_target(&self, a: &Json) -> R<Target<B::Id>> {
        Ok(self.target_and_center(a, false, false).await?.0)
    }

    /// `target` (`input`) or `read_target`, and with `center` the element's
    /// centre when it was cheap to measure alongside the last lookups
    /// (found by query); None then to measure it after.
    async fn target_and_center(
        &self,
        a: &Json,
        center: bool,
        input: bool,
    ) -> R<(Target<B::Id>, Option<R<(i32, i32)>>)> {
        if let Some(rid) = a.get("id").and_then(Json::as_i64) {
            let r = trace::time("resolve_ref", self.resolve_ref(rid)).await?;
            trace::time("permit", self.permit(&r.app)).await?;
            note_step_element(rid as u64, true);
            let t = Target {
                id: r.id,
                found: None,
                app: r.app,
                role: r.role,
            };
            return Ok((t, None));
        }
        // `role` alone (no `query`) names the first element of that role,
        // e.g. an editor's text: input tools still refuse a tie.
        let has_role = !arg_s(a, "role", "").trim().is_empty();
        let (Some(app), Some(query)) = (
            a.get("app").and_then(Json::as_str),
            a.get("query")
                .and_then(Json::as_str)
                .or(has_role.then_some("")),
        ) else {
            return Err("pass `id` (from tree/find), or `app` and `query` (or `role`)".into());
        };
        if !self.is_allowed(app) {
            return Err(self.not_allowed(app));
        }
        trace::time("permit", self.permit(app)).await?;
        trace::time("enable", self.b.enable()).await;
        let role = arg_s(a, "role", "");
        let window = a.get("window").and_then(Json::as_str).unwrap_or("");
        let app_key = app.to_lowercase();
        let mut kept = true;
        let (id, cache, at, ties, synonym) = loop {
            let located = self.locate(app, query, &role, true, kept, window);
            let located = trace::time("locate", located).await?;
            let (mut m, mut cache, cached) = match located {
                Located::Found(m, cache, cached) => (m, cache, cached),
                Located::Missed(windows, mut cache) => {
                    let with_role = if role.is_empty() {
                        String::new()
                    } else {
                        format!(" with role '{role}'")
                    };
                    let report = self
                        .miss_report(&windows, &mut cache, &app_key, query, &role)
                        .await;
                    return Err(format!(
                        "no visible element in '{app}' matches '{query}'{with_role}{report}"
                    ));
                }
            };
            if input && m.picked.len() > 1 {
                // Candidates come from one read: a kept one is read afresh.
                if cached {
                    self.forget_reads(&app_key);
                    kept = false;
                    continue;
                }
                self.untie(&mut m, &mut cache, app, query, &role).await?;
            }
            let ties = m.picked.len();
            let roles: Vec<&str> = m
                .picked
                .iter()
                .map(|(id, _)| cache[id].role.as_str())
                .collect();
            let synonym = render::synonym_note(&role, m.by_synonym, &roles);
            let id = m.first().clone();
            // The window it is in, from the nodes read: no walk up to it.
            let frame = if center { frame_in(&cache, &id) } else { None };
            // An element from a kept read is checked to be still there, in
            // the same round trip as its actions (and centre).
            let (there, _, at) = tokio::join!(
                async { !cached || trace::time("still_there", self.b.still_there(&id)).await },
                trace::time(
                    "fill_actions",
                    self.b.fill_actions(&mut cache, std::slice::from_ref(&id))
                ),
                async {
                    if !center {
                        return None;
                    }
                    let c = async {
                        match &frame {
                            Some(f) => self.b.element_center_in(&id, f).await,
                            None => self.b.element_center(&id).await,
                        }
                    };
                    Some(trace::time("element_center", c).await)
                }
            );
            if there {
                break (id, cache, at, ties, synonym);
            }
            // Gone or hidden without an event: read afresh.
            self.forget_reads(&app_key);
            kept = false;
        };
        let rid = trace::time("add_ref", self.add_ref(&id, &cache[&id], &app_key)).await;
        note_step_element(rid, true);
        let mut found = render::describe(&cache[&id], rid);
        if ties > 1 {
            found.push_str(&format!(" (first of {ties} matches)"));
        }
        if !synonym.is_empty() {
            found.push_str(&format!(" ({synonym})"));
        }
        let found = Some(found);
        let role = cache[&id].role.clone();
        let t = Target {
            id,
            found,
            app: app_key,
            role,
        };
        Ok((t, at))
    }

    /// What is on screen, for a reply that found nothing or failed: the
    /// active window and the focused element, so the next call can be
    /// planned without a screenshot. Windows of apps not allowed are not
    /// named.
    async fn where_note(&self) -> String {
        let (print, focus) = tokio::join!(self.fingerprint(), self.focus_note(false));
        let window = match print.window {
            Some((app, title)) if self.is_allowed(&app) => {
                format!("active window {} [{app}]", render::short_title(&title))
            }
            Some((app, _)) => format!("active window of '{app}' (not allowed)"),
            None => "no active window".into(),
        };
        format!("\nnow: {window}{focus}")
    }

    /// "; active window now '…'" when the window in front is not the one
    /// last seen (after an input that was not asked to `observe`): the
    /// cheap part of the effect line, without waiting.
    async fn window_change_note(&self) -> String {
        let Some(now) = self.active_title().await else {
            return String::new();
        };
        let mut last = self.seen_active.lock().await;
        let first = last.is_none();
        if last.as_deref() == Some(now.as_str()) {
            return String::new();
        }
        *last = Some(now.clone());
        // Nothing seen before to compare with: no change to report.
        if first {
            return String::new();
        }
        format!("; active window now {}", render::short_title(&now))
    }

    /// "\nopened: …" for what an input without `observe` opened: when it is
    /// likely to open something (`opens_surface`, a click on an
    /// OPENER_ROLES element) it first waits up to SETTLE_MS for a menu,
    /// list or dialog to show; otherwise only what showed already counts.
    async fn opened_note(&self, tool: &str, a: &Json, mark: u64) -> String {
        if !self.b.names_menus() {
            return String::new();
        }
        let opener =
            opens_surface(tool, a) || GATE.try_with(|g| g.borrow().opener).unwrap_or(false);
        if opener {
            let _ = GATE.try_with(|g| g.borrow_mut().opener = true);
            let wait = Duration::from_millis(SETTLE_MS as u64);
            trace::time("wait_shown", self.b.wait_shown(&[], mark, wait)).await;
        }
        self.surfaces_note(mark, opener).await
    }

    /// "\nopened: …" lines for the menus, popups, combo box lists and
    /// dialogs of allowed apps shown since `mark`, with their entries' ids
    /// (`surfaces`), after this call's "before it, opened: …" lines
    /// (`opened_before`). No round trip when nothing was shown. Without
    /// such events (`shown_since` None), only with `scan`: the active app's
    /// windows are looked at, and what the last scan listed is not again.
    /// What shows after this is left to the next input's reply.
    async fn surfaces_note(&self, mark: u64, scan: bool) -> String {
        if !self.b.names_menus() {
            return String::new();
        }
        let next = self.b.change_mark();
        let quiet = GATE.try_with(|g| g.borrow().quiet).unwrap_or(false);
        let lines = match quiet {
            true => Vec::new(),
            false => self.surfaces_since(mark, scan).await,
        };
        let (before, listed) = GATE
            .try_with(|g| {
                let mut g = g.borrow_mut();
                (std::mem::take(&mut g.before), g.reported.clone())
            })
            .unwrap_or_default();
        *self.shown_pending.lock().await = Some((next, listed));
        let mut out = before;
        for l in lines {
            out.push_str(&format!("\nopened: {l}"));
        }
        out
    }

    /// The surface lines (`surfaces`) for what showed since `mark`, or
    /// with `scan` and no such events, among the active app's windows.
    async fn surfaces_since(&self, mark: u64, scan: bool) -> Vec<String> {
        match self.b.shown_since(mark) {
            Some(shown) if shown.is_empty() => Vec::new(),
            Some(shown) => self.surfaces(cap_candidates(shown), false, None, &[]).await,
            None if scan => {
                let app = self.b.active_app().await;
                if app.is_empty() || !self.is_allowed(&app) {
                    return Vec::new();
                }
                let app = app.to_lowercase();
                let Some(mut windows) = self.b.app_windows(&app).await else {
                    return Vec::new();
                };
                // Menus inside the windows (`open_menus`), looked for after
                // an input likely to open one (a search of the whole app),
                // which may show a little later: up to SETTLE_MS.
                if GATE.try_with(|g| g.borrow().opener).unwrap_or(false) {
                    let start = Instant::now();
                    while let Some(menus) = self.b.open_menus(&windows).await {
                        if !menus.is_empty() {
                            // Opened by this input, though the last look
                            // listed it (the same flyout, opened again).
                            let hashes: Vec<u64> = menus.iter().map(id_hash).collect();
                            self.seen_surfaces
                                .lock()
                                .await
                                .retain(|h| !hashes.contains(h));
                            windows.extend(menus);
                            break;
                        }
                        if start.elapsed() >= Duration::from_millis(SETTLE_MS as u64) {
                            break;
                        }
                        tokio::time::sleep(MENU_POLL).await;
                    }
                }
                self.surfaces(windows, true, Some(&app), &[]).await
            }
            None => Vec::new(),
        }
    }

    /// "\nbefore it, opened: …" lines for the surfaces that showed after
    /// the last input's reply listed what it opened (a dialog a button
    /// opened a few ms after its click), but for those it listed. Taken
    /// before this call's input; said with its own "opened:" lines. No
    /// round trip when nothing was shown.
    async fn opened_before(&self) -> String {
        if !self.b.names_menus() {
            return String::new();
        }
        let Some((mark, listed)) = self.shown_pending.lock().await.clone() else {
            return String::new();
        };
        match self.b.shown_since(mark) {
            Some(shown) if !shown.is_empty() => self
                .surfaces(cap_candidates(shown), false, None, &listed)
                .await
                .iter()
                .map(|l| format!("\nbefore it, opened: {l}"))
                .collect(),
            _ => String::new(),
        }
    }

    /// A line (`render::surface_line`) for each surface among `candidates`
    /// (elements shown lately, or windows): a menu, a combo box's list or
    /// a dialog of an allowed app, still showing, with entries (dialogs
    /// may have none), and neither in `skip` nor listed by this call
    /// already (`id_hash`es): those are not read. `scanned`: the
    /// candidates are windows looked at without change events; one the
    /// last scan listed is left out while it shows. `app`: the (allowed)
    /// app all candidates belong to, when known.
    async fn surfaces(
        &self,
        candidates: Vec<B::Id>,
        scanned: bool,
        app: Option<&str>,
        skip: &[u64],
    ) -> Vec<String> {
        self.surfaces_as(candidates, scanned, app, skip, false)
            .await
    }

    /// `surfaces`; with `dialogs`, every candidate is taken for a dialog
    /// (`dialogs_of`'s, whose roles do not say so).
    async fn surfaces_as(
        &self,
        candidates: Vec<B::Id>,
        scanned: bool,
        app: Option<&str>,
        skip: &[u64],
        dialogs: bool,
    ) -> Vec<String> {
        if candidates.is_empty() {
            return Vec::new();
        }
        let candidates: Vec<(B::Id, String)> = match app {
            Some(app) => candidates.into_iter().map(|c| (c, app.into())).collect(),
            None => self.candidate_apps(candidates).await,
        };
        let candidates: Vec<(B::Id, String)> = candidates
            .into_iter()
            .filter(|(_, app)| !app.is_empty() && self.is_allowed(app))
            .collect();
        if candidates.is_empty() {
            return Vec::new();
        }
        let roles =
            futures_util::future::join_all(candidates.iter().map(|(c, _)| self.b.identity(c)))
                .await;
        let mut picked: Vec<(B::Id, String, render::Surface)> = candidates
            .into_iter()
            .zip(roles)
            .filter_map(|((c, app), (role, _))| {
                let kind = match dialogs {
                    true => render::Surface::Dialog,
                    false => render::surface_kind(&role)?,
                };
                Some((c, app, kind))
            })
            .collect();
        // A list counts only under a combo box.
        let keep = futures_util::future::join_all(picked.iter().map(|(c, _, kind)| async move {
            if *kind != render::Surface::List {
                return true;
            }
            match self.b.parent(c).await {
                Some(p) => self.b.identity(&p).await.0 == "combo box",
                None => false,
            }
        }))
        .await;
        let mut keep = keep.into_iter();
        picked.retain(|_| keep.next().unwrap_or(false));
        // Dialogs first: what they hold is not listed again on its own.
        picked.sort_by_key(|(_, _, kind)| match kind {
            render::Surface::Dialog => 0,
            render::Surface::Menu => 1,
            render::Surface::List => 2,
        });
        let reported = GATE
            .try_with(|g| g.borrow().reported.clone())
            .unwrap_or_default();
        let seen = match scanned {
            true => self.seen_surfaces.lock().await.clone(),
            false => Vec::new(),
        };
        let mut cache = Cache::new();
        let mut found = Vec::new();
        let mut lines = Vec::new();
        for (id, app, kind) in picked {
            if lines.len() >= SURFACE_MAX {
                break;
            }
            // Inside a surface read already.
            if cache.contains_key(&id) {
                continue;
            }
            let hash = id_hash(&id);
            // Listed already: not read again.
            if reported.contains(&hash) || skip.contains(&hash) {
                continue;
            }
            if seen.contains(&hash) {
                // Listed by the last scan: still so while it shows (its
                // own node is enough to tell).
                let fetch = Fetch {
                    limit: 1,
                    ..surface_fetch(kind)
                };
                let mut one = Cache::new();
                self.b
                    .prefetch_with(&mut one, vec![id.clone()], &fetch)
                    .await;
                if one.get(&id).is_some_and(|i| i.showing) {
                    found.push(hash);
                }
                continue;
            }
            let listed = self.surface_entries(&mut cache, &id, kind).await;
            let Some(root) = cache.get(&id) else {
                continue;
            };
            // A dialog with nothing to list says nothing either (Qt keeps a
            // second, empty file dialog window next to the shown one).
            let empty = listed.entries.is_empty()
                && (kind != render::Surface::Dialog || listed.message.is_none());
            if !root.showing || empty {
                continue;
            }
            found.push(hash);
            if kind == render::Surface::Dialog
                && listed.maybe_file_dialog(root)
                && self.b.fills_file_dialog(&id).await
            {
                lines.push(render::file_dialog_line(root));
                continue;
            }
            let mut items = Vec::new();
            for (i, e) in listed.entries.iter().enumerate() {
                // Ids only for the entries the line can show.
                let rid = match i < render::SURFACE_ENTRIES {
                    true => self.add_ref(e, &cache[e], &app).await,
                    false => 0,
                };
                items.push((rid, &cache[e]));
            }
            lines.push(render::surface_line(
                root,
                kind,
                listed.message.as_deref(),
                &items,
            ));
        }
        if scanned {
            *self.seen_surfaces.lock().await = found.clone();
        }
        let _ = GATE.try_with(|g| g.borrow_mut().reported.extend(found));
        lines
    }

    /// Each candidate with the lower-case name of its app ("" when it
    /// could not be read: not allowed), asked once per app. Only for a
    /// backend that cannot tell an element's app (`app_key` None) is it
    /// taken to be the active app's.
    async fn candidate_apps(&self, candidates: Vec<B::Id>) -> Vec<(B::Id, String)> {
        let mut firsts: Vec<(Option<String>, B::Id)> = Vec::new();
        for c in &candidates {
            let key = self.b.app_key(c);
            if !firsts.iter().any(|(k, _)| *k == key) {
                firsts.push((key, c.clone()));
            }
        }
        let names = futures_util::future::join_all(firsts.iter().map(|(k, c)| async move {
            match k {
                Some(_) => self.b.app_name(c).await.unwrap_or_default(),
                None => self.b.active_app().await,
            }
        }))
        .await;
        let apps: Vec<(Option<String>, String)> = firsts
            .into_iter()
            .zip(names)
            .map(|((k, _), n)| (k, n.to_lowercase()))
            .collect();
        candidates
            .into_iter()
            .filter_map(|c| {
                let key = self.b.app_key(&c);
                let app = apps.iter().find(|(k, _)| *k == key)?.1.clone();
                Some((c, app))
            })
            .collect()
    }

    /// The entries of the surface `root` (`render::surface_entries`),
    /// reading the showing nodes below it into `cache`; after
    /// SURFACE_ROUNDS reads, what was found.
    async fn surface_entries(
        &self,
        cache: &mut Cache<B::Id>,
        root: &B::Id,
        kind: render::Surface,
    ) -> render::SurfaceEntries<B::Id> {
        let fetch = surface_fetch(kind);
        self.b
            .prefetch_with(cache, vec![root.clone()], &fetch)
            .await;
        let mut round = 1;
        loop {
            let (listed, missing) = render::surface_entries(cache, root, kind);
            if missing.is_empty() || round >= SURFACE_ROUNDS {
                return listed;
            }
            self.b.prefetch_with(cache, missing, &fetch).await;
            round += 1;
        }
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

    /// The active window and focused element (no app, no page).
    async fn fingerprint(&self) -> Fingerprint<B::Id> {
        let (apps, focus) = tokio::join!(self.b.list_apps(), self.b.focused());
        let window = apps.into_iter().find_map(|a| {
            let name = a.name;
            a.windows
                .into_iter()
                .find(|w| w.active)
                .map(|w| (name.to_lowercase(), w.title))
        });
        Fingerprint {
            window,
            focus: focus.map(|f| (f.id, f.info.role, f.info.name, f.info.states)),
            app: None,
            page: None,
            content: None,
        }
    }

    /// What an input's effect line compares before and after it: the
    /// `fingerprint`, and for `app` (the app the input names, else the
    /// active window's), when allowed, its windows and the page shown.
    /// `content`: the app's content hash when just read (else read here,
    /// for a backend without change events).
    async fn input_fingerprint(
        &self,
        app: Option<String>,
        content: Option<u64>,
    ) -> Fingerprint<B::Id> {
        let named = app.map(|a| a.to_lowercase());
        let allowed = named.clone().filter(|a| self.is_allowed(a));
        // An app named: its windows read alongside.
        let (mut print, named_windows) = tokio::join!(self.fingerprint(), async {
            match &allowed {
                Some(app) => self.b.app_windows(app).await,
                None => None,
            }
        });
        let (app, windows) = match named {
            Some(_) => (allowed, named_windows),
            None => {
                let app = print
                    .window
                    .as_ref()
                    .map(|(a, _)| a.to_lowercase())
                    .filter(|a| self.is_allowed(a));
                let windows = match &app {
                    Some(app) => self.b.app_windows(app).await,
                    None => None,
                };
                (app, windows)
            }
        };
        if let (Some(app), Some(windows)) = (app, windows) {
            print.page = trace::time("page", self.page_of(&app, &windows)).await;
            if self.b.compares_content() {
                print.content = match content {
                    Some(c) => Some(c),
                    None => Some(trace::time("content", self.content_hash(&windows)).await),
                };
            }
            print.app = Some((app, windows));
        }
        print
    }

    /// The web page a browser `app` (Firefox, Chromium and Electron apps)
    /// shows in the window an input goes to (`page_window`), for the effect
    /// line. The document found for that window is read again while it
    /// shows (one round trip), else searched for in that window only; a
    /// window without one is remembered as such until another window is
    /// the one. None for other apps, and when the window is not known.
    async fn page_of(&self, app: &str, windows: &[B::Id]) -> Option<backend::Page> {
        let toolkit = self.b.toolkit_of(windows.first()?).await;
        if !WEB_TOOLKITS.contains(&toolkit.to_lowercase().as_str()) {
            return None;
        }
        let mut cache = Cache::new();
        let window = self.page_window(windows, &mut cache).await?;
        let known = self.page_docs.lock().unwrap().get(app).cloned();
        if let Some((w, doc, at)) = known
            && w == window
        {
            match doc {
                // No document in this window when last looked.
                None if at.elapsed() < NO_PAGE_RECHECK => return None,
                None => {}
                Some(doc) => {
                    if let Some(page) = self.b.page(&doc).await {
                        return Some(page);
                    }
                }
            }
        }
        let found = self
            .active_document(app, vec![window.clone()], &mut cache)
            .await
            .ok();
        let page = match &found {
            Some(doc) => self.b.page(doc).await,
            None => None,
        };
        // A document that shows no page is searched for again next time.
        let doc = match (&found, &page) {
            (Some(_), None) => {
                self.page_docs.lock().unwrap().remove(app);
                return None;
            }
            _ => found,
        };
        self.page_docs
            .lock()
            .unwrap()
            .insert(app.to_string(), (window, doc, Instant::now()));
        page
    }

    /// The window of `app` (`windows`) an input goes to, for `page_of`:
    /// its active one, else its only showing one; None when several show
    /// and none is active (which one the input reaches is not known).
    async fn page_window(&self, windows: &[B::Id], cache: &mut Cache<B::Id>) -> Option<B::Id> {
        // The windows themselves only (limit 1 stops after their level).
        let level = search_fetch(0, 1);
        self.b.prefetch_with(cache, windows.to_vec(), &level).await;
        let showing: Vec<&B::Id> = windows
            .iter()
            .filter(|w| cache.get(*w).is_some_and(|i| i.showing))
            .collect();
        if let Some(w) = showing
            .iter()
            .find(|w| cache[**w].states.contains(&"active"))
        {
            return Some((*w).clone());
        }
        match showing.as_slice() {
            [w] => Some((*w).clone()),
            _ => None,
        }
    }

    /// The app an input tool's arguments name: `app`, else the app of the
    /// element `id` refers to; None for neither.
    async fn input_app(&self, a: &Json) -> Option<String> {
        if let Some(app) = a.get("app").and_then(Json::as_str)
            && !app.trim().is_empty()
        {
            return Some(app.trim().to_lowercase());
        }
        let rid = a.get("id").and_then(Json::as_i64)?;
        let refs = self.refs.lock().await;
        refs.map.get(&(rid as u64)).map(|r| r.app.clone())
    }

    /// After an input tool ran: wait up to `settle_ms` for the app to react
    /// (ending early once it reported a change since `mark`), then the
    /// effect line and what `observe` asks for. `sent`: the tool sent
    /// input (a verified act may find the state there already).
    /// `content`: the element whose page the content wait waited for
    /// (`content_wait`): no settle on top, and its app's changes count.
    async fn after_input(
        &self,
        tool: &str,
        a: &Json,
        observe: Observe,
        (mark, before): (u64, Fingerprint<B::Id>),
        sent: bool,
        content: Option<B::Id>,
    ) -> Out {
        let settle = match content {
            Some(_) => 0,
            None => arg_i(a, "settle_ms", SETTLE_MS).clamp(0, MAX_SETTLE_MS) as u64,
        };
        // The focused element's app reacts (its bus, on Linux), the app the
        // input went to (`act` reaches a browser behind the terminal), and
        // the element whose page the content wait watched.
        let mut near: Vec<B::Id> = before.focus.iter().map(|f| f.0.clone()).collect();
        if let Some((_, windows)) = &before.app {
            near.extend(windows.iter().cloned());
        }
        near.extend(content);
        // Without change events, the app's content is read again until it
        // differs: a display that changed shows although focus and title
        // stayed. A page answers late: a little longer.
        let mut content_now = None;
        let events = if let (Some(was), Some((_, windows)), None) = (
            before.content,
            &before.app,
            self.b.changed_since(&near, mark),
        ) {
            let mut limit = Duration::from_millis(settle);
            if settle > 0 && sent && before.page.is_some() {
                limit += PAGE_SETTLE;
            }
            let now = trace::time("content settle", self.content_settle(windows, was, limit)).await;
            content_now = Some(now);
            None
        } else {
            if settle > 0 {
                self.b
                    .wait_change(&near, mark, Duration::from_millis(settle))
                    .await;
            }
            let mut events = self.b.changed_since(&near, mark);
            // A page answers late: wait a little longer for it to start
            // (not when nothing was sent).
            if settle > 0 && sent && before.page.is_some() && events != Some(true) {
                trace::time("page settle", self.b.wait_change(&near, mark, PAGE_SETTLE)).await;
                events = self.b.changed_since(&near, mark);
            }
            events
        };
        let app = before.app.as_ref().map(|(app, _)| app.clone());
        let after = self.input_fingerprint(app, content_now).await;
        let allowed = self.allowed();
        let is_allowed = |app: &str| allowed.contains(&app.to_lowercase());
        let mut text = format!(
            "; effect: {}",
            effect_text(&before, &after, events, is_allowed)
        );
        text.push_str(&trace::time("surfaces", self.surfaces_note(mark, true)).await);
        if before.window != after.window
            && let Some((app, title)) = &after.window
            && is_allowed(app)
        {
            // Named in the next "not in the allowlist" hint.
            *self.last_active.lock().await = Some(title.clone());
        }
        let mut images = Vec::new();
        match observe {
            Observe::Nothing => {}
            // key and type_text name the focus in their reply already.
            Observe::Focus if matches!(tool, "key" | "type_text") => {}
            Observe::Focus => {
                let note = self.focus_note(false).await;
                match note.strip_prefix("; ") {
                    Some(n) => text.push_str(&format!("\n{n}")),
                    None => text.push_str("\nfocus: none reported in an allowed app"),
                }
            }
            Observe::Tree => {
                let app = a.get("app").and_then(Json::as_str).map(str::to_string);
                match self.observe_tree(app).await {
                    Ok(t) => text.push_str(&format!("\n{t}")),
                    Err(e) => text.push_str(&format!("\nobserve tree failed: {e}")),
                }
            }
            Observe::Screenshot => match self.screenshot(&json!({})).await {
                Ok(o) => {
                    text.push_str(&format!("\n{}", o.text));
                    images = o.images;
                }
                Err(e) => text.push_str(&format!("\nobserve screenshot failed: {e}")),
            },
        }
        Out { text, images }
    }

    /// A compact tree of the active window of `app` (else of the app in
    /// front), cut at OBSERVE_TREE_CHARS.
    async fn observe_tree(&self, app: Option<String>) -> R<String> {
        let app = match app {
            Some(a) if !a.trim().is_empty() => a,
            _ => self.require_active_allowed().await?,
        };
        let windows = self.app_windows(&app).await?;
        let title = self.active_title().await;
        let mut root = None;
        for w in &windows {
            let info = self.b.info(w, 0).await;
            // Hyprland's active window is no AT-SPI state: match the title.
            if info.states.contains(&"active") || title.as_deref() == Some(info.name.as_str()) {
                root = Some(self.add_ref(w, &info, &app.to_lowercase()).await);
                break;
            }
        }
        let args = match root {
            Some(rid) => json!({"root_id": rid, "max_nodes": OBSERVE_TREE_NODES}),
            None => json!({"app": app, "max_nodes": OBSERVE_TREE_NODES}),
        };
        Ok(cap_lines(self.tree(&args).await?.text, OBSERVE_TREE_CHARS))
    }

    /// The lines as text, and the id of the first element printed.
    async fn print(
        &self,
        lines: Vec<Line<B::Id>>,
        cache: &Cache<B::Id>,
        app: &str,
    ) -> (String, Option<u64>) {
        let printed = self.printed(lines, cache, app).await;
        let first = printed.iter().find_map(|p| match p {
            Printed::Node { rid, .. } => Some(*rid),
            Printed::Raw(_) => None,
        });
        (render::format(&printed), first)
    }

    /// The lines with their ids issued.
    async fn printed<'c>(
        &self,
        lines: Vec<Line<B::Id>>,
        cache: &'c Cache<B::Id>,
        app: &str,
    ) -> Vec<Printed<'c, B::Id>> {
        let mut out = Vec::with_capacity(lines.len());
        for line in lines {
            out.push(match line {
                Line::Raw(s) => Printed::Raw(s),
                Line::Node { depth, id } => {
                    let rid = self.add_ref(&id, &cache[&id], app).await;
                    Printed::Node {
                        depth,
                        rid,
                        info: &cache[&id],
                    }
                }
            });
        }
        out
    }

    // ------------------------------------------------------------ tools

    async fn apps(&self) -> R<String> {
        self.b.enable().await;
        let list = self.b.list_apps().await;
        Ok(apps_text(
            list,
            &self.policy(),
            &self.allow_file,
            self.b.apps_note(),
        ))
    }

    async fn windows(&self, a: &Json) -> R<String> {
        let query = arg_s(a, "query", "");
        let allowed = self.allowed();
        let layout = self.b.layout(&query, &allowed).await;
        let mut lines: Vec<String> = layout
            .screens
            .iter()
            .map(|s| {
                format!(
                    "screen {}: at ({},{}) {}x{}, scale {}",
                    s.name, s.x, s.y, s.width, s.height, s.scale
                )
            })
            .collect();
        if !layout.desktops.is_empty() {
            lines.push(desktops_line(&layout.desktops));
        }
        let list = layout.windows?;
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
            desktop: desktop_arg(a)?,
        };
        if let Some(st) = &change.state
            && !["maximize", "minimize", "restore"].contains(&st.as_str())
        {
            return Err(format!(
                "state must be maximize, minimize or restore, not '{st}'"
            ));
        }
        if change.state.is_none()
            && change.desktop.is_none()
            && [change.x, change.y, change.width, change.height]
                .iter()
                .all(Option::is_none)
        {
            return Err("pass x/y, width/height, state or desktop".into());
        }
        // The window it will move, for the tier check (and confirmation);
        // only apps with that tier are passed on, so it stays the same one.
        let policy = self.policy();
        let allowed = policy.names();
        if let Ok(list) = self.b.windows(&query, &allowed).await
            && let Some(w) = self.b.window_to_change(&list).and_then(|i| list.get(i))
        {
            self.permit(&self.window_app(w).await).await?;
        }
        let tiered = policy.names_with(policy::needed_tier("move_window", a));
        let mut w = self.b.set_window(&query, &tiered, &change).await?;
        // On Wayland the app applies a new size (and KWin a state) a moment
        // later; wait for it so the reply, and the next call, see the result.
        let start = Instant::now();
        while !window_reached(&w, &change) && start.elapsed() < WINDOW_SETTLE {
            tokio::time::sleep(Duration::from_millis(25)).await;
            let now = self.b.windows(&w.title, &allowed).await?;
            match now.into_iter().find(|n| n.app == w.app && same_id(n, &w)) {
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

    async fn close_window(&self, a: &Json) -> R<String> {
        let query = req_s(a, "query")?;
        if query.trim().is_empty() {
            return Err("pass the title (or app name) of the window to close as `query`".into());
        }
        let allowed = self.allowed();
        // Exactly one window, named in the confirmation and the reply.
        let list = self.b.windows(&query, &allowed).await?;
        let [w] = list.as_slice() else {
            return Err(backend::not_one_window(&query, &list));
        };
        // Always confirmed, not only on the allowlist's confirm list:
        // unsaved work may go with it.
        let app = self.window_app(w).await;
        self.permit(&app).await?;
        if !GATE.try_with(|g| g.borrow().confirmed).unwrap_or(false) {
            self.confirm("close_window", Some(&app), a).await?;
        }
        let closed = self.b.close_window(&query, &allowed).await?;
        let start = Instant::now();
        let gone = loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let open = self
                .b
                .windows(&closed.title, &allowed)
                .await
                .unwrap_or_default()
                .iter()
                .any(|n| {
                    n.app == closed.app
                        && n.pid == closed.pid
                        && n.title == closed.title
                        && same_id(n, &closed)
                });
            if !open || start.elapsed() >= CLOSE_SETTLE {
                break !open;
            }
        };
        let what = format!("{} [{}]", render::py_repr(&closed.title), closed.app);
        if gone {
            return Ok(format!("closed {what}"));
        }
        // The app's dialogs (a save prompt), with their buttons' ids.
        let named = self.b.names_surfaces();
        let windows = match named {
            true => self
                .b
                .app_windows(&app.to_lowercase())
                .await
                .unwrap_or_default(),
            false => self.b.dialogs_of(&closed).await,
        };
        let shown = self
            .surfaces_as(windows, false, Some(&app.to_lowercase()), &[], !named)
            .await;
        Ok(if shown.is_empty() {
            format!(
                "asked {what} to close; it is still open, probably asking whether to save \
                 changes (see apps, then find/act on its dialog)"
            )
        } else {
            format!(
                "asked {what} to close; it is still open, showing {}",
                shown.join("; ")
            )
        })
    }

    async fn launch(&self, a: &Json) -> R<String> {
        let app = req_s(a, "app")?.trim().to_lowercase();
        // Refused before anything is resolved or started.
        if !self.is_allowed(&app) {
            return Err(self.not_allowed(&app));
        }
        self.permit(&app).await?;
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
        let policy = self.policy();
        let allow = policy.names();
        let mut lines = vec![format!(
            "desktop-mcp {} ({} {})",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH
        )];
        let update = update::line(env!("CARGO_PKG_VERSION")).await;
        let mut row = |ok: bool, name: &str, detail: &str| {
            lines.push(format!(
                "[{}] {name}: {detail}",
                if ok { "ok" } else { "FAIL" }
            ));
        };
        row(true, "update", &update);
        let listed = self.policy_file();
        let aliased: Vec<String> = allow
            .iter()
            .filter(|a| listed.tier(a).is_none())
            .cloned()
            .collect();
        let aliased = match aliased.is_empty() {
            true => String::new(),
            false => format!("; allowed through an alias: {}", aliased.join(", ")),
        };
        row(
            !allow.is_empty(),
            "allowlist",
            &format!(
                "{} ({} apps: {}){aliased}",
                self.allow_file.display(),
                listed.apps.len(),
                listed.apps_line()
            ),
        );
        for w in &policy.warnings {
            row(false, "allowlist line", w);
        }
        let asks = if self.link.borrow().elicitation {
            "the client asks the user (elicitation)"
        } else {
            "this client cannot ask the user (no elicitation): calls need confirm: true"
        };
        row(
            true,
            "confirm",
            &if policy.confirm.is_empty() {
                format!(
                    "none (add e.g. 'confirm: {}' to the allowlist)",
                    self.b.confirm_example()
                )
            } else {
                let list: Vec<String> = policy.confirm.iter().map(|r| r.to_string()).collect();
                format!("{}; {asks}", list.join(", "))
            },
        );
        let env_on = std::env::var(policy::PASSWORDS_ENV).is_ok_and(|v| v.trim() == "1");
        row(
            true,
            "password fields",
            &match (env_on, policy.passwords.is_empty()) {
                (true, _) => format!(
                    "may be filled in every allowed app ({}=1)",
                    policy::PASSWORDS_ENV
                ),
                (false, true) => "never read or filled in".to_string(),
                (false, false) => format!(
                    "never read; filled in only in: {}",
                    policy.passwords.join(", ")
                ),
            },
        );
        let (mut allowed, mut blocked) = (Vec::new(), Vec::new());
        for app in self.b.list_apps().await {
            if app.windows.is_empty() {
                continue;
            }
            if let Some(tier) = policy.tier(&app.name) {
                allowed.push(match tier {
                    Tier::Full => app.name,
                    t => format!("{} ({t})", app.name),
                });
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
        if let Some(t) = &self.takeover {
            let (ok, detail) = t.report();
            row(ok, "takeover detection", &detail);
        }
        if !self.b.apps_note().is_empty() {
            lines.push(format!("note: {}", self.b.apps_note()));
        }
        Ok(lines.join("\n"))
    }

    async fn tree(&self, a: &Json) -> R<Out> {
        self.b.enable().await;
        let root_id = arg_i(a, "root_id", 0);
        let interactive = arg_b(a, "interactive", false);
        let opts = WalkOpts {
            max_depth: arg_i(a, "max_depth", 12).max(0) as usize,
            budget: arg_i(a, "max_nodes", 300).max(0) as usize,
            only_showing: !arg_b(a, "include_hidden", false),
            boring_roles: self.b.boring_roles(),
            interactive,
        };
        let max_chars = arg_i(a, "max_chars", TREE_MAX_CHARS).max(0) as usize;
        let mut cache = Cache::new();
        // Only a tree of the app's windows reuses and keeps reads.
        let whole_app = root_id == 0 && !arg_b(a, "document", false);
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
        // The element the previous page ended with.
        let after_rid = arg_i(a, "after", 0);
        let after = if after_rid != 0 {
            let (id, of) = self.resolve_with_app(after_rid).await?;
            if of != app {
                return Err(format!(
                    "`after` id {after_rid} belongs to '{of}', not '{app}'"
                ));
            }
            Some(id)
        } else {
            None
        };
        let fetch = |limit| Fetch {
            only_showing: opts.only_showing,
            text_limit: TREE_TEXT_LIMIT,
            limit,
            actions: true,
            stop_roles: &[],
            offscreen: false,
        };
        let header = match after_rid {
            0 => header,
            rid => format!("{header} (continued after [{rid}])"),
        };
        let ((lines, walked), cache, _) = self
            .read_app(
                whole_app.then_some(app.as_str()),
                &roots,
                cache,
                fetch((opts.budget * 4).max(1000)),
                fetch(PREFETCH_MORE),
                |c| {
                    let mut lines = vec![Line::Raw(header.clone())];
                    render::walk(c, &roots, 1, &opts, after.as_ref(), &mut lines)
                        .map(|walked| (lines, walked))
                },
            )
            .await;
        if !walked.after_found {
            return Err(format!(
                "`after` id {after_rid} is not in this tree (any more); call tree without `after`"
            ));
        }
        let printed = self.printed(lines, &cache, &app).await;
        // Room for the continuation line.
        let room = match max_chars {
            0 => 0,
            n => n.saturating_sub(CONTINUATION_CHARS).max(1),
        };
        let capped = render::format_capped(&printed, room);
        let mut text = capped.text;
        if capped.left > 0 || walked.truncated {
            text.push('\n');
            text.push_str(&continuation(
                a,
                &app,
                capped.last,
                capped.left,
                walked.truncated,
            ));
        }
        if !arg_b(a, "screenshot", false) {
            return Ok(Out::from(text));
        }
        // The image of the window the tree shows: only while it is in front,
        // as screenshots capture the active window.
        let active = self.b.active_app().await.to_lowercase();
        if active != app {
            text.push_str(&format!(
                "\nno screenshot: the active window is not {app}'s (screenshot with \
                 window=… brings it to the front)"
            ));
            return Ok(Out::from(text));
        }
        match self.screenshot(&json!({})).await {
            Ok(shot) => Ok(Out {
                text: format!("{text}\nscreenshot: {}", shot.text),
                images: shot.images,
            }),
            Err(e) => Ok(Out::from(format!("{text}\nno screenshot: {e}"))),
        }
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
        // `role` alone lists the elements of that role (query "" matches
        // every name).
        let has_role = !arg_s(a, "role", "").trim().is_empty();
        let query = match a.get("query").and_then(Json::as_str) {
            Some(q) => q.to_string(),
            None if has_role => String::new(),
            None => return Err("missing string argument 'query' (or pass `role`)".into()),
        };
        let app = req_s(a, "app")?;
        let role = arg_s(a, "role", "");
        let max_results = arg_i(a, "max_results", 30).max(0) as usize;
        let windows = trace::time("app_windows", self.app_windows(&app)).await?;
        let ((hits, by_synonym), mut cache, _) = self
            .read_app(
                Some(&app.to_lowercase()),
                &windows,
                Cache::new(),
                search_fetch(render::FIND_TEXT_LIMIT, render::FIND_MAX_SEEN),
                search_fetch(render::FIND_TEXT_LIMIT, PREFETCH_MORE),
                |c| {
                    render::with_role_synonyms(
                        &role,
                        |h: &Vec<B::Id>| !h.is_empty(),
                        |r| render::find(c, &windows, &query, r, max_results),
                    )
                },
            )
            .await;
        if hits.is_empty() {
            let report = self
                .miss_report(&windows, &mut cache, &app.to_lowercase(), &query, &role)
                .await;
            let head = if report.starts_with("\noff-screen") {
                "no visible matches"
            } else {
                "no matches"
            };
            let with_role = if role.trim().is_empty() {
                String::new()
            } else {
                format!(" with role '{}'", role.trim())
            };
            return Ok(format!("{head}{with_role}{report}"));
        }
        self.b.fill_actions(&mut cache, &hits).await;
        let roles: Vec<&str> = hits.iter().map(|id| cache[id].role.as_str()).collect();
        let synonym = render::synonym_note(&role, by_synonym, &roles);
        let lines = hits
            .into_iter()
            .map(|id| Line::Node { depth: 0, id })
            .collect();
        let (text, first) = self.print(lines, &cache, &app.to_lowercase()).await;
        if let Some(rid) = first {
            note_step_element(rid, true);
        }
        Ok(if synonym.is_empty() {
            text
        } else {
            format!("{synonym}:\n{text}")
        })
    }

    async fn act(&self, a: &Json) -> R<String> {
        let action = arg_s(a, "action", "");
        // Clients that fill every optional field send "": no option.
        let option = match a.get("option") {
            None | Some(Json::Null) => None,
            Some(Json::String(s)) if s.trim().is_empty() => None,
            Some(Json::String(s)) => Some(s.trim().to_string()),
            Some(_) => return Err("option must be the label of the entry to select".into()),
        };
        let verb = match (Verb::parse(&action), option.is_some()) {
            (None, true) if action.is_empty() => Some(Verb::Select),
            (Some(Verb::Select), has) => has.then_some(Verb::Select),
            (v, false) => v,
            (_, true) => {
                return Err(format!(
                    "option goes with action=select, not '{action}'; nothing sent"
                ));
            }
        };
        let t = self.target(a).await?;
        let id = &t.id;
        // Expanding a menu entry opens its submenu: the reply waits for it
        // (`opened_note`).
        let opens = |name: &str| ["expand", "show menu"].contains(&name.to_lowercase().as_str());
        let menu_like =
            render::role_in(&t.role, &OPENER_ROLES) || render::role_in(&t.role, &DROP_DOWN_ROLES);
        if menu_like && (verb == Some(Verb::Expand) || opens(&action)) {
            let _ = GATE.try_with(|g| g.borrow_mut().opener = true);
        }
        // A drop-down button (UIA: a button that expands) opens a menu
        // whatever its action.
        let drop_down = |names: &[String]| {
            render::role_in(&t.role, &DROP_DOWN_ROLES)
                && names.iter().any(|n| n.eq_ignore_ascii_case("expand"))
        };
        if let Some(verb) = verb {
            let done = self.verified(&t, verb, &action, option.as_deref()).await;
            // None: not of the kind, but it has an action of that name.
            if let Some(reply) = done.map_err(|e| t.reply(&e))? {
                return Ok(t.reply(&reply));
            }
        }
        let names = self.b.actions(id).await;
        if names.is_empty() {
            return Err("element has no actions; try click, or focus + key".into());
        }
        // Chromium lists showContextMenu on every element: never the
        // default. Tree and find do not show it, so an element with nothing
        // else looks plain there; opening its context menu instead (a chat
        // row's Archive / Delete) would surprise.
        let mut idx = names
            .iter()
            .position(|n| !n.eq_ignore_ascii_case("showcontextmenu"))
            .unwrap_or(usize::MAX);
        if action.is_empty() && idx == usize::MAX {
            return Err(
                "element has no default action (only showContextMenu); use click, \
                 or act with action=showContextMenu"
                    .into(),
            );
        }
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
        if (render::role_in(&t.role, &OPENER_ROLES) && opens(&names[idx])) || drop_down(&names) {
            let _ = GATE.try_with(|g| g.borrow_mut().opener = true);
        }
        let pre = self.pre_content(Some(id));
        let ok = self.b.do_action(id, idx).await?;
        if ok && !names[idx].eq_ignore_ascii_case("showcontextmenu") {
            self.note_content(pre, id, &t.role, false).await;
        }
        // A window that opened or closed shows in the effect line.
        Ok(t.reply(&format!(
            "{} -> {}",
            names[idx],
            if ok { "ok" } else { "failed" }
        )))
    }

    // ------------------------------------------------- verified actions

    /// `act`'s verified actions: the reply, or Ok(None) when the element is
    /// not of the kind the verb needs (not checkable, not expandable) but
    /// has an action named `action`, which then runs as a plain action.
    /// Nothing is sent when the element already is as asked; a state read
    /// back that contradicts the request is an error saying what was sent.
    async fn verified(
        &self,
        t: &Target<B::Id>,
        verb: Verb,
        action: &str,
        option: Option<&str>,
    ) -> R<Option<String>> {
        match verb {
            Verb::Check | Verb::Uncheck | Verb::Toggle => self.set_checked(t, verb, action).await,
            Verb::Expand => self.set_expanded(t, Some(true), action).await,
            Verb::Collapse => self.set_expanded(t, Some(false), action).await,
            Verb::Select => self
                .select_option(t, option.unwrap_or_default())
                .await
                .map(Some),
            Verb::ScrollTo => self.scroll_to(t).await.map(Some),
        }
    }

    /// Read `id`'s states until `done` holds or `limit` passed; the last
    /// read.
    async fn await_state(
        &self,
        id: &B::Id,
        limit: Duration,
        done: impl Fn(&Toggles) -> bool,
    ) -> Toggles {
        let start = Instant::now();
        let near = std::slice::from_ref(id);
        loop {
            let mark = self.b.change_mark();
            let s = self.b.toggles(id).await;
            let left = limit.saturating_sub(start.elapsed());
            if done(&s) || left.is_zero() {
                return s;
            }
            self.b.wait_change(near, mark, left.min(VERIFY_POLL)).await;
        }
    }

    /// check, uncheck, toggle: idempotent, then verified.
    async fn set_checked(&self, t: &Target<B::Id>, verb: Verb, action: &str) -> R<Option<String>> {
        let id = &t.id;
        let (info, now) = tokio::join!(self.b.info(id, 0), self.b.toggles(id));
        let checkable = now.checkable || semantic::is_check_role(&info.role);
        if !checkable {
            // An action of that name runs as before (Qt's Toggle on an
            // item-view row selects it); toggle flips the expansion only
            // of an element without one.
            let want = semantic::norm(action);
            let named = info.actions.iter().any(|a| semantic::norm(a) == want);
            let expandable =
                now.expandable || now.expanded || semantic::has_expand_action(&info.actions);
            if verb == Verb::Toggle && expandable && !named {
                return self.set_expanded(t, None, action).await;
            }
            return not_of_kind(&info, action, "checkable");
        }
        let want = match verb {
            Verb::Check => true,
            Verb::Uncheck => false,
            _ => !now.checked,
        };
        let word = |on: bool| if on { "checked" } else { "unchecked" };
        if now.checked == want {
            return Ok(Some(format!("already {}; nothing sent", word(want))));
        }
        if !want && semantic::is_radio(&info.role) {
            return Err(
                "a radio button is unchecked by checking another one of its group; nothing sent"
                    .into(),
            );
        }
        if now.disabled {
            return Err("it is disabled; nothing sent".into());
        }
        // Qt's Toggle on a row selects it rather than check it.
        let qt = semantic::is_item_role(&info.role) && self.b.toolkit_of(id).await == "qt";
        let how = match semantic::check_action(&info.role, &info.actions, want, qt) {
            Some(idx) => {
                self.b.do_action(id, idx).await?;
                info.actions[idx].clone()
            }
            // A checkable row or cell: its generic action would select or
            // open it; space on it checks it.
            None if semantic::is_item_role(&info.role) => {
                let what = if want { "check it" } else { "uncheck it" };
                self.key_on(t, "space", 1, what).await?
            }
            None => return Err(no_action(&info, if want { "check" } else { "uncheck" })),
        };
        let name = &how;
        let after = self
            .await_state(id, VERIFY_WAIT, |s| s.checked == want)
            .await;
        if after.checked == want {
            Ok(Some(format!("{name} -> {} (verified)", word(want))))
        } else {
            Err(format!(
                "sent {name}, but it reads {} afterwards",
                word(after.checked)
            ))
        }
    }

    /// expand, collapse (`want`; None: the other way): idempotent, then
    /// verified. An element with no action for it (Qt's tree items) is
    /// focused and sent right or left, where the app's tier allows keys.
    async fn set_expanded(
        &self,
        t: &Target<B::Id>,
        want: Option<bool>,
        action: &str,
    ) -> R<Option<String>> {
        let id = &t.id;
        let (info, now) = tokio::join!(self.b.info(id, 0), self.b.toggles(id));
        if !(now.expandable || now.expanded || semantic::has_expand_action(&info.actions)) {
            return not_of_kind(&info, action, "expandable");
        }
        let want = want.unwrap_or(!now.expanded);
        let word = |on: bool| if on { "expanded" } else { "collapsed" };
        if now.expanded == want {
            return Ok(Some(format!("already {}; nothing sent", word(want))));
        }
        if now.disabled {
            return Err("it is disabled; nothing sent".into());
        }
        if semantic::opens_menu(&info.role) && !self.b.may_open_menu(id).await {
            return Err(NO_MENU_OPEN.into());
        }
        let how = match semantic::expand_action(&info.role, &info.actions, want) {
            Some(idx) => {
                self.b.do_action(id, idx).await?;
                info.actions[idx].clone()
            }
            None => {
                let key = if want { "right" } else { "left" };
                let what = if want { "expand it" } else { "collapse it" };
                self.key_on(t, key, 1, what).await?
            }
        };
        let after = self
            .await_state(id, VERIFY_WAIT, |s| s.expanded == want)
            .await;
        if after.expanded == want {
            Ok(Some(format!("{how} -> {} (verified)", word(want))))
        } else {
            Err(format!(
                "sent {how}, but it reads {} afterwards",
                word(after.expanded)
            ))
        }
    }

    /// The keyboard fallback for what an element has no action for: focus
    /// `t` without the pointer, then press `combo` `times` times. Needs the
    /// full tier (keys) in its app; refused, with nothing sent, when the
    /// focus cannot be moved there or its window is not in front. The
    /// reply says what was done.
    async fn key_on(&self, t: &Target<B::Id>, combo: &str, times: usize, what: &str) -> R<String> {
        self.keys_permitted(t, combo, what).await?;
        let Some(method) = self.b.try_focus(&t.id).await? else {
            return Err(format!(
                "there is no action to {what}, and it cannot be focused without the \
                 pointer; nothing sent: click it, then key {combo}"
            ));
        };
        // Keys go to the focused element of the window in front.
        match self.b.focused().await {
            Some(f) if f.id == t.id => {}
            _ => {
                return Err(format!(
                    "focused it ({method}) to {what}, but its window is not in front; no key \
                     sent: pass window=…"
                ));
            }
        }
        for i in 0..times {
            // The user may take over between presses.
            if i > 0 {
                self.gate(true)
                    .await
                    .map_err(|e| format!("{e} after {i} of {times} presses of {combo}"))?;
            }
            self.b.press_combo(combo).await?;
        }
        let times = if times > 1 {
            format!(" x{times}")
        } else {
            String::new()
        };
        Ok(format!("focus ({method}) + key {combo}{times}"))
    }

    /// Whether the key fallback may press `combo` for `t`'s app: the full
    /// tier, its confirmation, and the key not held already.
    async fn keys_permitted(&self, t: &Target<B::Id>, combo: &str, what: &str) -> R<()> {
        let policy = self.policy();
        let tier = policy.tier(&t.app).unwrap_or(Tier::View);
        if tier < Tier::Full {
            return Err(refusal(format!(
                "there is no action to {what}, and the fallback, key {combo}, needs the \
                 full tier ('{}' has {tier}); nothing sent",
                t.app
            )));
        }
        // One confirmation covers the whole call: the act call's own
        // (by elicitation, or its confirm: true) covers its keys too.
        let (confirmed, confirm_arg) = GATE
            .try_with(|g| {
                let g = g.borrow();
                (g.confirmed, g.args.get("confirm").cloned())
            })
            .unwrap_or((false, None));
        if !confirmed && policy.needs_confirm("key", Some(&t.app)) {
            let mut args = json!({"combo": combo});
            if let Some(c) = confirm_arg {
                args["confirm"] = c;
            }
            self.confirm("key", Some(&t.app), &args).await?;
        }
        // A held key stays down: pressing it again would release it.
        if chord_keys(combo)
            .iter()
            .any(|k| self.is_held(&Held::Key(k.clone())))
        {
            return Err(format!(
                "there is no action to {what}, and the fallback, key {combo}, is held already \
                 (hold with down=false releases it); nothing sent"
            ));
        }
        Ok(())
    }

    /// The fallback for a closed combo box that takes no focus without the
    /// pointer (Qt's): open its list through its own action (a shown popup
    /// takes the keyboard), press down or up from the entry it highlights
    /// (selected or focused there, else the current value) to `want`, and
    /// Enter. Needs the full tier, like `key_on`.
    async fn select_in_popup(&self, t: &Target<B::Id>, want: &str) -> R<String> {
        self.keys_permitted(t, "enter", "select it").await?;
        let control = &t.id;
        if !self.b.may_open_menu(control).await {
            return Err(NO_MENU_OPEN.into());
        }
        let actions = self.b.actions(control).await;
        let open = actions
            .iter()
            .position(|a| semantic::norm(a) == "showmenu")
            .or_else(|| actions.iter().position(|a| semantic::norm(a) == "press"))
            .ok_or("it has no action that opens its list")?;
        self.b.do_action(control, open).await?;
        // Keys go to the popup once its list shows (Qt reports no focus
        // for it): wait for that.
        let start = Instant::now();
        let (cache, items) = loop {
            let (cache, items) = self.read_options(control).await;
            if items.iter().any(|id| cache[id].showing) {
                break (cache, items);
            }
            if start.elapsed() >= VERIFY_WAIT {
                return Err(format!(
                    "opened its list ({}), but no list showed; no key sent (it may still be \
                     open: key escape closes it)",
                    actions[open]
                ));
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        let enabled: Vec<&B::Id> = items
            .iter()
            .filter(|id| !cache[*id].states.contains(&"disabled"))
            .collect();
        let at = |id: &B::Id| enabled.iter().position(|e| *e == id);
        let wanted = enabled
            .iter()
            .position(|id| render::menu_norm(&cache[*id].name) == render::menu_norm(want))
            .ok_or("its list does not show that entry; no key sent (key escape closes it)")?;
        let lit = |state: &str| {
            enabled
                .iter()
                .position(|id| cache[*id].states.contains(&state))
        };
        let from = lit("focused")
            .or_else(|| lit("selected"))
            .or_else(|| semantic::current(&cache, control, &items).and_then(|c| at(&c)))
            .ok_or(
                "cannot tell which entry its list highlights; no key sent (key escape closes it)",
            )?;
        let steps = wanted as i64 - from as i64;
        let key = if steps > 0 { "down" } else { "up" };
        let n = steps.unsigned_abs();
        let open_note = "; its list may still be open: key escape closes it";
        for i in 0..n {
            if i > 0 {
                self.gate(true)
                    .await
                    .map_err(|e| format!("{e} after {i} of {n} presses of {key}{open_note}"))?;
            }
            self.b.press_combo(key).await?;
        }
        self.gate(true)
            .await
            .map_err(|e| format!("{e} after opening its list, before enter{open_note}"))?;
        self.b.press_combo("enter").await?;
        Ok(match steps {
            0 => format!("{} + key enter", actions[open]),
            n => format!(
                "{} + key {key} x{} + enter",
                actions[open],
                n.unsigned_abs()
            ),
        })
    }

    /// The control's subtree, hidden parts included (a closed combo box's
    /// list), and its entries.
    async fn read_options(&self, control: &B::Id) -> (Cache<B::Id>, Vec<B::Id>) {
        let mut cache = Cache::new();
        let fetch = Fetch {
            only_showing: false,
            text_limit: render::FIND_TEXT_LIMIT,
            limit: OPTIONS_FETCH,
            actions: false,
            stop_roles: &[],
            offscreen: false,
        };
        // Its entries may have come or gone since it was found.
        let mut next = vec![self.b.live(control)];
        for _ in 0..MENU_FETCH_ROUNDS {
            self.b.prefetch_with(&mut cache, next, &fetch).await;
            let (items, missing) = semantic::walk_options(&cache, control);
            // What the read skipped (past its limit, or a backend that
            // reads one level a call), until enough was read.
            if missing.is_empty() || cache.len() >= OPTIONS_FETCH {
                return (cache, items);
            }
            next = missing;
        }
        let items = semantic::options(&cache, control);
        (cache, items)
    }

    /// Whether `control` shows the entry `label` (of `labels`, all its
    /// entries) as its value, or the entry `item` reads selected or
    /// checked; waits up to VERIFY_WAIT for it. Also what it shows then.
    async fn option_shown(
        &self,
        control: &B::Id,
        item: &B::Id,
        label: &str,
        labels: &[String],
    ) -> (bool, String) {
        let start = Instant::now();
        let near = std::slice::from_ref(control);
        let want = render::menu_norm(label);
        loop {
            let mark = self.b.change_mark();
            let (info, state) = tokio::join!(self.b.info(control, 0), self.b.toggles(item));
            let named = semantic::named_option(&info.name, info.text.as_deref(), labels);
            let ok = match named {
                Some(i) => render::menu_norm(&labels[i]) == want,
                None => state.selected || state.checked,
            };
            let left = VERIFY_WAIT.saturating_sub(start.elapsed());
            if ok || left.is_zero() {
                let shows = match named {
                    Some(i) => format!("it shows {}", render::json_str(&labels[i])),
                    None if ok => format!("{} reads selected", render::json_str(label)),
                    None => format!("{} is not selected", render::json_str(label)),
                };
                return (ok, shows);
            }
            self.b.wait_change(near, mark, left.min(VERIFY_POLL)).await;
        }
    }

    /// select with `option`: pick the entry of a combo box, list, tab list
    /// or menu button named `want`, without the pointer: through the
    /// list's Selection, else the entry's own action, else (a closed combo
    /// box, full tier) focus and arrow keys; each tried only while the one
    /// before is not verified.
    async fn select_option(&self, t: &Target<B::Id>, want: &str) -> R<String> {
        let control = &t.id;
        let (mut cache, mut items) = self.read_options(control).await;
        let opened = items.is_empty();
        let mut open_note = String::new();
        if opened {
            let (how, found) = self.open_options(t, &mut cache).await?;
            items = found;
            open_note = format!("opened it ({how}); ");
        }
        let item = semantic::pick_option(&cache, &items, want).map_err(|e| {
            let left = if opened {
                " (it may still be open: key escape closes it)"
            } else {
                "; nothing sent"
            };
            format!("{open_note}{e}{left}")
        })?;
        let label = cache[&item].name.trim().to_string();
        let quoted = render::json_str(&label);
        let labels: Vec<String> = items.iter().map(|id| cache[id].name.clone()).collect();
        let current = semantic::current(&cache, control, &items);
        if !opened && current.as_ref() == Some(&item) {
            return Ok(format!("already {quoted}; nothing sent"));
        }
        // Opened, its menu may still show: the opening was sent.
        let still_open = if opened {
            " (it may still be open: key escape closes it)"
        } else {
            ""
        };
        let none = if opened {
            "nothing selected"
        } else {
            "nothing sent"
        };
        if cache[&item].states.contains(&"disabled") {
            return Err(format!(
                "{open_note}{quoted} is disabled; {none}{still_open}"
            ));
        }
        let mut tried: Vec<String> = Vec::new();
        let mut shows = String::new();
        // In a web page, Selection sets the entry's selected state without
        // the events the page's script listens to: the entry's own action
        // runs them.
        let web = matches!(
            self.b.toolkit_of(control).await.as_str(),
            "gecko" | "chromium"
        );
        if !opened && !web && self.b.select_item(&item).await.is_ok() {
            tried.push("Selection".into());
            let (ok, s) = self.option_shown(control, &item, &label, &labels).await;
            if ok {
                let more = self.also_selected(control, &item).await;
                return Ok(format!("{quoted} selected (Selection; verified){more}"));
            }
            shows = s;
        }
        let actions = self.b.actions(&item).await;
        if let Some(idx) = semantic::item_action(&actions) {
            self.b.do_action(&item, idx).await?;
            tried.push(format!("the entry's {} action", actions[idx]));
            let (ok, s) = self.option_shown(control, &item, &label, &labels).await;
            if ok {
                let more = match opened {
                    true => String::new(),
                    false => self.also_selected(control, &item).await,
                };
                return Ok(format!(
                    "{open_note}{quoted} selected ({}; verified){more}",
                    actions[idx]
                ));
            }
            if opened && !self.b.toggles(&item).await.showing {
                // A menu button that does not show its value: the entry ran
                // and its menu closed.
                return Ok(format!(
                    "{open_note}{quoted}: {} -> ok (the menu closed; the control does not \
                     show a value to verify)",
                    actions[idx]
                ));
            }
            shows = s;
        }
        // A closed combo box takes arrow keys: as many as entries between
        // the one shown now (read afresh after the steps above) and the one
        // wanted, by label; disabled ones are skipped.
        let steps = if opened || self.b.toggles(control).await.expanded {
            None
        } else {
            let (now, now_items) = if tried.is_empty() {
                (cache.clone(), items.clone())
            } else {
                self.read_options(control).await
            };
            let enabled: Vec<String> = now_items
                .iter()
                .filter(|id| !now[*id].states.contains(&"disabled"))
                .map(|id| render::menu_norm(&now[id].name))
                .collect();
            let at = |label: &str| enabled.iter().position(|l| *l == render::menu_norm(label));
            semantic::current(&now, control, &now_items)
                .and_then(|cur| at(&now[&cur].name))
                .zip(at(&label))
                .map(|(c, w)| w as i64 - c as i64)
        };
        let sent = |tried: &[String]| {
            if tried.is_empty() {
                "nothing sent".to_string()
            } else {
                format!("sent {}", tried.join(", then "))
            }
        };
        if let Some(steps) = steps.filter(|s| *s != 0) {
            let key = if steps > 0 { "down" } else { "up" };
            match self
                .key_on(t, key, steps.unsigned_abs() as usize, "select it")
                .await
            {
                Ok(how) => {
                    tried.push(how.clone());
                    let (ok, s) = self.option_shown(control, &item, &label, &labels).await;
                    if ok {
                        return Ok(format!("{quoted} selected ({how}; verified)"));
                    }
                    shows = s;
                }
                // Qt's combo boxes take no focus without the pointer:
                // open the list through its action and pick from there.
                Err(e) if e.contains("cannot be focused without the pointer") => {
                    match self.select_in_popup(t, &label).await {
                        Ok(how) => {
                            tried.push(how.clone());
                            let (ok, s) = self.option_shown(control, &item, &label, &labels).await;
                            if ok {
                                return Ok(format!("{quoted} selected ({how}; verified)"));
                            }
                            return Err(format!("{}, but {s} afterwards", sent(&tried)));
                        }
                        Err(e2) if tried.is_empty() => return Err(e2),
                        Err(e2) => {
                            return Err(format!(
                                "{}, but {shows} afterwards; the key fallback: {e2}",
                                sent(&tried)
                            ));
                        }
                    }
                }
                Err(e) if tried.is_empty() => return Err(e),
                Err(e) => {
                    return Err(format!(
                        "{}, but {shows} afterwards; the key fallback: {e}",
                        sent(&tried)
                    ));
                }
            }
        }
        if tried.is_empty() {
            return Err(format!(
                "{open_note}found {quoted}, but no way to select it without the pointer; \
                 {none}: click the control, then click the entry{still_open}"
            ));
        }
        Err(format!(
            "{open_note}{}, but {shows} afterwards{still_open}",
            sent(&tried)
        ))
    }

    /// After `item` was selected in `control`: "; N other entries stay
    /// selected too" when a multi-selection list kept others selected
    /// (a following Delete or Enter would act on all of them), else "".
    async fn also_selected(&self, control: &B::Id, item: &B::Id) -> String {
        let (cache, items) = self.read_options(control).await;
        let others = items
            .iter()
            .filter(|id| *id != item && cache[*id].states.contains(&"selected"))
            .count();
        match others {
            0 => String::new(),
            1 => "; 1 other entry stays selected too".into(),
            n => format!("; {n} other entries stay selected too"),
        }
    }

    /// For a control that lists no entries until opened (a menu button):
    /// open it through its action and wait for a popup menu of its app
    /// to show entries. The action that opened it, and the entries.
    async fn open_options(
        &self,
        t: &Target<B::Id>,
        cache: &mut Cache<B::Id>,
    ) -> R<(String, Vec<B::Id>)> {
        let id = &t.id;
        // Only a control that opens a popup: pressing anything else (OK,
        // Delete, a file's row) would do what it does.
        let (info, now) = tokio::join!(self.b.info(id, 0), self.b.toggles(id));
        if !semantic::may_open(&info.role, &info.actions, now.popup) {
            return Err(format!(
                "it lists no entries ({}): not a combo box, list or menu button; nothing sent",
                info.role
            ));
        }
        if !self.b.may_open_menu(id).await {
            return Err(format!(
                "it lists no entries until opened, and {NO_MENU_OPEN}"
            ));
        }
        let actions = info.actions;
        let idx = semantic::open_action(&actions)
            .ok_or("it lists no entries and has no action to open; nothing sent")?;
        let windows = self.app_windows(&t.app).await?;
        let how = actions[idx].clone();
        let mut mark = self.b.change_mark();
        self.b.do_action(id, idx).await?;
        let start = Instant::now();
        loop {
            let (fresh, items) = self.read_options(id).await;
            let showing: Vec<B::Id> = items.into_iter().filter(|i| fresh[i].showing).collect();
            cache.extend(fresh);
            if !showing.is_empty() {
                return Ok((how, showing));
            }
            let mut popups = Cache::new();
            let mut next = windows.clone();
            for _ in 0..MENU_FETCH_ROUNDS {
                let fetch = search_fetch(0, render::FIND_MAX_SEEN);
                self.b.prefetch_with(&mut popups, next, &fetch).await;
                next = popups
                    .values()
                    .filter(|i| i.showing)
                    .flat_map(|i| i.children.iter())
                    .filter(|c| !popups.contains_key(*c))
                    .cloned()
                    .collect();
                if next.is_empty() || popups.len() >= render::FIND_MAX_SEEN {
                    break;
                }
            }
            let menus: Vec<B::Id> = popups
                .iter()
                .filter(|(_, i)| {
                    i.showing && render::surface_kind(&i.role) == Some(render::Surface::Menu)
                })
                .map(|(id, _)| id.clone())
                .collect();
            let found: Vec<B::Id> = menus
                .iter()
                .flat_map(|m| semantic::options(&popups, m))
                .filter(|i| popups[i].showing)
                .collect();
            cache.extend(popups);
            if !found.is_empty() {
                return Ok((how, found));
            }
            let left = MENU_OPEN_WAIT.saturating_sub(start.elapsed());
            if left.is_zero() {
                return Err(format!(
                    "opened it ({how}), but no entries showed within {} ms (it may still be \
                     open: key escape closes it)",
                    MENU_OPEN_WAIT.as_millis()
                ));
            }
            self.b
                .wait_change(&windows, mark, left.min(WAIT_FALLBACK))
                .await;
            mark = self.b.change_mark();
        }
    }

    /// scroll_to: ask the app to scroll the element into view, else turn
    /// the wheel over its scrollable container (the pointer moves; said
    /// so) until it shows.
    async fn scroll_to(&self, t: &Target<B::Id>) -> R<String> {
        let id = &t.id;
        let now = self.b.toggles(id).await;
        if now.showing {
            return Ok("already in view; nothing sent".into());
        }
        let why = match self.b.scroll_into_view(id).await {
            Ok(true) => {
                if self
                    .await_state(id, VERIFY_WAIT, |s| s.showing)
                    .await
                    .showing
                {
                    return Ok("in view (ScrollTo; verified)".into());
                }
                "ScrollTo was accepted, but it did not show".to_string()
            }
            Ok(false) => "the app declined ScrollTo".to_string(),
            Err(e) => e,
        };
        // A wheel cannot show what a closed popup or collapsed branch holds.
        if !now.offscreen {
            return Err(format!(
                "{why}; it is hidden (in a closed popup or collapsed branch?), not scrolled \
                 out of view; no wheel turned: open or expand what holds it"
            ));
        }
        let Some(pane) = self.b.scroll_container(id).await else {
            return Err(format!(
                "{why}; no scrollable container found to turn the wheel over: scroll it with \
                 scroll on the list or page"
            ));
        };
        let (active, pane_at, at) = tokio::join!(
            self.require_active_allowed(),
            self.b.wheel_point(&pane),
            self.b.element_center(id)
        );
        let (x, y) = active
            .and(pane_at)
            .map_err(|e| format!("{why}; the wheel fallback: {e}"))?;
        // Off-screen elements still have a position: above or below.
        let dir = match at {
            Ok((_, ey)) if ey < y => -1,
            _ => 1,
        };
        let way = if dir > 0 { "down" } else { "up" };
        let mut total = 0;
        for round in 0..WHEEL_ROUNDS {
            // The user may take over between turns.
            if round > 0 {
                self.gate(true)
                    .await
                    .map_err(|e| format!("{e} after wheel {total} {way} at {x},{y}"))?;
            }
            self.b
                .pointer(x as i64, y as i64, "left", 0, dir * WHEEL_STEP)
                .await?;
            total += WHEEL_STEP;
            if self
                .await_state(id, WHEEL_WAIT, |s| s.showing)
                .await
                .showing
            {
                return Ok(format!(
                    "in view (wheel {total} {way} at {x},{y}, the pointer moved; verified)"
                ));
            }
        }
        Err(format!(
            "{why}; turned the wheel {total} {way} at {x},{y} (the pointer moved), but it is \
             still not in view"
        ))
    }

    async fn menu(&self, a: &Json) -> R<String> {
        let app = req_s(a, "app")?;
        let path = render::menu_path(a.get("path"))?;
        self.b.enable().await;
        let windows = self.app_windows(&app).await?;
        self.permit(&app).await?;
        let app_key = app.to_lowercase();
        let mut cache = Cache::new();
        let bar = self.menu_bar(&app, &windows, &mut cache).await?;
        let hidden = !cache[&bar].showing;
        let mut node = bar;
        let mut trail: Vec<String> = Vec::new();
        // The menu items walked, from the bar down (`open_parents`).
        let mut chain: Vec<B::Id> = Vec::new();
        let mut opened: Vec<String> = Vec::new();
        let where_ = |trail: &[String]| {
            if trail.is_empty() {
                format!("{app}'s menu bar")
            } else {
                render::json_str(&trail.join(" > "))
            }
        };
        let open_note = |opened: &[String]| {
            if opened.is_empty() {
                String::new()
            } else {
                format!(
                    " (opened {} to read it; it may still be open: key escape closes it)",
                    opened.join(", ")
                )
            }
        };
        for seg in &path {
            let mut items = self.menu_entries(&mut cache, &node).await;
            if items.is_empty() && !trail.is_empty() {
                self.open_parents(&chain, &windows, &app_key)
                    .await
                    .map_err(|e| format!("{}: {e}{}", where_(&trail), open_note(&opened)))?;
                // Those stay open too: each takes an escape.
                for parent in trail.iter().take(trail.len().saturating_sub(1)) {
                    let name = render::json_str(parent);
                    if !opened.contains(&name) {
                        opened.push(name);
                    }
                }
                items = self
                    .open_submenu(&mut cache, &node, &windows)
                    .await
                    .map_err(|e| format!("{}: {e}{}", where_(&trail), open_note(&opened)))?;
                opened.push(render::json_str(trail.last().unwrap()));
            }
            let item = match render::pick_menu_item(&cache, &items, seg) {
                Ok(item) => item,
                // A path in another language than the UI ("File > Save As"
                // in a Turkish Kate): KDE's English object names.
                Err(e) => {
                    let ids = futures_util::future::join_all(
                        items
                            .iter()
                            .map(|id| async { (id.clone(), self.b.object_id(id).await) }),
                    )
                    .await;
                    render::pick_by_object_name(&ids, seg).ok_or_else(|| {
                        format!("in {}: {e}{}", where_(&trail), open_note(&opened))
                    })?
                }
            };
            trail.push(cache[&item].name.trim().to_string());
            chain.push(item.clone());
            node = item;
        }
        self.b
            .fill_actions(&mut cache, std::slice::from_ref(&node))
            .await;
        let list = trail.is_empty() || render::is_submenu(&cache, &cache[&node]);
        if list {
            // The listing is what opened.
            let _ = GATE.try_with(|g| g.borrow_mut().quiet = true);
            let mut items = self.menu_entries(&mut cache, &node).await;
            if items.is_empty() && !trail.is_empty() {
                self.open_parents(&chain, &windows, &app_key)
                    .await
                    .map_err(|e| format!("{}: {e}{}", where_(&trail), open_note(&opened)))?;
                // Those stay open too: each takes an escape.
                for parent in trail.iter().take(trail.len().saturating_sub(1)) {
                    let name = render::json_str(parent);
                    if !opened.contains(&name) {
                        opened.push(name);
                    }
                }
                items = self
                    .open_submenu(&mut cache, &node, &windows)
                    .await
                    .map_err(|e| format!("{}: {e}{}", where_(&trail), open_note(&opened)))?;
                opened.push(render::json_str(trail.last().unwrap()));
            }
            self.b.fill_actions(&mut cache, &items).await;
            let mut lines = vec![format!(
                "{}{}:",
                where_(&trail),
                if hidden && trail.is_empty() {
                    " (hidden: its entries still work through `menu`)"
                } else {
                    ""
                }
            )];
            for id in &items {
                let rid = self.add_ref(id, &cache[id], &app_key).await;
                // A submenu's action only opens it: "(submenu)" says that
                // ("click" is implied for menu roles, so nothing is listed).
                let line = if render::is_submenu(&cache, &cache[id]) {
                    let info = Info {
                        actions: vec!["click".into()],
                        ..cache[id].clone()
                    };
                    format!("{} (submenu)", render::describe(&info, rid))
                } else {
                    render::describe(&cache[id], rid)
                };
                lines.push(line);
            }
            if items.is_empty() {
                lines.push("(no entries)".into());
            }
            let note = open_note(&opened);
            if !note.is_empty() {
                lines.push(note.trim().to_string());
            }
            return Ok(lines.join("\n"));
        }
        let info = &cache[&node];
        let what = render::json_str(&trail.join(" > "));
        if info.states.contains(&"disabled") {
            return Err(format!("{what} is disabled{}", open_note(&opened)));
        }
        let idx = menu_action(&info.actions)
            .ok_or_else(|| format!("{what} has no action{}", open_note(&opened)))?;
        let ok = self.b.do_action(&node, idx).await?;
        let reply = format!("{what} -> {}", if ok { "ok" } else { "failed" });
        Ok(reply)
    }

    /// The menu bar of `app`'s active window, else of its other windows (a
    /// dialog in front has none). Hidden menu bars count: KDE apps keep
    /// theirs in the tree when the hamburger menu replaces it, and their
    /// entries still work.
    async fn menu_bar(&self, app: &str, windows: &[B::Id], cache: &mut Cache<B::Id>) -> R<B::Id> {
        let fetch = |limit| Fetch {
            only_showing: false,
            text_limit: 0,
            limit,
            actions: false,
            stop_roles: &render::MENU_BAR_STOP,
            offscreen: false,
        };
        self.b
            .prefetch_with(cache, windows.to_vec(), &fetch(1))
            .await;
        let mut order: Vec<&B::Id> = windows.iter().filter(|w| cache.contains_key(*w)).collect();
        order.sort_by_key(|w| {
            let i = &cache[*w];
            (!i.states.contains(&"active"), !i.showing)
        });
        for w in order {
            let kids = cache[w].children.clone();
            self.b
                .prefetch_with(cache, kids, &fetch(render::FIND_MAX_SEEN))
                .await;
            for _ in 0..MENU_FETCH_ROUNDS {
                match render::menu_bars(cache, std::slice::from_ref(w)) {
                    Ok(bars) => {
                        if let Some(bar) = bars.into_iter().next() {
                            return Ok(bar);
                        }
                        break;
                    }
                    Err(missing) => {
                        self.b
                            .prefetch_with(cache, missing, &fetch(PREFETCH_MORE))
                            .await
                    }
                }
            }
        }
        Err(format!(
            "no menu bar in '{app}'s windows; the app may have only a hamburger or \
             context menu: find its button and act on it, or use key"
        ))
    }

    /// The entries of `node` (a menu bar or submenu entry), fetching what is
    /// missing.
    async fn menu_entries(&self, cache: &mut Cache<B::Id>, node: &B::Id) -> Vec<B::Id> {
        let fetch = Fetch {
            only_showing: false,
            text_limit: 0,
            limit: PREFETCH_MORE,
            actions: false,
            stop_roles: &[],
            offscreen: false,
        };
        for _ in 0..MENU_FETCH_ROUNDS {
            match render::menu_items(cache, node) {
                Ok(items) => return items,
                Err(missing) => self.b.prefetch_with(cache, missing, &fetch).await,
            }
        }
        Vec::new()
    }

    /// Open a submenu whose entries are not in the tree until it is shown
    /// (Firefox), and wait for them.
    /// Open the menus above `node` on the way to it (`chain`, from the menu
    /// bar down, `node` last), so it opens on screen: KDE fills some menus
    /// (Dolphin's "Create New", KNewFileMenu) only when they are shown,
    /// and a submenu of a menu bar Dolphin hides is not shown by its own
    /// action. Menus already listed need no wait for their entries.
    async fn open_parents(&self, chain: &[B::Id], windows: &[B::Id], app: &str) -> R<()> {
        let Some((_, parents)) = chain.split_last() else {
            return Ok(());
        };
        // On Wayland an app shows a popup menu only after recent input from
        // the user (an input serial): actions alone left Dolphin's "Create
        // New" empty. A shift press and release, which types nothing, gives
        // it one; only while that app's window is in front.
        if self
            .require_active_allowed()
            .await
            .is_ok_and(|front| front.eq_ignore_ascii_case(app))
        {
            self.b.press_combo("shift").await?;
        }
        for p in parents {
            if !self.b.may_open_menu(p).await {
                return Ok(());
            }
            let actions = self.b.actions(p).await;
            if let Some(idx) = actions.iter().position(|a| a.to_lowercase() == "show menu") {
                // Shown before its submenu's action runs, or that one is
                // not shown either.
                let mark = self.b.change_mark();
                self.b.do_action(p, idx).await?;
                self.b.wait_change(windows, mark, PARENT_MENU_WAIT).await;
            }
        }
        Ok(())
    }

    /// The entries of the submenu `node` opened when they are not below it
    /// (`open_menus`): of the first menu showing that does not hold `node`,
    /// one that showed before it was opened (`before`) only with `old`.
    async fn detached_entries(
        &self,
        cache: &mut Cache<B::Id>,
        node: &B::Id,
        windows: &[B::Id],
        before: &[B::Id],
        old: bool,
    ) -> Vec<B::Id> {
        let Some(menus) = self.b.open_menus(windows).await else {
            return Vec::new();
        };
        let (new, seen): (Vec<B::Id>, Vec<B::Id>) =
            menus.into_iter().partition(|m| !before.contains(m));
        let menus = match old {
            true => new.into_iter().chain(seen).collect(),
            false => new,
        };
        for menu in menus {
            let mut one = Cache::new();
            self.b
                .prefetch_with(
                    &mut one,
                    vec![menu.clone()],
                    &search_fetch(0, render::FIND_MAX_SEEN),
                )
                .await;
            if render::holds(&one, &menu, node) {
                continue;
            }
            let items = self.menu_entries(&mut one, &menu).await;
            cache.extend(one);
            if !items.is_empty() {
                return items;
            }
        }
        Vec::new()
    }

    async fn open_submenu(
        &self,
        cache: &mut Cache<B::Id>,
        node: &B::Id,
        windows: &[B::Id],
    ) -> R<Vec<B::Id>> {
        if !self.b.may_open_menu(node).await {
            return Err(
                "this app lists a submenu's entries only while it is open, and opening \
                 its menus through accessibility can hang or crash it (Firefox); open the \
                 menu with key (alt + its underlined letter, or f10), then call menu again"
                    .into(),
            );
        }
        let actions = self.b.actions(node).await;
        let idx = actions
            .iter()
            .position(|a| {
                ["show menu", "expand", "open", "click", "press", "activate"]
                    .contains(&a.to_lowercase().as_str())
            })
            .or((!actions.is_empty()).then_some(0))
            .ok_or(
                "its entries are not in the accessibility tree and it has no action to open it",
            )?;
        // Menus open already (the one holding `node`, or one left open) are
        // not the submenu, unless nothing else shows.
        let mut before = self.b.open_menus(windows).await.unwrap_or_default();
        // Open already (a listing left it so): closed first, with what it
        // opened in turn, so the menu that shows is its own.
        if !before.is_empty()
            && let Some(c) = actions
                .iter()
                .position(|a| a.eq_ignore_ascii_case("collapse"))
            && self.b.info(node, 0).await.states.contains(&"expanded")
        {
            self.b.do_action(node, c).await?;
            let start = Instant::now();
            while start.elapsed() < MENU_OLD_AFTER {
                tokio::time::sleep(MENU_POLL).await;
                let now = self.b.open_menus(windows).await.unwrap_or_default();
                if now.len() < before.len() {
                    before = now;
                    break;
                }
            }
        }
        let mut mark = self.b.change_mark();
        self.b.do_action(node, idx).await?;
        let start = Instant::now();
        loop {
            // Read the entry afresh: what the cache holds predates the popup.
            let mut fresh = Cache::new();
            fresh.insert(node.clone(), self.b.info(node, 0).await);
            let mut items = self.menu_entries(&mut fresh, node).await;
            if items.is_empty() {
                let old = start.elapsed() >= MENU_OLD_AFTER;
                items = self
                    .detached_entries(&mut fresh, node, windows, &before, old)
                    .await;
            }
            cache.extend(fresh);
            if !items.is_empty() {
                return Ok(items);
            }
            let left = MENU_OPEN_WAIT.saturating_sub(start.elapsed());
            if left.is_zero() {
                return Err(format!(
                    "opened it, but no entries appeared within {} ms",
                    MENU_OPEN_WAIT.as_millis()
                ));
            }
            self.b
                .wait_change(windows, mark, left.min(WAIT_FALLBACK))
                .await;
            mark = self.b.change_mark();
        }
    }

    async fn set_text(&self, a: &Json) -> R<String> {
        let text = req_s(a, "text")?;
        let t = self.target(a).await?;
        if t.protected() && !self.passwords_allowed(&t.app) {
            return Err(t.reply(&self.password_refusal("set_text", &t.app)));
        }
        let typed = match self.write_text(&t.id, &text, false).await? {
            Written::Refused => return Ok(t.reply("failed")),
            Written::Set => false,
            Written::Typed => true,
        };
        if t.protected() {
            return Ok(t.reply("ok (password field: contents cannot be read back)"));
        }
        if typed {
            let how = "; typed: the element has no text interface";
            return match self.read_back(&t.id, &text, SET_TEXT_VERIFY).await {
                Ok(None) => Ok(t.reply(&format!("ok (verified{how})"))),
                Ok(Some(now)) => Err(t.reply(&written_mismatch(&now, &text, how))),
                Err(e) => Ok(t.reply(&format!("ok (not verified: {e}{how})"))),
            };
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
        if matches!(&seen, Ok(Some(_))) && retypable && self.retype(&t.id, &text, false).await {
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
            Ok(Some(now)) => Err(t.reply(&written_mismatch(&now, &text, how))),
            Err(e) => Ok(t.reply(&format!("ok (not verified: {e})"))),
        }
    }

    /// Write `text` into an element: through its text interface, else, for an
    /// editable element without one (Qt's editable combo boxes, e.g. a KDE
    /// file dialog's name field), by focusing it, selecting all and typing.
    /// Text with a newline is never typed: Enter would submit. Err when it
    /// is neither, or typing could not start (nothing typed then).
    /// `front_checked`: the caller made sure the window in front is the
    /// one to type into (file_dialog: a portal dialog an allowed app
    /// opened, itself not on the allowlist), so typing skips the check that
    /// the active window's app is allowed.
    async fn write_text(&self, id: &B::Id, text: &str, front_checked: bool) -> R<Written> {
        match self.b.set_text(id, text).await {
            Ok(true) => Ok(Written::Set),
            Ok(false) => Ok(Written::Refused),
            Err(e) => {
                let editable = self.b.info(id, 0).await.states.contains(&"editable");
                if text.contains('\n') || !editable {
                    return Err(e);
                }
                if self.retype(id, text, front_checked).await {
                    Ok(Written::Typed)
                } else {
                    Err(format!(
                        "{e}, and it would not take the focus to be typed into (not the \
                         field itself? find it by role, e.g. role \"text\"); nothing typed"
                    ))
                }
            }
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
    async fn retype(&self, id: &B::Id, text: &str, front_checked: bool) -> bool {
        if !front_checked && self.require_active_allowed().await.is_err() {
            return false;
        }
        if !matches!(self.b.try_focus(id).await, Ok(Some(_))) && !self.b.is_focused(id).await {
            return false;
        }
        self.b.press_combo("ctrl+a").await.is_ok() && self.b.type_text(text).await.is_ok()
    }

    /// Why a password field is not filled in, and how the user can allow it.
    fn password_refusal(&self, tool: &str, app: &str) -> String {
        refusal(format!(
            "{tool} refused: the target is a password field. Ask the user to type it \
             themselves; only they can allow agents to fill in passwords ('passwords: {app}' \
             in {}, or {}=1)",
            self.allow_file.display(),
            policy::PASSWORDS_ENV
        ))
    }

    async fn get_text(&self, a: &Json) -> R<String> {
        let t = self.read_target(a).await?;
        if t.protected() {
            return Err(t.reply("a password field: its text is never read"));
        }
        let limit = arg_i(a, "limit", GET_TEXT_LIMIT).max(0) as usize;
        let offset = arg_i(a, "offset", 0).max(0) as usize;
        // One character more than asked for tells whether there is more.
        let want = offset.saturating_add(limit).saturating_add(1);
        let mut text = trace::time("full_text", self.b.full_text(&t.id, want)).await?;
        if render::has_embedded_objects(&text) {
            text = self.container_text(&t.id, want).await;
        }
        let more = text.chars().count() > offset + limit;
        let mut text: String = text.chars().skip(offset).take(limit).collect();
        if more {
            let next = offset + limit;
            text.push_str(&format!(
                "\n… cut at character {next}; get_text with offset={next} reads on"
            ));
        }
        // An empty reply reads as "no output" to a client: say it.
        if text.is_empty() && offset == 0 {
            text = "(empty: the element has no text)".into();
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
                                operable: false,
                            });
                        }
                    }
                }
            }
        }
    }

    async fn wait_for(&self, a: &Json) -> R<String> {
        if let Some(window) = a.get("window").and_then(Json::as_str) {
            if a.get("query").is_some() {
                return Err(
                    "pass `query` (an element) or `window` (a window title), not both".into(),
                );
            }
            return self.wait_window(a, window).await;
        }
        let app = req_s(a, "app")?;
        let query = req_s(a, "query")?;
        let role = arg_s(a, "role", "");
        let gone = arg_b(a, "gone", false);
        let state = arg_s(a, "state", "");
        let state = (!state.is_empty())
            .then(|| WaitState::parse(&state))
            .transpose()?;
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
            // With `state`, a match without it counts as no match.
            let has = |info: &Info<B::Id>| state.is_none_or(|s| s.holds(&info.states));
            match found {
                Some((id, mut cache, by_synonym)) if gone != has(&cache[&id]) => {
                    self.b
                        .fill_actions(&mut cache, std::slice::from_ref(&id))
                        .await;
                    let rid = self.add_ref(&id, &cache[&id], &app.to_lowercase()).await;
                    let mut line = render::describe(&cache[&id], rid);
                    let synonym = render::synonym_note(&role, by_synonym, &[&cache[&id].role]);
                    if !synonym.is_empty() {
                        line.push_str(&format!(" ({synonym})"));
                    }
                    if gone {
                        let s = state.map_or("", WaitState::name);
                        return Ok(format!("no longer {s} after {ms} ms{note}: {line}"));
                    }
                    note_step_element(rid, true);
                    return Ok(format!("found after {ms} ms{note}: {line}"));
                }
                None if gone => return Ok(format!("gone after {ms} ms{note}")),
                _ => {}
            }
            if start.elapsed() >= limit {
                let what = match (state, gone) {
                    (None, false) => "to appear".to_string(),
                    (None, true) => "to disappear".to_string(),
                    (Some(s), false) => format!("to be {}", s.name()),
                    (Some(s), true) => format!("to stop being {}", s.name()),
                };
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

    /// `wait_for` a window of an allowed app (`app`, when given) whose
    /// title or app name contains `query` to open, or with `gone` to close.
    /// Re-checks the compositor's window list when an app's windows changed
    /// (AT-SPI), or every WAIT_FALLBACK.
    async fn wait_window(&self, a: &Json, query: &str) -> R<String> {
        let app = arg_s(a, "app", "").to_lowercase();
        let gone = arg_b(a, "gone", false);
        let (limit_ms, note) = wait_ms(a, "timeout_ms", 5000);
        let limit = Duration::from_millis(limit_ms);
        if !app.is_empty() && !self.is_allowed(&app) {
            return Err(self.not_allowed(&app));
        }
        self.b.enable().await;
        let start = Instant::now();
        loop {
            let mark = self.b.change_mark();
            let allowed = self.allowed();
            let list = self.b.windows_quick(query, &allowed).await?;
            // KWin's class may carry a prefix ("org.kde.kate").
            let hit = list.iter().find(|w| w.app.to_lowercase().contains(&app));
            let ms = start.elapsed().as_millis();
            match hit {
                Some(w) if !gone => {
                    return Ok(format!(
                        "window found after {ms} ms{note}: {}",
                        describe_window(w)
                    ));
                }
                None if gone => return Ok(format!("window gone after {ms} ms{note}")),
                _ => {}
            }
            if start.elapsed() >= limit {
                let what = if gone { "to close" } else { "to open" };
                let of = if app.is_empty() {
                    "an allowed app".to_string()
                } else {
                    format!("'{app}'")
                };
                return Err(format!(
                    "timed out after {ms} ms waiting for a window of {of} matching '{query}' {what}{note}"
                ));
            }
            let checked = Instant::now();
            let left = limit.saturating_sub(start.elapsed());
            // Window changes of every app wake this; the app's own changes
            // too (a renamed window).
            let near = if app.is_empty() {
                Vec::new()
            } else {
                self.b.app_windows(&app).await.unwrap_or_default()
            };
            self.b
                .wait_change(&near, mark, left.min(WAIT_FALLBACK))
                .await;
            tokio::time::sleep(WAIT_MIN_GAP.saturating_sub(checked.elapsed()).min(left)).await;
        }
    }

    /// Wait until `app` stopped changing for `quiet_ms`: no accessibility
    /// change events from it (children added or removed, names, text), or,
    /// where the backend hears none, the same fingerprint of its visible
    /// tree.
    async fn wait_idle(&self, a: &Json) -> R<String> {
        let app = req_s(a, "app")?;
        let quiet = Duration::from_millis(arg_i(a, "quiet_ms", 300).clamp(10, MAX_WAIT_MS) as u64);
        let (limit_ms, note) = wait_ms(a, "timeout_ms", 10_000);
        let limit = Duration::from_millis(limit_ms);
        self.b.enable().await;
        let windows = self.app_windows(&app).await?;
        let start = Instant::now();
        let mut since = Instant::now();
        let mut print = None;
        let mut loading = false;
        loop {
            if since.elapsed() >= quiet {
                loading = self.b.loading(&windows).await;
                if loading {
                    // Quiet only because the page waits for the network.
                    since = Instant::now();
                }
            }
            if since.elapsed() >= quiet {
                let ms = start.elapsed().as_millis();
                // Which page it settled on, so no find is needed to check:
                // the app's active window, else its first.
                let key = app.to_lowercase();
                let title = self
                    .b
                    .list_apps()
                    .await
                    .into_iter()
                    .filter(|a| a.name.to_lowercase() == key)
                    .flat_map(|a| a.windows)
                    .min_by_key(|w| !w.active)
                    .map(|w| format!("; window {}", render::py_repr(&w.title)))
                    .unwrap_or_default();
                return Ok(format!(
                    "idle after {ms} ms (no change for {} ms){title}{note}",
                    quiet.as_millis()
                ));
            }
            if start.elapsed() >= limit {
                let ms = start.elapsed().as_millis();
                let why = if loading {
                    "a page is still loading".to_string()
                } else {
                    format!("never quiet for {} ms", quiet.as_millis())
                };
                return Err(format!(
                    "'{app}' still changing after {ms} ms ({why}){note}"
                ));
            }
            let left = limit
                .saturating_sub(start.elapsed())
                .min(quiet.saturating_sub(since.elapsed()));
            if self.b.changes_live() {
                let mark = self.b.change_mark();
                if self.b.wait_change(&windows, mark, left).await {
                    since = Instant::now();
                }
            } else {
                let now = self.content_hash(&windows).await;
                if print.replace(now) != Some(now) {
                    since = Instant::now();
                }
                tokio::time::sleep(left.min(IDLE_POLL)).await;
            }
        }
    }

    /// `content_hash` of `windows` read until it differs from `was` or
    /// `limit` passed (read at least once): the last hash read.
    async fn content_settle(&self, windows: &[B::Id], was: u64, limit: Duration) -> u64 {
        let start = Instant::now();
        loop {
            let now = self.content_hash(windows).await;
            let left = limit.saturating_sub(start.elapsed());
            if now != was || left.is_zero() {
                return now;
            }
            tokio::time::sleep(left.min(CONTENT_POLL)).await;
        }
    }

    /// A hash of what the visible part of `windows` shows.
    async fn content_hash(&self, windows: &[B::Id]) -> u64 {
        let mut cache = Cache::new();
        self.b
            .prefetch_with(
                &mut cache,
                windows.to_vec(),
                &search_fetch(TREE_TEXT_LIMIT, render::FIND_MAX_SEEN),
            )
            .await;
        render::fingerprint(&cache, windows)
    }

    async fn pause(&self, a: &Json) -> R<String> {
        let (ms, note) = wait_ms(a, "ms", 1000);
        tokio::time::sleep(Duration::from_millis(ms)).await;
        Ok(format!("paused {ms} ms{note}"))
    }

    async fn focus(&self, a: &Json) -> R<String> {
        let t = self.target(a).await?;
        let id = &t.id;
        if let Some(method) = trace::time("try_focus", self.b.try_focus(id)).await? {
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
        if clicks > 0 && render::role_in(&t.role, &OPENER_ROLES) {
            let _ = GATE.try_with(|g| g.borrow_mut().opener = true);
        }
        let pre = self.pre_content(Some(&t.id));
        self.with_modifiers(
            &mods,
            self.b.pointer(x as i64, y as i64, &button, clicks, 0),
        )
        .await?;
        // A plain left click: with modifiers it opens a new tab or selects.
        if button == "left" && clicks > 0 && mods.is_empty() {
            self.note_content(pre, &t.id, &t.role, false).await;
        }
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
        let (active, found) = tokio::join!(
            trace::time("require_active_allowed", self.require_active_allowed()),
            async {
                let target = self.target_and_center(a, true, true);
                let (t, center) = trace::time("target", target).await?;
                let center = match center {
                    Some(c) => c,
                    None => trace::time("element_center", self.b.element_center(&t.id)).await,
                };
                Ok::<_, String>((t, center))
            }
        );
        let (t, center) = found?;
        active?;
        Ok((t, center?))
    }

    /// One end of a drag: `<end>_id` (an element, resolved), else
    /// `<end>_x`/`<end>_y`, as screenshot pixels when `image` is true.
    async fn drag_end(&self, a: &Json, end: &str) -> R<DragEnd<B::Id>> {
        if let Some(rid) = a.get(format!("{end}_id")).and_then(Json::as_i64) {
            let (id, app) = self.resolve_with_app(rid).await?;
            self.permit(&app).await?;
            return Ok(DragEnd::Element(id));
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
            // The user took over (or pressed the stop shortcut): let go.
            moved = self.gate(true).await.map_err(|e| {
                format!(
                    "{e} after {} of {DRAG_STEPS} moves from ({x0},{y0}); the button was released",
                    i - 1
                )
            });
            if moved.is_err() {
                break;
            }
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
        // Enter submits, alt+left goes back: the page may change.
        let content = match CONTENT_KEYS.contains(&send.as_str()) && self.b.changes_live() {
            true => self.b.focused().await.map(|f| (f.id, f.info.role)),
            false => None,
        };
        let pre = self.pre_content(content.as_ref().map(|(id, _)| id));
        let times = repeat.clamp(1, 100);
        for i in 0..times {
            if i > 0 {
                self.gate(true)
                    .await
                    .map_err(|e| format!("{e} after {i} of {times} presses of {send}"))?;
            }
            self.b.press_combo(&send).await?;
        }
        if let Some((id, role)) = &content {
            // Enter in a search field submits it.
            self.note_content(pre, id, role, true).await;
        }
        let focus = self.focus_note(false).await;
        Ok(format!("sent {combo} x{repeat} to {app}{focus}"))
    }

    async fn type_text(&self, a: &Json) -> R<String> {
        let text = req_s(a, "text")?;
        let app = self.require_active_allowed().await?;
        if let Some(f) = self.b.focused().await
            && render::is_protected_role(&f.info.role)
            && !self.passwords_allowed(&f.app)
        {
            return Err(self.password_refusal("type_text", &f.app.to_lowercase()));
        }
        // A trailing newline is pressed as Enter: inserted or pasted, it
        // does not submit a single-line field (a browser's address bar, a
        // search box). Newlines inside the text stay text.
        let body = text.trim_end_matches('\n');
        let enters = text.len() - body.len();
        let note = match body.is_empty() {
            true => String::new(),
            false => self.b.type_text(body).await?,
        };
        for i in 0..enters {
            if i > 0 || !body.is_empty() {
                self.gate(true).await?;
            }
            self.b.press_combo("enter").await?;
        }
        let pressed = match enters {
            0 => String::new(),
            1 => ", then pressed Enter".into(),
            n => format!(", then pressed Enter x{n}"),
        };
        let focus = self.focus_note(true).await;
        Ok(format!(
            "typed {} chars into {app}{pressed}{focus}{note}",
            body.chars().count()
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
        let mut focused = self.b.focused().await;
        // Focus events arrive after the input: read it once they did.
        let near: Vec<B::Id> = focused.iter().map(|f| f.id.clone()).collect();
        if self.settle(&near).await {
            focused = self.b.focused().await;
        }
        let Some(Focused { id, info, app }) = focused else {
            return String::new();
        };
        if !self.is_allowed(&app) {
            return String::new();
        }
        let rid = self.add_ref(&id, &info, &app.to_lowercase()).await;
        note_step_element(rid, false);
        let desc = render::describe_focused(&info, rid);
        // In a batch, focus that stayed put is not described again.
        let same = BATCH_FOCUS
            .try_with(|last| last.replace(Some(desc.clone())).as_deref() == Some(desc.as_str()))
            .unwrap_or(false);
        let mut note = if same {
            format!("; focus: [{rid}] (unchanged)")
        } else {
            format!("; focus: {desc}")
        };
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
        match self.b.windows(query, &self.allowed()).await {
            // In front only if it is (one of) the best match: "Claude" with
            // a terminal titled "~ : claude" in front still activates
            // Claude Desktop (backend::window_rank).
            Ok(list) if !list.is_empty() => {
                let ranked: Vec<(u8, bool)> = list
                    .iter()
                    .filter_map(|w| {
                        backend::window_rank(query, &w.title, &[&w.app]).map(|r| (r, w.active))
                    })
                    .collect();
                let best = ranked.iter().map(|(r, _)| *r).min();
                ranked.iter().any(|(r, active)| *active && Some(*r) == best)
            }
            Ok(_) => false,
            Err(_) => {
                app.to_lowercase().contains(&q)
                    || self
                        .active_title()
                        .await
                        .is_some_and(|t| t.to_lowercase().contains(&q))
            }
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
        let path = expand_path(&req_s(a, "path")?, home_dir().as_deref())?;
        check_dialog_path(&path, &action)?;
        let (limit_ms, note) = wait_ms(a, "timeout_ms", 5000);
        self.b.enable().await;
        let d = self.b.active_file_dialog().await?;
        let allowed = self.allowed();
        let owner = file_dialog_owner(&d, &allowed)
            .map_err(|e| refusal(format!("{e} (allowlist: {})", self.allow_file.display())))?;
        self.permit(&owner).await?;
        let mut text = path.display().to_string();
        if cfg!(windows) {
            // `~/x` joined to the profile folder; the dialog's own form.
            text = text.replace('/', "\\");
        }
        // file_dialog found the dialog in front and checked whose it is.
        if matches!(
            self.write_text(&d.field, &text, true).await?,
            Written::Refused
        ) {
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
            let shows = match self.b.file_dialog_prompt(&d.window).await {
                Some(t) => format!(
                    "it shows {} in front (an overwrite confirmation or an error), \
                     left unanswered",
                    render::py_repr(&t)
                ),
                None => "it may ask to confirm overwriting a file, or show an error".into(),
            };
            return Err(format!(
                "wrote {text} and {how}, but the dialog is still open after {ms} ms{note}: {shows}"
            ));
        }
        *self.last_active.lock().await = self.active_title().await;
        Ok(format!(
            "chose {text} in {owner}'s {action} dialog ({how}); dialog closed after {ms} ms{note}"
        ))
    }

    /// Capture the active window. With `region` (pixels of the last
    /// screenshot): capture again and keep only that part, at up to full
    /// resolution, mapped so image_x/image_y refer to the zoomed image.
    async fn screenshot(&self, a: &Json) -> R<Out> {
        let region = region_arg(a)?;
        trace::time("require_active_allowed", self.require_active_allowed()).await?;
        // The zoom's screen area, from the map of the image it was read in.
        let zoom = match region {
            Some(r) => {
                let map = (*self.last_shot.lock().await)
                    .ok_or("region needs a screenshot taken first (its pixels)")?;
                Some((r, map.area_of(r)))
            }
            None => None,
        };
        *self.last_shot.lock().await = None;
        let Shot { path, pixels, area } = trace::time("capture", self.b.screenshot())
            .await
            .ok_or("screenshot failed")?;
        let frac = match (zoom, area) {
            (None, _) => shot::Frac::ALL,
            (Some((_, want)), Some(area)) => region_frac(area, want).ok_or_else(|| {
                let _ = std::fs::remove_file(&path);
                "region is outside the active window now; take a new screenshot".to_string()
            })?,
            (Some(_), None) => {
                let _ = std::fs::remove_file(&path);
                return Err("region: the screen area of the capture is unknown here".into());
            }
        };
        let max = arg_i(a, "max_size", SHOT_MAX_SIZE).clamp(64, 8192) as u32;
        let inline = arg_b(a, "inline", true);
        // Converting, shrinking and encoding take milliseconds: keep other
        // calls moving.
        let file = path.clone();
        let done = trace::time(
            "convert+encode",
            tokio::task::spawn_blocking(move || shot::finish(&file, pixels, max, frac, inline)),
        )
        .await
        .map_err(|e| e.to_string())??;
        // The screen area the image shows: the part of the capture kept.
        let area = area.map(|(x, y, w, h)| {
            let k = done.kept;
            (x + k.x * w, y + k.y * h, k.w * w, k.h * h)
        });
        let map = area.and_then(|a| ShotMap::new(done.size, a));
        *self.last_shot.lock().await = map;
        let (iw, ih) = done.size;
        // The image inline is all the agent sees (it is told not to read
        // the file): no path to read, no file left behind. Only
        // inline=false keeps the file and names it.
        let mut text = if inline {
            let _ = std::fs::remove_file(&path);
            format!("{iw}x{ih} px")
        } else {
            format!("{path}\n{iw}x{ih} px")
        };
        if let (Some((x, y, w, h)), Some(_)) = (area, map) {
            // How to click a point in it is in the server instructions.
            let r = |v: f64| (v * 100.0).round() / 100.0;
            text.push_str(&format!(
                " of screen area at ({},{}), {}x{}",
                r(x),
                r(y),
                r(w),
                r(h)
            ));
        }
        if let Some(((x, y, w, h), _)) = zoom {
            text.push_str(&format!(
                " (zoom of region [{x}, {y}, {w}, {h}] of the previous screenshot; \
                 image_x/image_y now refer to this image)"
            ));
        }
        Ok(Out {
            text,
            images: if inline {
                let (bytes, mime) = done.inline();
                vec![(shot::base64(bytes), mime)]
            } else {
                Vec::new()
            },
        })
    }

    /// Run `steps` in order, with their control flow (`flow`: if, on_fail,
    /// repeat, stop_if), all checked before anything runs. A batch with any
    /// input step holds the input lock for its whole run (its steps are
    /// called as already holding it), so two batches' input never
    /// interleaves. A step's `id`/`*_id` argument "$N" names the element
    /// step N produced (`STEP_ELEMENT`).
    async fn batch(&self, a: &Json) -> R<Out> {
        // Steps share the focus they reported last (BATCH_FOCUS).
        BATCH_FOCUS
            .scope(std::cell::RefCell::new(None), self.batch_steps(a))
            .await
    }

    async fn batch_steps(&self, a: &Json) -> R<Out> {
        let raw = a
            .get("steps")
            .and_then(Json::as_array)
            .ok_or("missing array argument 'steps'")?;
        let steps =
            flow::parse(raw, tools::is_tool).map_err(|e| format!("not run (nothing sent): {e}"))?;
        // A step with `window` activates a window: input too.
        let input = steps.iter().flat_map(flow::Step::calls).any(|c| {
            INPUT_TOOLS.contains(&c.tool.as_str())
                || matches!(window_arg(&c.tool, &c.args), Ok(Some(_)))
        });
        let _input = if input {
            let guard = self.input.lock().await;
            self.start_run();
            Some(guard)
        } else {
            None
        };
        // Input steps a later condition tests settle before their reply.
        let tested: Vec<usize> = steps
            .iter()
            .filter_map(|s| match (&s.cond, &s.body) {
                (_, flow::Body::Stop(c)) | (Some(c), _) => Some(c.step),
                _ => None,
            })
            .collect();
        let mut run = BatchRun::default();
        for (i, step) in steps.iter().enumerate() {
            let n = i + 1;
            let left = steps.len() - n;
            if let Some(c) = &step.cond
                && !c.holds(&run.done)
            {
                run.out.push(format!("[{n}] skipped (if {})", c.describe()));
                run.skip(step.name());
                continue;
            }
            let (ok, hard) = match &step.body {
                flow::Body::Stop(c) => {
                    let stop = c.holds(&run.done);
                    run.skip("stop_if");
                    if stop {
                        let rest = match left {
                            0 => String::new(),
                            k => format!("; {k} step(s) not run"),
                        };
                        run.out.push(format!("[{n}] stop ({}){rest}", c.describe()));
                        break;
                    }
                    continue;
                }
                flow::Body::Tool(call) => {
                    let settle = tested.contains(&n);
                    let r = self.batch_call(call, n, &run.done, input, settle).await;
                    run.deliver(n, &r);
                    let hard = r.refused;
                    let ok = r.result.is_ok();
                    run.print(&n.to_string(), call, &r, step.optional);
                    run.record(call.tool.clone(), ok, r);
                    (ok, hard)
                }
                flow::Body::Repeat { steps, until, max } => {
                    let r = flow::Repeat {
                        steps,
                        until,
                        max: *max,
                    };
                    self.repeat(&mut run, n, r, step.optional, input).await
                }
            };
            if ok {
                continue;
            }
            // Failed: its on_fail steps instead (never after a refusal or
            // the user taking over), else the batch stops unless optional.
            let mut handled = false;
            if !step.on_fail.is_empty() && hard {
                run.out.push(format!("[{n}.on_fail] not run (refused)"));
            }
            if !step.on_fail.is_empty() && !hard {
                handled = true;
                for (k, call) in step.on_fail.iter().enumerate() {
                    let label = match step.on_fail.len() {
                        1 => format!("{n}.on_fail"),
                        _ => format!("{n}.on_fail.{}", k + 1),
                    };
                    let r = self.batch_call(call, n, &run.done, input, false).await;
                    run.deliver(n, &r);
                    run.print(&label, call, &r, call.optional);
                    if r.result.is_err() && !call.optional {
                        handled = false;
                        break;
                    }
                }
            }
            if !handled && !step.optional {
                // What reached the desktop, so a retry repeats none of it.
                if input || left > 0 {
                    let line = delivery::stopped(&run.sent, &run.maybe, n, steps.len(), input);
                    run.out.push(line);
                }
                return Err(format!("{}{}", run.out.join("\n"), self.where_note().await));
            }
        }
        let mut text = run.out.join("\n");
        // A step failed or found nothing: say what is on screen once, unless
        // the batch ends with a look at it.
        let failed = run.failed || run.out.iter().any(|l| l.ends_with(": no matches"));
        // (Run to the end, the last step not skipped.)
        let looked = run.done.len() == steps.len()
            && steps
                .last()
                .is_some_and(|s| ["screenshot", "tree"].contains(&s.name()))
            && run
                .done
                .last()
                .is_some_and(|r| !matches!(r.outcome, flow::Outcome::Skipped));
        if failed && !looked {
            text.push_str(&self.where_note().await);
        }
        Ok(Out {
            text,
            images: run.images,
        })
    }

    /// One tool call of batch step `n`: its `"$N"` resolved, then run as a
    /// call of its own, noting the element it produced and whether it was
    /// refused. `settle`: its reply is tested (`call_inner`).
    async fn batch_call(
        &self,
        call: &flow::Call,
        n: usize,
        done: &[flow::Record],
        held: bool,
        settle: bool,
    ) -> StepResult {
        let mut args = call.args.clone();
        if let Err(e) = flow::resolve_refs(&mut args, n, done) {
            return StepResult {
                result: Err(e),
                element: None,
                refused: false,
                wrote: false,
            };
        }
        let (((result, refused), element), wrote) =
            delivery::track(STEP_ELEMENT.scope(std::cell::Cell::new(None), async {
                let r = with_refusals(self.call_inner(&call.tool, &args, held, settle)).await;
                (r, STEP_ELEMENT.with(|e| e.get()))
            }))
            .await;
        StepResult {
            result,
            element,
            refused,
            wrote,
        }
    }

    /// Run a repeat (batch step `n`) until `until` holds on its last step,
    /// at most `max` runs and REPEAT_TIME. Prints its header and its last
    /// run's lines and records it. Returns (succeeded, failed by a refusal
    /// or the user taking over).
    async fn repeat(
        &self,
        run: &mut BatchRun,
        n: usize,
        flow::Repeat { steps, until, max }: flow::Repeat<'_>,
        optional: bool,
        held: bool,
    ) -> (bool, bool) {
        let start = Instant::now();
        let mut runs = 0;
        loop {
            runs += 1;
            let mut lines = BatchRun::default();
            let mut last = None;
            let mut broke = None;
            for (k, call) in steps.iter().enumerate() {
                // Its until (or a later step) tests what the input did.
                let r = self.batch_call(call, n, &run.done, held, true).await;
                run.deliver(n, &r);
                let label = format!("{n}.{}", k + 1);
                lines.print(&label, call, &r, call.optional);
                let hard = r.refused;
                if r.result.is_err() && (!call.optional || hard) {
                    broke = Some((label, hard));
                }
                last = Some(r);
                if broke.is_some() {
                    break;
                }
            }
            let last = last.expect("a repeat has steps");
            let text = match &last.result {
                Ok(o) => o.text.clone(),
                Err(e) => e.clone(),
            };
            let in_runs = match runs {
                1 => "1 run".to_string(),
                k => format!("{k} runs"),
            };
            let (ok, head, hard) = match broke {
                Some((label, hard)) => (false, format!("step {label} failed in run {runs}"), hard),
                None if until.holds(last.result.is_ok(), last.element, &text) => {
                    (true, format!("until met in run {runs}"), false)
                }
                None if runs >= max => (false, format!("until not met in {in_runs}"), false),
                None if start.elapsed() >= REPEAT_TIME => (
                    false,
                    format!("until not met in {in_runs} (time limit)"),
                    false,
                ),
                None => continue,
            };
            let head = match (ok, optional) {
                (true, _) => format!("[{n}] repeat: {head}"),
                (false, false) => format!("[{n}] repeat failed: {head}"),
                (false, true) => format!("[{n}] repeat failed (optional): {head}"),
            };
            run.out.push(head);
            run.out.append(&mut lines.out);
            run.images.append(&mut lines.images);
            run.failed |= lines.failed || !ok;
            run.done.push(flow::Record {
                tool: "repeat".into(),
                outcome: flow::Outcome::Ran {
                    ok,
                    element: last.element,
                    text,
                },
            });
            return (ok, hard);
        }
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
            let (result, refused) =
                with_refusals(trace::time(name, self.call_inner(name, a, false, false))).await;
            let result = match result {
                Err(e) if !self.held_names().is_empty() => {
                    let (names, released) = self.release_all().await;
                    let failed = match released {
                        Ok(()) => String::new(),
                        Err(r) => format!(" (release failed: {r})"),
                    };
                    Err(format!("{e}; released {}{failed}", names.join(", ")))
                }
                r => r,
            };
            // What is on screen when it failed or found nothing, so the next
            // call needs no screenshot to see why.
            match result {
                // Not after a refusal: that is about the allowlist, a
                // confirmation or the user taking over, not the screen.
                Err(e) if CONTEXT_TOOLS.contains(&name) && !refused => {
                    Err(format!("{e}{}", self.where_note().await))
                }
                Ok(mut o) if name == "find" && o.text.starts_with("no matches") => {
                    o.text.push_str(&self.where_note().await);
                    Ok(o)
                }
                r => r,
            }
        })
    }

    /// `call`, for a caller that may already hold the input lock (`held`: a
    /// batch with input steps), which then is not taken again. `settle`: a
    /// batch tests its reply (a repeat's until, a condition), so an input
    /// tool without `observe` lets the app react first (`Server::settle`):
    /// its focus note and the next step then see the input's effect.
    fn call_inner<'a>(
        &'a self,
        name: &'a str,
        a: &'a Json,
        held: bool,
        settle: bool,
    ) -> Pin<Box<dyn Future<Output = R<Out>> + 'a>> {
        let gate = Gate {
            tool: name.to_string(),
            args: a.clone(),
            confirmed: false,
            settle: None,
            opener: false,
            quiet: false,
            reported: Vec::new(),
            before: String::new(),
        };
        Box::pin(GATE.scope(std::cell::RefCell::new(gate), async move {
            let (result, wrote) = delivery::track(self.call_body(name, a, held, settle)).await;
            if !DELIVERY_TOOLS.contains(&name) {
                return result;
            }
            match result {
                Ok(mut o) => {
                    delivery::reply(&mut o.text, wrote);
                    Ok(o)
                }
                Err(e) => Err(delivery::error(e, wrote)),
            }
        }))
    }

    /// `call_inner`'s work, inside its GATE scope.
    async fn call_body(&self, name: &str, a: &Json, held: bool, settle: bool) -> R<Out> {
        let window = window_arg(name, a)?;
        // Tools that reach no app through `permit` are confirmed here,
        // by their `app` argument if any.
        if NO_PERMIT_TOOLS.contains(&name) {
            let app = a.get("app").and_then(Json::as_str);
            if self.policy().needs_confirm(name, app) {
                self.confirm(name, app, a).await?;
            }
        }
        let observe = observe_arg(name, a)?;
        // Reads kept from before are not reused once this may have
        // changed what an app shows, nor while it runs.
        let _changes = (!READ_ONLY_TOOLS.contains(&name) || window.is_some())
            .then(|| InputRun::start(&self.reads));
        // What the input sets for the content wait (input tools run one
        // at a time: the input lock is held).
        let input_tool = INPUT_TOOLS.contains(&name);
        if input_tool {
            self.content
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
        }
        // A screenshot sends no input, but activating its window does. A
        // batch that holds the input lock already runs its steps `held`.
        let _input = if (INPUT_TOOLS.contains(&name) || window.is_some()) && !held {
            let guard = trace::time("input lock", self.input.lock()).await;
            self.start_run();
            Some(guard)
        } else {
            None
        };
        let gated = GATED_TOOLS.contains(&name) || window.is_some();
        // Only an activate_window call of its own resumes after the stop
        // shortcut, never a step of a batch that was running then.
        let resumed = name == "activate_window"
            && !held
            && self.takeover.as_ref().is_some_and(|t| t.resume());
        let waited = match gated {
            true => trace::time("gate", self.gate_wait(held)).await?,
            false => None,
        };
        let _ours = match &self.takeover {
            Some(t) if gated => Some(t.ours()),
            _ => None,
        };
        if let Some(label) = indicator_label(name, a) {
            trace::time("indicate", self.b.indicate(&label)).await;
        }
        let activated = match &window {
            Some(q) => {
                // Bringing it to the front again is harmless: not
                // counted as sent.
                let ensured = delivery::untracked(self.ensure_window(q));
                trace::time("ensure_window", ensured).await?
            }
            None => None,
        };
        if input_tool {
            // An activation's own events would count as the app's.
            let base = activated.is_none().then(|| self.b.change_mark());
            *self.content_base.lock().unwrap_or_else(|e| e.into_inner()) = base;
        }
        // Taken once the window is in front, before the input.
        let before = match observe {
            Some(_) => Some((
                self.b.change_mark(),
                trace::time(
                    "fingerprint before",
                    self.input_fingerprint(self.input_app(a).await, None),
                )
                .await,
            )),
            None => None,
        };
        // What the app the focus is in reports from now on ends the
        // settle; `focus_note` waits for it, or else the end of the call.
        let settle_near = match observe {
            None if settle && WINDOW_NOTE_TOOLS.contains(&name) => {
                let mark = self.b.change_mark();
                let near: Vec<B::Id> = self.b.focused().await.map(|f| f.id).into_iter().collect();
                let _ = GATE.try_with(|g| g.borrow_mut().settle = Some(mark));
                Some(near)
            }
            _ => None,
        };
        // A window that came to the front since the last input (a dialog
        // that opened after its reply): said before this input's result.
        let pre_note = match observe {
            None if WINDOW_NOTE_TOOLS.contains(&name) && window.is_none() => {
                self.window_change_note().await
            }
            _ => String::new(),
        };
        // Before the input: what shows after it was opened by it.
        let shown_mark =
            (observe.is_none() && SURFACE_TOOLS.contains(&name)).then(|| self.b.change_mark());
        // What showed after the last input's reply, said with this
        // one's "opened:" lines (`surfaces_note`).
        if observe.is_some() || shown_mark.is_some() {
            let before = trace::time("opened before", self.opened_before()).await;
            let _ = GATE.try_with(|g| g.borrow_mut().before = before);
        }
        let result = match name {
            "apps" => self.apps().await.map(Out::from),
            "tree" => self.tree(a).await,
            "find" => self.find(a).await.map(Out::from),
            "act" => self.act(a).await.map(Out::from),
            "set_text" => self.set_text(a).await.map(Out::from),
            "get_text" => self.get_text(a).await.map(Out::from),
            "wait_for" => self.wait_for(a).await.map(Out::from),
            "wait_idle" => self.wait_idle(a).await.map(Out::from),
            "pause" => self.pause(a).await.map(Out::from),
            "focus" => self.focus(a).await.map(Out::from),
            "click" => self.click(a).await.map(Out::from),
            "drag" => self.drag(a).await.map(Out::from),
            "windows" => self.windows(a).await.map(Out::from),
            "move_window" => self.move_window(a).await.map(Out::from),
            "close_window" => self.close_window(a).await.map(Out::from),
            "menu" => self.menu(a).await.map(Out::from),
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
        // A web page the input changes in place: wait for its new
        // content, so the next read does not see the old one.
        let content = match (&result, input_tool) {
            (Ok(_), true) => self
                .content
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take(),
            _ => None,
        };
        let limit = match a.get("settle_ms").is_some_and(|v| !v.is_null()) {
            true => arg_i(a, "settle_ms", CONTENT_MS),
            false => CONTENT_MS,
        }
        .clamp(0, MAX_SETTLE_MS) as u64;
        let content = content.filter(|_| limit > 0);
        let mut result = result;
        if let (Ok(o), Some(c)) = (&mut result, &content) {
            let limit = Duration::from_millis(limit);
            let note = trace::time("content", self.content_wait(c, limit)).await;
            o.text.push_str(&note);
            // An accessibility action is no user gesture to a browser: a
            // file input's picker (and some popups) open only for a real
            // click. Nothing changed: say so instead of leaving a guess.
            if name == "act" && note.starts_with("; no page change seen") {
                o.text.push_str(
                    " (a browser opens file pickers and some popups only for a real click: \
                     click it)",
                );
            }
            // Waited already: no settle on top.
            let _ = GATE.try_with(|g| g.borrow_mut().settle = None);
        }
        if let (Ok(_), Some(near)) = (&result, &settle_near) {
            trace::time("settle", self.settle(near)).await;
        }
        let content_id = content.map(|c| c.id);
        let result = match (result, observe, before) {
            (Ok(mut o), Some(observe), Some((mark, before))) => {
                // A verified act that found it so already sent nothing.
                let sent = !o.text.contains("nothing sent");
                let seen = trace::time(
                    "after_input",
                    self.after_input(name, a, observe, (mark, before), sent, content_id),
                )
                .await;
                o.text.push_str(&seen.text);
                o.images.extend(seen.images);
                Ok(o)
            }
            // Without `observe`, still say when another window came to
            // the front (a dialog, a new browser window).
            (Ok(mut o), None, _) if WINDOW_NOTE_TOOLS.contains(&name) => {
                let opened = match shown_mark {
                    Some(mark) => trace::time("surfaces", self.opened_note(name, a, mark)).await,
                    None => String::new(),
                };
                if let Some(before) = pre_note.strip_prefix("; active window now ") {
                    o.text
                        .push_str(&format!("; before it, {before} had come to the front"));
                }
                o.text.push_str(&self.window_change_note().await);
                o.text.push_str(&opened);
                Ok(o)
            }
            (result, _, _) => result,
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
        let result = match activated {
            Some(title) => {
                let done = format!("activated {}; ", render::short_title(&title));
                result
                    .map(|mut o| {
                        o.text.insert_str(0, &done);
                        o
                    })
                    .map_err(|e| format!("{done}{e}"))
            }
            None => result,
        };
        let result = match waited {
            Some(w) => {
                let done = format!("waited {} ms for the user's input to stop; ", w.as_millis());
                result
                    .map(|mut o| {
                        o.text.insert_str(0, &done);
                        o
                    })
                    .map_err(|e| format!("{done}{e}"))
            }
            None => result,
        };
        match resumed {
            true => result.map(|mut o| {
                o.text.insert_str(0, "resumed after the user's stop; ");
                o
            }),
            false => result,
        }
    }
}

/// How `Server::write_text` got the text in.
enum Written {
    /// Through the text interface.
    Set,
    /// Typed: the element had no text interface.
    Typed,
    /// The text interface refused it.
    Refused,
}

/// The reply when a written field holds something else than `text`.
fn written_mismatch(now: &str, text: &str, how: &str) -> String {
    let shown: String = now.chars().take(120).collect();
    let more = if now.chars().count() > 120 { "…" } else { "" };
    format!(
        "written, but the field now holds {shown:?}{more} ({} chars, {} asked{how}); \
         it may limit or reformat input",
        now.chars().count(),
        text.chars().count()
    )
}

/// Longest wait for a parent menu `open_parents` opened to show.
const PARENT_MENU_WAIT: Duration = Duration::from_millis(500);

/// Tools whose failure reply ends with what is on screen (`where_note`).
const CONTEXT_TOOLS: [&str; 14] = [
    "act",
    "click",
    "focus",
    "get_text",
    "set_text",
    "scroll",
    "drag",
    "wait_for",
    "wait_idle",
    "menu",
    "key",
    "type_text",
    "mouse",
    "find",
];

/// Input tools that, without `observe`, note when another window came to
/// the front. activate_window says so itself; act, menu and close_window
/// report the effect line by default.
const WINDOW_NOTE_TOOLS: [&str; 9] = [
    "click",
    "focus",
    "key",
    "type_text",
    "mouse",
    "set_text",
    "scroll",
    "drag",
    "file_dialog",
];

/// Input tools that, without `observe`, list the menus, popups and dialogs
/// the input opened (`Server::opened_note`); act, menu and close_window do
/// it with their effect line.
const SURFACE_TOOLS: [&str; 4] = ["click", "mouse", "key", "type_text"];

/// Roles of elements a click on likely opens a menu, list or dialog with.
const OPENER_ROLES: [&str; 4] = ["combo box", "push button menu", "menu", "menu item"];

/// Roles of a button that opens a menu when it also has an "expand"
/// action (UIA's drop-down and split buttons).
const DROP_DOWN_ROLES: [&str; 3] = ["button", "push button", "split button"];

/// Whether an input is likely to open a menu, list or dialog, so its reply
/// waits (up to SETTLE_MS) for one to show: a right click, the menu key,
/// F10 or alt + a letter (menus), alt+down (a combo box's list), and the
/// shortcuts that open, save, print or close documents (dialogs).
fn opens_surface(tool: &str, a: &Json) -> bool {
    match tool {
        "click" | "mouse" => {
            arg_s(a, "button", "left").eq_ignore_ascii_case("right") && arg_i(a, "clicks", 1) > 0
        }
        "key" => {
            let keys = chord_keys(&arg_s(a, "combo", ""));
            let k: Vec<&str> = keys.iter().map(String::as_str).collect();
            match k.as_slice() {
                ["menu"] | ["f10"] | ["shift", "f10"] | ["alt", "down"] | ["alt", "f4"] => true,
                ["alt", c] => c.chars().count() == 1 && c.chars().all(char::is_alphanumeric),
                ["ctrl", c] => ["o", "s", "w", "q", "p"].contains(c),
                ["ctrl", "shift", "s"] | ["shift", "ctrl", "s"] => true,
                _ => false,
            }
        }
        _ => false,
    }
}

/// How a surface of `kind` is read: its showing nodes, a dialog's not
/// below its views, documents and tool bars (`render::DIALOG_STOP`).
fn surface_fetch(kind: render::Surface) -> Fetch {
    Fetch {
        only_showing: true,
        text_limit: 0,
        limit: SURFACE_NODES,
        actions: false,
        stop_roles: match kind {
            render::Surface::Dialog => &render::DIALOG_STOP,
            _ => &[],
        },
        offscreen: false,
    }
}

/// At most SURFACE_CANDIDATES of the elements shown: the oldest and the
/// newest half.
fn cap_candidates<T>(mut shown: Vec<T>) -> Vec<T> {
    if shown.len() > SURFACE_CANDIDATES {
        let half = SURFACE_CANDIDATES / 2;
        shown.drain(half..shown.len() - half);
    }
    shown
}

/// A backend id as a number, to remember which surfaces a reply listed
/// (`Gate::reported`, `Server::seen_surfaces`) without reading them.
fn id_hash<I: std::hash::Hash>(id: &I) -> u64 {
    use std::hash::{BuildHasher, BuildHasherDefault, DefaultHasher};
    BuildHasherDefault::<DefaultHasher>::default().hash_one(id)
}

/// Most surfaces one reply lists.
const SURFACE_MAX: usize = 3;
/// Shown elements whose role is looked at: the oldest and the newest half
/// (Qt says a dialog's widgets show before the dialog itself).
const SURFACE_CANDIDATES: usize = 48;
/// Nodes read below a surface per round, and rounds.
const SURFACE_NODES: usize = 300;
const SURFACE_ROUNDS: usize = 3;

/// Tools that never call `Server::permit` (they reach no app, or only read
/// with a tier every allowed app has); `call_inner` handles their
/// confirmation.
const NO_PERMIT_TOOLS: [&str; 11] = [
    "apps",
    "windows",
    "doctor",
    "tree",
    "find",
    "wait_for",
    "wait_idle",
    "cursor_position",
    "pause",
    "activate_window",
    "batch",
];

// ---------------------------------------------------------------- MCP

const PROTOCOL_VERSIONS: [&str; 4] = ["2025-06-18", "2025-03-26", "2024-11-05", "2025-11-25"];

async fn handle<B: Backend>(server: &Server<B>, msg: &Json) -> Option<Json> {
    let id = msg.get("id")?.clone();
    let method = msg.get("method").and_then(Json::as_str).unwrap_or("");
    let params = msg.get("params").cloned().unwrap_or(Json::Null);
    let result =
        match method {
            "initialize" => {
                let asked = params["protocolVersion"].as_str().unwrap_or("");
                server.link.borrow_mut().elicitation = params
                    .pointer("/capabilities/elicitation")
                    .is_some_and(Json::is_object);
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
                content.extend(out.images.into_iter().map(
                    |(data, mime)| json!({ "type": "image", "data": data, "mimeType": mime }),
                ));
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
    server.b.shutdown();
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
    server.link.borrow_mut().out = Some(tx.clone());
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
        // The client's response to a request the server sent (a running
        // call waits for it).
        if msg.get("method").is_none()
            && (msg.get("result").is_some() || msg.get("error").is_some())
        {
            server.deliver(msg);
            continue;
        }
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
    // No answer can come any more: requests still waiting fail now, so the
    // calls behind them finish, and the writer ends once they did.
    {
        let mut link = server.link.borrow_mut();
        link.out = None;
        link.pending.clear();
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
    if matches!(std::env::args().nth(1).as_deref(), Some("--version" | "-V")) {
        println!("desktop-mcp {}", env!("CARGO_PKG_VERSION"));
        return;
    }
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
    // Exit now: after a signal, tokio's stdin reader is still blocked in a
    // read on a pipe the client keeps open, and dropping the runtime would
    // wait for it forever.
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    use backend::{AppEntry, Info, WindowEntry};
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
        /// Fed by the tests: user input and the stop shortcut.
        takeover: std::sync::Arc<takeover::Takeover>,
        /// Elements other than `focus`, by id. An action on an empty
        /// "menu" fills it with entry 99 "Opened", as Firefox shows a
        /// submenu's entries only once it is open.
        nodes: Rc<RefCell<HashMap<u32, Info<u32>>>>,
        /// What kept reads see: see `FakeReads`.
        reads: Rc<FakeReads>,
        /// Scrolls left until "report.pdf" (REVEALED) shows under the
        /// focus element, when Some.
        reveal: Rc<std::cell::Cell<Option<u32>>>,
        /// "down" moves the focus this many ms after the key is released
        /// (as focus events arrive late), when not 0.
        lag: Rc<std::cell::Cell<u64>>,
        /// Windows of the app after `focus`, by id in `nodes`.
        more_windows: Rc<RefCell<Vec<u32>>>,
        /// Elements without an on-screen area.
        no_area: Rc<RefCell<Vec<u32>>>,
        /// Elements' windows are not in front: as on KDE and Hyprland,
        /// `element_center` then fails for every element with an area.
        background: Rc<std::cell::Cell<bool>>,
        /// What inputs show: see `FakeShown`.
        shown: Rc<FakeShown>,
        /// What `act`'s verified actions meet: see `FakeSemantic`.
        sem: Rc<FakeSemantic>,
    }

    /// How the fake answers `act`'s verified actions. Actions flip a check
    /// box's `checked` and an expandable (operable) element's `expanded`,
    /// unless its name has "stuck"; an entry's select or click action picks
    /// it (and its combo box shows it).
    #[derive(Default)]
    struct FakeSemantic {
        /// `select_item`: None: no Selection interface; Some(true): picks
        /// the entry; Some(false): only marks it selected, as Qt's hidden
        /// combo box list does.
        selection: std::cell::Cell<Option<bool>>,
        /// `scroll_into_view`: None: no ScrollTo; Some(true) shows the
        /// element; Some(false): declined.
        scroll_to: std::cell::Cell<Option<bool>>,
        /// The wheel shows element `.0` after `.1` turns.
        wheel: std::cell::Cell<Option<(u32, u32)>>,
        /// `try_focus` works.
        focusable: std::cell::Cell<bool>,
        /// `may_open_menu` says no (Firefox).
        no_menu_open: std::cell::Cell<bool>,
        /// `toolkit_of` for every element ("" by default).
        toolkit: RefCell<String>,
        /// `select_item` adds to the selection, as AT-SPI's SelectChild
        /// does in a multi-selection list that is not cleared first.
        adds: std::cell::Cell<bool>,
        /// The user types once this many more key presses went out.
        user_after_presses: std::cell::Cell<Option<usize>>,
    }

    /// The fake's node `id` (the focus element too), changed by `f`.
    fn fake_edit(
        focus: &RefCell<Info<u32>>,
        nodes: &RefCell<HashMap<u32, Info<u32>>>,
        id: u32,
        f: impl FnOnce(&mut Info<u32>),
    ) {
        if id == FAKE_FOCUS {
            f(&mut focus.borrow_mut());
        } else if let Some(n) = nodes.borrow_mut().get_mut(&id) {
            f(n);
        }
    }

    /// Toggle `state` in `states`.
    fn flip(states: &mut Vec<&'static str>, state: &'static str) {
        match states.iter().position(|s| *s == state) {
            Some(i) => {
                states.remove(i);
            }
            None => states.push(state),
        }
    }

    /// Pick entry `item` of a fake list: it alone is selected, and with
    /// `commit` the combo box holding the list is named after it.
    fn fake_pick(
        focus: &RefCell<Info<u32>>,
        nodes: &RefCell<HashMap<u32, Info<u32>>>,
        item: u32,
        commit: bool,
    ) {
        let (list, label) = {
            let n = nodes.borrow();
            let Some(list) = n
                .iter()
                .find(|(_, i)| i.children.contains(&item))
                .map(|(k, _)| *k)
            else {
                return;
            };
            (list, n[&item].name.clone())
        };
        let siblings = nodes.borrow()[&list].children.clone();
        for sib in siblings {
            fake_edit(focus, nodes, sib, |i| {
                i.states.retain(|s| *s != "selected");
                if sib == item {
                    i.states.push("selected");
                }
            });
        }
        if commit {
            let combo = if focus.borrow().children.contains(&list) {
                Some(FAKE_FOCUS)
            } else {
                nodes
                    .borrow()
                    .iter()
                    .find(|(_, i)| i.children.contains(&list))
                    .map(|(k, _)| *k)
            };
            if let Some(c) = combo {
                fake_edit(focus, nodes, c, |i| {
                    if i.role == "combo box" && i.name != "Encoding" {
                        i.name = label.clone();
                    }
                });
            }
        }
    }

    /// Elements the fake app shows (a menu or dialog opening) and the
    /// events it sends for them.
    #[derive(Default)]
    struct FakeShown {
        /// `shown_since` hears of them (else None: callers scan).
        live: std::cell::Cell<bool>,
        /// The change sequence: `change_mark`.
        seq: std::cell::Cell<u64>,
        /// (sequence number, element) per shown event.
        events: RefCell<Vec<(u64, u32)>>,
        /// Elements the next input (click, key, action) shows.
        next: RefCell<Vec<u32>>,
        /// Parent of an element, by id.
        parents: RefCell<HashMap<u32, u32>>,
        /// `dialogs_of` names the dialog `close_window` opens.
        names_dialogs: std::cell::Cell<bool>,
        /// `close_window` leaves the window open and opens this dialog
        /// window; without it, the fake has no window control.
        close_opens: std::cell::Cell<Option<u32>>,
        /// `identity` calls so far.
        identities: std::cell::Cell<u32>,
        /// Elements of another app (another `app_key`) whose name cannot
        /// be read (`app_name` None).
        nameless: RefCell<Vec<u32>>,
        /// Roles do not name surfaces (as UIA's): `names_surfaces` false.
        uia_roles: std::cell::Cell<bool>,
        /// Dialogs `fills_file_dialog` takes for file dialogs.
        file_dialogs: RefCell<Vec<u32>>,
        /// Submenus open apart from their entry, as UIA's XAML flyouts:
        /// expanding entry k shows menu `flyouts[k]` in the pane
        /// FAKE_POPUP_HOST; `open_menus` finds them.
        flyouts: RefCell<HashMap<u32, u32>>,
    }

    /// Where the fake shows a flyout (`FakeShown::flyouts`).
    const FAKE_POPUP_HOST: u32 = 40;

    impl FakeShown {
        /// The elements an input shows appear, with an event each.
        fn input(&self) {
            for id in self.next.take() {
                self.seq.set(self.seq.get() + 1);
                self.events.borrow_mut().push((self.seq.get(), id));
            }
        }
    }

    /// A change an input makes: after this many ms, renaming an element.
    type FakeChange = (u64, Option<(u32, String)>);

    /// The fake's change events, for kept reads: off unless a test turns
    /// them on.
    #[derive(Default)]
    struct FakeReads {
        /// Change events are listened to and the app sends them.
        live: std::cell::Cell<bool>,
        /// Bumped by a test for each change the app reports.
        changes: std::cell::Cell<u64>,
        /// A document of the app is loading.
        loading: std::cell::Cell<bool>,
        /// Elements of kept reads are gone (`still_there` false).
        gone: std::cell::Cell<bool>,
        /// `prefetch` calls so far: reads that were not reused.
        prefetches: std::cell::Cell<u32>,
        /// `still_there` calls so far.
        checks: std::cell::Cell<u32>,
        /// The page the app's window shows (`page`), when Some.
        page: RefCell<Option<backend::Page>>,
        /// The page the next action leaves shown (a link followed).
        page_next: RefCell<Option<backend::Page>>,
        /// `changed_since` says the app changed when asked about this
        /// element of it (the app's windows), when Some.
        events_on: std::cell::Cell<Option<u32>>,
        /// The effect line compares the app's content (`compares_content`).
        content: std::cell::Cell<bool>,
        /// `wait_change` hears of changes (`changes_live`): each bumps the
        /// change mark (`FakeShown::seq`) and `changes`.
        events: std::cell::Cell<bool>,
        /// Changes the next input (action, click, key) makes, each this
        /// many ms after it, renaming an element when Some.
        next: RefCell<Vec<FakeChange>>,
        /// The app changes on each read of an element's actions, as a live
        /// page does between an input call's start and its input.
        noise: std::cell::Cell<bool>,
    }

    /// Windows on the fake desktop: (app, title).
    const FAKE_WINDOWS: [(&str, &str); 3] = [
        ("konsole", "~ : bash — Konsole"),
        ("fake", "notes.txt — Fake"),
        ("fake", "todo.txt — Fake"),
    ];

    impl FakeBackend {
        /// The changes an input makes (`FakeReads::next`), each later.
        fn content_input(&self) {
            for (ms, name) in self.reads.next.take() {
                let (reads, shown) = (self.reads.clone(), self.shown.clone());
                let (focus, nodes) = (self.focus.clone(), self.nodes.clone());
                tokio::task::spawn_local(async move {
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                    if let Some((id, n)) = name {
                        fake_edit(&focus, &nodes, id, |i| i.name = n);
                    }
                    shown.seq.set(shown.seq.get() + 1);
                    reads.changes.set(reads.changes.get() + 1);
                });
            }
        }
    }

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
        fn confirm_example(&self) -> &'static str {
            "key:fake"
        }
        /// As a Store app hosted by the allowed "fake".
        fn app_aliases(&self) -> Vec<(String, &'static str)> {
            vec![("hosted".into(), "fake")]
        }
        fn boring_roles(&self) -> &'static [&'static str] {
            &[]
        }
        async fn enable(&self) {}
        fn takeover(&self) -> Option<std::sync::Arc<takeover::Takeover>> {
            Some(self.takeover.clone())
        }
        async fn indicate(&self, label: &str) {
            if label.starts_with("paused") || label.starts_with("stopped") {
                self.keys.borrow_mut().push(format!("indicate {label}"));
            }
        }
        async fn list_apps(&self) -> Vec<AppEntry> {
            // Only `fake`'s windows: the others belong to apps the fake
            // accessibility tree does not have.
            let (app, title) = self.active.borrow().clone();
            let windows = FAKE_WINDOWS
                .iter()
                .filter(|(a, _)| *a == "fake")
                .map(|(_, t)| WindowEntry {
                    kind: "frame".into(),
                    title: t.to_string(),
                    active: app == "fake" && *t == title,
                })
                .collect();
            vec![AppEntry {
                name: "fake".into(),
                windows,
            }]
        }
        async fn app_windows(&self, app: &str) -> Option<Vec<u32>> {
            (app == "fake").then(|| {
                let mut windows = vec![FAKE_FOCUS];
                windows.extend(self.more_windows.borrow().iter());
                windows
            })
        }
        async fn active_app(&self) -> String {
            self.active.borrow().0.clone()
        }
        async fn info(&self, id: &u32, _: usize) -> Info<u32> {
            match *id {
                FAKE_FOCUS => self.focus.borrow().clone(),
                _ => self.nodes.borrow().get(id).cloned().unwrap_or_default(),
            }
        }
        async fn identity(&self, id: &u32) -> (String, String) {
            let s = &self.shown;
            s.identities.set(s.identities.get() + 1);
            let info = self.info(id, 0).await;
            (info.role, info.name)
        }
        fn change_mark(&self) -> u64 {
            self.shown.seq.get()
        }
        fn shown_since(&self, mark: u64) -> Option<Vec<u32>> {
            let s = &self.shown;
            s.live.get().then(|| {
                let mut out: Vec<u32> = Vec::new();
                for (n, id) in s.events.borrow().iter() {
                    if *n > mark && !out.contains(id) {
                        out.push(*id);
                    }
                }
                out
            })
        }
        async fn parent(&self, id: &u32) -> Option<u32> {
            self.shown.parents.borrow().get(id).copied()
        }
        fn names_surfaces(&self) -> bool {
            !self.shown.uia_roles.get()
        }
        fn names_menus(&self) -> bool {
            self.names_surfaces() || !self.shown.flyouts.borrow().is_empty()
        }
        async fn open_menus(&self, windows: &[u32]) -> Option<Vec<u32>> {
            if self.shown.flyouts.borrow().is_empty() {
                return None;
            }
            let mut out = Vec::new();
            let mut stack: Vec<u32> = Vec::new();
            for w in windows {
                stack.extend(self.info(w, 0).await.children);
            }
            while let Some(id) = stack.pop() {
                let info = self.info(&id, 0).await;
                if !info.showing {
                    continue;
                }
                if info.role == "menu" {
                    out.push(id);
                }
                stack.extend(info.children);
            }
            Some(out)
        }
        async fn fills_file_dialog(&self, window: &u32) -> bool {
            self.shown.file_dialogs.borrow().contains(window)
        }
        async fn app_name(&self, id: &u32) -> Option<String> {
            (!self.shown.nameless.borrow().contains(id)).then(|| "fake".into())
        }
        async fn dialogs_of(&self, _w: &WindowGeo) -> Vec<u32> {
            // The dialog `close_window` opened, when set to name it.
            match self.shown.names_dialogs.get() {
                true => self.shown.close_opens.get().into_iter().collect(),
                false => Vec::new(),
            }
        }
        async fn close_window(&self, query: &str, allowed: &[String]) -> R<WindowGeo> {
            let dialog = self
                .shown
                .close_opens
                .get()
                .ok_or_else(|| delivery::nothing_sent(backend::NO_WINDOW_CONTROL))?;
            let list = self.windows(query, allowed).await?;
            let [w] = list.as_slice() else {
                return Err(backend::not_one_window(query, &list));
            };
            self.more_windows.borrow_mut().push(dialog);
            self.shown.next.borrow_mut().push(dialog);
            self.shown.input();
            Ok(w.clone())
        }
        fn read_mark(&self) -> Option<u64> {
            self.reads.live.get().then(|| self.reads.changes.get())
        }
        fn changes_live(&self) -> bool {
            self.reads.events.get()
        }
        fn compares_content(&self) -> bool {
            self.reads.content.get()
        }
        fn changed_since(&self, near: &[u32], mark: u64) -> Option<bool> {
            // Events on one element of the app (its windows), when set;
            // else any change since `mark` while events are on.
            if let Some(w) = self.reads.events_on.get() {
                return Some(near.contains(&w));
            }
            self.reads.events.get().then(|| self.shown.seq.get() > mark)
        }
        /// As on Linux: returns once a change came and its burst settled.
        async fn wait_change(&self, _: &[u32], mark: u64, timeout: Duration) -> bool {
            if !self.reads.events.get() {
                tokio::time::sleep(timeout.min(Duration::from_millis(50))).await;
                return true;
            }
            let start = Instant::now();
            loop {
                if self.shown.seq.get() > mark {
                    loop {
                        let seq = self.shown.seq.get();
                        tokio::time::sleep(Duration::from_millis(15)).await;
                        if self.shown.seq.get() == seq {
                            return true;
                        }
                    }
                }
                let left = timeout.saturating_sub(start.elapsed());
                if left.is_zero() {
                    return false;
                }
                tokio::time::sleep(left.min(Duration::from_millis(5))).await;
            }
        }
        async fn loading(&self, _: &[u32]) -> bool {
            self.reads.loading.get()
        }
        fn unchanged_since(&self, _: &[u32], mark: u64) -> bool {
            self.reads.live.get() && self.reads.changes.get() == mark
        }
        async fn page(&self, doc: &u32) -> Option<backend::Page> {
            let info = self.info(doc, 0).await;
            (info.showing && render::DOCUMENT_ROLES.contains(&info.role.as_str()))
                .then(|| self.reads.page.borrow().clone())
                .flatten()
        }
        async fn may_keep(&self, _: &[u32]) -> bool {
            self.reads.live.get() && !self.reads.loading.get()
        }
        async fn still_there(&self, _: &u32) -> bool {
            self.reads.checks.set(self.reads.checks.get() + 1);
            !self.reads.gone.get()
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
            self.reads.prefetches.set(self.reads.prefetches.get() + 1);
            for id in roots {
                let info = self.info(&id, 0).await;
                cache.insert(id, info);
            }
        }
        /// As on Linux, a read without `actions` leaves them out:
        /// `fill_actions` reads them.
        async fn prefetch_with(&self, cache: &mut Cache<u32>, roots: Vec<u32>, f: &Fetch) {
            // Off-screen reads (a page's rows below the fold) take the
            // whole subtree, as Linux's does up to its limit.
            let mut read = roots.clone();
            if f.offscreen {
                let mut stack = roots;
                while let Some(id) = stack.pop()
                    && read.len() < 10_000
                {
                    let kids = self.info(&id, 0).await.children;
                    read.extend(kids.iter().copied());
                    stack.extend(kids);
                }
            }
            self.prefetch(cache, read.clone(), f.only_showing, f.text_limit, f.limit)
                .await;
            if !f.actions {
                for id in &read {
                    if let Some(info) = cache.get_mut(id) {
                        info.actions.clear();
                    }
                }
            }
        }
        async fn fill_actions(&self, cache: &mut Cache<u32>, ids: &[u32]) {
            for id in ids {
                let actions = self.actions(id).await;
                if let Some(info) = cache.get_mut(id) {
                    info.actions = actions;
                }
            }
        }
        async fn actions(&self, id: &u32) -> Vec<String> {
            if self.reads.noise.get() {
                self.shown.seq.set(self.shown.seq.get() + 1);
            }
            self.info(id, 0).await.actions
        }
        async fn toggles(&self, id: &u32) -> backend::Toggles {
            let info = self.info(id, 0).await;
            backend::Toggles {
                checkable: info.states.contains(&"checkable"),
                expandable: info.operable,
                // Hidden rather than scrolled away: named so.
                offscreen: !info.showing && !info.name.contains("(hidden)"),
                ..backend::Toggles::from_info(&info)
            }
        }
        async fn select_item(&self, item: &u32) -> R<bool> {
            let commit = self.sem.selection.get().ok_or_else(|| {
                delivery::nothing_sent("its list offers no accessibility selection")
            })?;
            self.keys.borrow_mut().push(format!("select {item}"));
            if self.sem.adds.get() {
                fake_edit(&self.focus, &self.nodes, *item, |i| {
                    i.states.push("selected")
                });
            } else {
                fake_pick(&self.focus, &self.nodes, *item, commit);
            }
            Ok(true)
        }
        async fn toolkit_of(&self, _: &u32) -> String {
            self.sem.toolkit.borrow().clone()
        }
        /// "multi line" stands for the state tree and find do not show.
        async fn edit_states(&self, id: &u32) -> Option<(bool, bool)> {
            let st = self.info(id, 0).await.states;
            Some((st.contains(&"editable"), st.contains(&"multi line")))
        }
        async fn scroll_into_view(&self, id: &u32) -> R<bool> {
            let works = self.sem.scroll_to.get().ok_or(backend::NO_SCROLL_TO)?;
            self.keys.borrow_mut().push(format!("scroll_to {id}"));
            if works {
                fake_edit(&self.focus, &self.nodes, *id, |i| i.showing = true);
            }
            Ok(works)
        }
        async fn scroll_container(&self, id: &u32) -> Option<u32> {
            if self.focus.borrow().children.contains(id) {
                return Some(FAKE_FOCUS);
            }
            self.nodes
                .borrow()
                .iter()
                .find(|(_, i)| i.children.contains(id))
                .map(|(k, _)| *k)
        }
        async fn may_open_menu(&self, _: &u32) -> bool {
            !self.sem.no_menu_open.get()
        }
        async fn do_action(&self, id: &u32, index: usize) -> R<bool> {
            self.keys
                .borrow_mut()
                .push(format!("action {index} on {id}"));
            self.shown.input();
            if let Some(next) = self.reads.page_next.take() {
                *self.reads.page.borrow_mut() = Some(next);
            }
            self.content_input();
            let info = self.info(id, 0).await;
            // As AT-SPI's DoAction when the app does not reply in time.
            if info.name.contains("timeout") {
                return Err("DoAction got no reply in 2 s".into());
            }
            let action = info.actions.get(index).map(|a| semantic::norm(a));
            if !info.name.to_lowercase().contains("stuck") {
                if semantic::is_check_role(&info.role) || info.states.contains(&"checkable") {
                    fake_edit(&self.focus, &self.nodes, *id, |i| {
                        flip(&mut i.states, "checked")
                    });
                } else if info.operable {
                    fake_edit(&self.focus, &self.nodes, *id, |i| {
                        flip(&mut i.states, "expanded")
                    });
                } else if info.role == "list item"
                    && matches!(action.as_deref(), Some("select" | "click"))
                {
                    fake_pick(&self.focus, &self.nodes, *id, true);
                }
            }
            let mut nodes = self.nodes.borrow_mut();
            let flyouts = self.shown.flyouts.borrow();
            if let Some(menu) = flyouts.get(id)
                && let Some(host) = nodes.get_mut(&FAKE_POPUP_HOST)
            {
                match action.as_deref() {
                    Some("expand") if !host.children.contains(menu) => host.children.push(*menu),
                    // Closes the submenus opened from it too (higher ids).
                    Some("collapse") => host.children.retain(|m| m < menu),
                    _ => {}
                }
            }
            drop(flyouts);
            if let Some(n) = nodes.get_mut(id)
                && n.role == "menu"
                && n.children.is_empty()
            {
                n.children = vec![99];
                nodes.insert(99, fake_node("menu item", "Opened", &[]));
            }
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
        fn app_key(&self, id: &u32) -> Option<String> {
            match self.shown.nameless.borrow().contains(id) {
                true => Some(":1.99".into()),
                false => Some("fake".into()),
            }
        }
        async fn is_focused(&self, _: &u32) -> bool {
            self.field.borrow().is_some()
        }
        async fn try_focus(&self, _: &u32) -> R<Option<String>> {
            Ok(self.sem.focusable.get().then(|| "grab_focus".into()))
        }
        async fn element_center(&self, id: &u32) -> R<(i32, i32)> {
            if self.no_area.borrow().contains(id) {
                return Err(backend::NO_AREA.into());
            }
            if self.background.get() {
                return Err("element's window is not active; call activate_window first".into());
            }
            Ok((0, 0))
        }
        async fn has_area(&self, ids: &[u32]) -> Vec<Option<bool>> {
            let no_area = self.no_area.borrow();
            ids.iter().map(|id| Some(!no_area.contains(id))).collect()
        }
        fn check_combo(&self, _: &str) -> R<()> {
            Ok(())
        }
        async fn press_combo(&self, combo: &str) -> R<()> {
            self.keys.borrow_mut().push(format!("down {combo}"));
            self.shown.input();
            self.content_input();
            if let Some(n) = self.sem.user_after_presses.get() {
                if n <= 1 {
                    self.sem.user_after_presses.set(None);
                    self.takeover
                        .user_input(Instant::now(), takeover::Device::Keyboard);
                } else {
                    self.sem.user_after_presses.set(Some(n - 1));
                }
            }
            // A list whose focused "Item N" moves down.
            let down = |focus: &RefCell<Info<u32>>| {
                let mut focus = focus.borrow_mut();
                if let Some(k) = focus
                    .name
                    .strip_prefix("Item ")
                    .and_then(|k| k.parse::<u32>().ok())
                {
                    focus.name = format!("Item {}", k + 1);
                }
            };
            let lag = self.lag.get();
            if combo == "down" && lag == 0 {
                down(&self.focus);
            }
            // A focused tree item expands and collapses; a focused combo
            // box moves to the next or previous entry of its list.
            let role = self.focus.borrow().role.clone();
            match (role.as_str(), combo) {
                ("tree item", "right") => self.focus.borrow_mut().states.push("expanded"),
                ("tree item", "space") => flip(&mut self.focus.borrow_mut().states, "checked"),
                ("tree item", "left") => {
                    self.focus.borrow_mut().states.retain(|s| *s != "expanded")
                }
                ("combo box", "down" | "up") => {
                    let list = self.focus.borrow().children[0];
                    let items = self.nodes.borrow()[&list].children.clone();
                    let name = self.focus.borrow().name.clone();
                    let at = items
                        .iter()
                        .position(|i| self.nodes.borrow()[i].name == name)
                        .unwrap_or(0);
                    let to = if combo == "down" {
                        (at + 1).min(items.len() - 1)
                    } else {
                        at.saturating_sub(1)
                    };
                    fake_pick(&self.focus, &self.nodes, items[to], true);
                }
                _ => {}
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
            self.keys.borrow_mut().push(format!("up {combo}"));
            if combo == "down" && lag > 0 {
                let focus = self.focus.clone();
                tokio::task::spawn_local(async move {
                    tokio::time::sleep(Duration::from_millis(lag)).await;
                    down(&focus);
                });
            }
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
                self.shown.input();
                self.content_input();
            }
            if scroll != 0 {
                self.keys.borrow_mut().push(format!("scroll {scroll}"));
                if let Some((id, n)) = self.sem.wheel.get() {
                    if n > 1 {
                        self.sem.wheel.set(Some((id, n - 1)));
                    } else {
                        self.sem.wheel.set(None);
                        fake_edit(&self.focus, &self.nodes, id, |i| i.showing = true);
                    }
                }
                match self.reveal.get() {
                    Some(n) if n > 1 => self.reveal.set(Some(n - 1)),
                    Some(_) => {
                        self.reveal.set(None);
                        self.focus.borrow_mut().children.push(REVEALED);
                        self.nodes
                            .borrow_mut()
                            .insert(REVEALED, fake_node("list item", "report.pdf", &[]));
                    }
                    None => {}
                }
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
                .ok_or_else(|| {
                    delivery::nothing_sent(format!("no window of an allowed app matches '{query}'"))
                })?;
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
    /// The element `FakeBackend::reveal` shows.
    const REVEALED: u32 = 60;
    /// `pointer` at this x fails.
    const FAIL_X: i64 = 666;
    /// `pointer` at this x takes 5 s.
    const SLOW_X: i64 = 777;

    fn fake_node(role: &str, name: &str, kids: &[u32]) -> Info<u32> {
        Info {
            role: role.into(),
            name: name.into(),
            showing: true,
            actions: vec!["Press".into()],
            children: kids.to_vec(),
            ..Info::default()
        }
    }

    struct Session {
        stdin: DuplexStream,
        stdout: tokio::io::Lines<BufReader<DuplexStream>>,
        keys: Rc<RefCell<Vec<String>>>,
        focus: Rc<RefCell<Info<u32>>>,
        focus_app: Rc<RefCell<String>>,
        field: Rc<RefCell<Option<String>>>,
        active: Rc<RefCell<(String, String)>>,
        takeover: std::sync::Arc<takeover::Takeover>,
        nodes: Rc<RefCell<HashMap<u32, Info<u32>>>>,
        reads: Rc<FakeReads>,
        reveal: Rc<std::cell::Cell<Option<u32>>>,
        lag: Rc<std::cell::Cell<u64>>,
        more_windows: Rc<RefCell<Vec<u32>>>,
        no_area: Rc<RefCell<Vec<u32>>>,
        background: Rc<std::cell::Cell<bool>>,
        shown: Rc<FakeShown>,
        sem: Rc<FakeSemantic>,
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
        let takeover = std::sync::Arc::new(takeover::Takeover::new(
            Duration::from_millis(1500),
            Some("meta+alt+esc"),
        ));
        let nodes = Rc::new(RefCell::new(HashMap::new()));
        let reads = Rc::new(FakeReads::default());
        let reveal = Rc::new(std::cell::Cell::new(None));
        let lag = Rc::new(std::cell::Cell::new(0));
        let more_windows = Rc::new(RefCell::new(Vec::new()));
        let no_area = Rc::new(RefCell::new(Vec::new()));
        let background = Rc::new(std::cell::Cell::new(false));
        let shown = Rc::new(FakeShown::default());
        let sem = Rc::new(FakeSemantic::default());
        let server = Rc::new(Server::new(FakeBackend {
            shown: shown.clone(),
            sem: sem.clone(),
            lag: lag.clone(),
            reveal: reveal.clone(),
            more_windows: more_windows.clone(),
            no_area: no_area.clone(),
            background: background.clone(),
            reads: reads.clone(),
            takeover: takeover.clone(),
            allow_file,
            keys: keys.clone(),
            focus: focus.clone(),
            focus_app: focus_app.clone(),
            clipboard: RefCell::new(String::new()),
            field: field.clone(),
            active: active.clone(),
            nodes: nodes.clone(),
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
            takeover,
            nodes,
            reads,
            reveal,
            lag,
            more_windows,
            no_area,
            background,
            shown,
            sem,
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
    fn wait_for_state_needs_the_element_to_have_it() {
        local(async {
            let mut s = session("wait-state");
            let wait = |state: &str, gone: bool| {
                json!({"app": "fake", "query": "OK", "state": state, "gone": gone,
                       "timeout_ms": 60})
            };
            // The fake's button is enabled, not focused.
            let r = call_tool(&mut s, 2, "wait_for", wait("enabled", false)).await;
            assert!(reply_text(&r).starts_with("found after "), "{r}");
            let r = call_tool(&mut s, 3, "wait_for", wait("focused", false)).await;
            assert_eq!(r["result"]["isError"], json!(true), "{r}");
            assert!(
                reply_text(&r).contains("waiting for 'OK' in 'fake' to be focused"),
                "{r}"
            );
            let r = call_tool(&mut s, 4, "wait_for", wait("focused", true)).await;
            assert!(
                reply_text(&r).starts_with("no longer focused after "),
                "{r}"
            );
            s.focus.borrow_mut().states = vec!["focused", "checked"];
            let r = call_tool(&mut s, 5, "wait_for", wait("Checked", false)).await;
            assert!(reply_text(&r).contains("{focused,checked}"), "{r}");
            s.focus.borrow_mut().states = vec!["disabled"];
            let r = call_tool(&mut s, 6, "wait_for", wait("enabled", false)).await;
            assert!(reply_text(&r).contains("to be enabled"), "{r}");
            let r = call_tool(&mut s, 7, "wait_for", wait("enabled", true)).await;
            assert!(
                reply_text(&r).starts_with("no longer enabled after "),
                "{r}"
            );
            let r = call_tool(&mut s, 8, "wait_for", wait("pressed", false)).await;
            assert!(reply_text(&r).contains(": unknown state 'pressed'"), "{r}");
            // Without `state`, as before.
            let r = call_tool(
                &mut s,
                9,
                "wait_for",
                json!({"app": "fake", "query": "OK", "gone": true, "timeout_ms": 60}),
            )
            .await;
            assert!(
                reply_text(&r).contains("waiting for 'OK' in 'fake' to disappear"),
                "{r}"
            );
        });
    }

    #[test]
    fn wait_for_window_watches_allowed_windows() {
        local(async {
            let mut s = session("wait-window");
            let r = call_tool(&mut s, 2, "wait_for", json!({"window": "TODO"})).await;
            let text = reply_text(&r);
            assert!(text.starts_with("window found after "), "{r}");
            assert!(text.contains("'todo.txt — Fake' [fake]"), "{r}");
            // Konsole's window is not an allowed app's.
            let r = call_tool(
                &mut s,
                3,
                "wait_for",
                json!({"window": "bash", "timeout_ms": 60}),
            )
            .await;
            assert_eq!(r["result"]["isError"], json!(true), "{r}");
            let text = reply_text(&r);
            assert!(
                text.contains("waiting for a window of an allowed app matching 'bash' to open"),
                "{r}"
            );
            let r = call_tool(
                &mut s,
                4,
                "wait_for",
                json!({"window": "bash", "gone": true}),
            )
            .await;
            assert!(reply_text(&r).starts_with("window gone after "), "{r}");
            let r = call_tool(
                &mut s,
                5,
                "wait_for",
                json!({"window": "notes", "app": "konsole"}),
            )
            .await;
            assert!(reply_text(&r).contains("is not in"), "{r}");
            let r = call_tool(
                &mut s,
                6,
                "wait_for",
                json!({"window": "notes", "query": "OK"}),
            )
            .await;
            assert!(reply_text(&r).contains("not both"), "{r}");
        });
    }

    #[test]
    fn wait_idle_returns_once_the_app_stops_changing() {
        local(async {
            let mut s = session("wait-idle");
            let r = call_tool(
                &mut s,
                2,
                "wait_idle",
                json!({"app": "fake", "quiet_ms": 50, "timeout_ms": 2000}),
            )
            .await;
            assert!(reply_text(&r).starts_with("idle after "), "{r}");
            assert!(reply_text(&r).contains("no change for 50 ms"), "{r}");
            // Renamed on every look: never quiet.
            let focus = s.focus.clone();
            let busy = tokio::task::spawn_local(async move {
                for k in 0..=u32::MAX {
                    focus.borrow_mut().name = format!("OK {k}");
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            });
            let r = call_tool(
                &mut s,
                3,
                "wait_idle",
                json!({"app": "fake", "quiet_ms": 150, "timeout_ms": 400}),
            )
            .await;
            busy.abort();
            assert_eq!(r["result"]["isError"], json!(true), "{r}");
            assert!(reply_text(&r).contains("still changing after "), "{r}");
            let r = call_tool(&mut s, 4, "wait_idle", json!({"app": "konsole"})).await;
            assert!(reply_text(&r).contains("is not in"), "{r}");
        });
    }

    #[test]
    fn tree_continuation_repeats_the_call_after_the_last_element() {
        let a = json!({"app": "Firefox", "document": true, "interactive": true, "max_nodes": 50});
        assert_eq!(
            continuation(&a, "firefox", Some(42), 7, false),
            "… 7 more nodes; continue with tree(app=\"firefox\", document=true, \
             interactive=true, max_nodes=50, after=42)"
        );
        let a = json!({"root_id": 9, "max_chars": 5000});
        assert_eq!(
            continuation(&a, "kate", Some(12), 3, true),
            "… 3+ more nodes; continue with tree(root_id=9, max_chars=5000, after=12)"
        );
        assert_eq!(
            continuation(&a, "kate", Some(12), 0, true),
            "… more nodes; continue with tree(root_id=9, max_chars=5000, after=12)"
        );
        assert_eq!(continuation(&a, "kate", None, 0, true), render::TRUNCATED);
    }

    #[test]
    fn tree_with_screenshot_adds_the_active_window_image() {
        local(async {
            let mut s = session("tree-shot");
            let r = call_tool(
                &mut s,
                2,
                "tree",
                json!({"app": "fake", "screenshot": true}),
            )
            .await;
            let content = r["result"]["content"].as_array().unwrap();
            assert_eq!(content.len(), 2, "{r}");
            assert_eq!(content[1]["type"], json!("image"));
            let text = reply_text(&r);
            assert!(text.starts_with("fake\n [1] push button \"OK\""), "{r}");
            assert!(text.contains("\nscreenshot: "), "{r}");
            // Another app in front: the tree alone, and why.
            *s.active.borrow_mut() = ("konsole".into(), "~ : bash — Konsole".into());
            let r = call_tool(
                &mut s,
                3,
                "tree",
                json!({"app": "fake", "screenshot": true}),
            )
            .await;
            assert_eq!(r["result"]["content"].as_array().unwrap().len(), 1, "{r}");
            assert!(reply_text(&r).contains("no screenshot: the active window is not fake's"));
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

    /// The fake window as a KDE app with its menu bar hidden behind a
    /// hamburger button: File (Qt-style popup, entries in the tree while
    /// closed) and Tools (Firefox-style, empty until opened).
    fn fake_menus(s: &Session) {
        *s.focus.borrow_mut() = Info {
            role: "frame".into(),
            name: "notes.txt — Fake".into(),
            showing: true,
            states: vec!["active"],
            children: vec![20],
            ..Info::default()
        };
        let mut n = s.nodes.borrow_mut();
        let mut bar = fake_node("menu bar", "", &[21, 30]);
        bar.showing = false;
        bar.actions.clear();
        n.insert(20, bar);
        let mut file = fake_node("menu item", "&File", &[22]);
        file.actions = vec!["Show Menu".into()];
        n.insert(21, file);
        n.insert(22, fake_node("popup menu", "", &[23, 28, 24, 25]));
        n.insert(23, fake_node("menu item", "Save As…", &[]));
        n.insert(28, fake_node("separator", "", &[]));
        let mut quit = fake_node("menu item", "Quit", &[]);
        quit.states = vec!["disabled"];
        n.insert(24, quit);
        let mut recent = fake_node("menu item", "Open Recent", &[26]);
        recent.actions = vec!["Show Menu".into()];
        n.insert(25, recent);
        n.insert(26, fake_node("popup menu", "", &[27]));
        n.insert(27, fake_node("menu item", "a.txt", &[]));
        let mut tools = fake_node("menu", "Tools", &[]);
        tools.actions = vec!["click".into()];
        n.insert(30, tools);
    }

    #[test]
    fn menu_lists_and_invokes_entries_by_path() {
        local(async {
            let mut s = session("menu");
            fake_menus(&s);
            let r = call_tool(&mut s, 2, "menu", json!({"app": "fake"})).await;
            let text = reply_text(&r).to_string();
            assert_eq!(r["result"]["isError"], false, "{r}");
            assert!(text.starts_with("fake's menu bar (hidden"), "{text}");
            assert!(text.contains("[1] menu item \"&File\" (submenu)"), "{text}");
            assert!(text.contains("[2] menu \"Tools\" (submenu)"), "{text}");
            let r = call_tool(
                &mut s,
                3,
                "menu",
                json!({"app": "fake", "path": "file > SAVE AS"}),
            )
            .await;
            assert_eq!(
                reply_text(&r),
                "\"&File > Save As…\" -> ok; effect: no visible change",
                "{r}"
            );
            assert_eq!(s.keys.borrow().last().unwrap(), "action 0 on 23");
            let r = call_tool(
                &mut s,
                4,
                "menu",
                json!({"app": "fake", "path": ["File", "recent"]}),
            )
            .await;
            let text = reply_text(&r);
            assert!(text.starts_with("\"&File > Open Recent\":\n"), "{text}");
            assert!(text.contains("menu item \"a.txt\""), "{text}");
            let r = call_tool(
                &mut s,
                5,
                "menu",
                json!({"app": "fake", "path": "File > Quit"}),
            )
            .await;
            assert_eq!(r["result"]["isError"], true, "{r}");
            assert!(reply_text(&r).contains("is disabled"), "{r}");
            let r = call_tool(
                &mut s,
                6,
                "menu",
                json!({"app": "fake", "path": "File > Print"}),
            )
            .await;
            assert!(
                reply_text(&r).contains("there are: \"Save As…\", \"Quit\", \"Open Recent\""),
                "{r}"
            );
            let before = s.keys.borrow().len();
            // Tools has no entries until opened: opened, then the entry runs.
            let r = call_tool(
                &mut s,
                7,
                "menu",
                json!({"app": "fake", "path": "Tools > Opened"}),
            )
            .await;
            assert_eq!(
                reply_text(&r),
                "\"Tools > Opened\" -> ok; effect: no visible change",
                "{r}"
            );
            // A shift tap first: on Wayland a popup shows only after input.
            assert_eq!(
                s.keys.borrow()[before..].to_vec(),
                vec![
                    "down shift".to_string(),
                    "up shift".to_string(),
                    "action 0 on 30".to_string(),
                    "action 0 on 99".to_string()
                ]
            );
            let r = call_tool(&mut s, 8, "menu", json!({"app": "evil"})).await;
            assert!(reply_text(&r).contains("is not in"), "{r}");
        });
    }

    /// The fake window as Win11 Notepad through UIA: menu bar entries
    /// that expand, whose submenus open as flyouts in a popup host.
    fn fake_flyout_menus(s: &Session) {
        *s.focus.borrow_mut() = Info {
            role: "window".into(),
            name: "notes.txt - Fake".into(),
            showing: true,
            states: vec!["active"],
            children: vec![FAKE_POPUP_HOST, 41],
            ..Info::default()
        };
        let entry = |name: &str, kids: &[u32]| Info {
            actions: vec!["expand".into(), "collapse".into(), "invoke".into()],
            operable: true,
            ..fake_node("menuitem", name, kids)
        };
        let leaf = |name: &str| Info {
            actions: vec!["invoke".into()],
            ..fake_node("menuitem", name, &[])
        };
        let mut n = s.nodes.borrow_mut();
        n.insert(FAKE_POPUP_HOST, fake_node("pane", "PopupHost", &[]));
        n.insert(41, fake_node("menubar", "", &[42]));
        n.insert(42, entry("Dosya", &[43]));
        // The entry's label is a button of the same name.
        n.insert(43, fake_node("button", "Dosya", &[]));
        n.insert(44, fake_node("menu", "", &[45, 46]));
        n.insert(45, leaf("Farklı kaydet"));
        n.insert(46, entry("En Son", &[]));
        n.insert(47, fake_node("menu", "", &[48]));
        n.insert(48, leaf("a.txt"));
        s.shown.uia_roles.set(true);
        *s.shown.flyouts.borrow_mut() = HashMap::from([(42, 44), (46, 47)]);
    }

    #[test]
    fn menus_open_as_flyouts_apart_from_their_entries() {
        local(async {
            let mut s = session("menu-flyout");
            fake_flyout_menus(&s);
            let r = call_tool(&mut s, 2, "menu", json!({"app": "fake", "path": "Dosya"})).await;
            let text = reply_text(&r).to_string();
            assert!(text.starts_with("\"Dosya\":\n"), "{text}");
            assert!(text.contains("menuitem \"Farklı kaydet\""), "{text}");
            assert!(
                text.contains("\"En Son\" actions=click (submenu)"),
                "{text}"
            );
            assert!(text.contains("(opened \"Dosya\" to read it"), "{text}");
            assert_eq!(s.keys.borrow().last().unwrap(), "action 0 on 42");
            // The submenu of En Son, not the flyout holding it.
            let r = call_tool(
                &mut s,
                3,
                "menu",
                json!({"app": "fake", "path": "Dosya > En Son"}),
            )
            .await;
            let text = reply_text(&r).to_string();
            assert!(text.contains("menuitem \"a.txt\""), "{text}");
            assert!(!text.contains("Farklı kaydet"), "{text}");
            let r = call_tool(
                &mut s,
                4,
                "menu",
                json!({"app": "fake", "path": "Dosya > Farklı kaydet"}),
            )
            .await;
            assert!(
                reply_text(&r).starts_with("\"Dosya > Farklı kaydet\" -> ok"),
                "{r}"
            );
            assert_eq!(s.keys.borrow().last().unwrap(), "action 0 on 45");
            // act takes the entry, not its label's button, expands it by
            // default and lists the flyout it opened.
            s.nodes
                .borrow_mut()
                .get_mut(&FAKE_POPUP_HOST)
                .unwrap()
                .children
                .clear();
            let r = call_tool(&mut s, 5, "act", json!({"app": "fake", "query": "Dosya"})).await;
            let text = reply_text(&r);
            assert!(text.contains("menuitem \"Dosya\""), "{text}");
            assert!(text.contains("expand -> ok"), "{text}");
            assert!(
                text.contains("\nopened: menu (2 entries): [") && text.contains("\"En Son\""),
                "{text}"
            );
            // Opened again (the same flyout): listed again.
            let r = call_tool(&mut s, 6, "act", json!({"app": "fake", "query": "Dosya"})).await;
            assert!(
                reply_text(&r).contains("\nopened: menu (2 entries): ["),
                "{r}"
            );
        });
    }

    #[test]
    fn close_window_names_one_window_and_needs_confirmation() {
        local(async {
            let mut s = session("close");
            let r = call_tool(&mut s, 2, "close_window", json!({"query": "fake"})).await;
            assert!(reply_text(&r).contains("2 windows match 'fake'"), "{r}");
            let r = call_tool(&mut s, 3, "close_window", json!({"query": "konsole"})).await;
            assert!(
                reply_text(&r).contains("no window of an allowed app"),
                "{r}"
            );
            let r = call_tool(&mut s, 4, "close_window", json!({"query": "notes"})).await;
            let text = reply_text(&r);
            assert_eq!(r["result"]["isError"], true, "{r}");
            assert!(
                text.contains("close_window on fake {\"query\":\"notes\"}")
                    && text.contains("unsaved work")
                    && text.contains("confirm: true"),
                "{text}"
            );
            // Confirmed: it reaches the backend, which has no window control.
            let r = call_tool(
                &mut s,
                5,
                "close_window",
                json!({"query": "notes", "confirm": true}),
            )
            .await;
            let text = reply_text(&r);
            assert!(text.contains(backend::NO_WINDOW_CONTROL), "{r}");
            assert!(
                text.ends_with("; nothing sent") && !text.contains(delivery::MAYBE_SENT),
                "{text}"
            );
            assert!(s.keys.borrow().is_empty());
        });
    }

    #[test]
    fn move_window_without_window_control_says_nothing_sent() {
        local(async {
            let mut s = session("move-unsupported");
            let r = call_tool(&mut s, 2, "move_window", json!({"query": "notes", "x": 1})).await;
            assert_eq!(r["result"]["isError"], true, "{r}");
            let text = reply_text(&r);
            assert!(text.contains(backend::NO_WINDOW_CONTROL), "{text}");
            assert!(
                text.ends_with("; nothing sent") && !text.contains(delivery::MAYBE_SENT),
                "{text}"
            );
        });
    }

    #[test]
    fn doctor_confirm_hint_names_an_app_of_the_platform() {
        local(async {
            let mut s = session("doctor-hint");
            let r = call_tool(&mut s, 2, "doctor", json!({})).await;
            let text = reply_text(&r);
            assert!(
                text.contains("none (add e.g. 'confirm: key:fake' to the allowlist)"),
                "{text}"
            );
            assert!(!text.contains("konsole"), "{text}");
        });
    }

    #[test]
    fn doctor_lists_the_file_and_names_apps_allowed_through_an_alias() {
        local(async {
            let mut s = session("doctor-alias");
            let r = call_tool(&mut s, 2, "doctor", json!({})).await;
            let text = reply_text(&r);
            assert!(
                text.contains("(1 apps: fake); allowed through an alias: hosted"),
                "{text}"
            );
        });
    }

    #[test]
    fn windows_lines_show_desktops() {
        let mut w = WindowGeo {
            title: "a".into(),
            app: "kate".into(),
            output: "eDP-1".into(),
            desktops: vec!["2".into()],
            ..WindowGeo::default()
        };
        assert!(describe_window(&w).ends_with(" on eDP-1, desktop 2"));
        w.desktops = vec!["all".into()];
        assert!(describe_window(&w).ends_with(", all desktops"));
        w.desktops = vec!["1".into(), "3".into()];
        assert!(describe_window(&w).ends_with(", desktops 1,3"));
        let d = |number, name: &str, current, output: &str| backend::Desktop {
            number,
            name: name.into(),
            current,
            output: output.into(),
        };
        assert_eq!(
            desktops_line(&[d(1, "Masaüstü 1", true, ""), d(2, "2", false, "")]),
            "desktops: 1 'Masaüstü 1' (current), 2"
        );
        assert_eq!(
            desktops_line(&[d(1, "1", true, "DP-1"), d(5, "mail", false, "DP-1")]),
            "workspaces: 1 (shown on DP-1), 5 'mail' (on DP-1)"
        );
        assert_eq!(
            desktops_line(&[d(0, "mail", true, "DP-1")]),
            "workspaces: 'mail' (shown on DP-1)"
        );
        assert_eq!(
            desktop_arg(&json!({"desktop": 2})).unwrap(),
            Some("2".into())
        );
        assert_eq!(
            desktop_arg(&json!({"desktop": " Web "})).unwrap(),
            Some("Web".into())
        );
        assert_eq!(desktop_arg(&json!({})).unwrap(), None);
        assert!(desktop_arg(&json!({"desktop": true})).is_err());
        let names = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(menu_action(&names(&["Show Menu", "Press"])), Some(1));
        assert_eq!(menu_action(&names(&["<Alt>D;"])), Some(0));
        assert_eq!(menu_action(&[]), None);
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
            let t = text(&r);
            assert!(t.contains(backend::NO_LAUNCH), "{t}");
            assert!(
                t.ends_with("; nothing sent") && !t.contains(delivery::MAYBE_SENT),
                "{t}"
            );
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
            assert_eq!(lines.len(), 4, "{r}");
            assert!(lines[0].starts_with("[1] key: "), "{r}");
            assert!(
                lines[1].starts_with("[2] wait_for failed (optional): "),
                "{r}"
            );
            assert!(lines[2].starts_with("[3] key: "), "{r}");
            // A step failed: what is on screen, once.
            assert!(lines[3].starts_with("now: "), "{r}");
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
            assert_eq!(lines.len(), 4, "{r}");
            assert!(lines[3].starts_with("now: "), "{r}");
            assert!(
                lines[0].starts_with("Error executing tool batch: [1] key: "),
                "{r}"
            );
            assert!(lines[1].starts_with("[2] wait_for failed: "), "{r}");
            assert_eq!(lines[2], "stopped; sent: [1]; not sent: [2..3]");
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
            // Inline, the reply names no file and leaves none behind.
            assert!(text.starts_with("100x50 px"), "{text}");
            // Pixel (50, 25) of a 4x-scaled image of an area at (10, 20).
            let r = call_tool(&mut s, 3, "mouse", json!({"image_x": 50, "image_y": 25})).await;
            let text = r["result"]["content"][0]["text"].as_str().unwrap();
            assert_eq!(text, "pointer at (212,122) in fake");
            let r = call_tool(&mut s, 4, "screenshot", json!({"inline": false})).await;
            assert_eq!(r["result"]["content"].as_array().unwrap().len(), 1, "{r}");
            // Not inline: the file on disk is the shrunk image the
            // coordinates refer to.
            let path = r["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .lines()
                .next()
                .unwrap();
            let img = shot::read_png(path).unwrap();
            let size = format!("\n{}x{} px", img.width, img.height);
            assert!(reply_text(&r).contains(&size), "{r}");
            std::fs::remove_file(path).unwrap();
        });
    }

    #[test]
    fn a_zoomed_screenshot_is_a_full_resolution_crop_and_clickable() {
        local(async {
            let mut s = session("zoom");
            let r = call_tool(&mut s, 2, "screenshot", json!({"region": [0, 0, 5, 5]})).await;
            assert!(
                reply_text(&r).contains("needs a screenshot taken first"),
                "{r}"
            );
            // 100x50 of the 400x200 area at (10, 20): 4 screen px per pixel.
            call_tool(&mut s, 3, "screenshot", json!({"max_size": 100})).await;
            // Pixels (25, 10)-(35, 15) are screen (110, 60), 40x20: the
            // 2x capture's pixels (50, 20)-(70, 30), sent as they are.
            let r = call_tool(
                &mut s,
                4,
                "screenshot",
                json!({"region": [25, 10, 10, 5], "inline": false}),
            )
            .await;
            let content = &r["result"]["content"];
            let text = content[0]["text"].as_str().unwrap();
            assert!(
                text.contains("20x10 px of screen area at (110,60), 40x20 (zoom of region"),
                "{text}"
            );
            let path = text.lines().next().unwrap();
            let img = shot::read_png(path).unwrap();
            assert_eq!((img.width, img.height), (20, 10));
            // image_x/image_y now map through the zoom: 2 screen px each.
            let r = call_tool(&mut s, 5, "mouse", json!({"image_x": 5, "image_y": 5})).await;
            assert!(
                reply_text(&r).starts_with("pointer at (121,71) in fake"),
                "{r}"
            );
            // A region is read in the zoomed image's pixels; it may reach
            // past that image, into the rest of the window.
            let r = call_tool(&mut s, 6, "screenshot", json!({"region": [10, 5, 50, 50]})).await;
            assert!(
                reply_text(&r).contains("50x50 px of screen area at (130,70), 100x100"),
                "{r}"
            );
            // Off the window: refused, and the last map is gone with it.
            let r = call_tool(&mut s, 7, "screenshot", json!({"region": [500, 0, 5, 5]})).await;
            assert!(reply_text(&r).contains("outside the active window"), "{r}");
            for bad in [json!([1, 2, 3]), json!([0, 0, 0, 5]), json!("x")] {
                let r = call_tool(&mut s, 8, "screenshot", json!({ "region": bad })).await;
                assert!(reply_text(&r).contains("region must be"), "{r}");
            }
            let _ = std::fs::remove_file(path);
        });
    }

    #[test]
    fn region_maps_back_to_the_capture_it_lies_in() {
        let map = ShotMap::new((100, 50), (10.0, 20.0, 400.0, 200.0)).unwrap();
        let want = map.area_of((25.0, 10.0, 10.0, 5.0));
        assert_eq!(want, (110.0, 60.0, 40.0, 20.0));
        let f = region_frac((10.0, 20.0, 400.0, 200.0), want).unwrap();
        assert_eq!(
            (f.x, f.y, f.w, f.h),
            (0.25, 0.2, 0.1, 0.1),
            "fractions of the capture"
        );
        // Clipped to the capture; outside it: None.
        let f = region_frac((0.0, 0.0, 100.0, 100.0), (90.0, -10.0, 20.0, 20.0)).unwrap();
        assert_eq!((f.x, f.y, f.w, f.h), (0.9, 0.0, 0.1, 0.1));
        assert_eq!(
            region_frac((0.0, 0.0, 100.0, 100.0), (100.0, 0.0, 5.0, 5.0)),
            None
        );
    }

    #[test]
    fn effect_names_window_focus_and_event_changes() {
        let fp = |window: Option<(&str, &str)>, focus: Option<&str>| Fingerprint {
            window: window.map(|(a, t)| (a.to_string(), t.to_string())),
            focus: focus.map(|n| (1u32, "push button".to_string(), n.to_string(), vec![])),
            app: None,
            page: None,
            content: None,
        };
        let allowed = |app: &str| app != "secret";
        let kate = Some(("kate", "a.txt — Kate"));
        let before = fp(kate, Some("OK"));
        let effect =
            |after: &Fingerprint<u32>, events| effect_text(&before, after, events, allowed);
        assert_eq!(effect(&fp(kate, Some("OK")), None), "no visible change");
        assert_eq!(
            effect(&fp(kate, Some("OK")), Some(false)),
            "no visible change"
        );
        assert_eq!(effect(&fp(kate, Some("OK")), Some(true)), "changed");
        assert_eq!(effect(&fp(kate, Some("Cancel")), Some(false)), "changed");
        assert_eq!(
            effect(&fp(Some(("kate", "Save As — Kate")), None), None),
            "changed (window title now 'Save As — Kate')"
        );
        assert_eq!(
            effect(&fp(Some(("dolphin", "Home")), None), None),
            "window changed to 'Home'"
        );
        // Titles of windows of apps not allowed are not shown.
        assert_eq!(
            effect(&fp(Some(("secret", "Bank — Secret")), None), None),
            "window changed to one of 'secret' (not allowed)"
        );
        assert_eq!(
            effect(&fp(None, None), None),
            "window changed (none active now)"
        );
    }

    #[test]
    fn effect_without_events_compares_the_content() {
        let fp = |content: Option<u64>| Fingerprint {
            window: Some(("calculatorapp".to_string(), "Hesap Makinesi".to_string())),
            focus: Some((1u32, "button".to_string(), "Yedi".to_string(), vec![])),
            app: None,
            page: None,
            content,
        };
        let allowed = |_: &str| true;
        // Only the display changed: focus and title stayed.
        assert_eq!(
            effect_text(&fp(Some(1)), &fp(Some(2)), None, allowed),
            "changed"
        );
        assert_eq!(
            effect_text(&fp(Some(1)), &fp(Some(1)), None, allowed),
            "no visible change"
        );
        // Not read on one side: nothing to compare.
        assert_eq!(
            effect_text(&fp(None), &fp(Some(2)), None, allowed),
            "no visible change"
        );
        // Events, where the backend gets them, decide.
        assert_eq!(
            effect_text(&fp(Some(1)), &fp(Some(2)), Some(false), allowed),
            "no visible change"
        );
    }

    #[test]
    fn observe_adds_a_view_after_the_input() {
        local(async {
            let mut s = session("observe");
            let r = call_tool(
                &mut s,
                2,
                "click",
                json!({"app": "fake", "query": "OK", "observe": "screenshot", "settle_ms": 0}),
            )
            .await;
            let content = &r["result"]["content"];
            let text = content[0]["text"].as_str().unwrap();
            assert!(
                text.starts_with(
                    "[1] push button \"OK\": clicked left x1 at 0,0; effect: no visible change\n"
                ),
                "{text}"
            );
            assert!(
                text.contains("200x100 px of screen area at (10,20)"),
                "{text}"
            );
            assert_eq!(content[1]["type"], "image", "{r}");
            let _ = std::fs::remove_file(text.lines().nth(1).unwrap());
            let r = call_tool(&mut s, 3, "act", json!({"id": 1, "observe": "focus"})).await;
            assert_eq!(
                reply_text(&r),
                "Press -> ok; effect: no visible change\nfocus: [1] push button \"OK\"",
                "{r}"
            );
            let r = call_tool(
                &mut s,
                4,
                "mouse",
                json!({"x": 1, "y": 2, "observe": "tree"}),
            )
            .await;
            assert!(
                reply_text(&r)
                    .starts_with("pointer at (1,2) in fake; effect: no visible change\nfake"),
                "{r}"
            );
            // A bad value fails before any input.
            let r = call_tool(&mut s, 5, "key", json!({"combo": "a", "observe": "all"})).await;
            assert!(reply_text(&r).contains("observe 'all': use one of"), "{r}");
            assert!(s.keys.borrow().iter().all(|k| !k.contains("down a")));
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
                text.contains("(not an editable field: check where the text went)"),
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
    fn batch_steps_do_not_repeat_an_unchanged_focus() {
        local(async {
            let mut s = session("batch-focus");
            s.focus.borrow_mut().states = vec!["focused", "editable"];
            let r = call_tool(
                &mut s,
                2,
                "batch",
                json!({"steps": [
                    {"tool": "key", "args": {"combo": "a"}},
                    {"tool": "pause", "args": {"ms": 0}},
                    {"tool": "key", "args": {"combo": "b"}},
                    {"tool": "type_text", "args": {"text": "x"}},
                ]}),
            )
            .await;
            assert_eq!(r["result"]["isError"], false, "{r}");
            let lines: Vec<&str> = reply_text(&r).lines().collect();
            // Described once, without the state every focus has.
            assert_eq!(
                lines[0], "[1] key: sent a x1 to fake; focus: [1] push button \"OK\" {editable}",
                "{r}"
            );
            assert_eq!(
                lines[2],
                "[3] key: sent b x1 to fake; focus: [1] (unchanged)"
            );
            assert!(lines[3].contains("; focus: [1] (unchanged)"), "{r}");
            // Once focus moves, it is described again.
            s.focus.borrow_mut().name = "Cancel".into();
            let r = call_tool(
                &mut s,
                3,
                "batch",
                json!({"steps": [
                    {"tool": "key", "args": {"combo": "a"}},
                    {"tool": "key", "args": {"combo": "b"}},
                ]}),
            )
            .await;
            let lines: Vec<&str> = reply_text(&r).lines().collect();
            assert!(
                lines[0].ends_with("push button \"Cancel\" {editable}"),
                "{r}"
            );
            assert!(lines[1].ends_with("(unchanged)"), "{r}");
            // A call of its own always describes it.
            let r = call_tool(&mut s, 4, "key", json!({"combo": "a"})).await;
            assert!(reply_text(&r).ends_with("\"Cancel\" {editable}"), "{r}");
        });
    }

    #[test]
    fn get_text_says_where_a_cut_reply_goes_on() {
        local(async {
            let mut s = session("get-text-cut");
            *s.field.borrow_mut() = Some("çağ 0123456789".into());
            let args = |extra: Json| {
                let mut a = json!({"app": "fake", "query": "OK"});
                a.as_object_mut()
                    .unwrap()
                    .extend(extra.as_object().unwrap().clone());
                a
            };
            let r = call_tool(&mut s, 2, "get_text", args(json!({"limit": 6}))).await;
            let text = reply_text(&r);
            assert!(
                text.ends_with(":\nçağ 01\n… cut at character 6; get_text with offset=6 reads on"),
                "{r}"
            );
            let r = call_tool(
                &mut s,
                3,
                "get_text",
                args(json!({"limit": 6, "offset": 6})),
            )
            .await;
            assert!(
                reply_text(&r).ends_with(
                    ":\n234567\n… cut at character 12; get_text with offset=12 reads on"
                ),
                "{r}"
            );
            // The rest fits: no note.
            let r = call_tool(&mut s, 4, "get_text", args(json!({"offset": 12}))).await;
            assert!(reply_text(&r).ends_with(":\n89"), "{r}");
            let r = call_tool(&mut s, 5, "get_text", args(json!({"limit": 14}))).await;
            assert!(reply_text(&r).ends_with(":\nçağ 0123456789"), "{r}");
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
            assert_eq!(reply_text(&r), "nothing was held; nothing sent");
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
                reply_text(&r).lines().next().unwrap(),
                "Error executing tool mouse: pointer failed; maybe sent: look before retrying; \
                 released ctrl"
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
            assert!(
                reply_text(&r)
                    .lines()
                    .next()
                    .unwrap()
                    .ends_with("; released left"),
                "{r}"
            );
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
        // On Windows `~\` expands too; elsewhere a backslash is part of
        // a file name.
        let home = std::path::Path::new("/home/u");
        let win = |p: &str| expand_path_with(p, Some(home), &['/', '\\']);
        assert_eq!(
            win("~\\Belgeler\\a.txt").unwrap(),
            home.join("Belgeler\\a.txt")
        );
        assert_eq!(win("~/a.txt").unwrap(), home.join("a.txt"));
        assert!(expand_path_with("~\\a", Some(home), &['/']).is_err());
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
            assert!(text.ends_with("; nothing sent"), "{text}");
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
        let allow = Policy::parse("firefox");
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
            apps_text(list(), &Policy::default(), &file, "restart note"),
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
            assert_eq!(
                reply_text(&r),
                "Press -> ok; effect: no visible change",
                "{r}"
            );
            assert_eq!(actions_run(&s), 1);
        });
    }

    /// The fake window holding `kids` (nodes the test adds).
    fn fake_window(s: &Session, kids: &[u32]) {
        *s.focus.borrow_mut() = Info {
            role: "frame".into(),
            name: "notes.txt — Fake".into(),
            showing: true,
            states: vec!["active"],
            children: kids.to_vec(),
            ..Info::default()
        };
    }

    fn with_states(mut info: Info<u32>, states: &[&'static str]) -> Info<u32> {
        info.states = states.to_vec();
        info
    }

    fn with_actions(mut info: Info<u32>, actions: &[&str]) -> Info<u32> {
        info.actions = actions.iter().map(|a| a.to_string()).collect();
        info
    }

    fn is_error(r: &Json) -> bool {
        r["result"]["isError"] == json!(true)
    }

    /// The id `tree(include_hidden)` gives the element named `name`.
    async fn hidden_id(s: &mut Session, call: u64, name: &str) -> i64 {
        let r = call_tool(
            s,
            call,
            "tree",
            json!({"app": "fake", "include_hidden": true}),
        )
        .await;
        let text = reply_text(&r).to_string();
        let line = text
            .lines()
            .find(|l| l.contains(&format!("\"{name}\"")))
            .unwrap_or_else(|| panic!("{name} not in {text}"));
        // Siblings may share the line: the id just before the name.
        let name_at = line.find(&format!("\"{name}\"")).unwrap();
        let at = line[..name_at].rfind('[').unwrap() + 1;
        line[at..].split(']').next().unwrap().parse().unwrap()
    }

    #[test]
    fn check_and_uncheck_are_idempotent_and_verified() {
        local(async {
            let mut s = session("check");
            fake_window(&s, &[10, 11, 12, 13]);
            {
                let mut n = s.nodes.borrow_mut();
                let qt =
                    |name| with_actions(fake_node("check box", name, &[]), &["Toggle", "Press"]);
                n.insert(10, with_states(qt("Wrap lines"), &["checked"]));
                n.insert(11, qt("Stuck option"));
                n.insert(12, fake_node("push button", "Apply", &[]));
                n.insert(
                    13,
                    with_actions(fake_node("section", "Agree", &[]), &["check"]),
                );
            }
            let call = |a: Json| {
                let mut args = json!({"app": "fake"});
                args.as_object_mut()
                    .unwrap()
                    .extend(a.as_object().unwrap().clone());
                args
            };
            let r = call_tool(
                &mut s,
                2,
                "act",
                call(json!({"query": "Wrap lines", "action": "check"})),
            )
            .await;
            assert_eq!(
                reply_text(&r),
                "[1] check box \"Wrap lines\" {checked} actions=Toggle,Press: already checked; \
                 nothing sent; effect: no visible change",
                "{r}"
            );
            assert_eq!(actions_run(&s), 0);
            let r = call_tool(
                &mut s,
                3,
                "act",
                call(json!({"query": "Wrap lines", "action": "uncheck"})),
            )
            .await;
            assert!(
                reply_text(&r).contains(
                    "\"Wrap lines\" {checked} actions=Toggle,Press: Toggle -> unchecked (verified)"
                ),
                "{r}"
            );
            assert_eq!(actions_run(&s), 1);
            assert!(!s.nodes.borrow()[&10].states.contains(&"checked"));
            // toggle flips it back, verified.
            let r = call_tool(
                &mut s,
                4,
                "act",
                call(json!({"query": "Wrap lines", "action": "Toggle"})),
            )
            .await;
            assert!(
                reply_text(&r).contains("Toggle -> checked (verified)"),
                "{r}"
            );
            // The state read back contradicts: an error saying what was sent.
            let r = call_tool(
                &mut s,
                5,
                "act",
                call(json!({"query": "Stuck option", "action": "check"})),
            )
            .await;
            assert!(is_error(&r), "{r}");
            assert!(
                reply_text(&r).contains("[2] check box \"Stuck option\" actions=Toggle,Press: sent Toggle, but it reads unchecked afterwards"),
                "{r}"
            );
            assert_eq!(actions_run(&s), 3);
            // Not checkable: nothing sent, its actions named.
            let r = call_tool(
                &mut s,
                6,
                "act",
                call(json!({"query": "Apply", "action": "check"})),
            )
            .await;
            assert!(is_error(&r), "{r}");
            assert!(
                reply_text(&r).contains(
                    "it is not checkable (push button); nothing sent: its actions are ['Press']"
                ),
                "{r}"
            );
            assert_eq!(actions_run(&s), 3);
            // Not checkable, but an action of that name: run as before.
            let r = call_tool(
                &mut s,
                7,
                "act",
                call(json!({"query": "Agree", "action": "check"})),
            )
            .await;
            assert!(reply_text(&r).contains("check -> ok"), "{r}");
            assert_eq!(actions_run(&s), 4);
            // `option` belongs to select only.
            let r = call_tool(
                &mut s,
                8,
                "act",
                call(json!({"query": "Apply", "action": "check", "option": "x"})),
            )
            .await;
            assert!(
                reply_text(&r).contains("option goes with action=select"),
                "{r}"
            );
            assert_eq!(actions_run(&s), 4);
        });
    }

    #[test]
    fn a_radio_button_is_not_unchecked() {
        local(async {
            let mut s = session("radio");
            fake_window(&s, &[10]);
            s.nodes.borrow_mut().insert(
                10,
                with_states(fake_node("radio button", "Tabs", &[]), &["checked"]),
            );
            let r = call_tool(
                &mut s,
                2,
                "act",
                json!({"app": "fake", "query": "Tabs", "action": "uncheck"}),
            )
            .await;
            assert!(is_error(&r), "{r}");
            assert!(
                reply_text(&r).contains("checking another one of its group; nothing sent"),
                "{r}"
            );
            assert_eq!(actions_run(&s), 0);
        });
    }

    #[test]
    fn expand_and_collapse_are_verified_with_a_key_fallback() {
        local(async {
            let mut s = session("expand");
            fake_window(&s, &[10, 11]);
            {
                let mut n = s.nodes.borrow_mut();
                let mut gtk = with_actions(
                    fake_node("tree item", "src", &[]),
                    &["activate", "expand or contract"],
                );
                gtk.operable = true;
                n.insert(10, gtk);
                let mut combo = fake_node("combo box", "Mode", &[]);
                combo.operable = true;
                n.insert(11, combo);
            }
            let q = |query: &str, action: &str| json!({"app": "fake", "query": query, "action": action});
            let r = call_tool(&mut s, 2, "act", q("src", "expand")).await;
            assert!(
                reply_text(&r).contains(": expand or contract -> expanded (verified)"),
                "{r}"
            );
            assert_eq!(s.keys.borrow().last().unwrap(), "action 1 on 10");
            let r = call_tool(&mut s, 3, "act", q("src", "expand")).await;
            assert!(
                reply_text(&r).contains("already expanded; nothing sent"),
                "{r}"
            );
            let r = call_tool(&mut s, 4, "act", q("src", "collapse")).await;
            assert!(reply_text(&r).contains("collapsed (verified)"), "{r}");
            // toggle on an expandable element without a Toggle flips it.
            let r = call_tool(&mut s, 12, "act", q("src", "toggle")).await;
            assert!(
                reply_text(&r).contains(": expand or contract -> expanded (verified)"),
                "{r}"
            );
            let r = call_tool(&mut s, 13, "act", q("src", "collapse")).await;
            assert!(reply_text(&r).contains("collapsed (verified)"), "{r}");
            assert_eq!(actions_run(&s), 4);
            // Firefox: no menu is opened through accessibility.
            s.sem.no_menu_open.set(true);
            let r = call_tool(&mut s, 5, "act", q("Mode", "expand")).await;
            assert!(is_error(&r), "{r}");
            assert!(
                reply_text(&r).contains("can hang or crash this app (Firefox); nothing sent"),
                "{r}"
            );
            assert_eq!(actions_run(&s), 4);
            // A Qt tree item: only Toggle (which selects), so focus + key.
            let mut qt = with_actions(fake_node("tree item", "docs", &[]), &["Toggle"]);
            qt.operable = true;
            *s.focus.borrow_mut() = qt;
            let id = focus_id(&mut s, 6).await;
            let r = call_tool(&mut s, 7, "act", json!({"id": id, "action": "expand"})).await;
            assert!(is_error(&r), "{r}");
            assert!(
                reply_text(&r).contains(
                    "cannot be focused without the pointer; nothing sent: click it, then key right"
                ),
                "{r}"
            );
            assert!(!s.keys.borrow().iter().any(|k| k.contains("right")));
            s.sem.focusable.set(true);
            let r = call_tool(&mut s, 8, "act", json!({"id": id, "action": "expand"})).await;
            assert!(
                reply_text(&r).starts_with("focus (grab_focus) + key right -> expanded (verified)"),
                "{r}"
            );
            let r = call_tool(&mut s, 9, "act", json!({"id": id, "action": "collapse"})).await;
            assert!(
                reply_text(&r).starts_with("focus (grab_focus) + key left -> collapsed (verified)"),
                "{r}"
            );
            assert_eq!(actions_run(&s), 4, "Toggle never ran");
            // Its own Toggle (which selects the row) still runs as asked.
            let keys = s.keys.borrow().len();
            let r = call_tool(&mut s, 11, "act", json!({"id": id, "action": "Toggle"})).await;
            assert!(reply_text(&r).starts_with("Toggle -> ok"), "{r}");
            assert_eq!(actions_run(&s), 5);
            assert!(
                !s.keys.borrow()[keys..]
                    .iter()
                    .any(|k| k.starts_with("down")),
                "no key sent: {:?}",
                &s.keys.borrow()[keys..]
            );
            // Keys need the full tier.
            let allow = std::env::temp_dir().join(format!(
                "desktop-mcp-test-{}-expand.txt",
                std::process::id()
            ));
            std::fs::write(&allow, "fake:click\n").unwrap();
            // (The fake's Toggle expanded it.)
            let r = call_tool(&mut s, 10, "act", json!({"id": id, "action": "collapse"})).await;
            assert!(is_error(&r), "{r}");
            assert!(
                reply_text(&r).contains("needs the full tier ('fake' has click); nothing sent"),
                "{r}"
            );
        });
    }

    /// A combo box (10, or the focus element) named after its value, with
    /// a hidden list (11) of three entries (12-14) whose action is Qt's
    /// Toggle.
    fn fake_combo(s: &Session, focused: bool, name: &str, actions: &[&str]) {
        let mut combo = fake_node("combo box", name, &[11]);
        combo.actions = vec!["Show Menu".into()];
        if focused {
            *s.focus.borrow_mut() = combo;
        } else {
            fake_window(s, &[10]);
            s.nodes.borrow_mut().insert(10, combo);
        }
        let mut n = s.nodes.borrow_mut();
        let mut list = fake_node("list", "", &[12, 13, 14]);
        list.showing = false;
        n.insert(11, list);
        for (id, label) in [(12, "UTF-8"), (13, "UTF-16"), (14, "ISO-8859-1")] {
            let mut item = with_actions(fake_node("list item", label, &[]), actions);
            item.showing = false;
            n.insert(id, item);
        }
    }

    #[test]
    fn select_picks_an_entry_without_the_pointer_and_verifies() {
        local(async {
            let mut s = session("select");
            fake_combo(&s, false, "UTF-8", &["Toggle"]);
            let q = |option: &str| json!({"app": "fake", "query": "UTF-8", "role": "combo box", "option": option});
            let r = call_tool(&mut s, 2, "act", q("utf-8")).await;
            assert!(
                reply_text(&r).starts_with(
                    "[1] combo box \"UTF-8\" actions=Show Menu: already \"UTF-8\"; nothing sent"
                ),
                "{r}"
            );
            let r = call_tool(&mut s, 3, "act", q("Latin")).await;
            assert!(is_error(&r), "{r}");
            assert!(
                reply_text(&r).contains("no entry \"Latin\" here; there are: \"UTF-8\", \"UTF-16\", \"ISO-8859-1\"; nothing sent"),
                "{r}"
            );
            // Selection that picks it.
            s.sem.selection.set(Some(true));
            let r = call_tool(&mut s, 4, "act", q("utf-16")).await;
            assert!(
                reply_text(&r).contains(": \"UTF-16\" selected (Selection; verified)"),
                "{r}"
            );
            assert_eq!(s.nodes.borrow()[&10].name, "UTF-16");
            assert_eq!(actions_run(&s), 0);
        });
    }

    #[test]
    fn select_falls_back_to_the_entry_action_and_reports_a_contradiction() {
        local(async {
            let mut s = session("select-fallback");
            // Firefox: the control is named after its label; the entry's
            // select action picks it.
            fake_combo(&s, false, "Encoding", &["select"]);
            let q = |option: &str| json!({"app": "fake", "query": "Encoding", "option": option});
            let r = call_tool(&mut s, 2, "act", q("ISO")).await;
            assert!(
                reply_text(&r).contains(": \"ISO-8859-1\" selected (select; verified)"),
                "{r}"
            );
            assert_eq!(s.keys.borrow().last().unwrap(), "action 0 on 14");
            let r = call_tool(&mut s, 3, "act", q("ISO")).await;
            assert!(
                reply_text(&r).contains("already \"ISO-8859-1\"; nothing sent"),
                "{r}"
            );
            // Qt: Selection marks the hidden list's entry but the combo box
            // still shows the old value; Toggle is never run, and keys
            // need focus.
            fake_combo(&s, false, "UTF-8", &["Toggle"]);
            s.sem.selection.set(Some(false));
            let r = call_tool(
                &mut s,
                4,
                "act",
                json!({"app": "fake", "query": "UTF-8", "role": "combo box", "option": "UTF-16"}),
            )
            .await;
            assert!(is_error(&r), "{r}");
            assert!(
                reply_text(&r).contains(
                    "sent Selection, but it shows \"UTF-8\" afterwards; the key fallback: opened its list (Show Menu), but no list showed"
                ),
                "{r}"
            );
            // The Firefox step's action, and Show Menu opening the list.
            assert_eq!(actions_run(&s), 2);
        });
    }

    #[test]
    fn select_moves_a_closed_combo_box_with_arrow_keys() {
        local(async {
            let mut s = session("select-keys");
            fake_combo(&s, true, "UTF-8", &["Toggle"]);
            s.sem.focusable.set(true);
            let id = focus_id(&mut s, 2).await;
            let r = call_tool(&mut s, 3, "act", json!({"id": id, "option": "ISO-8859-1"})).await;
            assert_eq!(
                reply_text(&r),
                "\"ISO-8859-1\" selected (focus (grab_focus) + key down x2; verified); effect: changed",
                "{r}"
            );
            let downs = s.keys.borrow().iter().filter(|k| *k == "down down").count();
            assert_eq!(downs, 2);
            let r = call_tool(&mut s, 4, "act", json!({"id": id, "option": "UTF-8"})).await;
            assert!(
                reply_text(&r)
                    .starts_with("\"UTF-8\" selected (focus (grab_focus) + key up x2; verified)"),
                "{r}"
            );
        });
    }

    #[test]
    fn select_opens_a_menu_button_that_lists_no_entries() {
        local(async {
            let mut s = session("select-menu");
            fake_window(&s, &[10, 20]);
            {
                let mut n = s.nodes.borrow_mut();
                n.insert(
                    10,
                    with_actions(
                        fake_node("push button", "Sort", &[]),
                        &["Press", "Show Menu"],
                    ),
                );
                let mut popup = fake_node("popup menu", "", &[21, 22]);
                popup.showing = false;
                n.insert(20, popup);
                n.insert(
                    21,
                    with_actions(fake_node("radio menu item", "By name", &[]), &["click"]),
                );
                n.insert(
                    22,
                    with_actions(fake_node("radio menu item", "By date", &[]), &["click"]),
                );
            }
            // Firefox: its menus are not opened through accessibility.
            s.sem.no_menu_open.set(true);
            let r = call_tool(
                &mut s,
                2,
                "act",
                json!({"app": "fake", "query": "Sort", "option": "date"}),
            )
            .await;
            assert!(is_error(&r), "{r}");
            assert!(
                reply_text(&r).contains("lists no entries until opened, and opening its menu"),
                "{r}"
            );
            assert_eq!(actions_run(&s), 0);
            s.sem.no_menu_open.set(false);
            // The popup shows once opened (here: right away).
            s.nodes.borrow_mut().get_mut(&20).unwrap().showing = true;
            let r = call_tool(
                &mut s,
                3,
                "act",
                json!({"app": "fake", "query": "Sort", "option": "date"}),
            )
            .await;
            let text = reply_text(&r);
            assert!(text.contains("opened it (Show Menu); "), "{r}");
            assert_eq!(
                s.keys
                    .borrow()
                    .iter()
                    .filter(|k| k.starts_with("action"))
                    .cloned()
                    .collect::<Vec<_>>(),
                vec!["action 1 on 10", "action 0 on 22"]
            );
        });
    }

    #[test]
    fn scroll_to_uses_scroll_to_then_the_wheel() {
        local(async {
            let mut s = session("scroll-to");
            fake_window(&s, &[40]);
            {
                let mut n = s.nodes.borrow_mut();
                n.insert(40, fake_node("list", "Files", &[41, 42, 43]));
                n.insert(41, fake_node("list item", "near.txt", &[]));
                let mut far = fake_node("list item", "far.txt", &[]);
                far.showing = false;
                n.insert(42, far);
                let mut hidden = fake_node("list item", "inner.txt (hidden)", &[]);
                hidden.showing = false;
                n.insert(43, hidden);
            }
            let r = call_tool(
                &mut s,
                2,
                "act",
                json!({"app": "fake", "query": "near.txt", "action": "scroll_to"}),
            )
            .await;
            assert!(
                reply_text(&r).contains(": already in view; nothing sent"),
                "{r}"
            );
            let id = hidden_id(&mut s, 3, "far.txt").await;
            // Neither ScrollTo nor a wheel that reaches it.
            let r = call_tool(&mut s, 4, "act", json!({"id": id, "action": "scroll_to"})).await;
            assert!(is_error(&r), "{r}");
            assert!(
                reply_text(&r).contains(&format!(
                    "{}; turned the wheel 50 down at 0,0 (the pointer moved), but it is still not in view",
                    backend::NO_SCROLL_TO
                )),
                "{r}"
            );
            // A hidden element gets no wheel.
            let inner = hidden_id(&mut s, 7, "inner.txt (hidden)").await;
            let before = s.keys.borrow().len();
            let r = call_tool(
                &mut s,
                8,
                "act",
                json!({"id": inner, "action": "scroll_to"}),
            )
            .await;
            assert!(is_error(&r), "{r}");
            assert!(
                reply_text(&r).contains("not scrolled out of view; no wheel turned"),
                "{r}"
            );
            assert_eq!(s.keys.borrow().len(), before);
            // The wheel shows it on its second turn.
            s.sem.wheel.set(Some((42, 2)));
            let r = call_tool(&mut s, 5, "act", json!({"id": id, "action": "scroll to"})).await;
            assert!(
                reply_text(&r)
                    .starts_with("in view (wheel 10 down at 0,0, the pointer moved; verified)"),
                "{r}"
            );
            // ScrollTo, no pointer.
            s.nodes.borrow_mut().get_mut(&42).unwrap().showing = false;
            s.sem.scroll_to.set(Some(true));
            let before = s.keys.borrow().len();
            let r = call_tool(&mut s, 6, "act", json!({"id": id, "action": "scroll_to"})).await;
            assert!(
                reply_text(&r).starts_with("in view (ScrollTo; verified)"),
                "{r}"
            );
            assert_eq!(s.keys.borrow()[before..], ["scroll_to 42".to_string()]);
        });
    }

    #[test]
    fn select_never_presses_a_control_that_lists_no_entries() {
        local(async {
            let mut s = session("select-plain");
            fake_window(&s, &[10, 11]);
            {
                let mut n = s.nodes.borrow_mut();
                n.insert(
                    10,
                    with_actions(fake_node("push button", "Apply", &[]), &["Press"]),
                );
                n.insert(
                    11,
                    with_actions(fake_node("list item", "report.pdf", &[]), &["activate"]),
                );
            }
            for (i, (query, role)) in [("Apply", "push button"), ("report.pdf", "list item")]
                .into_iter()
                .enumerate()
            {
                let r = call_tool(
                    &mut s,
                    2 + i as u64,
                    "act",
                    json!({"app": "fake", "query": query, "option": "x"}),
                )
                .await;
                assert!(is_error(&r), "{r}");
                assert!(
                    reply_text(&r).contains(&format!(
                        "it lists no entries ({role}): not a combo box, list or menu button; \
                         nothing sent"
                    )),
                    "{r}"
                );
            }
            assert_eq!(actions_run(&s), 0);
            // A button that says it has a popup is opened.
            s.nodes
                .borrow_mut()
                .get_mut(&10)
                .unwrap()
                .states
                .push("has popup");
            let r = call_tool(
                &mut s,
                4,
                "act",
                json!({"app": "fake", "query": "Apply", "option": "x"}),
            )
            .await;
            assert!(reply_text(&r).contains("opened it (Press)"), "{r}");
            // An empty option is no option: a plain act.
            let r = call_tool(
                &mut s,
                5,
                "act",
                json!({"app": "fake", "query": "report.pdf", "option": "", "action": ""}),
            )
            .await;
            assert!(reply_text(&r).contains("activate -> ok"), "{r}");
        });
    }

    #[test]
    fn select_in_a_multi_selection_list_says_what_else_stays_selected() {
        local(async {
            let mut s = session("select-multi");
            fake_window(&s, &[40]);
            {
                let mut n = s.nodes.borrow_mut();
                n.insert(40, fake_node("list", "Files", &[41, 42, 43]));
                n.insert(
                    41,
                    with_states(fake_node("list item", "a.txt", &[]), &["selected"]),
                );
                n.insert(
                    42,
                    with_states(fake_node("list item", "b.txt", &[]), &["selected"]),
                );
                n.insert(43, fake_node("list item", "c.txt", &[]));
            }
            s.sem.selection.set(Some(true));
            s.sem.adds.set(true);
            let q = |option: &str| json!({"app": "fake", "query": "Files", "role": "list", "option": option});
            // b.txt is selected, but along with a.txt: not "already".
            let r = call_tool(&mut s, 2, "act", q("b.txt")).await;
            assert!(
                reply_text(&r).contains(
                    "\"b.txt\" selected (Selection; verified); 1 other entry stays selected too"
                ),
                "{r}"
            );
            let r = call_tool(&mut s, 3, "act", q("c.txt")).await;
            assert!(
                reply_text(&r).contains("; 2 other entries stay selected too"),
                "{r}"
            );
            // A list that keeps one selected says nothing more.
            s.sem.adds.set(false);
            let r = call_tool(&mut s, 4, "act", q("a.txt")).await;
            assert!(
                reply_text(&r).contains("\"a.txt\" selected (Selection; verified); effect"),
                "{r}"
            );
        });
    }

    #[test]
    fn select_in_a_web_page_runs_the_entry_action_not_selection() {
        local(async {
            let mut s = session("select-web");
            fake_combo(&s, false, "Encoding", &["select"]);
            s.sem.selection.set(Some(true));
            *s.sem.toolkit.borrow_mut() = "gecko".into();
            let r = call_tool(
                &mut s,
                2,
                "act",
                json!({"app": "fake", "query": "Encoding", "option": "UTF-16"}),
            )
            .await;
            assert!(
                reply_text(&r).contains(": \"UTF-16\" selected (select; verified)"),
                "{r}"
            );
            assert!(!s.keys.borrow().iter().any(|k| k.starts_with("select ")));
        });
    }

    #[test]
    fn a_checkable_row_is_checked_by_space_not_its_toggle() {
        local(async {
            let mut s = session("check-row");
            let mut qt = with_actions(fake_node("tree item", "Plugin", &[]), &["Toggle"]);
            qt.states = vec!["checkable"];
            *s.focus.borrow_mut() = qt;
            *s.sem.toolkit.borrow_mut() = "qt".into();
            let id = focus_id(&mut s, 2).await;
            let r = call_tool(&mut s, 3, "act", json!({"id": id, "action": "check"})).await;
            assert!(is_error(&r), "{r}");
            assert!(
                reply_text(&r).contains(
                    "there is no action to check it, and it cannot be focused without the \
                     pointer; nothing sent: click it, then key space"
                ),
                "{r}"
            );
            s.sem.focusable.set(true);
            let r = call_tool(&mut s, 4, "act", json!({"id": id, "action": "check"})).await;
            assert!(
                reply_text(&r).starts_with("focus (grab_focus) + key space -> checked (verified)"),
                "{r}"
            );
            assert_eq!(actions_run(&s), 0, "Toggle (select the row) never ran");
            // GTK's toggle cell checks with its toggle; Windows spells the
            // check box role without a space.
            fake_window(&s, &[10, 11]);
            {
                let mut n = s.nodes.borrow_mut();
                let mut cell = with_actions(
                    fake_node("table cell", "Enabled", &[]),
                    &["activate", "toggle"],
                );
                cell.states = vec!["checkable"];
                n.insert(10, cell);
                n.insert(
                    11,
                    with_actions(fake_node("checkbox", "Wrap", &[]), &["toggle"]),
                );
            }
            *s.sem.toolkit.borrow_mut() = "gtk".into();
            let r = call_tool(
                &mut s,
                5,
                "act",
                json!({"app": "fake", "query": "Enabled", "action": "check"}),
            )
            .await;
            assert!(
                reply_text(&r).contains(": toggle -> checked (verified)"),
                "{r}"
            );
            assert_eq!(s.keys.borrow().last().unwrap(), "action 1 on 10");
            let r = call_tool(
                &mut s,
                6,
                "act",
                json!({"app": "fake", "query": "Wrap", "action": "check"}),
            )
            .await;
            assert!(
                reply_text(&r).contains(": toggle -> checked (verified)"),
                "{r}"
            );
        });
    }

    #[test]
    fn the_key_fallback_is_covered_by_the_act_calls_confirmation() {
        local(async {
            let mut s = session("confirm-key-on");
            let mut qt = with_actions(fake_node("tree item", "docs", &[]), &["Toggle"]);
            qt.operable = true;
            *s.focus.borrow_mut() = qt;
            s.sem.focusable.set(true);
            let id = focus_id(&mut s, 2).await;
            set_allow("confirm-key-on", "fake\nconfirm: key:fake\n");
            let r = call_tool(&mut s, 3, "act", json!({"id": id, "action": "expand"})).await;
            assert!(is_error(&r), "{r}");
            assert!(reply_text(&r).contains("confirm: true"), "{r}");
            assert!(!s.keys.borrow().iter().any(|k| k.contains("right")));
            let r = call_tool(
                &mut s,
                4,
                "act",
                json!({"id": id, "action": "expand", "confirm": true}),
            )
            .await;
            assert!(
                reply_text(&r).starts_with("focus (grab_focus) + key right -> expanded (verified)"),
                "{r}"
            );
        });
    }

    #[test]
    fn user_input_stops_the_key_and_wheel_fallbacks() {
        local(async {
            let mut s = session("takeover-semantic");
            fake_combo(&s, true, "UTF-8", &["Toggle"]);
            s.sem.focusable.set(true);
            let id = focus_id(&mut s, 2).await;
            let t = s.takeover.clone();
            tokio::task::spawn_local(async move {
                tokio::time::sleep(Duration::from_millis(30)).await;
                t.user_input(Instant::now(), takeover::Device::Keyboard);
            });
            let r = call_tool(&mut s, 3, "act", json!({"id": id, "option": "ISO-8859-1"})).await;
            assert!(is_error(&r), "{r}");
            assert!(reply_text(&r).contains("interrupted"), "{r}");
            let downs = s.keys.borrow().iter().filter(|k| *k == "down down").count();
            assert_eq!(downs, 1);
        });
        local(async {
            let mut s = session("takeover-wheel");
            fake_window(&s, &[40]);
            {
                let mut n = s.nodes.borrow_mut();
                n.insert(40, fake_node("list", "Files", &[42]));
                let mut far = fake_node("list item", "far.txt", &[]);
                far.showing = false;
                n.insert(42, far);
            }
            let id = hidden_id(&mut s, 2, "far.txt").await;
            let t = s.takeover.clone();
            tokio::task::spawn_local(async move {
                tokio::time::sleep(Duration::from_millis(100)).await;
                t.user_input(Instant::now(), takeover::Device::Pointer);
            });
            let r = call_tool(&mut s, 3, "act", json!({"id": id, "action": "scroll_to"})).await;
            assert!(is_error(&r), "{r}");
            assert!(reply_text(&r).contains("interrupted"), "{r}");
            let turns = s
                .keys
                .borrow()
                .iter()
                .filter(|k| k.starts_with("scroll "))
                .count();
            assert_eq!(turns, 1);
        });
    }

    #[test]
    fn verified_actions_run_in_a_batch() {
        local(async {
            let mut s = session("verified-batch");
            fake_window(&s, &[10]);
            s.nodes.borrow_mut().insert(
                10,
                with_actions(fake_node("check box", "Wrap", &[]), &["Toggle"]),
            );
            let r = call_tool(
                &mut s,
                2,
                "batch",
                json!({"steps": [
                    {"tool": "act", "args": {"app": "fake", "query": "Wrap", "action": "check"}},
                    {"tool": "act", "args": {"id": "$1", "action": "check"}},
                ]}),
            )
            .await;
            let text = reply_text(&r);
            assert!(text.contains("Toggle -> checked (verified)"), "{text}");
            assert!(text.contains("already checked; nothing sent"), "{text}");
            assert_eq!(actions_run(&s), 1);
        });
    }

    #[test]
    fn act_never_opens_a_context_menu_by_default() {
        local(async {
            let mut s = session("ctx-menu");
            let id = focus_id(&mut s, 2).await;
            s.focus.borrow_mut().actions = vec!["showContextMenu".into()];
            let r = call_tool(&mut s, 3, "act", json!({"id": id})).await;
            assert_eq!(r["result"]["isError"], true, "{r}");
            assert!(
                reply_text(&r).contains("no default action (only showContextMenu)"),
                "{r}"
            );
            assert_eq!(actions_run(&s), 0);
            // Asked for by name, it runs.
            let r = call_tool(
                &mut s,
                4,
                "act",
                json!({"id": id, "action": "showContextMenu"}),
            )
            .await;
            assert!(reply_text(&r).starts_with("showContextMenu -> ok"), "{r}");
            // Another action is the default, wherever it is listed.
            s.focus.borrow_mut().actions = vec!["showContextMenu".into(), "doDefault".into()];
            let r = call_tool(&mut s, 5, "act", json!({"id": id})).await;
            assert!(reply_text(&r).starts_with("doDefault -> ok"), "{r}");
            assert!(s.keys.borrow().last().unwrap().starts_with("action 1 on "));
        });
    }

    #[test]
    fn a_trailing_newline_is_pressed_as_enter() {
        local(async {
            let mut s = session("type-enter");
            let r = call_tool(&mut s, 2, "type_text", json!({"text": "abc\n"})).await;
            assert!(
                reply_text(&r).contains("typed 3 chars into fake, then pressed Enter"),
                "{r}"
            );
            assert!(
                s.keys
                    .borrow()
                    .ends_with(&["down enter".to_string(), "up enter".to_string()])
            );
            let r = call_tool(&mut s, 3, "type_text", json!({"text": "a\nb"})).await;
            assert!(
                reply_text(&r).starts_with("typed 3 chars into fake;"),
                "{r}"
            );
        });
    }

    #[test]
    fn find_takes_a_role_without_a_query() {
        local(async {
            let mut s = session("find-role");
            let r = call_tool(
                &mut s,
                2,
                "find",
                json!({"app": "fake", "role": "push button"}),
            )
            .await;
            assert!(reply_text(&r).contains("push button \"OK\""), "{r}");
            let r = call_tool(&mut s, 3, "find", json!({"app": "fake"})).await;
            assert_eq!(r["result"]["isError"], true, "{r}");
            assert!(reply_text(&r).contains("(or pass `role`)"), "{r}");
        });
    }

    #[test]
    fn no_matches_and_failures_say_what_is_on_screen() {
        local(async {
            let mut s = session("where");
            let r = call_tool(&mut s, 2, "find", json!({"app": "fake", "query": "zzz"})).await;
            let text = reply_text(&r);
            assert!(text.starts_with("no matches\nnow: "), "{text}");
            assert!(text.contains("focus: [1] push button \"OK\""), "{text}");
            let r = call_tool(&mut s, 3, "act", json!({"app": "fake", "query": "zzz"})).await;
            assert_eq!(r["result"]["isError"], true, "{r}");
            assert!(reply_text(&r).contains("\nnow: "), "{r}");
            // Not for the allowlist: that is the user's setup, not the screen.
            let r = call_tool(&mut s, 4, "find", json!({"app": "nope", "query": "x"})).await;
            assert!(!reply_text(&r).contains("\nnow: "), "{r}");
        });
    }

    #[test]
    fn input_notes_a_window_that_came_to_the_front() {
        local(async {
            let mut s = session("window-note");
            // The first input only learns which window is in front.
            let r = call_tool(&mut s, 2, "key", json!({"combo": "a"})).await;
            assert!(!reply_text(&r).contains("active window now"), "{r}");
            *s.active.borrow_mut() = (FAKE_WINDOWS[2].0.to_string(), FAKE_WINDOWS[2].1.to_string());
            // It came to the front between two inputs (a dialog opening
            // after a reply): the next input says so.
            let r = call_tool(&mut s, 3, "key", json!({"combo": "b"})).await;
            let want = format!("; before it, '{}' had come to the front", FAKE_WINDOWS[2].1);
            assert!(reply_text(&r).ends_with(&want), "{r}");
            let r = call_tool(&mut s, 4, "key", json!({"combo": "c"})).await;
            assert!(!reply_text(&r).contains("active window now"), "{r}");
        });
    }

    /// A context menu (40) with Copy, Paste, a checked Wrap entry and a
    /// separator; a save dialog (60) asking with Save, Discard and Cancel;
    /// a combo box (69) whose list (70) holds two entries.
    fn fake_surfaces(s: &Session) {
        let mut nodes = s.nodes.borrow_mut();
        nodes.insert(40, fake_node("popup menu", "", &[41, 42, 43, 44]));
        nodes.insert(41, fake_node("menu item", "Copy", &[]));
        nodes.insert(42, fake_node("menu item", "Paste", &[]));
        let mut wrap = fake_node("check menu item", "Wrap", &[]);
        wrap.states = vec!["checked"];
        nodes.insert(43, wrap);
        nodes.insert(44, fake_node("separator", "", &[]));
        nodes.insert(60, fake_node("dialog", "Save?", &[61, 62, 63, 64]));
        nodes.insert(61, fake_node("label", "Save changes to notes.txt?", &[]));
        nodes.insert(62, fake_node("push button", "Save", &[]));
        nodes.insert(63, fake_node("push button", "Discard", &[]));
        nodes.insert(64, fake_node("push button", "Cancel", &[]));
        nodes.insert(69, fake_node("combo box", "Encoding", &[70]));
        nodes.insert(70, fake_node("list", "", &[71, 72]));
        nodes.insert(71, fake_node("list item", "UTF-8", &[]));
        nodes.insert(72, fake_node("list item", "Latin-1", &[]));
        nodes.insert(80, fake_node("label", "Saved", &[]));
        s.shown.parents.borrow_mut().insert(70, 69);
        s.shown.live.set(true);
    }

    /// The id printed just before `"label"` in `text`.
    fn id_before(text: &str, label: &str) -> i64 {
        let at = text.find(&format!("\"{label}\"")).expect(text);
        let open = text[..at].rfind('[').expect(text);
        text[open + 1..].split(']').next().unwrap().parse().unwrap()
    }

    #[test]
    fn a_right_click_lists_the_context_menu_it_opened() {
        local(async {
            let mut s = session("surface-menu");
            fake_surfaces(&s);
            let id = focus_id(&mut s, 2).await;
            // The menu and its entries each say they show.
            s.shown.next.borrow_mut().extend([40, 41, 42, 43]);
            let r = call_tool(&mut s, 3, "click", json!({"id": id, "button": "right"})).await;
            let text = reply_text(&r);
            assert!(
                text.contains("\nopened: popup menu (3 entries): ["),
                "{text}"
            );
            assert!(
                text.contains("\"Paste\" [")
                    && text.contains("] check menu item \"Wrap\" {checked}"),
                "{text}"
            );
            assert_eq!(text.matches("opened:").count(), 1, "{text}");
            // Its ids work as they are.
            let copy = id_before(text, "Copy");
            let r = call_tool(&mut s, 4, "act", json!({"id": copy})).await;
            assert!(reply_text(&r).starts_with("Press -> ok"), "{r}");
            assert!(s.keys.borrow().iter().any(|k| k == "action 0 on 41"));
            // Nothing opened by the action: nothing listed.
            assert!(!reply_text(&r).contains("opened:"), "{r}");
        });
    }

    #[test]
    fn act_key_and_a_combo_click_list_what_they_opened() {
        local(async {
            let mut s = session("surface-kinds");
            fake_surfaces(&s);
            let id = focus_id(&mut s, 2).await;
            // An action that opens a menu: listed after the effect line.
            s.shown.next.borrow_mut().push(40);
            let r = call_tool(&mut s, 3, "act", json!({"id": id})).await;
            let text = reply_text(&r);
            assert!(text.contains("; effect: "), "{text}");
            assert!(
                text.contains("\nopened: popup menu (3 entries): "),
                "{text}"
            );
            // A shortcut that opens a dialog: its question and buttons,
            // buttons named once by role.
            s.shown.next.borrow_mut().extend([62, 63, 64, 60]);
            let r = call_tool(&mut s, 4, "key", json!({"combo": "ctrl+s"})).await;
            let text = reply_text(&r);
            let at = text.find("\nopened: dialog 'Save?': ").expect(text);
            let line = &text[at..];
            assert!(
                line.contains("\"Save changes to notes.txt?\" [")
                    && line.contains("] push button \"Save\" [")
                    && line.contains("] \"Discard\" [")
                    && line.contains("] \"Cancel\""),
                "{line}"
            );
            // Its buttons were not listed again on their own.
            assert_eq!(text.matches("opened:").count(), 1, "{text}");
            // A combo box's list, shown by a plain click on the combo box.
            let r = call_tool(&mut s, 5, "find", json!({"app": "fake", "query": "OK"})).await;
            assert!(!reply_text(&r).contains("opened:"), "{r}");
            s.focus.borrow_mut().role = "combo box".into();
            s.shown.next.borrow_mut().push(70);
            let r = call_tool(&mut s, 6, "click", json!({"app": "fake", "query": "OK"})).await;
            let text = reply_text(&r);
            assert!(
                text.contains("\nopened: list (2 entries): [") && text.contains("\"Latin-1\""),
                "{text}"
            );
            // A list outside a combo box is no surface.
            s.shown.parents.borrow_mut().clear();
            s.shown.next.borrow_mut().push(70);
            let r = call_tool(&mut s, 7, "click", json!({"app": "fake", "query": "OK"})).await;
            assert!(!reply_text(&r).contains("opened:"), "{r}");
        });
    }

    #[test]
    fn nothing_opened_costs_no_reads_and_adds_nothing() {
        local(async {
            let mut s = session("surface-none");
            fake_surfaces(&s);
            let id = focus_id(&mut s, 2).await;
            let (ids, reads) = (s.shown.identities.get(), s.reads.prefetches.get());
            // No event: no read beyond the id's own check.
            let r = call_tool(&mut s, 3, "click", json!({"id": id})).await;
            assert!(!reply_text(&r).contains("opened:"), "{r}");
            let r = call_tool(&mut s, 4, "key", json!({"combo": "ctrl+s"})).await;
            assert!(!reply_text(&r).contains("opened:"), "{r}");
            let r = call_tool(&mut s, 5, "type_text", json!({"text": "x"})).await;
            assert!(!reply_text(&r).contains("opened:"), "{r}");
            assert_eq!(s.shown.identities.get(), ids + 1);
            assert_eq!(s.reads.prefetches.get(), reads);
            // Something shown that is no menu or dialog: its role is read,
            // nothing more.
            s.shown.next.borrow_mut().push(80);
            let r = call_tool(&mut s, 6, "key", json!({"combo": "a"})).await;
            assert!(!reply_text(&r).contains("opened:"), "{r}");
            assert_eq!(s.shown.identities.get(), ids + 2);
            assert_eq!(s.reads.prefetches.get(), reads);
            // A menu that closed again before the reply is not listed.
            s.nodes.borrow_mut().get_mut(&40).unwrap().showing = false;
            s.shown.next.borrow_mut().push(40);
            let r = call_tool(
                &mut s,
                7,
                "mouse",
                json!({"x": 5, "y": 5, "button": "right"}),
            )
            .await;
            assert!(!reply_text(&r).contains("opened:"), "{r}");
        });
    }

    #[test]
    fn without_events_likely_openers_scan_the_app_windows_once() {
        local(async {
            let mut s = session("surface-scan");
            fake_surfaces(&s);
            s.shown.live.set(false);
            s.more_windows.borrow_mut().push(60);
            // A plain key looks at nothing.
            let ids = s.shown.identities.get();
            let r = call_tool(&mut s, 2, "key", json!({"combo": "a"})).await;
            assert!(!reply_text(&r).contains("opened:"), "{r}");
            assert_eq!(s.shown.identities.get(), ids);
            // One that may open a dialog looks at the app's windows.
            let r = call_tool(&mut s, 3, "key", json!({"combo": "ctrl+s"})).await;
            assert!(reply_text(&r).contains("\nopened: dialog 'Save?': "), "{r}");
            // Still open: not listed again.
            let r = call_tool(&mut s, 4, "key", json!({"combo": "ctrl+o"})).await;
            assert!(!reply_text(&r).contains("opened:"), "{r}");
        });
    }

    #[test]
    fn close_window_lists_the_save_prompt_it_left_open() {
        local(async {
            let mut s = session("surface-close");
            fake_surfaces(&s);
            s.shown.close_opens.set(Some(60));
            let r = call_tool(
                &mut s,
                2,
                "close_window",
                json!({"query": "notes", "confirm": true}),
            )
            .await;
            let text = reply_text(&r);
            assert!(
                text.contains("still open, showing dialog 'Save?': \"Save changes to notes.txt?\"")
                    && text.contains("] push button \"Save\" ["),
                "{text}"
            );
            // Listed once, not again after the effect line.
            assert!(!text.contains("opened:"), "{text}");
            let save = id_before(text, "Save");
            let r = call_tool(&mut s, 3, "act", json!({"id": save})).await;
            assert!(reply_text(&r).starts_with("Press -> ok"), "{r}");
        });
    }

    #[test]
    fn close_window_lists_a_dialog_the_backend_names_without_its_role() {
        local(async {
            let mut s = session("surface-close-uia");
            fake_surfaces(&s);
            s.shown.uia_roles.set(true);
            s.shown.names_dialogs.set(true);
            // UIA calls a save prompt a "window", with plain buttons.
            {
                let mut nodes = s.nodes.borrow_mut();
                nodes.get_mut(&60).unwrap().role = "window".into();
                nodes.get_mut(&62).unwrap().role = "button".into();
            }
            s.shown.close_opens.set(Some(60));
            let r = call_tool(
                &mut s,
                2,
                "close_window",
                json!({"query": "notes", "confirm": true}),
            )
            .await;
            let text = reply_text(&r);
            assert!(
                text.contains("still open, showing dialog 'Save?'")
                    && text.contains("] button \"Save\" ["),
                "{text}"
            );
            let save = id_before(text, "Save");
            let r = call_tool(&mut s, 3, "act", json!({"id": save})).await;
            assert!(reply_text(&r).starts_with("Press -> ok"), "{r}");
        });
    }

    #[test]
    fn a_surface_whose_app_name_cannot_be_read_is_not_listed() {
        local(async {
            let mut s = session("surface-nameless");
            fake_surfaces(&s);
            // Another app's menu (another bus) whose name reads empty: not
            // taken for the active app's, so neither listed nor given ids.
            {
                let mut nodes = s.nodes.borrow_mut();
                nodes.insert(90, fake_node("popup menu", "", &[91]));
                nodes.insert(91, fake_node("menu item", "Delete all", &[]));
            }
            s.shown.nameless.borrow_mut().extend([90, 91]);
            s.shown.next.borrow_mut().extend([90, 40]);
            let r = call_tool(
                &mut s,
                2,
                "mouse",
                json!({"x": 5, "y": 5, "button": "right"}),
            )
            .await;
            let text = reply_text(&r);
            assert!(text.contains("\nopened: popup menu (3 entries)"), "{text}");
            assert!(!text.contains("Delete all"), "{text}");
            assert_eq!(text.matches("opened:").count(), 1, "{text}");
        });
    }

    #[test]
    fn without_surface_roles_nothing_waits_or_scans() {
        local(async {
            let mut s = session("surface-uia");
            fake_surfaces(&s);
            s.shown.live.set(false);
            s.shown.uia_roles.set(true);
            s.more_windows.borrow_mut().push(60);
            let ids = s.shown.identities.get();
            for (n, combo) in [(2, "ctrl+s"), (3, "menu")] {
                let r = call_tool(&mut s, n, "key", json!({"combo": combo})).await;
                assert!(!reply_text(&r).contains("opened:"), "{r}");
            }
            let id = focus_id(&mut s, 4).await;
            let r = call_tool(&mut s, 5, "act", json!({"id": id})).await;
            assert!(!reply_text(&r).contains("opened:"), "{r}");
            // No window's role read (but the act's own target check).
            assert_eq!(s.shown.identities.get(), ids + 1);
            // close_window gives the plain hint.
            s.shown.close_opens.set(Some(60));
            let r = call_tool(
                &mut s,
                6,
                "close_window",
                json!({"query": "notes", "confirm": true}),
            )
            .await;
            assert!(
                reply_text(&r).contains("probably asking whether to save"),
                "{r}"
            );
        });
    }

    #[test]
    fn a_dialog_that_opened_after_a_reply_is_listed_by_the_next() {
        local(async {
            let mut s = session("surface-late");
            fake_surfaces(&s);
            let id = focus_id(&mut s, 2).await;
            // A plain click: nothing showed before its reply.
            let r = call_tool(&mut s, 3, "click", json!({"id": id})).await;
            assert!(!reply_text(&r).contains("opened:"), "{r}");
            // The dialog it opened shows a few ms later.
            s.shown.next.borrow_mut().extend([62, 63, 64, 60]);
            s.shown.input();
            let r = call_tool(&mut s, 4, "key", json!({"combo": "a"})).await;
            let text = reply_text(&r);
            assert!(
                text.contains("\nbefore it, opened: dialog 'Save?': \"Save changes"),
                "{text}"
            );
            assert_eq!(text.matches("opened:").count(), 1, "{text}");
            // Its ids work.
            let save = id_before(text, "Save");
            let r = call_tool(&mut s, 5, "act", json!({"id": save})).await;
            assert!(reply_text(&r).starts_with("Press -> ok"), "{r}");
            assert!(!reply_text(&r).contains("opened:"), "{r}");
            // Listed once: an act's reply after more shows, but not again.
            s.shown.next.borrow_mut().extend([60, 40]);
            s.shown.input();
            let r = call_tool(&mut s, 6, "act", json!({"id": id})).await;
            let text = reply_text(&r);
            assert!(
                text.contains("\nbefore it, opened: dialog 'Save?'"),
                "{text}"
            );
            assert!(text.contains("before it, opened: popup menu"), "{text}");
            // Something shown again after the reply of a call that listed
            // it is not listed again.
            s.shown.next.borrow_mut().push(40);
            s.shown.input();
            let r = call_tool(&mut s, 7, "key", json!({"combo": "b"})).await;
            assert!(!reply_text(&r).contains("opened:"), "{r}");
        });
    }

    #[test]
    fn a_surface_listed_once_is_not_read_again() {
        local(async {
            let mut s = session("surface-reads");
            fake_surfaces(&s);
            s.shown.close_opens.set(Some(60));
            let reads = s.reads.prefetches.get();
            let r = call_tool(
                &mut s,
                2,
                "close_window",
                json!({"query": "notes", "confirm": true}),
            )
            .await;
            assert!(reply_text(&r).contains("showing dialog 'Save?'"), "{r}");
            // The dialog and its children: two reads, none in the effect line.
            assert_eq!(s.reads.prefetches.get(), reads + 2);
            // Scanning without events: a dialog still open is checked by
            // its own node only.
            s.shown.live.set(false);
            let r = call_tool(&mut s, 3, "key", json!({"combo": "ctrl+s"})).await;
            assert!(reply_text(&r).contains("opened: dialog 'Save?'"), "{r}");
            let reads = s.reads.prefetches.get();
            let r = call_tool(&mut s, 4, "key", json!({"combo": "ctrl+o"})).await;
            assert!(!reply_text(&r).contains("opened:"), "{r}");
            assert_eq!(s.reads.prefetches.get(), reads + 1);
            // Once hidden, it is listed again when it shows.
            s.nodes.borrow_mut().get_mut(&60).unwrap().showing = false;
            let r = call_tool(&mut s, 5, "key", json!({"combo": "ctrl+o"})).await;
            assert!(!reply_text(&r).contains("opened:"), "{r}");
            s.nodes.borrow_mut().get_mut(&60).unwrap().showing = true;
            let r = call_tool(&mut s, 6, "key", json!({"combo": "ctrl+o"})).await;
            assert!(reply_text(&r).contains("opened: dialog 'Save?'"), "{r}");
        });
    }

    #[test]
    fn a_file_dialog_is_named_for_file_dialog_and_others_skip_their_views() {
        local(async {
            let mut s = session("surface-file");
            fake_surfaces(&s);
            {
                let mut nodes = s.nodes.borrow_mut();
                nodes.insert(
                    100,
                    fake_node("dialog", "Open File", &[101, 108, 103, 104, 105]),
                );
                nodes.insert(101, fake_node("tool bar", "", &[102]));
                nodes.insert(102, fake_node("push button", "Back", &[]));
                nodes.insert(108, fake_node("panel", "", &[109, 110]));
                nodes.insert(109, fake_node("push button", "home", &[]));
                nodes.insert(110, fake_node("push button", "docs", &[]));
                nodes.insert(103, fake_node("list", "", &[111]));
                nodes.insert(111, fake_node("list item", "notes.txt", &[]));
                let mut name = fake_node("text", "", &[]);
                name.states = vec!["editable"];
                nodes.insert(104, name);
                nodes.insert(105, fake_node("panel", "", &[106, 107]));
                nodes.insert(106, fake_node("push button", "Open", &[]));
                nodes.insert(107, fake_node("push button", "Cancel", &[]));
            }
            // Not one file_dialog fills: its buttons, OK and Cancel first,
            // without the tool bar's or the file list's.
            s.shown.next.borrow_mut().push(100);
            let r = call_tool(&mut s, 2, "key", json!({"combo": "ctrl+o"})).await;
            let text = reply_text(&r);
            let at = text.find("\nopened: dialog 'Open File': [").expect(text);
            let line = &text[at..];
            assert!(
                line.find("\"Open\"") < line.find("\"home\"")
                    && line.find("\"Cancel\"") < line.find("\"home\""),
                "{line}"
            );
            assert!(
                !line.contains("Back") && !line.contains("notes.txt"),
                "{line}"
            );
            // One it fills: named, nothing read for entries.
            s.shown.file_dialogs.borrow_mut().push(100);
            s.shown.next.borrow_mut().push(100);
            let r = call_tool(&mut s, 3, "key", json!({"combo": "ctrl+o"})).await;
            let text = reply_text(&r);
            assert!(
                text.contains("\nopened: file dialog 'Open File' (use file_dialog)"),
                "{text}"
            );
            assert!(!text.contains("\"Open\""), "{text}");
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
                reply_text(&r).lines().next().unwrap(),
                format!(
                    "Error executing tool act: stale id {id} (was push button \"OK\", \
                     now push button \"Cancel\"); call tree/find again; nothing sent"
                )
            );
            assert!(reply_text(&r).contains("\nnow: "), "{r}");
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
            assert_eq!(
                reply_text(&r),
                "Activate -> ok; effect: no visible change",
                "{r}"
            );
            // An unnamed element may gain a name; its role still counts.
            *s.focus.borrow_mut() = Info {
                role: "push button".into(),
                actions: vec!["Press".into()],
                ..Info::default()
            };
            let unnamed = focus_id(&mut s, 4).await;
            s.focus.borrow_mut().name = "Play".into();
            let r = call_tool(&mut s, 5, "act", json!({"id": unnamed})).await;
            assert_eq!(
                reply_text(&r),
                "Press -> ok; effect: no visible change",
                "{r}"
            );
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
            assert_eq!(
                reply_text(&r),
                "Press -> ok; effect: no visible change",
                "{r}"
            );
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
            assert_eq!(
                lines[1], "[2] act: Press -> ok; effect: no visible change",
                "{r}"
            );
            // The query picked the same element, under the same id.
            assert_eq!(
                lines[3],
                format!(
                    "[4] act: [{found}] push button \"OK\": Press -> ok; effect: no visible change"
                ),
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
                    // Text arguments are never references.
                    {"tool": "clipboard_set", "args": {"text": "$1"}},
                    {"tool": "act", "args": {"id": "$2"}},
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
            assert!(lines[4].starts_with("[5] clipboard_set: "), "{r}");
            assert_eq!(
                lines[5],
                "[6] act failed: `id` \"$2\": step 2 (wait_for) failed"
            );
            assert_eq!(lines[6], "stopped; sent: [5]; not sent: [6..7]");
            assert_eq!(actions_run(&s), 0);
            assert!(s.keys.borrow().is_empty());
            // A reference that can never work fails the whole batch before
            // anything runs.
            for (bad, why) in [
                ("$3", "refer to an earlier step (1 to 1)"),
                ("$0", "refer to an earlier step (1 to 1)"),
                ("$x", "use \"$N\" for the element step N produced"),
            ] {
                let r = call_tool(
                    &mut s,
                    3,
                    "batch",
                    json!({"steps": [
                        {"tool": "key", "args": {"combo": "a"}},
                        {"tool": "act", "args": {"id": bad}},
                    ]}),
                )
                .await;
                assert_eq!(r["result"]["isError"], true, "{r}");
                assert_eq!(
                    reply_text(&r),
                    format!(
                        "Error executing tool batch: not run (nothing sent): [2] act: `id` \"{bad}\": {why}"
                    ),
                );
            }
            assert!(s.keys.borrow().is_empty());
        });
    }

    /// Run a batch and return (is error, its lines).
    async fn run_batch(s: &mut Session, id: u64, steps: Json) -> (bool, Vec<String>) {
        let r = call_tool(s, id, "batch", json!({ "steps": steps })).await;
        let lines = reply_text(&r).lines().map(String::from).collect();
        (r["result"]["isError"] == true, lines)
    }

    #[test]
    fn a_batch_step_with_if_runs_only_when_its_condition_holds() {
        local(async {
            let mut s = session("batch-if");
            let (err, lines) = run_batch(
                &mut s,
                2,
                json!([
                    {"tool": "find", "args": {"app": "fake", "query": "OK"}},
                    {"tool": "act", "args": {"id": "$1"}, "if": {"step": 1, "found": true}},
                    {"tool": "find", "args": {"app": "fake", "query": "zzz"}},
                    {"tool": "key", "args": {"combo": "a"}, "if": {"step": 3, "found": true}},
                    {"tool": "key", "args": {"combo": "b"}, "if": {"step": 3, "contains": "NO MATCHES"}},
                    {"tool": "wait_for", "optional": true,
                     "args": {"app": "fake", "query": "Overwrite?", "timeout_ms": 0}},
                    {"tool": "key", "args": {"combo": "c"}, "if": {"step": 6, "ok": true}},
                    {"tool": "key", "args": {"combo": "d"}, "if": {"step": 6, "ok": false}},
                    // A skipped step meets no condition, and "$N" to it fails.
                    {"tool": "key", "args": {"combo": "e"}, "if": {"step": 7, "found": false}},
                    {"tool": "act", "args": {"id": "$4"}, "optional": true},
                ]),
            )
            .await;
            assert!(!err, "{lines:?}");
            assert!(lines[1].starts_with("[2] act: Press -> ok"), "{lines:?}");
            assert_eq!(lines[3], "[4] skipped (if step 3 found)");
            assert!(lines[4].starts_with("[5] key: sent b"), "{lines:?}");
            assert!(
                lines[5].starts_with("[6] wait_for failed (optional): "),
                "{lines:?}"
            );
            assert_eq!(lines[6], "[7] skipped (if step 6 ok)");
            assert!(lines[7].starts_with("[8] key: sent d"), "{lines:?}");
            assert_eq!(lines[8], "[9] skipped (if step 7 not found)");
            assert_eq!(
                lines[9],
                "[10] act failed (optional): `id` \"$4\": step 4 (key) was skipped"
            );
            assert!(lines[10].starts_with("now: "), "{lines:?}");
            assert_eq!(lines.len(), 11, "{lines:?}");
            assert_eq!(
                *s.keys.borrow(),
                ["action 0 on 7", "down b", "up b", "down d", "up d"]
            );
        });
    }

    #[test]
    fn on_fail_steps_run_only_when_their_step_failed() {
        local(async {
            let mut s = session("batch-on-fail");
            let (err, lines) = run_batch(
                &mut s,
                2,
                json!([
                    {"tool": "act", "args": {"app": "fake", "query": "Save"},
                     "on_fail": [{"tool": "key", "args": {"combo": "ctrl+s"}}]},
                    {"tool": "key", "args": {"combo": "a"},
                     "on_fail": [{"tool": "key", "args": {"combo": "x"}}]},
                    {"tool": "key", "args": {"combo": "b"}, "if": {"step": 1, "ok": false}},
                    {"tool": "mouse", "args": {"x": FAIL_X, "y": 0},
                     "on_fail": [
                        {"tool": "act", "args": {"app": "fake", "query": "zzz"}, "optional": true},
                        {"tool": "key", "args": {"combo": "c"}},
                     ]},
                ]),
            )
            .await;
            assert!(!err, "{lines:?}");
            assert!(lines[0].starts_with("[1] act failed: "), "{lines:?}");
            assert!(
                lines[1].starts_with("[1.on_fail] key: sent ctrl+s"),
                "{lines:?}"
            );
            assert!(lines[2].starts_with("[2] key: sent a"), "{lines:?}");
            assert!(lines[3].starts_with("[3] key: sent b"), "{lines:?}");
            assert_eq!(
                lines[4],
                "[4] mouse failed: pointer failed; maybe sent: look before retrying"
            );
            assert!(
                lines[5].starts_with("[4.on_fail.1] act failed (optional): "),
                "{lines:?}"
            );
            assert!(
                lines[6].starts_with("[4.on_fail.2] key: sent c"),
                "{lines:?}"
            );
            assert!(lines[7].starts_with("now: "), "{lines:?}");
            assert_eq!(
                *s.keys.borrow(),
                [
                    "down ctrl+s",
                    "up ctrl+s",
                    "down a",
                    "up a",
                    "down b",
                    "up b",
                    "down c",
                    "up c"
                ]
            );
            // An on_fail step failing stops the batch, and what is held is
            // released.
            s.keys.borrow_mut().clear();
            call_tool(&mut s, 3, "hold", json!({"key": "shift"})).await;
            let (err, lines) = run_batch(
                &mut s,
                4,
                json!([
                    {"tool": "act", "args": {"app": "fake", "query": "Save"},
                     "on_fail": [{"tool": "act", "args": {"app": "fake", "query": "Save as"}}]},
                    {"tool": "key", "args": {"combo": "a"}},
                ]),
            )
            .await;
            assert!(err, "{lines:?}");
            assert!(
                lines[1].starts_with("[1.on_fail] act failed: "),
                "{lines:?}"
            );
            assert_eq!(lines[2], "stopped; nothing sent");
            assert!(lines[3].starts_with("now: "), "{lines:?}");
            assert!(lines[3].ends_with("; released shift"), "{lines:?}");
            assert_eq!(*s.keys.borrow(), ["keydown shift", "keyup shift"]);
            // Never after the user stopped input: the failure stands.
            s.keys.borrow_mut().clear();
            s.takeover.stop();
            let (err, lines) = run_batch(
                &mut s,
                5,
                json!([
                    {"tool": "key", "args": {"combo": "a"}, "optional": true,
                     "on_fail": [{"tool": "find", "args": {"app": "fake", "query": "OK"}}]},
                ]),
            )
            .await;
            assert!(!err, "{lines:?}");
            assert!(
                lines[0].starts_with("[1] key failed (optional): stopped by the user"),
                "{lines:?}"
            );
            assert_eq!(lines[1], "[1.on_fail] not run (refused)");
            assert!(lines[2].starts_with("now: "), "{lines:?}");
        });
    }

    #[test]
    fn a_repeat_scrolls_until_an_element_shows_and_a_later_step_uses_it() {
        local(async {
            let mut s = session("batch-repeat-scroll");
            s.reveal.set(Some(3));
            let scroll_find = json!({"steps": [
                {"tool": "scroll", "args": {"app": "fake", "query": "OK", "observe": "effect"}},
                {"tool": "find", "args": {"app": "fake", "query": "report.pdf"}},
            ], "until": {"found": true}, "max": 5});
            let (err, lines) = run_batch(
                &mut s,
                2,
                json!([
                    {"repeat": scroll_find},
                    {"tool": "act", "args": {"id": "$1"}},
                    {"tool": "key", "args": {"combo": "a"}, "if": {"step": 1, "contains": "report"}},
                ]),
            )
            .await;
            assert!(!err, "{lines:?}");
            assert_eq!(lines[0], "[1] repeat: until met in run 3");
            assert!(
                lines[1].starts_with("[1.1] scroll: ") && lines[1].contains("effect: "),
                "{lines:?}"
            );
            let found = printed_ids(lines[2].strip_prefix("[1.2] find: ").unwrap())[0];
            assert_eq!(
                lines[2],
                format!("[1.2] find: [{found}] list item \"report.pdf\" actions=Press")
            );
            assert!(lines[3].starts_with("[2] act: Press -> ok"), "{lines:?}");
            assert!(lines[4].starts_with("[3] key: sent a"), "{lines:?}");
            assert_eq!(lines.len(), 5, "{lines:?}");
            assert_eq!(
                *s.keys.borrow(),
                [
                    "scroll 3",
                    "scroll 3",
                    "scroll 3",
                    "action 0 on 60",
                    "down a",
                    "up a"
                ]
            );
            // Never there within `max` runs: the repeat fails.
            let mut s = session("batch-repeat-max");
            s.reveal.set(Some(10));
            let (err, lines) = run_batch(
                &mut s,
                2,
                json!([
                    {"repeat": {"steps": [
                        {"tool": "scroll", "args": {"app": "fake", "query": "OK"}},
                        {"tool": "find", "args": {"app": "fake", "query": "report.pdf"}},
                    ], "until": {"found": true}, "max": 2}},
                    {"tool": "act", "args": {"id": "$1"}},
                ]),
            )
            .await;
            assert!(err, "{lines:?}");
            assert_eq!(
                lines[0],
                "Error executing tool batch: [1] repeat failed: until not met in 2 runs"
            );
            assert!(lines[1].starts_with("[1.1] scroll: "), "{lines:?}");
            assert_eq!(lines[2], "[1.2] find: no matches");
            assert_eq!(lines[3], "stopped; maybe sent: [1]; not sent: [2]");
            assert!(lines[4].starts_with("now: "), "{lines:?}");
            assert_eq!(*s.keys.borrow(), ["scroll 3", "scroll 3"]);
            // Optional: the batch goes on, but "$1" names a failed step.
            let (err, lines) = run_batch(
                &mut s,
                3,
                json!([
                    {"repeat": {"steps": [
                        {"tool": "find", "args": {"app": "fake", "query": "report.pdf"}},
                    ], "until": {"found": true}, "max": 1}, "optional": true},
                    {"tool": "act", "args": {"id": "$1"}, "optional": true},
                    {"tool": "key", "args": {"combo": "a"}, "if": {"step": 1, "ok": false}},
                ]),
            )
            .await;
            assert!(!err, "{lines:?}");
            assert_eq!(
                lines[0],
                "[1] repeat failed (optional): until not met in 1 run"
            );
            assert_eq!(
                lines[2],
                "[2] act failed (optional): `id` \"$1\": step 1 (repeat) failed"
            );
            assert!(lines[3].starts_with("[3] key: sent a"), "{lines:?}");
        });
    }

    #[test]
    fn a_repeat_presses_down_until_the_focus_is_right() {
        local(async {
            let mut s = session("batch-repeat-key");
            s.focus.borrow_mut().role = "list item".into();
            s.focus.borrow_mut().name = "Item 1".into();
            let (err, lines) = run_batch(
                &mut s,
                2,
                json!([
                    {"repeat": {"steps": [{"tool": "key", "args": {"combo": "down"}}],
                                "until": {"contains": "item 4\""}}},
                    {"tool": "key", "args": {"combo": "Return"}},
                ]),
            )
            .await;
            assert!(!err, "{lines:?}");
            assert_eq!(lines[0], "[1] repeat: until met in run 3");
            assert!(
                lines[1].starts_with("[1.1] key: sent down") && lines[1].contains("\"Item 4\""),
                "{lines:?}"
            );
            assert!(lines[2].starts_with("[2] key: sent Return"), "{lines:?}");
            assert_eq!(s.keys.borrow().len(), 8);
            // A failing inner step ends the repeat at once.
            let (err, lines) = run_batch(
                &mut s,
                3,
                json!([
                    {"repeat": {"steps": [
                        {"tool": "key", "args": {"combo": "down"}},
                        {"tool": "mouse", "args": {"x": FAIL_X, "y": 0}},
                    ], "until": {"ok": true}}},
                ]),
            )
            .await;
            assert!(err, "{lines:?}");
            assert_eq!(
                lines[0],
                "Error executing tool batch: [1] repeat failed: step 1.2 failed in run 1"
            );
            assert_eq!(
                lines[2],
                "[1.2] mouse failed: pointer failed; maybe sent: look before retrying"
            );
            // An optional one does not.
            let (err, lines) = run_batch(
                &mut s,
                4,
                json!([
                    {"repeat": {"steps": [
                        {"tool": "mouse", "args": {"x": FAIL_X, "y": 0}, "optional": true},
                        {"tool": "key", "args": {"combo": "down"}},
                    ], "until": {"contains": "item 7"}}},
                ]),
            )
            .await;
            assert!(!err, "{lines:?}");
            assert_eq!(lines[0], "[1] repeat: until met in run 2");
            assert_eq!(
                lines[1],
                "[1.1] mouse failed (optional): pointer failed; maybe sent: look before retrying"
            );
        });
    }

    #[test]
    fn a_repeat_reads_the_focus_once_the_input_took_effect() {
        local(async {
            let mut s = session("batch-repeat-lag");
            s.focus.borrow_mut().role = "list item".into();
            s.focus.borrow_mut().name = "Item 1".into();
            // The focus moves 30 ms after each key: read at once, the
            // reply would name the item before it, and until would hold
            // one press late, on Item 5.
            s.lag.set(30);
            let (err, lines) = run_batch(
                &mut s,
                2,
                json!([
                    {"repeat": {"steps": [{"tool": "key", "args": {"combo": "down"}}],
                                "until": {"contains": "item 4\""}}},
                ]),
            )
            .await;
            assert!(!err, "{lines:?}");
            assert_eq!(lines[0], "[1] repeat: until met in run 3");
            assert!(lines[1].contains("\"Item 4\""), "{lines:?}");
            assert_eq!(s.focus.borrow().name, "Item 4");
            // A step a condition tests settles too; others do not wait.
            let (err, lines) = run_batch(
                &mut s,
                3,
                json!([
                    {"tool": "key", "args": {"combo": "down"}},
                    {"tool": "key", "args": {"combo": "down"}},
                    {"tool": "key", "args": {"combo": "x"}, "if": {"step": 2, "contains": "item 6\""}},
                ]),
            )
            .await;
            assert!(!err, "{lines:?}");
            assert!(lines[2].starts_with("[3] key: sent x"), "{lines:?}");
        });
    }

    #[test]
    fn a_refused_step_runs_no_on_fail_and_ends_a_repeat() {
        local(async {
            let mut s = session("batch-refused");
            // The allowlist refuses: on_fail does not run, and says so.
            let (err, lines) = run_batch(
                &mut s,
                2,
                json!([
                    {"tool": "find", "args": {"app": "secret", "query": "Save"},
                     "on_fail": [{"tool": "key", "args": {"combo": "ctrl+s"}}]},
                ]),
            )
            .await;
            assert!(err, "{lines:?}");
            assert!(
                lines[0].contains("[1] find failed: app 'secret' is not in"),
                "{lines:?}"
            );
            assert_eq!(lines[1], "[1.on_fail] not run (refused)");
            assert!(s.keys.borrow().is_empty(), "{:?}", s.keys.borrow());
            // A miss whose message echoes "confirmation" is no refusal:
            // on_fail runs, and an optional one does not end a repeat.
            let (err, lines) = run_batch(
                &mut s,
                3,
                json!([
                    {"tool": "wait_for", "args": {"app": "fake", "query": "delete confirmation",
                                                  "timeout_ms": 0},
                     "on_fail": [{"tool": "key", "args": {"combo": "a"}}]},
                    {"repeat": {"steps": [
                        {"tool": "wait_for", "args": {"app": "fake", "query": "confirmation",
                                                      "timeout_ms": 0}, "optional": true},
                        {"tool": "key", "args": {"combo": "b"}},
                    ], "until": {"contains": "never"}, "max": 2}, "optional": true},
                ]),
            )
            .await;
            assert!(!err, "{lines:?}");
            assert!(lines[1].starts_with("[1.on_fail] key: sent a"), "{lines:?}");
            assert_eq!(
                lines[2],
                "[2] repeat failed (optional): until not met in 2 runs"
            );
            assert_eq!(
                *s.keys.borrow(),
                ["down a", "up a", "down b", "up b", "down b", "up b"]
            );
            // Nor does it lose the screen context of a plain call.
            let r = call_tool(
                &mut s,
                4,
                "wait_for",
                json!({"app": "fake", "query": "delete confirmation", "timeout_ms": 0}),
            )
            .await;
            assert!(reply_text(&r).contains("\nnow: "), "{r}");
        });
    }

    #[test]
    fn a_declined_confirmation_runs_no_on_fail_and_ends_a_repeat() {
        local(async {
            let mut s = session("batch-declined");
            set_allow("batch-declined", "fake\nconfirm: key:fake\n");
            send(
                &mut s,
                json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {"elicitation": {}},
                    "clientInfo": {"name": "test", "version": "0"}}}),
            )
            .await;
            replies_within(&mut s, Duration::from_millis(200)).await;
            let batches = [
                // A fallback must not do what the user just declined.
                json!([{"tool": "key", "args": {"combo": "ctrl+s"},
                        "on_fail": [{"tool": "mouse", "args": {"x": 5, "y": 5}}]}]),
                // Asked once, not on every run.
                json!([{"repeat": {"steps": [
                    {"tool": "key", "args": {"combo": "down"}, "optional": true},
                ], "until": {"contains": "never"}, "max": 20}}]),
            ];
            for (call, steps) in (2..).zip(batches) {
                send(
                    &mut s,
                    json!({"jsonrpc": "2.0", "id": call, "method": "tools/call",
                    "params": {"name": "batch", "arguments": {"steps": steps}}}),
                )
                .await;
                let ask = next(&mut s).await;
                assert_eq!(ask["method"], "elicitation/create", "{ask}");
                send(
                    &mut s,
                    json!({"jsonrpc": "2.0", "id": ask["id"], "result": {"action": "decline"}}),
                )
                .await;
                let r = next(&mut s).await;
                assert_eq!(r["id"], call, "asked again: {r}");
                assert_eq!(r["result"]["isError"], true, "{r}");
                let text = reply_text(&r);
                assert!(text.contains("the user declined key"), "{text}");
                match call {
                    2 => assert!(text.contains("[1.on_fail] not run (refused)"), "{text}"),
                    _ => assert!(text.contains("step 1.1 failed in run 1"), "{text}"),
                }
                assert!(s.keys.borrow().is_empty(), "{:?}", s.keys.borrow());
            }
        });
    }

    #[test]
    fn user_input_during_a_repeat_ends_it_without_on_fail() {
        local(async {
            let mut s = session("batch-repeat-takeover");
            let t = s.takeover.clone();
            tokio::task::spawn_local(async move {
                tokio::time::sleep(Duration::from_millis(150)).await;
                t.user_input(Instant::now(), takeover::Device::Pointer);
            });
            // Only nested steps send input: the batch still holds the input
            // lock and counts as one run.
            let (err, lines) = run_batch(
                &mut s,
                2,
                json!([
                    {"repeat": {"steps": [
                        {"tool": "key", "args": {"combo": "a"}, "optional": true},
                        {"tool": "pause", "args": {"ms": 60}},
                    ], "until": {"contains": "never"}, "max": 20},
                     "on_fail": [{"tool": "key", "args": {"combo": "z"}}]},
                ]),
            )
            .await;
            assert!(err, "{lines:?}");
            assert!(
                lines[0].starts_with(
                    "Error executing tool batch: [1] repeat failed: step 1.1 failed in run "
                ),
                "{lines:?}"
            );
            assert!(
                lines[1].starts_with("[1.1] key failed (optional): interrupted: "),
                "{lines:?}"
            );
            assert!(
                !s.keys.borrow().iter().any(|k| k.contains('z')),
                "{:?}",
                s.keys.borrow()
            );
        });
    }

    #[test]
    fn stop_if_ends_the_batch_early_and_successfully() {
        local(async {
            let mut s = session("batch-stop");
            let (err, lines) = run_batch(
                &mut s,
                2,
                json!([
                    {"tool": "find", "args": {"app": "fake", "query": "zzz"}},
                    {"stop_if": {"step": 1, "found": true}},
                    {"tool": "find", "args": {"app": "fake", "query": "OK"}},
                    {"stop_if": {"step": 3, "found": true}},
                    {"tool": "key", "args": {"combo": "a"}},
                    {"tool": "key", "args": {"combo": "b"}},
                ]),
            )
            .await;
            assert!(!err, "{lines:?}");
            assert_eq!(lines[0], "[1] find: no matches");
            // A stop_if that does not hold says nothing.
            assert!(lines[1].starts_with("[3] find: ["), "{lines:?}");
            assert_eq!(lines[2], "[4] stop (step 3 found); 2 step(s) not run");
            assert!(lines[3].starts_with("now: "), "{lines:?}");
            assert_eq!(lines.len(), 4, "{lines:?}");
            assert!(s.keys.borrow().is_empty());
        });
    }

    #[test]
    fn a_bad_batch_is_refused_before_anything_runs() {
        local(async {
            let mut s = session("batch-invalid");
            for (steps, why) in [
                (
                    json!([{"tool": "key", "args": {"combo": "a"}},
                           {"tool": "key", "args": {"combo": "b"}, "if": {"step": 3, "ok": true}}]),
                    "[2] if: `step` 3 is not an earlier step (1 to 1)",
                ),
                (
                    json!([{"tool": "key", "args": {"combo": "a"}}, {"tool": "kye"}]),
                    "[2] unknown tool 'kye'",
                ),
                (
                    json!([{"tool": "key", "args": {"combo": "a"}},
                           {"repeat": {"steps": [{"repeat": {}}], "until": {"ok": true}}}]),
                    "[2] repeat step 1: repeat cannot be nested; put it at the top level",
                ),
                (
                    json!([{"tool": "key", "args": {"combo": "a"}},
                           {"tool": "key", "args": {"combo": "b"}, "if": {"step": 1, "ok": false}}]),
                    "[2] if: {\"ok\": false} on step 1 never holds: the batch stops when it \
                     fails; mark it \"optional\": true or give it on_fail",
                ),
                (
                    json!([{"tool": "key", "args": {"combo": "a"}},
                           {"repeat": {"steps": [{"tool": "key"}], "until": {"ok": true}, "max": 50}}]),
                    "[2] repeat: `max` must be 1 to 20, not 50",
                ),
            ] {
                let (err, lines) = run_batch(&mut s, 2, steps).await;
                assert!(err);
                assert_eq!(
                    lines,
                    [format!(
                        "Error executing tool batch: not run (nothing sent): {why}"
                    )]
                );
            }
            assert!(s.keys.borrow().is_empty());
        });
    }

    #[test]
    fn a_missed_query_offers_the_closest_names_with_ids() {
        local(async {
            let mut s = session("closest");
            let r = call_tool(&mut s, 2, "find", json!({"app": "fake", "query": "okk"})).await;
            let text = reply_text(&r);
            assert!(text.starts_with("no matches; closest:\n["), "{r}");
            assert!(text.contains("push button \"OK\""), "{r}");
            let r = call_tool(&mut s, 3, "find", json!({"app": "fake", "query": "zzz"})).await;
            assert!(reply_text(&r).starts_with("no matches\nnow: "), "{r}");
            // A tool given `query` fails, naming the candidates.
            let r = call_tool(
                &mut s,
                4,
                "get_text",
                json!({"app": "fake", "query": "okk"}),
            )
            .await;
            let text = reply_text(&r);
            assert!(
                text.contains("matches 'okk'; closest:\n[") && text.contains("\"OK\""),
                "{r}"
            );
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
            assert_eq!(
                reply_text(&r),
                "Press -> ok; effect: no visible change",
                "{r}"
            );
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
    /// The user types every 100 ms for `ms`.
    fn keep_typing(t: &std::sync::Arc<takeover::Takeover>, ms: u64) {
        let t = t.clone();
        tokio::task::spawn_local(async move {
            let start = Instant::now();
            while start.elapsed() < Duration::from_millis(ms) {
                t.user_input(Instant::now(), takeover::Device::Keyboard);
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });
    }

    /// The one delivery marker of `text`: "nothing sent", "maybe sent" or
    /// none (sent), failing when it has several.
    fn marker(text: &str) -> &'static str {
        let none = text.matches(delivery::NOTHING_SENT).count();
        let maybe = text.matches(delivery::MAYBE_SENT).count();
        assert!(none + maybe <= 1, "{text}");
        match (none, maybe) {
            (1, _) => "no",
            (_, 1) => "maybe",
            _ => "yes",
        }
    }

    #[test]
    fn every_input_reply_says_whether_input_was_sent() {
        local(async {
            let mut s = session("delivery");
            // Sent: no marker.
            let r = call_tool(&mut s, 2, "act", json!({"app": "fake", "query": "OK"})).await;
            assert_eq!(r["result"]["isError"], false, "{r}");
            assert_eq!(marker(reply_text(&r)), "yes", "{r}");
            // Refused before sending: lookup miss, validation, a window
            // that does not match, the allowlist.
            for (n, tool, args) in [
                (3, "act", json!({"app": "fake", "query": "Nowhere"})),
                (4, "key", json!({})),
                (5, "hold", json!({"key": "a", "button": "left"})),
                (6, "activate_window", json!({"query": "zzz"})),
                (7, "click", json!({"app": "konsole", "query": "x"})),
                (8, "click", json!({"id": 999})),
            ] {
                let r = call_tool(&mut s, n, tool, args).await;
                assert_eq!(r["result"]["isError"], true, "{r}");
                assert_eq!(marker(reply_text(&r)), "no", "{tool}: {r}");
            }
            // A DoAction that got no reply may have acted: never "nothing".
            s.focus.borrow_mut().name = "Send timeout".into();
            let r = call_tool(&mut s, 9, "act", json!({"app": "fake", "query": "Send"})).await;
            assert_eq!(r["result"]["isError"], true, "{r}");
            assert_eq!(marker(reply_text(&r)), "maybe", "{r}");
            assert!(
                reply_text(&r)
                    .contains("DoAction got no reply in 2 s; maybe sent: look before retrying"),
                "{r}"
            );
            // The user keeps typing: refused, nothing sent, said once.
            keep_typing(&s.takeover, 4000);
            let r = call_tool(&mut s, 10, "key", json!({"combo": "a"})).await;
            assert_eq!(r["result"]["isError"], true, "{r}");
            assert_eq!(marker(reply_text(&r)), "no", "{r}");
        });
    }

    #[test]
    fn a_stopped_batch_says_which_steps_sent_input() {
        local(async {
            let mut s = session("delivery-batch");
            s.focus.borrow_mut().name = "Send timeout".into();
            let (err, lines) = run_batch(
                &mut s,
                2,
                json!([
                    {"tool": "key", "args": {"combo": "a"}},
                    {"tool": "find", "args": {"app": "fake", "query": "Send"}},
                    {"tool": "act", "args": {"app": "fake", "query": "Send"}},
                    {"tool": "key", "args": {"combo": "b"}},
                    {"tool": "key", "args": {"combo": "c"}},
                ]),
            )
            .await;
            assert!(err, "{lines:?}");
            assert!(
                lines[2]
                    .ends_with("DoAction got no reply in 2 s; maybe sent: look before retrying"),
                "{lines:?}"
            );
            assert_eq!(
                lines[3],
                "stopped; sent: [1]; maybe sent: [3]; not sent: [4..5]"
            );
            // A step that sent nothing before any input: the batch says so.
            let (err, lines) = run_batch(
                &mut s,
                3,
                json!([
                    {"tool": "act", "args": {"app": "fake", "query": "Nowhere"}},
                    {"tool": "key", "args": {"combo": "b"}},
                ]),
            )
            .await;
            assert!(err, "{lines:?}");
            assert!(lines[0].contains("; nothing sent"), "{lines:?}");
            assert_eq!(lines[1], "stopped; nothing sent");
        });
    }

    #[test]
    fn a_call_that_wrote_never_says_nothing_sent() {
        local(async {
            // A menu button opened through its action (sent), then its
            // entry is disabled: nothing selected, but the menu is open.
            let mut s = session("delivery-opened");
            fake_window(&s, &[10, 20]);
            {
                let mut n = s.nodes.borrow_mut();
                n.insert(
                    10,
                    with_actions(
                        fake_node("push button", "Sort", &[]),
                        &["Press", "Show Menu"],
                    ),
                );
                n.insert(20, fake_node("popup menu", "", &[21, 22]));
                n.insert(
                    21,
                    with_actions(fake_node("radio menu item", "By name", &[]), &["click"]),
                );
                let mut date =
                    with_actions(fake_node("radio menu item", "By date", &[]), &["click"]);
                date.states = vec!["disabled"];
                n.insert(22, date);
            }
            let q = json!({"app": "fake", "query": "Sort", "option": "date"});
            let r = call_tool(&mut s, 2, "act", q).await;
            assert!(is_error(&r), "{r}");
            let text = reply_text(&r);
            assert!(
                text.contains(
                    "opened it (Show Menu); \"By date\" is disabled; nothing selected (it may \
                     still be open: key escape closes it); maybe sent: look before retrying"
                ),
                "{text}"
            );
            assert_eq!(marker(text), "maybe", "{text}");
            // Selection sent but not shown, then the key fallback refused
            // by the tier: the Selection may have acted.
            let mut s = session("delivery-fallback");
            fake_combo(&s, false, "UTF-8", &["Toggle"]);
            s.sem.selection.set(Some(false));
            let allow = std::env::temp_dir().join(format!(
                "desktop-mcp-test-{}-delivery-fallback.txt",
                std::process::id()
            ));
            std::fs::write(&allow, "fake:click\n").unwrap();
            let r = call_tool(
                &mut s,
                2,
                "act",
                json!({"app": "fake", "query": "UTF-8", "role": "combo box", "option": "UTF-16"}),
            )
            .await;
            assert!(is_error(&r), "{r}");
            let text = reply_text(&r);
            assert!(
                text.contains(
                    "sent Selection, but it shows \"UTF-8\" afterwards; the key fallback:"
                ) && text.contains(
                    "needs the full tier ('fake' has click); nothing more sent; \
                         maybe sent: look before retrying"
                ),
                "{text}"
            );
            assert_eq!(marker(text), "maybe", "{text}");
            // No Selection interface: nothing went out, and the refusal
            // says so.
            s.sem.selection.set(None);
            let r = call_tool(
                &mut s,
                3,
                "act",
                json!({"app": "fake", "query": "UTF-8", "role": "combo box", "option": "UTF-16"}),
            )
            .await;
            assert!(is_error(&r), "{r}");
            assert_eq!(marker(reply_text(&r)), "no", "{r}");
        });
    }

    #[test]
    fn input_interrupted_part_way_says_how_far_it_got() {
        local(async {
            let mut s = session("delivery-interrupted");
            s.sem.user_after_presses.set(Some(2));
            let r = call_tool(&mut s, 2, "key", json!({"combo": "down", "repeat": 5})).await;
            assert!(is_error(&r), "{r}");
            let text = reply_text(&r);
            assert!(
                text.contains(
                    "key: interrupted: the user started using the keyboard; input stopped there \
                     after 2 of 5 presses of down; maybe sent: look before retrying"
                ),
                "{text}"
            );
            assert!(!text.contains("Retry when they stop"), "{text}");
            assert_eq!(marker(text), "maybe", "{text}");
            let downs = s.keys.borrow().iter().filter(|k| *k == "down down").count();
            assert_eq!(downs, 2);
        });
    }

    #[test]
    fn input_waits_out_a_pause_that_ends_soon() {
        local(async {
            let mut s = session("takeover-wait");
            // The user pressed Enter in their terminal 1.2 s ago.
            s.takeover.user_input(
                Instant::now() - Duration::from_millis(1200),
                takeover::Device::Keyboard,
            );
            let start = Instant::now();
            let r = call_tool(&mut s, 2, "key", json!({"combo": "a"})).await;
            assert_eq!(r["result"]["isError"], false, "{r}");
            let text = reply_text(&r);
            assert!(
                text.starts_with("waited ")
                    && text.contains(" ms for the user's input to stop; sent a x1"),
                "{text}"
            );
            let took = start.elapsed();
            assert!(took >= Duration::from_millis(250), "{took:?}");
            assert!(took < Duration::from_millis(1000), "{took:?}");
            assert_eq!(
                *s.keys.borrow(),
                [
                    "indicate paused while you use the keyboard",
                    "down a",
                    "up a"
                ]
            );
            // Nothing recent: no wait, no note.
            let r = call_tool(&mut s, 3, "key", json!({"combo": "b"})).await;
            assert!(reply_text(&r).starts_with("sent b x1"), "{r}");
        });
    }

    #[test]
    fn input_pauses_while_the_user_keeps_typing_and_reads_go_on() {
        local(async {
            let mut s = session("takeover-pause");
            keep_typing(&s.takeover, 4000);
            let start = Instant::now();
            let r = call_tool(&mut s, 2, "key", json!({"combo": "a"})).await;
            assert_eq!(r["result"]["isError"], true, "{r}");
            let text = reply_text(&r);
            assert!(
                text.contains("paused: the user is using the keyboard")
                    && text.contains("ms for them to stop); nothing sent"),
                "{text}"
            );
            // Given up once the pause could not end within the limit.
            let took = start.elapsed();
            assert!(took >= Duration::from_millis(900), "{took:?}");
            assert!(took <= takeover::WAIT_LIMIT, "{took:?}");
            assert!(
                s.keys
                    .borrow()
                    .iter()
                    .all(|k| k.starts_with("indicate paused")),
                "{:?}",
                s.keys.borrow()
            );
            // Reading and the clipboard are not held up.
            let start = Instant::now();
            let r = call_tool(&mut s, 3, "find", json!({"app": "fake", "query": "OK"})).await;
            assert_eq!(r["result"]["isError"], false, "{r}");
            let r = call_tool(&mut s, 4, "clipboard_set", json!({"text": "x"})).await;
            assert_eq!(r["result"]["isError"], false, "{r}");
            assert!(start.elapsed() < Duration::from_millis(300));
        });
    }

    #[test]
    fn the_stop_shortcut_ends_a_wait_for_the_user() {
        local(async {
            let mut s = session("takeover-wait-stop");
            s.takeover
                .user_input(Instant::now(), takeover::Device::Pointer);
            let t = s.takeover.clone();
            tokio::task::spawn_local(async move {
                tokio::time::sleep(Duration::from_millis(200)).await;
                t.stop();
            });
            let start = Instant::now();
            let r = call_tool(&mut s, 2, "click", json!({"x": 1, "y": 2})).await;
            assert!(reply_text(&r).contains("stopped by the user"), "{r}");
            assert!(start.elapsed() < Duration::from_millis(600));
            assert!(
                !s.keys.borrow().iter().any(|k| k.starts_with("click")),
                "{:?}",
                s.keys.borrow()
            );
        });
    }

    #[test]
    fn a_cancelled_wait_for_the_user_sends_nothing() {
        local(async {
            let mut s = session("takeover-wait-cancel");
            s.takeover
                .user_input(Instant::now(), takeover::Device::Keyboard);
            send(
                &mut s,
                json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                "params": {"name": "key", "arguments": {"combo": "a"}}}),
            )
            .await;
            tokio::time::sleep(Duration::from_millis(200)).await;
            send(
                &mut s,
                json!({"jsonrpc": "2.0", "method": "notifications/cancelled",
                "params": {"requestId": 2}}),
            )
            .await;
            tokio::time::sleep(Duration::from_millis(1600)).await;
            assert!(
                !s.keys.borrow().iter().any(|k| k == "down a"),
                "{:?}",
                s.keys.borrow()
            );
        });
    }

    #[test]
    fn user_input_before_a_batch_sends_anything_only_pauses_it() {
        local(async {
            let mut s = session("takeover-batch-wait");
            let t = s.takeover.clone();
            tokio::task::spawn_local(async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                t.user_input(Instant::now(), takeover::Device::Keyboard);
            });
            // The user typed while the batch read: its first input waits,
            // and the run starts then, so later steps go on.
            let steps = json!({"steps": [
                {"tool": "find", "args": {"app": "fake", "query": "OK"}},
                {"tool": "pause", "args": {"ms": 100}},
                {"tool": "key", "args": {"combo": "a"}},
                {"tool": "key", "args": {"combo": "b"}},
            ]});
            let r = call_tool(&mut s, 2, "batch", steps).await;
            assert_eq!(r["result"]["isError"], false, "{r}");
            let text = reply_text(&r);
            assert!(
                text.contains("[3] key: waited ") && text.contains("[4] key: sent b"),
                "{text}"
            );
            let keys: Vec<String> = s
                .keys
                .borrow()
                .iter()
                .filter(|k| !k.starts_with("indicate"))
                .cloned()
                .collect();
            assert_eq!(keys, ["down a", "up a", "down b", "up b"]);
        });
    }

    #[test]
    fn the_stop_shortcut_holds_until_activate_window() {
        local(async {
            let mut s = session("takeover-stop");
            s.takeover.stop();
            let r = call_tool(&mut s, 2, "key", json!({"combo": "a"})).await;
            assert!(
                reply_text(&r).contains("stopped by the user (Meta+Alt+Esc)"),
                "{r}"
            );
            // A batch step cannot resume.
            let steps = json!({"steps": [
                {"tool": "activate_window", "args": {"query": "todo"}},
                {"tool": "key", "args": {"combo": "b"}},
            ]});
            let r = call_tool(&mut s, 3, "batch", steps).await;
            let text = reply_text(&r);
            assert!(
                text.contains("[1] activate_window failed: stopped by the user")
                    && text.contains("\nstopped; nothing sent\n"),
                "{text}"
            );
            // Neither does `window` on an input tool.
            let r = call_tool(&mut s, 4, "key", json!({"combo": "c", "window": "todo"})).await;
            assert!(reply_text(&r).contains("stopped by the user"), "{r}");
            assert!(
                s.keys
                    .borrow()
                    .iter()
                    .all(|k| k.starts_with("indicate stopped")),
                "{:?}",
                s.keys.borrow()
            );
            s.keys.borrow_mut().clear();
            let r = call_tool(&mut s, 5, "activate_window", json!({"query": "todo"})).await;
            assert_eq!(r["result"]["isError"], false, "{r}");
            assert!(
                reply_text(&r).starts_with("resumed after the user's stop; "),
                "{r}"
            );
            let r = call_tool(&mut s, 6, "key", json!({"combo": "d"})).await;
            assert_eq!(r["result"]["isError"], false, "{r}");
            assert_eq!(
                *s.keys.borrow(),
                ["activate todo.txt — Fake", "down d", "up d"]
            );
        });
    }

    #[test]
    fn user_input_during_a_batch_stops_its_remaining_input() {
        local(async {
            let mut s = session("takeover-batch");
            let t = s.takeover.clone();
            tokio::task::spawn_local(async move {
                tokio::time::sleep(Duration::from_millis(150)).await;
                t.user_input(Instant::now(), takeover::Device::Pointer);
            });
            let steps = json!({"steps": [
                {"tool": "key", "args": {"combo": "a"}},
                {"tool": "pause", "args": {"ms": 300}},
                {"tool": "key", "args": {"combo": "b"}},
                {"tool": "find", "args": {"app": "fake", "query": "OK"}},
            ]});
            let r = call_tool(&mut s, 2, "batch", steps).await;
            assert_eq!(r["result"]["isError"], true, "{r}");
            let text = reply_text(&r);
            assert!(
                text.contains(
                    "[3] key failed: interrupted: the user started using the mouse; input \
                     stopped there; nothing sent\n\
                     stopped; sent: [1]; not sent: [3..4]"
                ),
                "{text}"
            );
            assert_eq!(
                *s.keys.borrow(),
                ["down a", "up a", "indicate paused while you use the mouse"]
            );
        });
    }

    #[test]
    fn the_stop_shortcut_ends_a_drag_and_releases_the_button() {
        local(async {
            let mut s = session("takeover-drag");
            let t = s.takeover.clone();
            tokio::task::spawn_local(async move {
                tokio::time::sleep(Duration::from_millis(40)).await;
                t.stop();
            });
            let r = call_tool(
                &mut s,
                2,
                "drag",
                json!({"from_x": 0, "from_y": 0, "to_x": 120, "to_y": 60}),
            )
            .await;
            assert!(reply_text(&r).contains("stopped by the user"), "{r}");
            let log = s.keys.borrow().clone();
            assert_eq!(log[..2], ["move 0,0", "press left"]);
            assert_eq!(log.last().unwrap(), "release left", "{log:?}");
            assert!(log.len() < 2 + DRAG_STEPS as usize + 1, "{log:?}");
        });
    }

    /// Replace the allowlist of the session made by `session(name)`.
    fn set_allow(name: &str, text: &str) {
        let file = std::env::temp_dir().join(format!(
            "desktop-mcp-test-{}-{name}.txt",
            std::process::id()
        ));
        std::fs::write(file, text).unwrap();
    }

    /// One call of each tool class against the fake app, by tool name.
    fn tier_calls() -> Vec<(&'static str, Json)> {
        vec![
            ("tree", json!({"app": "fake"})),
            ("find", json!({"app": "fake", "query": "OK"})),
            ("get_text", json!({"app": "fake", "query": "OK"})),
            ("screenshot", json!({"inline": false})),
            ("clipboard_get", json!({})),
            ("act", json!({"app": "fake", "query": "OK"})),
            ("click", json!({"app": "fake", "query": "OK"})),
            ("focus", json!({"app": "fake", "query": "OK"})),
            ("scroll", json!({"app": "fake", "query": "OK"})),
            ("mouse", json!({"x": 1, "y": 2})),
            (
                "drag",
                json!({"from_x": 0, "from_y": 0, "to_x": 5, "to_y": 5}),
            ),
            ("key", json!({"combo": "a"})),
            ("type_text", json!({"text": "hi"})),
            (
                "set_text",
                json!({"app": "fake", "query": "OK", "text": "x"}),
            ),
            ("clipboard_set", json!({"text": "x"})),
        ]
    }

    #[test]
    fn tiers_limit_each_tool_class() {
        let view = ["tree", "find", "get_text", "screenshot", "clipboard_get"];
        let click = ["act", "click", "focus", "scroll", "mouse", "drag"];
        for (tier, ok) in [
            ("view", view.to_vec()),
            ("click", [&view[..], &click[..]].concat()),
            ("full", Vec::new()),
        ] {
            local(async move {
                let name = format!("tier-{tier}");
                let mut s = session(&name);
                set_allow(&name, &format!("fake:{tier}\n"));
                for (i, (tool, args)) in tier_calls().into_iter().enumerate() {
                    let r = call_tool(&mut s, 10 + i as u64, tool, args).await;
                    let text = reply_text(&r);
                    let refused = text.contains("needs the");
                    if tier == "full" || ok.contains(&tool) {
                        assert!(!refused, "{tier}: {tool} refused: {text}");
                    } else {
                        assert!(refused, "{tier}: {tool} not refused: {text}");
                        assert!(
                            text.contains(&format!("'fake' is allowed with the {tier} tier")),
                            "{text}"
                        );
                    }
                }
            });
        }
    }

    #[test]
    fn a_view_tier_app_gets_no_input_and_nothing_is_sent() {
        local(async {
            let mut s = session("tier-input");
            set_allow("tier-input", "fake:view\nkonsole\n");
            let r = call_tool(&mut s, 2, "type_text", json!({"text": "rm -rf ~"})).await;
            assert_eq!(r["result"]["isError"], true, "{r}");
            assert!(
                reply_text(&r).contains("type_text needs the full tier"),
                "{r}"
            );
            let r = call_tool(&mut s, 3, "key", json!({"combo": "enter"})).await;
            assert_eq!(r["result"]["isError"], true, "{r}");
            assert!(s.keys.borrow().is_empty(), "{:?}", s.keys.borrow());
            // A batch step is checked like a call of its own.
            let r = call_tool(
                &mut s,
                4,
                "batch",
                json!({"steps": [{"tool": "find", "args": {"app": "fake", "query": "OK"}},
                                  {"tool": "act", "args": {"id": "$1"}}]}),
            )
            .await;
            assert!(reply_text(&r).contains("act needs the click tier"), "{r}");
            assert_eq!(actions_run(&s), 0);
            // apps says which tier an app has.
            assert!(policy::Policy::parse("fake:view").tier("fake") == Some(Tier::View));
        });
    }

    #[test]
    fn password_fields_are_not_read_or_filled() {
        local(async {
            let mut s = session("password");
            *s.focus.borrow_mut() = Info {
                role: "password text".into(),
                name: "Password".into(),
                showing: true,
                states: vec!["focused", "editable"],
                text: Some("hunter2".into()),
                ..Info::default()
            };
            let r = call_tool(&mut s, 2, "find", json!({"app": "fake", "query": "pass"})).await;
            assert!(!reply_text(&r).contains("hunter2"), "{r}");
            let r = call_tool(
                &mut s,
                3,
                "get_text",
                json!({"app": "fake", "query": "Password"}),
            )
            .await;
            assert_eq!(r["result"]["isError"], true, "{r}");
            assert!(reply_text(&r).contains("never read"), "{r}");
            let r = call_tool(
                &mut s,
                4,
                "set_text",
                json!({"app": "fake", "query": "Password", "text": "x"}),
            )
            .await;
            assert_eq!(r["result"]["isError"], true, "{r}");
            assert!(reply_text(&r).contains("set_text refused"), "{r}");
            let r = call_tool(&mut s, 5, "type_text", json!({"text": "x"})).await;
            assert_eq!(r["result"]["isError"], true, "{r}");
            assert!(reply_text(&r).contains("type_text refused"), "{r}");
            // The user's opt-in for this app.
            set_allow("password", "fake\npasswords: fake\n");
            let r = call_tool(
                &mut s,
                6,
                "set_text",
                json!({"app": "fake", "query": "Password", "text": "x"}),
            )
            .await;
            assert!(reply_text(&r).contains("cannot be read back"), "{r}");
            let r = call_tool(&mut s, 7, "type_text", json!({"text": "x"})).await;
            assert_eq!(r["result"]["isError"], false, "{r}");
            // Still never read.
            let r = call_tool(
                &mut s,
                8,
                "get_text",
                json!({"app": "fake", "query": "Password"}),
            )
            .await;
            assert_eq!(r["result"]["isError"], true, "{r}");
        });
    }

    #[test]
    fn confirm_needs_confirm_true_without_elicitation() {
        local(async {
            let mut s = session("confirm-arg");
            set_allow("confirm-arg", "fake\nconfirm: key:fake, tree\n");
            let r = call_tool(&mut s, 2, "key", json!({"combo": "a"})).await;
            assert_eq!(r["result"]["isError"], true, "{r}");
            assert!(reply_text(&r).contains("confirm: true"), "{r}");
            assert!(s.keys.borrow().is_empty());
            let r = call_tool(&mut s, 3, "key", json!({"combo": "a", "confirm": true})).await;
            assert_eq!(r["result"]["isError"], false, "{r}");
            assert_eq!(*s.keys.borrow(), ["down a", "up a"]);
            // Other tools and apps are not asked about.
            let r = call_tool(&mut s, 4, "find", json!({"app": "fake", "query": "OK"})).await;
            assert_eq!(r["result"]["isError"], false, "{r}");
            // A tool that reaches no app through permit.
            let r = call_tool(&mut s, 5, "tree", json!({"app": "fake"})).await;
            assert_eq!(r["result"]["isError"], true, "{r}");
        });
    }

    /// The next message the server writes.
    async fn next(s: &mut Session) -> Json {
        let line = tokio::time::timeout(Duration::from_secs(2), s.stdout.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        serde_json::from_str(&line).unwrap()
    }

    #[test]
    fn confirm_asks_the_user_through_elicitation() {
        local(async {
            let mut s = session("elicit");
            set_allow("elicit", "fake\nconfirm: key:fake\n");
            send(
                &mut s,
                json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {"elicitation": {}},
                    "clientInfo": {"name": "test", "version": "0"}}}),
            )
            .await;
            let init = replies_within(&mut s, Duration::from_millis(200)).await;
            assert_eq!(ids(&init), vec![json!(1)]);
            for (call, answer, pressed) in [
                (
                    2,
                    json!({"action": "accept", "content": {"confirm": true}}),
                    true,
                ),
                (3, json!({"action": "decline"}), false),
                (
                    4,
                    json!({"action": "accept", "content": {"confirm": false}}),
                    false,
                ),
                (5, json!({"action": "cancel"}), false),
            ] {
                s.keys.borrow_mut().clear();
                send(
                    &mut s,
                    json!({"jsonrpc": "2.0", "id": call, "method": "tools/call",
                    "params": {"name": "key", "arguments": {"combo": "enter", "confirm": true}}}),
                )
                .await;
                let ask = next(&mut s).await;
                assert_eq!(ask["method"], "elicitation/create", "{ask}");
                let message = ask["params"]["message"].as_str().unwrap();
                assert!(
                    message.contains("key on fake {\"combo\":\"enter\"}"),
                    "{message}"
                );
                assert!(s.keys.borrow().is_empty(), "sent before the answer");
                // Other messages are answered while the user decides.
                send(&mut s, ping(100 + call)).await;
                assert_eq!(next(&mut s).await["id"], 100 + call);
                send(
                    &mut s,
                    json!({"jsonrpc": "2.0", "id": ask["id"], "result": answer}),
                )
                .await;
                let r = next(&mut s).await;
                assert_eq!(r["id"], call, "{r}");
                assert_eq!(r["result"]["isError"], !pressed, "{r}");
                assert_eq!(s.keys.borrow().is_empty(), !pressed, "{r}");
                if !pressed {
                    assert!(reply_text(&r).contains("nothing was done"), "{r}");
                }
            }
            // An error response refuses too.
            send(
                &mut s,
                json!({"jsonrpc": "2.0", "id": 9, "method": "tools/call",
                "params": {"name": "key", "arguments": {"combo": "a"}}}),
            )
            .await;
            let ask = next(&mut s).await;
            send(
                &mut s,
                json!({"jsonrpc": "2.0", "id": ask["id"],
                       "error": {"code": -32601, "message": "nope"}}),
            )
            .await;
            let r = next(&mut s).await;
            assert!(reply_text(&r).contains("asking failed"), "{r}");
            // A call waiting for the user is cancelled cleanly, and end of
            // input ends it too.
            send(
                &mut s,
                json!({"jsonrpc": "2.0", "id": 10, "method": "tools/call",
                "params": {"name": "key", "arguments": {"combo": "a"}}}),
            )
            .await;
            let _ask = next(&mut s).await;
            drop(std::mem::replace(&mut s.stdin, tokio::io::duplex(1).0));
            let replies = replies_within(&mut s, Duration::from_secs(2)).await;
            assert_eq!(ids(&replies), vec![json!(10)], "{replies:?}");
            assert!(reply_text(&replies[0]).contains("closed"), "{replies:?}");
            assert!(s.keys.borrow().is_empty());
        });
    }

    /// The fake window as a frame with two buttons, "Save" and "Open",
    /// and change events on.
    fn fake_reads(s: &Session) {
        *s.focus.borrow_mut() = Info {
            role: "frame".into(),
            name: "notes.txt — Fake".into(),
            showing: true,
            children: vec![30, 31],
            ..Info::default()
        };
        let mut nodes = s.nodes.borrow_mut();
        nodes.insert(30, fake_node("push button", "Save", &[]));
        nodes.insert(31, fake_node("push button", "Open", &[]));
        s.reads.live.set(true);
    }

    fn prefetches(s: &Session) -> u32 {
        s.reads.prefetches.get()
    }

    #[test]
    fn a_read_is_reused_while_the_app_reports_no_change() {
        local(async {
            let mut s = session("reads-reuse");
            fake_reads(&s);
            let find = json!({"app": "fake", "query": "Save"});
            let first = reply_text(&call_tool(&mut s, 2, "find", find.clone()).await).to_string();
            assert_eq!(first, "[1] push button \"Save\"");
            let read = prefetches(&s);
            assert!(read > 0);
            // Nothing changed: same answer, nothing read.
            let r = call_tool(&mut s, 3, "find", find.clone()).await;
            assert_eq!(reply_text(&r), first);
            let r = call_tool(&mut s, 4, "get_text", find.clone()).await;
            assert!(
                reply_text(&r).starts_with("[1] push button \"Save\""),
                "{r}"
            );
            let tree = json!({"app": "fake"});
            let t1 = reply_text(&call_tool(&mut s, 5, "tree", tree.clone()).await).to_string();
            let tree_read = prefetches(&s);
            let t2 = reply_text(&call_tool(&mut s, 6, "tree", tree.clone()).await).to_string();
            assert_eq!(t1, t2);
            assert_eq!(prefetches(&s), tree_read, "the tree was read again");
            // The app reported a change: read afresh, and it shows.
            s.nodes.borrow_mut().get_mut(&31).unwrap().name = "Save as".into();
            s.reads.changes.set(1);
            let r = call_tool(&mut s, 7, "find", find.clone()).await;
            assert!(reply_text(&r).contains("push button \"Save as\""), "{r}");
            assert!(prefetches(&s) > tree_read);
            let t3 = reply_text(&call_tool(&mut s, 8, "tree", tree).await).to_string();
            assert!(t3.contains("Save as"), "{t3}");
        });
    }

    #[test]
    fn input_and_user_activity_drop_kept_reads() {
        local(async {
            let mut s = session("reads-input");
            fake_reads(&s);
            let find = json!({"app": "fake", "query": "Save"});
            call_tool(&mut s, 2, "find", find.clone()).await;
            let read = prefetches(&s);
            call_tool(&mut s, 3, "find", find.clone()).await;
            assert_eq!(prefetches(&s), read);
            // Any tool that may change what the app shows.
            let mut call = 4;
            for (tool, args) in [
                ("key", json!({"combo": "a"})),
                ("act", json!({"app": "fake", "query": "Open"})),
                ("activate_window", json!({"query": "notes"})),
                (
                    "batch",
                    json!({"steps": [{"tool": "key", "args": {"combo": "b"}}]}),
                ),
                ("screenshot", json!({"window": "notes"})),
            ] {
                let r = call_tool(&mut s, call, tool, args).await;
                assert_ne!(r["result"]["isError"], true, "{tool}: {r}");
                let before = prefetches(&s);
                call_tool(&mut s, call + 1, "find", find.clone()).await;
                assert!(prefetches(&s) > before, "kept read reused after {tool}");
                call += 2;
            }
            // Read-only tools keep it.
            let before = prefetches(&s);
            call_tool(&mut s, call, "get_text", find.clone()).await;
            call_tool(&mut s, call + 1, "find", find.clone()).await;
            assert_eq!(prefetches(&s), before);
            // The user used the keyboard or mouse: scrolling shows other
            // elements without an event.
            s.takeover
                .user_input(Instant::now(), takeover::Device::Pointer);
            call_tool(&mut s, call + 2, "find", find.clone()).await;
            assert!(prefetches(&s) > before);
        });
    }

    #[test]
    fn reads_are_not_kept_without_events_or_while_loading() {
        local(async {
            let mut s = session("reads-off");
            fake_reads(&s);
            s.reads.live.set(false);
            let find = json!({"app": "fake", "query": "Save"});
            call_tool(&mut s, 2, "find", find.clone()).await;
            let read = prefetches(&s);
            call_tool(&mut s, 3, "find", find.clone()).await;
            assert!(prefetches(&s) > read, "reused without change events");
            s.reads.live.set(true);
            s.reads.loading.set(true);
            call_tool(&mut s, 4, "find", find.clone()).await;
            let read = prefetches(&s);
            call_tool(&mut s, 5, "find", find.clone()).await;
            assert!(prefetches(&s) > read, "reused while loading");
            s.reads.loading.set(false);
            call_tool(&mut s, 6, "find", find.clone()).await;
            let read = prefetches(&s);
            call_tool(&mut s, 7, "find", find.clone()).await;
            assert_eq!(prefetches(&s), read);
        });
    }

    /// `fake_reads`, as a web page (Firefox's toolkit) whose app sends
    /// change events.
    fn fake_page(s: &Session) {
        fake_reads(s);
        s.reads.events.set(true);
        *s.sem.toolkit.borrow_mut() = "gecko".into();
    }

    #[test]
    fn a_press_on_a_web_page_waits_for_its_new_content() {
        local(async {
            let mut s = session("content-act");
            fake_page(&s);
            let save = json!({"app": "fake", "query": "Save"});
            call_tool(&mut s, 2, "find", save.clone()).await;
            // "Next page": the page changes after a network round trip.
            s.reads
                .next
                .replace(vec![(250, Some((30, "Saved".into()))), (300, None)]);
            let start = Instant::now();
            let r = call_tool(&mut s, 3, "act", json!({"app": "fake", "query": "Open"})).await;
            let text = reply_text(&r);
            assert!(
                text.contains("Press -> ok; page changed (settled after ")
                    && text.ends_with("; effect: changed"),
                "{text}"
            );
            let took = start.elapsed();
            assert!(took >= Duration::from_millis(450), "{took:?}");
            assert!(took < Duration::from_millis(1200), "{took:?}");
            // The next read sees the new content, not the kept old read.
            let r = call_tool(&mut s, 4, "find", json!({"app": "fake", "query": "Saved"})).await;
            assert!(reply_text(&r).contains("push button \"Saved\""), "{r}");
        });
    }

    #[test]
    fn without_a_web_page_input_does_not_wait_for_content() {
        local(async {
            let mut s = session("content-native");
            fake_page(&s);
            *s.sem.toolkit.borrow_mut() = "qt".into();
            call_tool(&mut s, 2, "find", json!({"app": "fake", "query": "Save"})).await;
            s.reads
                .next
                .replace(vec![(250, Some((30, "Saved".into())))]);
            let r = call_tool(&mut s, 3, "act", json!({"app": "fake", "query": "Open"})).await;
            let text = reply_text(&r);
            assert!(!text.contains("page"), "{text}");
            // Read before the app changed: what it showed then...
            let saved = json!({"app": "fake", "query": "Saved"});
            let r = call_tool(&mut s, 4, "find", saved.clone()).await;
            assert!(reply_text(&r).starts_with("no matches"), "{r}");
            // ...and the change event drops that read once it comes.
            tokio::time::sleep(Duration::from_millis(300)).await;
            let r = call_tool(&mut s, 5, "find", saved).await;
            assert!(reply_text(&r).contains("push button \"Saved\""), "{r}");
        });
    }

    #[test]
    fn without_events_the_effect_line_sees_a_display_change() {
        local(async {
            let mut s = session("content-effect");
            s.reads.content.set(true);
            fake_window(&s, &[41]);
            {
                let mut n = s.nodes.borrow_mut();
                n.insert(41, fake_node("push button", "Plus", &[]));
            }
            // The window's text changes after the press; focus and title
            // stay. (The fake's prefetch reads only the roots it is given.)
            let focus = s.focus.clone();
            let later = tokio::task::spawn_local(async move {
                tokio::time::sleep(Duration::from_millis(60)).await;
                focus.borrow_mut().text = Some("12".into());
            });
            let r = call_tool(&mut s, 2, "act", json!({"app": "fake", "query": "Plus"})).await;
            later.await.unwrap();
            assert!(reply_text(&r).ends_with("; effect: changed"), "{r}");
            // Nothing changes: said so after the settle.
            let start = Instant::now();
            let r = call_tool(&mut s, 3, "act", json!({"app": "fake", "query": "Plus"})).await;
            assert!(
                reply_text(&r).ends_with("; effect: no visible change"),
                "{r}"
            );
            assert!(start.elapsed() < Duration::from_millis(500));
        });
    }

    #[test]
    fn content_waits_are_short_or_absent_when_nothing_changes() {
        local(async {
            let mut s = session("content-cost");
            fake_page(&s);
            // A plain key: no wait at all (the fake key takes 50 ms).
            let start = Instant::now();
            let r = call_tool(&mut s, 2, "key", json!({"combo": "a"})).await;
            assert!(!reply_text(&r).contains("page"), "{r}");
            assert!(start.elapsed() < Duration::from_millis(150));
            // Enter submits: waits for the page.
            s.reads.next.replace(vec![(100, None)]);
            let r = call_tool(&mut s, 3, "key", json!({"combo": "Return"})).await;
            assert!(
                reply_text(&r).contains("; page changed (settled after"),
                "{r}"
            );
            // Nothing changes: bounded by settle_ms.
            let start = Instant::now();
            let r = call_tool(
                &mut s,
                4,
                "act",
                json!({"app": "fake", "query": "Open", "settle_ms": 200}),
            )
            .await;
            assert!(reply_text(&r).contains("; no page change seen in 2"), "{r}");
            // ...and says a real click may be what it needs.
            assert!(reply_text(&r).contains("only for a real click"), "{r}");
            assert!(start.elapsed() < Duration::from_millis(500));
            // settle_ms 0: no content wait.
            let r = call_tool(
                &mut s,
                5,
                "act",
                json!({"app": "fake", "query": "Open", "settle_ms": 0}),
            )
            .await;
            assert!(!reply_text(&r).contains("page"), "{r}");
            // A click into a text field is for typing: no wait.
            s.nodes
                .borrow_mut()
                .insert(30, fake_node("entry", "Search", &[]));
            let start = Instant::now();
            let r = call_tool(
                &mut s,
                6,
                "click",
                json!({"app": "fake", "query": "Search"}),
            )
            .await;
            assert!(!reply_text(&r).contains("page"), "{r}");
            assert!(start.elapsed() < Duration::from_millis(150));
            // A click on a button waits.
            s.reads.next.replace(vec![(100, None)]);
            let r = call_tool(&mut s, 7, "click", json!({"app": "fake", "query": "Open"})).await;
            assert!(
                reply_text(&r).contains("; page changed (settled after"),
                "{r}"
            );
        });
    }

    #[test]
    fn a_content_wait_goes_on_while_the_page_loads() {
        local(async {
            let mut s = session("content-loading");
            fake_page(&s);
            s.reads.next.replace(vec![(50, None)]);
            s.reads.loading.set(true);
            let reads = s.reads.clone();
            tokio::task::spawn_local(async move {
                tokio::time::sleep(Duration::from_millis(600)).await;
                reads.loading.set(false);
            });
            let start = Instant::now();
            let r = call_tool(&mut s, 2, "act", json!({"app": "fake", "query": "Open"})).await;
            assert!(
                reply_text(&r).contains("; page changed (settled after"),
                "{r}"
            );
            assert!(start.elapsed() >= Duration::from_millis(600));
            // Still loading at the limit: said so.
            s.reads.next.replace(vec![(50, None)]);
            s.reads.loading.set(true);
            let r = call_tool(
                &mut s,
                3,
                "act",
                json!({"app": "fake", "query": "Open", "settle_ms": 400}),
            )
            .await;
            assert!(
                reply_text(&r).contains("; page changed, still loading after 4"),
                "{r}"
            );
        });
    }

    #[test]
    fn toggles_and_typing_get_no_content_wait() {
        local(async {
            let mut s = session("content-toggles");
            fake_page(&s);
            {
                let mut nodes = s.nodes.borrow_mut();
                nodes.insert(30, fake_node("check box", "Remember me", &[]));
                let mut body = fake_node("section", "Message body", &[]);
                body.states = vec!["editable", "multi line"];
                nodes.insert(31, body);
            }
            // A check box only toggles: its state change is no change event.
            for (i, tool) in [(2, "act"), (3, "click")] {
                let start = Instant::now();
                let r = call_tool(
                    &mut s,
                    i,
                    tool,
                    json!({"app": "fake", "query": "Remember me"}),
                )
                .await;
                assert!(!reply_text(&r).contains("page"), "{r}");
                assert!(start.elapsed() < Duration::from_millis(300), "{tool}");
            }
            // A click into an editable section (a rich text editor) is for
            // typing.
            let start = Instant::now();
            let r = call_tool(
                &mut s,
                4,
                "click",
                json!({"app": "fake", "query": "Message body"}),
            )
            .await;
            assert!(!reply_text(&r).contains("page"), "{r}");
            assert!(start.elapsed() < Duration::from_millis(300));
            // Enter in a terminal or a multi-line field types a line...
            for (i, role, states) in [
                (5, "terminal", vec!["focused", "editable"]),
                (6, "entry", vec!["focused", "editable", "multi line"]),
            ] {
                {
                    let mut f = s.focus.borrow_mut();
                    f.role = role.into();
                    f.states = states;
                }
                s.reads.next.replace(vec![(100, None)]);
                let start = Instant::now();
                let r = call_tool(&mut s, i, "key", json!({"combo": "Return"})).await;
                assert!(!reply_text(&r).contains("page"), "{role}: {r}");
                assert!(start.elapsed() < Duration::from_millis(250), "{role}");
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
            // ...in a single-line one (a search field) it submits.
            s.focus.borrow_mut().states = vec!["focused", "editable"];
            s.reads.next.replace(vec![(150, None)]);
            let r = call_tool(&mut s, 7, "key", json!({"combo": "Return"})).await;
            assert!(
                reply_text(&r).contains("; page changed (settled after"),
                "{r}"
            );
        });
    }

    #[test]
    fn a_content_wait_outlasts_a_placeholder_before_the_new_content() {
        local(async {
            let mut s = session("content-early");
            fake_page(&s);
            call_tool(&mut s, 2, "find", json!({"app": "fake", "query": "Save"})).await;
            // A spinner at once, the content after a network round trip.
            s.reads
                .next
                .replace(vec![(10, None), (300, Some((30, "Saved".into())))]);
            let start = Instant::now();
            let r = call_tool(&mut s, 3, "act", json!({"app": "fake", "query": "Open"})).await;
            assert!(
                reply_text(&r).contains("; page changed (settled after"),
                "{r}"
            );
            assert!(start.elapsed() >= Duration::from_millis(600));
            let r = call_tool(&mut s, 4, "find", json!({"app": "fake", "query": "Saved"})).await;
            assert!(reply_text(&r).contains("push button \"Saved\""), "{r}");
        });
    }

    #[test]
    fn a_content_wait_is_short_while_the_app_never_goes_quiet() {
        local(async {
            let mut s = session("content-noise");
            fake_page(&s);
            // Another tab's clock or video: a change every 20 ms.
            let shown = s.shown.clone();
            let ticking = Rc::new(std::cell::Cell::new(true));
            let ticks = ticking.clone();
            tokio::task::spawn_local(async move {
                while ticks.get() {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    shown.seq.set(shown.seq.get() + 1);
                }
            });
            let start = Instant::now();
            let r = call_tool(&mut s, 2, "act", json!({"app": "fake", "query": "Open"})).await;
            let took = start.elapsed();
            assert!(reply_text(&r).contains("still changing after "), "{r}");
            assert!(took < Duration::from_millis(900), "{took:?}");
            // The app changed between the call's start and the input: that
            // is not the page's change.
            s.reads.noise.set(true);
            let start = Instant::now();
            let r = call_tool(&mut s, 3, "act", json!({"app": "fake", "query": "Open"})).await;
            assert!(
                reply_text(&r).contains("; app was already changing; still changing after "),
                "{r}"
            );
            assert!(start.elapsed() < Duration::from_millis(900));
            ticking.set(false);
            tokio::time::sleep(Duration::from_millis(50)).await;
            // Once it is quiet, the wait ends at the first quiet.
            let r = call_tool(&mut s, 4, "act", json!({"app": "fake", "query": "Open"})).await;
            assert!(
                reply_text(&r).contains("; app was already changing; quiet after "),
                "{r}"
            );
        });
    }

    #[test]
    fn kept_reads_expire() {
        local(async {
            let mut s = session("reads-age");
            fake_reads(&s);
            let find = json!({"app": "fake", "query": "Save"});
            call_tool(&mut s, 2, "find", find.clone()).await;
            let read = prefetches(&s);
            tokio::time::sleep(READ_MAX_AGE + Duration::from_millis(50)).await;
            call_tool(&mut s, 3, "find", find).await;
            assert!(prefetches(&s) > read);
        });
    }

    #[test]
    fn an_element_from_a_kept_read_is_checked_before_use() {
        local(async {
            let mut s = session("reads-check");
            fake_reads(&s);
            let open = json!({"app": "fake", "query": "Open"});
            call_tool(&mut s, 2, "get_text", open.clone()).await;
            // Fresh reads need no check.
            assert_eq!(s.reads.checks.get(), 0);
            let read = prefetches(&s);
            let r = call_tool(&mut s, 3, "act", open.clone()).await;
            // `act` dropped the kept read: read afresh, no check either.
            assert!(reply_text(&r).contains("Press -> ok"), "{r}");
            assert!(prefetches(&s) > read);
            assert_eq!(s.reads.checks.get(), 0);
            call_tool(&mut s, 4, "get_text", open.clone()).await;
            let read = prefetches(&s);
            let r = call_tool(&mut s, 5, "get_text", open.clone()).await;
            assert_eq!(prefetches(&s), read);
            assert_eq!(s.reads.checks.get(), 1);
            assert!(
                reply_text(&r).starts_with("[1] push button \"Open\""),
                "{r}"
            );
            // Gone without an event: the check fails, read afresh.
            s.reads.gone.set(true);
            let r = call_tool(&mut s, 6, "get_text", open).await;
            assert_eq!(s.reads.checks.get(), 2);
            assert!(prefetches(&s) > read);
            assert!(
                reply_text(&r).starts_with("[1] push button \"Open\""),
                "{r}"
            );
        });
    }

    /// The fake app's first window: a frame "notes.txt — Fake" (active
    /// with `active`) holding `kids`.
    fn fake_frame(s: &Session, active: bool, kids: &[u32]) {
        *s.focus.borrow_mut() = Info {
            states: if active { vec!["active"] } else { vec![] },
            ..fake_node("frame", "notes.txt — Fake", kids)
        };
    }

    /// The message of an error reply, without the "Error executing tool"
    /// prefix.
    fn error_text(r: &Json) -> &str {
        assert_eq!(r["result"]["isError"], true, "{r}");
        let text = reply_text(r);
        text.split_once(": ").map_or(text, |(_, m)| m)
    }

    #[test]
    fn an_ambiguous_query_is_refused_by_input_tools_with_the_candidates() {
        local(async {
            let mut s = session("ambiguous");
            fake_frame(&s, true, &[40, 41, 42]);
            {
                let mut n = s.nodes.borrow_mut();
                n.insert(40, fake_node("push button", "Delete", &[]));
                n.insert(41, fake_node("push button", "Delete", &[]));
                // Only shows the name: a button of the name beats it.
                let mut label = fake_node("label", "Delete", &[]);
                label.actions.clear();
                n.insert(42, label);
            }
            let q = json!({"app": "fake", "query": "Delete"});
            let r = call_tool(&mut s, 2, "act", q.clone()).await;
            // The read has no action names (as on Linux): the lines have
            // them read, so a button does not say `actions=none`.
            assert_eq!(
                error_text(&r),
                "ambiguous: 2 elements in 'fake' match 'Delete'; nothing sent. pass one id:\n\
                 [1] push button \"Delete\" at 0,0\n\
                 [2] push button \"Delete\" at 0,0"
            );
            let mut call = 3;
            for tool in ["click", "focus", "scroll"] {
                let r = call_tool(&mut s, call, tool, q.clone()).await;
                assert!(error_text(&r).starts_with("ambiguous: 2 elements"), "{r}");
                call += 1;
            }
            let r = call_tool(
                &mut s,
                call,
                "set_text",
                json!({"app": "fake", "query": "Delete", "text": "x"}),
            )
            .await;
            assert!(error_text(&r).starts_with("ambiguous: 2 elements"), "{r}");
            // Nothing was sent.
            assert!(s.keys.borrow().is_empty(), "{:?}", s.keys.borrow());
            // A read takes the first and says so.
            let r = call_tool(&mut s, 10, "get_text", q.clone()).await;
            assert!(
                reply_text(&r).starts_with("[1] push button \"Delete\" (first of 2 matches):"),
                "{r}"
            );
            // The id from the list acts.
            let r = call_tool(&mut s, 11, "act", json!({"id": 2})).await;
            assert_ne!(r["result"]["isError"], true, "{r}");
            assert_eq!(s.keys.borrow().last().unwrap(), "action 0 on 41");
            // One of them has no on-screen area: the other is the target.
            s.no_area.borrow_mut().push(41);
            let r = call_tool(&mut s, 12, "act", q.clone()).await;
            assert!(
                reply_text(&r).starts_with("[1] push button \"Delete\""),
                "{r}"
            );
            assert_eq!(s.keys.borrow().last().unwrap(), "action 0 on 40");
            // The same with the app's window behind the terminal, where a
            // centre is not measured (KDE, Hyprland): the size still counts.
            s.background.set(true);
            s.keys.borrow_mut().clear();
            let r = call_tool(&mut s, 13, "act", q.clone()).await;
            assert_ne!(r["result"]["isError"], true, "{r}");
            assert_eq!(s.keys.borrow().last().unwrap(), "action 0 on 40");
            // Both with an area there: refused, without positions.
            s.no_area.borrow_mut().clear();
            let r = call_tool(&mut s, 14, "act", q.clone()).await;
            assert_eq!(
                error_text(&r),
                "ambiguous: 2 elements in 'fake' match 'Delete'; nothing sent. pass one id:\n\
                 [1] push button \"Delete\"\n\
                 [2] push button \"Delete\""
            );
            s.background.set(false);
            // With `role` and more candidates than are listed.
            let kids: Vec<u32> = (100..110).collect();
            fake_frame(&s, true, &kids);
            for k in &kids {
                s.nodes
                    .borrow_mut()
                    .insert(*k, fake_node("push button", "Delete", &[]));
            }
            let r = call_tool(
                &mut s,
                15,
                "act",
                json!({"app": "fake", "query": "Delete", "role": "push button"}),
            )
            .await;
            let text = error_text(&r);
            assert!(
                text.starts_with("ambiguous: 10 elements in 'fake' match 'Delete' with role 'push button'; nothing sent. pass one id:\n"),
                "{text}"
            );
            assert_eq!(text.lines().count(), 1 + AMBIGUOUS_SHOWN + 1, "{text}");
            assert!(text.ends_with("\n… 4 more"), "{text}");
            // No "now:" context line: the candidates say where they are.
            assert!(!text.contains("\nnow: "), "{text}");
        });
    }

    #[test]
    fn the_window_argument_and_the_active_window_break_a_tie_between_windows() {
        local(async {
            let mut s = session("ambiguous-windows");
            fake_frame(&s, false, &[40]);
            s.more_windows.borrow_mut().push(50);
            {
                let mut n = s.nodes.borrow_mut();
                n.insert(40, fake_node("push button", "Save", &[]));
                let mut todo = fake_node("frame", "todo.txt — Fake", &[51]);
                todo.states = vec!["active"];
                n.insert(50, todo);
                n.insert(51, fake_node("push button", "Save", &[]));
            }
            let q = json!({"app": "fake", "query": "Save"});
            let r = call_tool(&mut s, 2, "act", q.clone()).await;
            assert!(
                reply_text(&r).starts_with("[1] push button \"Save\""),
                "{r}"
            );
            assert_eq!(s.keys.borrow().last().unwrap(), "action 0 on 51");
            // Neither window active (a terminal in front): refused, with the
            // windows.
            s.nodes.borrow_mut().get_mut(&50).unwrap().states.clear();
            let r = call_tool(&mut s, 3, "act", q.clone()).await;
            let text = error_text(&r);
            assert!(
                text.contains("pass one id, or window= its title:"),
                "{text}"
            );
            assert!(
                text.contains("\"Save\" at 0,0 in 'notes.txt — Fake'\n"),
                "{text}"
            );
            assert!(text.ends_with("in 'todo.txt — Fake'"), "{text}");
            // The window argument names it.
            let r = call_tool(
                &mut s,
                4,
                "act",
                json!({"app": "fake", "query": "Save", "window": "todo"}),
            )
            .await;
            assert_ne!(r["result"]["isError"], true, "{r}");
            assert_eq!(s.keys.borrow().last().unwrap(), "action 0 on 51");
            // The named window beats one still marked ACTIVE (the toolkit
            // not yet caught up with `window` bringing todo to the front).
            fake_frame(&s, true, &[40]);
            s.keys.borrow_mut().clear();
            let r = call_tool(
                &mut s,
                5,
                "act",
                json!({"app": "fake", "query": "Save", "window": "todo"}),
            )
            .await;
            assert_ne!(r["result"]["isError"], true, "{r}");
            assert_eq!(s.keys.borrow().last().unwrap(), "action 0 on 51");
        });
    }

    #[test]
    fn a_dialog_breaks_a_tie_with_its_parent_window() {
        local(async {
            let mut s = session("ambiguous-dialog");
            fake_frame(&s, false, &[40]);
            s.more_windows.borrow_mut().push(60);
            {
                let mut n = s.nodes.borrow_mut();
                n.insert(40, fake_node("push button", "OK", &[]));
                n.insert(60, fake_node("dialog", "Save — Fake", &[61]));
                n.insert(61, fake_node("push button", "OK", &[]));
                n.insert(70, fake_node("dialog", "Overwrite? — Fake", &[71]));
                n.insert(71, fake_node("push button", "OK", &[]));
            }
            let q = json!({"app": "fake", "query": "OK"});
            let r = call_tool(&mut s, 2, "click", q.clone()).await;
            assert_ne!(r["result"]["isError"], true, "{r}");
            let r = call_tool(&mut s, 3, "act", q.clone()).await;
            assert_ne!(r["result"]["isError"], true, "{r}");
            assert_eq!(s.keys.borrow().last().unwrap(), "action 0 on 61");
            // Stacked dialogs: the one opened last.
            s.more_windows.borrow_mut().push(70);
            let r = call_tool(&mut s, 4, "act", q.clone()).await;
            assert_ne!(r["result"]["isError"], true, "{r}");
            assert_eq!(s.keys.borrow().last().unwrap(), "action 0 on 71");
        });
    }

    #[test]
    fn frame_in_finds_the_window_from_the_nodes_read() {
        let mut cache: Cache<u32> = Cache::new();
        let node = |role: &str, kids: &[u32]| Info {
            role: role.into(),
            children: kids.to_vec(),
            ..Info::default()
        };
        cache.insert(1, node("frame", &[2]));
        cache.insert(2, node("panel", &[3, 4]));
        cache.insert(3, node("dialog", &[5]));
        cache.insert(5, node("push button", &[]));
        cache.insert(4, node("push button", &[]));
        assert_eq!(frame_in(&cache, &5), Some(3));
        assert_eq!(frame_in(&cache, &4), Some(1));
        assert_eq!(frame_in(&cache, &1), Some(1));
        assert_eq!(frame_in(&cache, &9), None);
        // Listed by two parents: not known for sure.
        cache.insert(6, node("panel", &[4]));
        assert_eq!(frame_in(&cache, &4), None);
        // A cycle ends.
        cache.insert(7, node("panel", &[8]));
        cache.insert(8, node("panel", &[7]));
        assert_eq!(frame_in(&cache, &8), None);
    }

    /// A web mail page in the fake: a table of rows and cells.
    fn fake_mail(s: &Session) {
        *s.focus.borrow_mut() = Info {
            role: "frame".into(),
            name: "Inbox — Mail".into(),
            showing: true,
            children: vec![39],
            ..Info::default()
        };
        *s.sem.toolkit.borrow_mut() = "Gecko".into();
        let mut nodes = s.nodes.borrow_mut();
        nodes.insert(39, fake_node("document web", "Inbox", &[40]));
        nodes.insert(40, fake_node("table", "", &[41, 43]));
        nodes.insert(41, fake_node("table row", "Invoice March", &[42]));
        nodes.insert(42, fake_node("table cell", "Invoice March", &[]));
        nodes.insert(43, fake_node("table row", "Lunch", &[]));
        nodes.insert(44, fake_node("link", "Archive", &[]));
        nodes.get_mut(&40).unwrap().children.push(44);
    }

    #[test]
    fn a_role_that_names_nothing_matches_through_its_synonyms() {
        local(async {
            let mut s = session("role-synonyms");
            fake_mail(&s);
            let r = call_tool(
                &mut s,
                2,
                "find",
                json!({"app": "fake", "query": "invoice", "role": "row"}),
            )
            .await;
            assert_eq!(
                reply_text(&r),
                "role 'row' matched as 'table row':\n[1] table row \"Invoice March\" actions=Press",
                "{r}"
            );
            // The role as given wins: no note.
            let r = call_tool(
                &mut s,
                3,
                "find",
                json!({"app": "fake", "query": "invoice", "role": "Table Cell"}),
            )
            .await;
            assert_eq!(
                reply_text(&r),
                "[2] table cell \"Invoice March\" actions=Press",
                "{r}"
            );
            let r = call_tool(
                &mut s,
                4,
                "act",
                json!({"app": "fake", "query": "invoice", "role": "gridcell"}),
            )
            .await;
            assert!(
                reply_text(&r).starts_with(
                    "[2] table cell \"Invoice March\" actions=Press (role 'gridcell' matched as 'table cell'): Press -> ok"
                ),
                "{r}"
            );
            let r = call_tool(
                &mut s,
                5,
                "wait_for",
                json!({"app": "fake", "query": "lunch", "role": "listitem", "timeout_ms": 200}),
            )
            .await;
            assert!(
                reply_text(&r).ends_with(
                    "[3] table row \"Lunch\" actions=Press (role 'listitem' matched as 'table row')"
                ),
                "{r}"
            );
            // A role nothing of the query has: the roles it has instead.
            let r = call_tool(
                &mut s,
                6,
                "find",
                json!({"app": "fake", "query": "archive", "role": "button"}),
            )
            .await;
            assert!(
                reply_text(&r).starts_with(
                    "no matches with role 'button'; it matches elements of role 'link' \
                     (pass that role, or none):\n[4] link \"Archive\" actions=Press\n"
                ),
                "{r}"
            );
            let r = call_tool(
                &mut s,
                7,
                "act",
                json!({"app": "fake", "query": "archive", "role": "checkbox"}),
            )
            .await;
            assert!(
                reply_text(&r).contains(
                    "no visible element in 'fake' matches 'archive' with role 'checkbox'; \
                     it matches elements of role 'link'"
                ),
                "{r}"
            );
        });
    }

    fn page(title: &str, url: &str, busy: bool) -> backend::Page {
        backend::Page {
            title: title.into(),
            url: url.into(),
            busy,
        }
    }

    #[test]
    fn the_effect_line_names_a_page_that_changed_or_loads() {
        local(async {
            let mut s = session("page-effect");
            fake_mail(&s);
            *s.reads.page.borrow_mut() = Some(page("Inbox (3)", "https://mail/#inbox", false));
            let act = json!({"app": "fake", "query": "Invoice March", "role": "table row", "settle_ms": 0});
            let effect = |r: &Json| {
                let t = reply_text(r);
                t[t.find("effect: ").expect(t) + 8..].to_string()
            };
            // Nothing changed: still "no visible change".
            let r = call_tool(&mut s, 2, "act", act.clone()).await;
            assert_eq!(effect(&r), "no visible change", "{r}");
            // The row opened its mail: the page's title changed, though
            // the window's did not (yet).
            *s.reads.page_next.borrow_mut() =
                Some(page("Invoice March - Mail", "https://mail/#inbox/1", false));
            let r = call_tool(&mut s, 3, "act", act.clone()).await;
            assert_eq!(effect(&r), "page changed to 'Invoice March - Mail'", "{r}");
            // The next page of results: same title, another address, loading.
            *s.reads.page_next.borrow_mut() =
                Some(page("Invoice March - Mail", "https://mail/#inbox/2", true));
            let r = call_tool(&mut s, 4, "act", act.clone()).await;
            assert_eq!(
                effect(&r),
                "page changed to 'Invoice March - Mail' (https://mail/#inbox/2) \
                 (still loading: wait_idle before reading it)",
                "{r}"
            );
            // Still loading, same page, as it was before the input: the
            // input made no change of its own.
            let r = call_tool(&mut s, 5, "act", act.clone()).await;
            assert_eq!(
                effect(&r),
                "no visible change (the page was already loading: wait_idle before reading it)",
                "{r}"
            );
            // Done loading; then the input starts a load of the same page.
            *s.reads.page.borrow_mut() =
                Some(page("Invoice March - Mail", "https://mail/#inbox/2", false));
            *s.reads.page_next.borrow_mut() =
                Some(page("Invoice March - Mail", "https://mail/#inbox/2", true));
            let r = call_tool(&mut s, 6, "act", act.clone()).await;
            assert_eq!(
                effect(&r),
                "changed: the page started loading (wait_idle before reading it)",
                "{r}"
            );
        });
    }

    #[test]
    fn a_page_loading_before_the_input_is_no_change_of_its() {
        let fp = |page: Option<backend::Page>, focus: &str| Fingerprint {
            window: Some(("firefox".to_string(), "Mail — Mozilla Firefox".to_string())),
            focus: Some((1u32, "link".to_string(), focus.to_string(), vec![])),
            app: None,
            page,
            content: None,
        };
        let allowed = |_: &str| true;
        let busy = Some(page("Mail", "u1", true));
        let idle = Some(page("Mail", "u1", false));
        // Busy before and after, nothing else: no change, events or not.
        assert_eq!(
            effect_text(
                &fp(busy.clone(), "a"),
                &fp(busy.clone(), "a"),
                Some(false),
                allowed
            ),
            "no visible change (the page was already loading: wait_idle before reading it)"
        );
        assert_eq!(
            effect_text(
                &fp(busy.clone(), "a"),
                &fp(busy.clone(), "a"),
                Some(true),
                allowed
            ),
            "not known: the page was already loading (wait_idle before reading it)"
        );
        // The focus moved meanwhile: a change.
        assert_eq!(
            effect_text(
                &fp(busy.clone(), "a"),
                &fp(busy.clone(), "b"),
                None,
                allowed
            ),
            "changed (the page is still loading: wait_idle before reading it)"
        );
        // It started loading after the input.
        assert_eq!(
            effect_text(
                &fp(idle.clone(), "a"),
                &fp(busy.clone(), "a"),
                None,
                allowed
            ),
            "changed: the page started loading (wait_idle before reading it)"
        );
        // It finished loading: its events are the ones the app sent.
        assert_eq!(
            effect_text(&fp(busy, "a"), &fp(idle.clone(), "a"), Some(false), allowed),
            "no visible change"
        );
        assert_eq!(
            effect_text(&fp(idle.clone(), "a"), &fp(idle, "a"), Some(true), allowed),
            "changed"
        );
    }

    /// `fake_mail` plus elements scrolled out of view in its page, and a
    /// visible link that shares a name with one of them.
    fn fake_mail_below(s: &Session) {
        fake_mail(s);
        let hidden = |role: &str, name: &str| Info {
            showing: false,
            ..fake_node(role, name, &[])
        };
        let mut n = s.nodes.borrow_mut();
        n.insert(45, hidden("heading", "Issues"));
        n.insert(46, fake_node("link", "Issues", &[]));
        n.insert(47, hidden("push button", "Next page"));
        n.insert(48, hidden("link", "New issue"));
        n.insert(49, hidden("push button", "Delete"));
        n.insert(50, fake_node("toggle button", "Delete mode", &[]));
        n.get_mut(&40)
            .unwrap()
            .children
            .extend([45, 46, 47, 48, 49, 50]);
    }

    #[test]
    fn a_miss_offers_the_role_scrolled_out_of_view_first() {
        local(async {
            let mut s = session("miss-offscreen-role");
            fake_mail_below(&s);
            // The heading asked for is below the fold; a visible link of
            // the same name is not offered instead.
            let r = call_tool(
                &mut s,
                2,
                "find",
                json!({"app": "fake", "query": "issues", "role": "heading"}),
            )
            .await;
            let t = reply_text(&r);
            assert!(
                t.starts_with(
                    "no visible matches with role 'heading'\noff-screen, scrolled out of view \
                     (act and get_text work on them; act action=scroll_to before click):\n"
                ),
                "{r}"
            );
            assert!(t.contains("heading \"Issues\""), "{r}");
            assert!(!t.contains("link"), "{r}");
            // Its synonyms off-screen too, said as such.
            let r = call_tool(
                &mut s,
                3,
                "find",
                json!({"app": "fake", "query": "next page", "role": "button"}),
            )
            .await;
            let t = reply_text(&r);
            assert!(
                t.contains(
                    "off-screen, scrolled out of view (role 'button' matched as 'push button')"
                ),
                "{r}"
            );
            assert!(t.contains("push button \"Next page\""), "{r}");
            // Neither visible nor off-screen in that role: off-screen ones
            // of another role, with it.
            let r = call_tool(
                &mut s,
                4,
                "find",
                json!({"app": "fake", "query": "new issue", "role": "button"}),
            )
            .await;
            let t = reply_text(&r);
            assert!(
                t.starts_with(
                    "no visible matches with role 'button'\noff-screen, scrolled out of view, \
                     of role 'link' (pass that role, or none; act action=scroll_to before click):\n"
                ),
                "{r}"
            );
            assert!(t.contains("link \"New issue\""), "{r}");
            // An input tool's miss offers the same.
            let r = call_tool(
                &mut s,
                5,
                "act",
                json!({"app": "fake", "query": "issues", "role": "heading"}),
            )
            .await;
            assert!(is_error(&r), "{r}");
            let t = reply_text(&r);
            assert!(t.contains("off-screen, scrolled out of view"), "{r}");
            assert!(t.contains("heading \"Issues\""), "{r}");
        });
    }

    #[test]
    fn a_synonym_never_stands_in_for_the_role_scrolled_out_of_view() {
        local(async {
            let mut s = session("synonym-offscreen");
            fake_mail_below(&s);
            let before = s.keys.borrow().len();
            // The push button is below the fold; the visible toggle button
            // only matches through the synonyms: not pressed.
            let r = call_tool(
                &mut s,
                2,
                "act",
                json!({"app": "fake", "query": "Delete", "role": "push button"}),
            )
            .await;
            assert!(is_error(&r), "{r}");
            let t = reply_text(&r);
            assert!(
                t.contains(
                    "no visible element in 'fake' matches 'Delete' with role 'push button'\n\
                     off-screen, scrolled out of view"
                ),
                "{r}"
            );
            assert!(t.contains("push button \"Delete\""), "{r}");
            assert_eq!(s.keys.borrow().len(), before, "nothing sent");
            // Without the push button anywhere, the synonym serves.
            s.nodes.borrow_mut().remove(&49);
            let r = call_tool(
                &mut s,
                3,
                "act",
                json!({"app": "fake", "query": "Delete", "role": "push button"}),
            )
            .await;
            assert!(
                reply_text(&r).contains("(role 'push button' matched as 'toggle button')"),
                "{r}"
            );
        });
    }

    #[test]
    fn the_page_is_the_one_of_the_window_the_input_goes_to() {
        local(async {
            let mut s = session("page-window");
            fake_mail(&s);
            *s.reads.page.borrow_mut() = Some(page("Inbox (3)", "https://mail/#inbox", false));
            let act = json!({"app": "fake", "query": "Invoice March", "role": "table row", "settle_ms": 0});
            let effect = |r: &Json| {
                let t = reply_text(r);
                t[t.find("effect: ").expect(t) + 8..].to_string()
            };
            *s.reads.page_next.borrow_mut() =
                Some(page("Invoice March - Mail", "https://mail/#inbox/1", false));
            let r = call_tool(&mut s, 2, "act", act.clone()).await;
            assert_eq!(effect(&r), "page changed to 'Invoice March - Mail'", "{r}");
            // A second browser window shows, and none is active: which
            // window the input reaches is not known, so no page is
            // compared (rather than the other window's).
            s.more_windows.borrow_mut().push(60);
            s.nodes
                .borrow_mut()
                .insert(60, fake_node("frame", "Other — Mail", &[]));
            *s.reads.page_next.borrow_mut() =
                Some(page("Lunch - Mail", "https://mail/#inbox/3", false));
            let r = call_tool(&mut s, 3, "act", act.clone()).await;
            assert_eq!(effect(&r), "no visible change", "{r}");
            // The other window is active: its page, and no document there.
            {
                let mut n = s.nodes.borrow_mut();
                n.get_mut(&60).unwrap().states.push("active");
            }
            let r = call_tool(&mut s, 4, "act", act.clone()).await;
            assert_eq!(effect(&r), "no visible change", "{r}");
            // The mail window active again: its page found again.
            s.nodes.borrow_mut().get_mut(&60).unwrap().states.clear();
            s.focus.borrow_mut().states.push("active");
            *s.reads.page_next.borrow_mut() =
                Some(page("Archive - Mail", "https://mail/#archive", false));
            let r = call_tool(&mut s, 5, "act", act).await;
            assert_eq!(effect(&r), "page changed to 'Archive - Mail'", "{r}");
        });
    }

    #[test]
    fn the_effect_line_hears_the_app_the_input_went_to() {
        local(async {
            let mut s = session("effect-app");
            fake_mail(&s);
            // The app reports changes on a window of its own, not on the
            // focused element (the terminal had focus).
            s.more_windows.borrow_mut().push(60);
            s.nodes
                .borrow_mut()
                .insert(60, fake_node("frame", "Other — Mail", &[]));
            s.reads.events_on.set(Some(60));
            let r = call_tool(
                &mut s,
                2,
                "act",
                json!({"app": "fake", "query": "Lunch", "settle_ms": 0}),
            )
            .await;
            assert!(reply_text(&r).ends_with("; effect: changed"), "{r}");
            // Without an app named: the active window's app.
            let r = call_tool(
                &mut s,
                3,
                "key",
                json!({"combo": "a", "observe": "effect", "settle_ms": 0}),
            )
            .await;
            assert!(reply_text(&r).contains("; effect: changed"), "{r}");
        });
    }

    #[test]
    fn effect_prefers_the_page_to_the_window_title() {
        let fp = |title: &str, page: Option<backend::Page>| Fingerprint {
            window: Some(("firefox".to_string(), format!("{title} — Mozilla Firefox"))),
            focus: None::<(u32, String, String, Vec<&'static str>)>,
            app: None,
            page,
            content: None,
        };
        let allowed = |_: &str| true;
        let before = fp("Inbox", Some(page("Inbox", "u1", false)));
        let after = fp("Mail 1", Some(page("Mail 1", "u2", false)));
        assert_eq!(
            effect_text(&before, &after, Some(true), allowed),
            "page changed to 'Mail 1'"
        );
        // A new page without a title yet: its address.
        let after = fp("Inbox", Some(page("", "https://x/2", true)));
        assert_eq!(
            effect_text(&before, &after, None, allowed),
            "page changed to https://x/2 (still loading: wait_idle before reading it)"
        );
        // Another app's window in front: the window line.
        let mut other = fp("x", None);
        other.window = Some(("kate".into(), "a.txt — Kate".into()));
        assert_eq!(
            effect_text(&before, &other, None, allowed),
            "window changed to 'a.txt — Kate'"
        );
    }
}
