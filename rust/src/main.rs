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

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::{Duration, Instant};

use serde_json::{Value as Json, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

use backend::{Backend, Cache, FileDialog, R, Shot, WindowChange, WindowGeo};
use render::{Line, Printed, WalkOpts};

const MAX_REFS: usize = 20000;
const TREE_TEXT_LIMIT: usize = 80;
/// Nodes fetched ahead when rendering reaches an unfetched part of the tree.
const PREFETCH_MORE: usize = 500;

struct Refs<Id> {
    map: HashMap<u64, (Id, String)>,
    next: u64,
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
}

/// Tools that send keyboard or pointer input.
const INPUT_TOOLS: [&str; 9] = [
    "focus",
    "click",
    "drag",
    "scroll",
    "key",
    "type_text",
    "mouse",
    "activate_window",
    "file_dialog",
];

/// Longest `wait_for` / `pause`, in ms (10 minutes).
const MAX_WAIT_MS: i64 = 600_000;

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
const SHOT_MAX_SIZE: i64 = 1280;

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

/// Whether a dialog title names `window` as its parent: the portal titles
/// its dialog "<dialog title> - <parent window title>".
fn title_names_parent(dialog: &str, window: &str) -> bool {
    let window = window.trim();
    !window.is_empty()
        && dialog
            .trim()
            .strip_suffix(window)
            .and_then(|rest| rest.strip_suffix(" - "))
            .is_some_and(|title| !title.trim().is_empty())
}

/// The allowed app a file dialog belongs to, or why it may not be filled
/// in. An allowed app's own (in-process) dialog is that app's. A portal
/// dialog belongs to its transient parent when the compositor reports one,
/// else to the allowed window its title names; `windows` are (app, title)
/// of allowed apps' windows.
fn file_dialog_owner<Id>(
    d: &FileDialog<Id>,
    allowed: &[String],
    windows: &[(String, String)],
) -> R<String> {
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
    windows
        .iter()
        .find(|(_, w)| d.titles.iter().any(|t| title_names_parent(t, w)))
        .map(|(a, _)| a.clone())
        .ok_or_else(|| {
            format!(
                "the active file dialog ({}) does not belong to a window of an allowed app",
                render::py_repr(d.titles.first().map(String::as_str).unwrap_or(""))
            )
        })
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
                next: 1,
            }),
            allow_file,
            last_active: Mutex::new(None),
            last_shot: Mutex::new(None),
            input: Mutex::new(()),
        }
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
            // Name the window this server last brought forward, so one
            // activate_window call (e.g. first step of the next batch) fixes it.
            let hint = match self.last_active.lock().await.as_deref() {
                Some(t) if !t.is_empty() => format!(" (last used: activate_window '{t}')"),
                _ => String::new(),
            };
            return Err(format!(
                "active window app '{}' is not in {}; activate an allowed window first{hint}",
                if name.is_empty() { "unknown" } else { &name },
                self.allow_file.display()
            ));
        }
        Ok(name)
    }

    // ------------------------------------------------------------ ids

    async fn add_ref(&self, id: &B::Id, app: &str) -> u64 {
        let mut refs = self.refs.lock().await;
        if refs.map.len() > MAX_REFS {
            refs.map.clear();
        }
        let rid = refs.next;
        refs.next += 1;
        refs.map.insert(rid, (id.clone(), app.to_string()));
        rid
    }

    async fn resolve_with_app(&self, rid: i64) -> R<(B::Id, String)> {
        let found = self.refs.lock().await.map.get(&(rid as u64)).cloned();
        let (id, app) = found.ok_or_else(|| format!("unknown id {rid}; call tree/find again"))?;
        if !self.is_allowed(&app) {
            return Err(self.not_allowed(&app));
        }
        Ok((id, app))
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

    /// Fetch the visible part of `app` and pick the element `query` names.
    async fn locate(&self, app: &str, query: &str, role: &str) -> R<Option<(B::Id, Cache<B::Id>)>> {
        let windows = self.app_windows(app).await?;
        let mut cache = Cache::new();
        self.b
            .prefetch(
                &mut cache,
                windows.clone(),
                true,
                render::FIND_TEXT_LIMIT,
                render::FIND_MAX_SEEN,
            )
            .await;
        loop {
            match render::locate(&cache, &windows, query, role) {
                Ok(found) => return Ok(found.map(|id| (id, cache))),
                Err(missing) => {
                    self.b
                        .prefetch(
                            &mut cache,
                            missing,
                            true,
                            render::FIND_TEXT_LIMIT,
                            PREFETCH_MORE,
                        )
                        .await;
                }
            }
        }
    }

    /// The element a tool call names: `id`, or `app` + `query` (+ `role`).
    async fn target(&self, a: &Json) -> R<Target<B::Id>> {
        if let Some(rid) = a.get("id").and_then(Json::as_i64) {
            return Ok(Target {
                id: self.resolve(rid).await?,
                found: None,
            });
        }
        let (Some(app), Some(query)) = (
            a.get("app").and_then(Json::as_str),
            a.get("query").and_then(Json::as_str),
        ) else {
            return Err("pass `id` (from tree/find), or `app` and `query`".into());
        };
        self.b.enable().await;
        let role = arg_s(a, "role", "");
        let (id, cache) = self.locate(app, query, &role).await?.ok_or_else(|| {
            let role = if role.is_empty() {
                String::new()
            } else {
                format!(" with role '{role}'")
            };
            format!("no visible element in '{app}' matches '{query}'{role}")
        })?;
        let rid = self.add_ref(&id, &app.to_lowercase()).await;
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

    async fn print(&self, lines: Vec<Line<B::Id>>, cache: &Cache<B::Id>, app: &str) -> String {
        let mut out = Vec::with_capacity(lines.len());
        for line in lines {
            out.push(match line {
                Line::Raw(s) => Printed::Raw(s),
                Line::Node { depth, id } => {
                    let rid = self.add_ref(&id, app).await;
                    Printed::Node {
                        depth,
                        rid,
                        info: &cache[&id],
                    }
                }
            });
        }
        render::format(&out)
    }

    // ------------------------------------------------------------ tools

    async fn apps(&self) -> R<String> {
        self.b.enable().await;
        let allow = self.allowed();
        let mut lines = Vec::new();
        for app in self.b.list_apps().await {
            if app.windows.is_empty() {
                continue;
            }
            let flag = if allow.contains(&app.name.to_lowercase()) {
                "allowed"
            } else {
                "blocked"
            };
            lines.push(format!("{} ({flag})", app.name));
            for w in app.windows {
                let act = if w.active { " [active]" } else { "" };
                lines.push(format!(
                    "  - {}: {}{act}",
                    w.kind,
                    render::py_repr(&w.title)
                ));
            }
        }
        lines.push(format!("allowlist file: {}", self.allow_file.display()));
        if !self.b.apps_note().is_empty() {
            lines.push(self.b.apps_note().into());
        }
        Ok(lines.join("\n"))
    }

    async fn windows(&self, a: &Json) -> R<String> {
        let query = arg_s(a, "query", "");
        let allowed = self.allowed();
        let (screens, list) = tokio::join!(self.b.screens(), self.b.windows(&query, &allowed));
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
            let rid = self.add_ref(&id, &app).await;
            let header = render::describe(&info, rid);
            let kids = info.children.clone();
            cache.insert(id, info);
            (header, kids, app)
        } else if arg_b(a, "document", false) {
            let app = arg_s(a, "app", "");
            let windows = self.app_windows(&app).await?;
            let doc = self.active_document(&app, windows, &mut cache).await?;
            let rid = self.add_ref(&doc, &app.to_lowercase()).await;
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
        Ok(self.print(lines, &cache, &app).await)
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
        self.b
            .prefetch(
                cache,
                windows.clone(),
                true,
                TREE_TEXT_LIMIT,
                render::FIND_MAX_SEEN,
            )
            .await;
        let window = windows
            .iter()
            .filter(|w| cache.get(*w).is_some_and(|i| i.showing))
            .max_by_key(|w| cache[*w].states.contains(&"active"))
            .cloned()
            .ok_or_else(|| format!("'{app}' has no visible window"))?;
        loop {
            match render::first_with_role(
                cache,
                std::slice::from_ref(&window),
                &render::DOCUMENT_ROLES,
            ) {
                Ok(Some(doc)) => return Ok(doc),
                Ok(None) => {
                    return Err(format!(
                        "no document in the active window of '{app}'; call tree without `document`"
                    ));
                }
                Err(missing) => {
                    self.b
                        .prefetch(cache, missing, true, TREE_TEXT_LIMIT, PREFETCH_MORE)
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
            .prefetch(
                &mut cache,
                windows.clone(),
                true,
                render::FIND_TEXT_LIMIT,
                render::FIND_MAX_SEEN,
            )
            .await;
        let hits = loop {
            match render::find(&cache, &windows, &query, &role, max_results) {
                Ok(h) => break h,
                Err(missing) => {
                    self.b
                        .prefetch(
                            &mut cache,
                            missing,
                            true,
                            render::FIND_TEXT_LIMIT,
                            PREFETCH_MORE,
                        )
                        .await;
                }
            }
        };
        if hits.is_empty() {
            return Ok("no matches".into());
        }
        let lines = hits
            .into_iter()
            .map(|id| Line::Node { depth: 0, id })
            .collect();
        Ok(self.print(lines, &cache, &app.to_lowercase()).await)
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
        let mut how = "";
        let mut seen = self.read_back(&t.id, &text).await;
        // Firefox reports success for accessibility writes and changes
        // nothing. Retype through the keyboard when the element takes focus;
        // never with a newline, which Enter would turn into a submit.
        if matches!(&seen, Ok(Some(_))) && !text.contains('\n') && self.retype(&t.id, &text).await {
            how = "; typed, the accessibility write had no effect";
            seen = self.read_back(&t.id, &text).await;
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

    /// Ok(None) once the element holds `text` (polled briefly, as some
    /// toolkits apply a change a moment later), else Ok(Some(what it holds)).
    async fn read_back(&self, id: &B::Id, text: &str) -> R<Option<String>> {
        let limit = text.chars().count() + 64;
        let start = Instant::now();
        loop {
            let now = self.b.full_text(id, limit).await?;
            if written_matches(text, &now) {
                return Ok(None);
            }
            if start.elapsed() >= SET_TEXT_VERIFY {
                return Ok(Some(now));
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    }

    /// Focus the element, select all and type `text`; false when the element
    /// would not take focus in an allowed active window (nothing typed).
    async fn retype(&self, id: &B::Id, text: &str) -> bool {
        let _input = self.input.lock().await;
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
        let text = self
            .b
            .full_text(&t.id, arg_i(a, "limit", 20000).max(0) as usize)
            .await?;
        Ok(match t.found {
            Some(d) => format!("{d}:\n{text}"),
            None => text,
        })
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
            // An app that is not running (yet) counts as "no match".
            let found = self.locate(&app, &query, &role).await.ok().flatten();
            let ms = start.elapsed().as_millis();
            match found {
                Some((id, cache)) if !gone => {
                    let rid = self.add_ref(&id, &app.to_lowercase()).await;
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
            tokio::time::sleep(Duration::from_millis(50)).await;
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
        self.require_active_allowed().await?;
        let (x, y) = self.b.element_center(id).await?;
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
        let clicks = arg_i(a, "clicks", 1);
        let t = self.target(a).await?;
        self.require_active_allowed().await?;
        let (x, y) = self.b.element_center(&t.id).await?;
        self.b
            .pointer(x as i64, y as i64, &button, clicks, 0)
            .await?;
        Ok(t.reply(&format!("clicked {button} x{clicks} at {x},{y}")))
    }

    /// One end of a drag: `<end>_id` (an element's centre), else
    /// `<end>_x`/`<end>_y`, as screenshot pixels when `image` is true.
    async fn drag_point(&self, a: &Json, end: &str) -> R<(i64, i64)> {
        if let Some(rid) = a.get(format!("{end}_id")).and_then(Json::as_i64) {
            let (id, _) = self.resolve_with_app(rid).await?;
            let (x, y) = self.b.element_center(&id).await?;
            return Ok((x as i64, y as i64));
        }
        let (x, y) = (
            req_i(a, &format!("{end}_x"))?,
            req_i(a, &format!("{end}_y"))?,
        );
        if arg_b(a, "image", false) {
            let map = (*self.last_shot.lock().await)
                .ok_or("image=true needs a screenshot taken first")?;
            return Ok(map.to_screen(x, y));
        }
        Ok((x, y))
    }

    async fn drag(&self, a: &Json) -> R<String> {
        let button = arg_s(a, "button", "left");
        let app = self.require_active_allowed().await?;
        let (x0, y0) = self.drag_point(a, "from").await?;
        let (x1, y1) = self.drag_point(a, "to").await?;
        self.b.move_pointer(x0, y0).await?;
        self.b.button(&button, true).await?;
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
        let released = self.b.button(&button, false).await;
        moved?;
        released?;
        Ok(format!(
            "dragged {button} from ({x0},{y0}) to ({x1},{y1}) in {app}"
        ))
    }

    async fn scroll(&self, a: &Json) -> R<String> {
        let amount = arg_i(a, "amount", 3).clamp(-50, 50);
        let t = self.target(a).await?;
        self.require_active_allowed().await?;
        let (x, y) = self.b.element_center(&t.id).await?;
        self.b
            .pointer(x as i64, y as i64, "left", 0, amount)
            .await?;
        Ok(t.reply(&format!("scrolled {amount} at {x},{y}")))
    }

    async fn key(&self, a: &Json) -> R<String> {
        let combo = req_s(a, "combo")?;
        let repeat = arg_i(a, "repeat", 1);
        self.b.check_combo(&combo)?;
        let app = self.require_active_allowed().await?;
        for _ in 0..repeat.clamp(1, 100) {
            self.b.press_combo(&combo).await?;
        }
        let focus = self.focus_note(&app, false).await;
        Ok(format!("sent {combo} x{repeat} to {app}{focus}"))
    }

    async fn type_text(&self, a: &Json) -> R<String> {
        let text = req_s(a, "text")?;
        let app = self.require_active_allowed().await?;
        self.b.type_text(&text).await?;
        let focus = self.focus_note(&app, true).await;
        Ok(format!(
            "typed {} chars into {app}{focus}",
            text.chars().count()
        ))
    }

    /// "; focus: [id] role "name"" for the element now holding focus, so a
    /// key or typed text that went somewhere unexpected shows at once. With
    /// `typing`, warns when that element does not take text.
    async fn focus_note(&self, app: &str, typing: bool) -> String {
        let Some(id) = self.b.focused().await else {
            return String::new();
        };
        let info = self.b.info(&id, 0).await;
        let rid = self.add_ref(&id, &app.to_lowercase()).await;
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
        let app = self.require_active_allowed().await?;
        self.b
            .pointer(
                x,
                y,
                &arg_s(a, "button", "left"),
                arg_i(a, "clicks", 1),
                arg_i(a, "scroll", 0),
            )
            .await?;
        Ok(format!("pointer at ({x},{y}) in {app}"))
    }

    async fn activate_window(&self, a: &Json) -> R<String> {
        let query = match a.get("terminal").and_then(Json::as_str) {
            Some(t) => self.terminal_window(t).await?,
            None => req_s(a, "query")?,
        };
        self.b.activate_window(&query, &self.allowed()).await?;
        *self.last_active.lock().await = self.active_title().await;
        let app = self.b.active_app().await;
        Ok(format!(
            "active app now: {}",
            if app.is_empty() { "unknown" } else { &app }
        ))
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
        let windows: Vec<(String, String)> = self
            .b
            .list_apps()
            .await
            .into_iter()
            .filter(|app| allowed.contains(&app.name.to_lowercase()))
            .flat_map(|app| {
                let name = app.name.to_lowercase();
                app.windows
                    .into_iter()
                    .map(move |w| (name.clone(), w.title))
            })
            .collect();
        let owner = file_dialog_owner(&d, &allowed, &windows)
            .map_err(|e| format!("{e} (allowlist: {})", self.allow_file.display()))?;
        let text = path.display().to_string();
        if !self.b.set_text(&d.field, &text).await? {
            return Err("the file name field refused the path".into());
        }
        if let Some(now) = self.read_back(&d.field, &text).await? {
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
        let Shot { path, area } = self.b.screenshot().await.ok_or("screenshot failed")?;
        let max = arg_i(a, "max_size", SHOT_MAX_SIZE).clamp(64, 8192) as u32;
        let inline = arg_b(a, "inline", true);
        // Decoding and shrinking take tens of ms: keep other calls moving.
        let file = path.clone();
        let (size, png) = tokio::task::spawn_blocking(move || -> R<((u32, u32), Vec<u8>)> {
            let img = shot::read_png(&file)?;
            let (w, h) = shot::fit((img.width, img.height), max, max);
            if (w, h) == (img.width, img.height) {
                return Ok(((w, h), std::fs::read(&file).map_err(|e| e.to_string())?));
            }
            let png = shot::write_png(&shot::shrink(&img, w, h))?;
            std::fs::write(&file, &png).map_err(|e| e.to_string())?;
            Ok(((w, h), png))
        })
        .await
        .map_err(|e| e.to_string())??;
        let map = area.and_then(|a| ShotMap::new(size, a));
        *self.last_shot.lock().await = map;
        let (iw, ih) = size;
        let mut text = format!("{path}\n{iw}x{ih} px");
        if let (Some((x, y, w, h)), Some(_)) = (area, map) {
            text.push_str(&format!(
                " image of screen area at ({x},{y}), {w}x{h}. To click a point seen in it, \
                 pass its pixel position as image_x/image_y to mouse."
            ));
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

    async fn batch(&self, a: &Json) -> R<Out> {
        let steps = a
            .get("steps")
            .and_then(Json::as_array)
            .ok_or("missing array argument 'steps'")?;
        let mut out = Vec::new();
        let mut images = Vec::new();
        for (i, step) in steps.iter().enumerate() {
            let tool = step.get("tool").and_then(Json::as_str).unwrap_or("");
            let args = step.get("args").cloned().unwrap_or_else(|| json!({}));
            if tool == "batch" {
                return Err("batch cannot contain batch".into());
            }
            match self.call(tool, &args).await {
                Ok(o) => {
                    out.push(format!("[{}] {tool}: {}", i + 1, o.text));
                    images.extend(o.images);
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

    fn call<'a>(
        &'a self,
        name: &'a str,
        a: &'a Json,
    ) -> Pin<Box<dyn Future<Output = R<Out>> + 'a>> {
        Box::pin(async move {
            let _input = if INPUT_TOOLS.contains(&name) {
                Some(self.input.lock().await)
            } else {
                None
            };
            if let Some(label) = indicator_label(name, a) {
                self.b.indicate(&label).await;
            }
            match name {
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
                "doctor" => self.doctor().await.map(Out::from),
                "scroll" => self.scroll(a).await.map(Out::from),
                "key" => self.key(a).await.map(Out::from),
                "type_text" => self.type_text(a).await.map(Out::from),
                "mouse" => self.mouse(a).await.map(Out::from),
                "activate_window" => self.activate_window(a).await.map(Out::from),
                "file_dialog" => self.file_dialog(a).await.map(Out::from),
                "screenshot" => self.screenshot(a).await,
                "batch" => self.batch(a).await,
                _ => Err(format!("Unknown tool: {name}")),
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
    let server = Server::new(backend);
    server.b.enable().await;
    server.allowed();
    run_loop(
        std::rc::Rc::new(server),
        BufReader::new(tokio::io::stdin()),
        tokio::io::stdout(),
    )
    .await
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
            if let Some(id) = msg.pointer("/params/requestId") {
                in_flight.borrow_mut().cancel(id);
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

    /// Abort the running call with this id, if any. An unknown or finished id
    /// is ignored, as the MCP spec asks.
    fn cancel(&mut self, id: &Json) {
        if let Some((_, handle)) = self.by_id.remove(&id.to_string()) {
            handle.abort();
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

    /// An app "fake" that is running but never shows the element asked for,
    /// so `wait_for` keeps polling until its timeout; key presses are logged.
    struct FakeBackend {
        allow_file: PathBuf,
        keys: Rc<RefCell<Vec<String>>>,
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
        fn boring_roles(&self) -> &'static [&'static str] {
            &[]
        }
        async fn enable(&self) {}
        async fn list_apps(&self) -> Vec<AppEntry> {
            Vec::new()
        }
        async fn app_windows(&self, app: &str) -> Option<Vec<u32>> {
            (app == "fake").then(Vec::new)
        }
        async fn active_app(&self) -> String {
            "fake".into()
        }
        async fn info(&self, id: &u32, _: usize) -> Info<u32> {
            match *id {
                FAKE_FOCUS => Info {
                    role: "push button".into(),
                    name: "OK".into(),
                    showing: true,
                    ..Info::default()
                },
                _ => Info::default(),
            }
        }
        async fn focused(&self) -> Option<u32> {
            Some(FAKE_FOCUS)
        }
        async fn prefetch(&self, _: &mut Cache<u32>, _: Vec<u32>, _: bool, _: usize, _: usize) {}
        async fn actions(&self, _: &u32) -> Vec<String> {
            Vec::new()
        }
        async fn do_action(&self, _: &u32, _: usize) -> R<bool> {
            Ok(true)
        }
        async fn full_text(&self, _: &u32, _: usize) -> R<String> {
            Ok(String::new())
        }
        async fn set_text(&self, _: &u32, _: &str) -> R<bool> {
            Ok(true)
        }
        async fn is_focused(&self, _: &u32) -> bool {
            false
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
        async fn type_text(&self, _: &str) -> R<()> {
            Ok(())
        }
        async fn pointer(&self, _: i64, _: i64, _: &str, _: i64, _: i64) -> R<()> {
            Ok(())
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
        async fn activate_window(&self, _: &str, _: &[String]) -> R<()> {
            Ok(())
        }
        async fn screenshot(&self) -> Option<Shot> {
            // A 200x100 image of a 400x200 area at (10, 20): 2x scaling.
            let path = self.allow_file.with_extension("png");
            let img = shot::Pixels {
                width: 200,
                height: 100,
                channels: 3,
                data: vec![128; 200 * 100 * 3],
            };
            std::fs::write(&path, shot::write_png(&img).ok()?).ok()?;
            Some(Shot {
                path: path.display().to_string(),
                area: Some((10.0, 20.0, 400.0, 200.0)),
            })
        }
    }

    const FAKE_FOCUS: u32 = 7;

    struct Session {
        stdin: DuplexStream,
        stdout: tokio::io::Lines<BufReader<DuplexStream>>,
        keys: Rc<RefCell<Vec<String>>>,
    }

    /// `run_loop` over in-memory pipes, on the current LocalSet.
    fn session(name: &str) -> Session {
        let allow_file = std::env::temp_dir().join(format!(
            "desktop-mcp-test-{}-{name}.txt",
            std::process::id()
        ));
        std::fs::write(&allow_file, "fake\n").unwrap();
        let keys = Rc::new(RefCell::new(Vec::new()));
        let server = Rc::new(Server::new(FakeBackend {
            allow_file,
            keys: keys.clone(),
        }));
        let (stdin, server_in) = tokio::io::duplex(64 * 1024);
        let (server_out, client_out) = tokio::io::duplex(64 * 1024);
        tokio::task::spawn_local(run_loop(server, BufReader::new(server_in), server_out));
        Session {
            stdin,
            stdout: BufReader::new(client_out).lines(),
            keys,
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
    fn screenshot_comes_back_inline_shrunk_and_clickable() {
        local(async {
            let mut s = session("shot");
            let r = call_tool(&mut s, 2, "screenshot", json!({"max_size": 100})).await;
            let content = &r["result"]["content"];
            let text = content[0]["text"].as_str().unwrap();
            assert!(
                text.contains("100x50 px image of screen area at (10,20), 400x200"),
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
            assert!(text.contains("; focus: [2] push button \"OK\""), "{text}");
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

    #[test]
    fn dialog_titles_name_their_parent_window() {
        let gemini = "Google Gemini — Mozilla Firefox";
        assert!(title_names_parent(
            "Dosya gönder - Google Gemini — Mozilla Firefox",
            gemini
        ));
        // A dialog title that itself has " - " in it.
        assert!(title_names_parent("Yükle - Dosya - x — Kate", "x — Kate"));
        assert!(!title_names_parent(gemini, gemini));
        assert!(!title_names_parent(
            " - Google Gemini — Mozilla Firefox",
            gemini
        ));
        assert!(!title_names_parent("Aç - Başka — Mozilla Firefox", gemini));
        assert!(!title_names_parent("Aç - ", ""));
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
        let windows = vec![(
            "firefox".to_string(),
            "Google Gemini — Mozilla Firefox".to_string(),
        )];
        let owner = |d: &FileDialog<u32>| file_dialog_owner(d, &allowed, &windows);
        let portal = "xdg-desktop-portal-kde";
        let title = "Dosya gönder - Google Gemini — Mozilla Firefox";
        // The compositor's transient parent decides when it is known.
        assert_eq!(
            owner(&dialog(portal, "x", Some("Firefox"))).unwrap(),
            "firefox"
        );
        assert!(owner(&dialog(portal, title, Some("telegram-desktop"))).is_err());
        assert!(owner(&dialog(portal, title, Some(""))).is_err());
        // Else the window its title names.
        assert_eq!(owner(&dialog(portal, title, None)).unwrap(), "firefox");
        assert!(owner(&dialog(portal, "Dosya gönder - Telegram", None)).is_err());
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
}
