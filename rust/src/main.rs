//! Screenshot-free desktop control MCP server for KDE Plasma (Wayland).
//!
//! Same tools and output as the Python server (server.py), plus `batch`.
//! Reads go straight to AT-SPI over D-Bus with requests pipelined; every
//! app-scoped operation is restricted to the allowlist (re-read on every call).

mod atspi;
mod input;
mod kwin;
mod render;
mod roles;

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value as Json, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

use atspi::{A11y, Obj, R};
use input::Input;
use kwin::KWin;
use render::{Cache, Line, WalkOpts};

const DEFAULT_ALLOW: &str = "\
# One AT-SPI application name per line (case-insensitive). See `apps` tool output.
# Keyboard/mouse input is only sent when the active window belongs to one of these.
firefox
chromium
google-chrome
dolphin
kate
okular
gwenview
soffice
";
const APPS_NOTE: &str = "Apps started before accessibility was enabled may need a restart \
(Chromium/Electron: --force-renderer-accessibility).";
const MAX_REFS: usize = 20000;
const TREE_TEXT_LIMIT: i32 = 80;
/// Nodes fetched ahead when rendering reaches an unfetched part of the tree.
const PREFETCH_MORE: usize = 500;

struct Refs {
    map: HashMap<u64, (Obj, String)>,
    next: u64,
}

struct Server {
    session: zbus::Connection,
    a11y: A11y,
    kwin: KWin,
    input: Arc<Input>,
    refs: Mutex<Refs>,
    allow_file: PathBuf,
}

fn arg_s(a: &Json, k: &str, d: &str) -> String {
    a.get(k).and_then(Json::as_str).unwrap_or(d).to_string()
}

fn arg_i(a: &Json, k: &str, d: i64) -> i64 {
    a.get(k).and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64))).unwrap_or(d)
}

fn arg_b(a: &Json, k: &str, d: bool) -> bool {
    a.get(k).and_then(Json::as_bool).unwrap_or(d)
}

fn req_s(a: &Json, k: &str) -> R<String> {
    a.get(k).and_then(Json::as_str).map(str::to_string)
        .ok_or_else(|| format!("missing string argument '{k}'"))
}

fn req_i(a: &Json, k: &str) -> R<i64> {
    a.get(k).and_then(Json::as_i64).ok_or_else(|| format!("missing integer argument '{k}'"))
}

fn norm_title(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
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

impl Server {
    // ------------------------------------------------------------ allowlist

    fn allowed(&self) -> Vec<String> {
        if !self.allow_file.exists() {
            if let Some(dir) = self.allow_file.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(&self.allow_file, DEFAULT_ALLOW);
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
        let name = self.a11y.active_app().await;
        if !self.is_allowed(&name) {
            return Err(format!(
                "active window app '{}' is not in {}; activate an allowed window first",
                if name.is_empty() { "unknown" } else { &name }, self.allow_file.display()));
        }
        Ok(name)
    }

    // ------------------------------------------------------------ ids

    async fn add_ref(&self, obj: &Obj, app: &str) -> u64 {
        let mut refs = self.refs.lock().await;
        if refs.map.len() > MAX_REFS {
            refs.map.clear();
        }
        let id = refs.next;
        refs.next += 1;
        refs.map.insert(id, (obj.clone(), app.to_string()));
        id
    }

    async fn resolve(&self, id: i64) -> R<(Obj, String)> {
        let found = self.refs.lock().await.map.get(&(id as u64)).cloned();
        let (obj, app) = found.ok_or_else(|| format!("unknown id {id}; call tree/find again"))?;
        if !self.is_allowed(&app) {
            return Err(self.not_allowed(&app));
        }
        Ok((obj, app))
    }

    /// Top-level windows of every process with this app name.
    async fn app_windows(&self, name: &str) -> R<Vec<Obj>> {
        if !self.is_allowed(name) {
            return Err(self.not_allowed(name));
        }
        let want = name.to_lowercase();
        let apps = self.a11y.apps_with_windows().await;
        let mut found = false;
        let mut windows = Vec::new();
        for (_, n, ws) in apps {
            if n.to_lowercase() == want {
                found = true;
                windows.extend(ws);
            }
        }
        if !found {
            return Err(format!("app '{name}' not found in the accessibility tree"));
        }
        Ok(windows)
    }

    async fn print(&self, lines: Vec<Line>, cache: &Cache, app: &str) -> Vec<String> {
        let mut out = Vec::with_capacity(lines.len());
        for line in lines {
            out.push(match line {
                Line::Raw(s) => s,
                Line::Node { depth, obj } => {
                    let id = self.add_ref(&obj, app).await;
                    format!("{}{}", "  ".repeat(depth), render::describe(&cache[&obj], id))
                }
            });
        }
        out
    }

    // ------------------------------------------------------------ tools

    async fn apps(&self) -> R<String> {
        atspi::enable(&self.session).await;
        let allow = self.allowed();
        let apps = self.a11y.apps_with_windows().await;
        let a11y = &self.a11y;
        let details = futures_util::future::join_all(apps.iter().map(|(_, _, ws)| {
            futures_util::future::join_all(ws.iter().map(|w| async move {
                tokio::join!(a11y.role(w), a11y.name(w), a11y.has_state(w, atspi::ACTIVE))
            }))
        }))
        .await;
        let mut lines = Vec::new();
        for ((_, name, _), windows) in apps.iter().zip(details) {
            if windows.is_empty() {
                continue;
            }
            let flag = if allow.contains(&name.to_lowercase()) { "allowed" } else { "blocked" };
            lines.push(format!("{name} ({flag})"));
            for (kind, title, active) in windows {
                let act = if active { " [active]" } else { "" };
                lines.push(format!("  - {kind}: {}{act}", render::py_repr(title.trim())));
            }
        }
        lines.push(format!("allowlist file: {}", self.allow_file.display()));
        lines.push(APPS_NOTE.into());
        Ok(lines.join("\n"))
    }

    async fn tree(&self, a: &Json) -> R<String> {
        atspi::enable(&self.session).await;
        let root_id = arg_i(a, "root_id", 0);
        let opts = WalkOpts {
            max_depth: arg_i(a, "max_depth", 12).max(0) as usize,
            budget: arg_i(a, "max_nodes", 300).max(0) as usize,
            only_showing: !arg_b(a, "include_hidden", false),
        };
        let mut cache = Cache::new();
        let (header, roots, app) = if root_id != 0 {
            let (obj, app) = self.resolve(root_id).await?;
            let info = self.a11y.info(&obj, TREE_TEXT_LIMIT).await;
            let id = self.add_ref(&obj, &app).await;
            let header = render::describe(&info, id);
            let kids = info.children.clone();
            cache.insert(obj, info);
            (header, kids, app)
        } else {
            let app = arg_s(a, "app", "");
            let windows = self.app_windows(&app).await?;
            (app.clone(), windows, app.to_lowercase())
        };
        let first = (opts.budget * 4).max(1000);
        self.a11y.prefetch(&mut cache, roots.clone(), opts.only_showing, TREE_TEXT_LIMIT,
                           first).await;
        let lines = loop {
            let mut lines = vec![Line::Raw(header.clone())];
            match render::walk(&cache, &roots, 1, &opts, &mut lines) {
                Ok(()) => break lines,
                Err(missing) => {
                    self.a11y.prefetch(&mut cache, missing, opts.only_showing,
                                       TREE_TEXT_LIMIT, PREFETCH_MORE).await;
                }
            }
        };
        Ok(self.print(lines, &cache, &app).await.join("\n"))
    }

    async fn find(&self, a: &Json) -> R<String> {
        atspi::enable(&self.session).await;
        let query = req_s(a, "query")?;
        let app = req_s(a, "app")?;
        let role = arg_s(a, "role", "");
        let max_results = arg_i(a, "max_results", 30).max(0) as usize;
        let windows = self.app_windows(&app).await?;
        let text_limit = render::FIND_TEXT_LIMIT as i32;
        let mut cache = Cache::new();
        self.a11y.prefetch(&mut cache, windows.clone(), true, text_limit,
                           render::FIND_MAX_SEEN).await;
        let hits = loop {
            match render::find(&cache, &windows, &query, &role, max_results) {
                Ok(h) => break h,
                Err(missing) => {
                    self.a11y.prefetch(&mut cache, missing, true, text_limit, PREFETCH_MORE).await;
                }
            }
        };
        if hits.is_empty() {
            return Ok("no matches".into());
        }
        let lines = hits.into_iter().map(|obj| Line::Node { depth: 0, obj }).collect();
        Ok(self.print(lines, &cache, &app.to_lowercase()).await.join("\n"))
    }

    async fn act(&self, a: &Json) -> R<String> {
        let (obj, _) = self.resolve(req_i(a, "id")?).await?;
        let action = arg_s(a, "action", "");
        let names = self.a11y.actions(&obj).await;
        if names.is_empty() {
            return Err("element has no actions; try click, or focus + key".into());
        }
        let mut idx = 0;
        if !action.is_empty() {
            let lowered: Vec<String> = names.iter().map(|n| n.to_lowercase()).collect();
            idx = lowered.iter().position(|n| *n == action.to_lowercase()).ok_or_else(|| {
                let list: Vec<String> = names.iter().map(|n| render::py_repr(n)).collect();
                format!("action '{action}' not in [{}]", list.join(", "))
            })?;
        }
        let ok = self.a11y.do_action(&obj, idx as i32).await?;
        Ok(format!("{} -> {}", names[idx], if ok { "ok" } else { "failed" }))
    }

    async fn set_text(&self, a: &Json) -> R<String> {
        let (obj, _) = self.resolve(req_i(a, "id")?).await?;
        let text = req_s(a, "text")?;
        if !self.a11y.interfaces(&obj).await.iter().any(|i| i == "org.a11y.atspi.EditableText") {
            return Err("element is not editable text; use focus + type".into());
        }
        Ok(if self.a11y.set_text(&obj, &text).await? { "ok" } else { "failed" }.into())
    }

    async fn get_text(&self, a: &Json) -> R<String> {
        let (obj, _) = self.resolve(req_i(a, "id")?).await?;
        let limit = arg_i(a, "limit", 20000).clamp(0, i32::MAX as i64) as i32;
        if self.a11y.interfaces(&obj).await.iter().any(|i| i == "org.a11y.atspi.Text") {
            self.a11y.text(&obj, limit).await
        } else {
            Ok(self.a11y.name(&obj).await)
        }
    }

    async fn element_center(&self, obj: &Obj) -> R<(i32, i32)> {
        let a11y = &self.a11y;
        let (frame_active, ext, origin) = tokio::join!(
            async {
                match a11y.frame_of(obj).await {
                    Some(f) => a11y.has_state(&f, atspi::ACTIVE).await,
                    None => false,
                }
            },
            a11y.extents(obj, atspi::COORD_WINDOW),
            self.kwin.active_window_origin());
        if !frame_active {
            return Err("element's window is not active; call activate_window first".into());
        }
        let (x, y, w, h) = ext?;
        if w <= 0 || h <= 0 {
            return Err("element has no on-screen area".into());
        }
        let (gx, gy) = origin?;
        Ok(((gx + x as f64 + w as f64 / 2.0) as i32, (gy + y as f64 + h as f64 / 2.0) as i32))
    }

    async fn pointer(&self, x: i64, y: i64, button: &str, clicks: i64, scroll: i64) -> R<()> {
        let (x0, y0, x1, y1) = self.kwin.screen_box().await;
        let ax = ((x - x0 as i64) * input::ABS_MAX as i64 / (x1 - x0).max(1) as i64) as i32;
        let ay = ((y - y0 as i64) * input::ABS_MAX as i64 / (y1 - y0).max(1) as i64) as i32;
        self.input.pointer(ax, ay, button, clicks, scroll).await
    }

    async fn focused(&self, obj: &Obj, limit: Duration) -> bool {
        wait_for(limit, || self.a11y.has_state(obj, atspi::FOCUSED)).await
    }

    async fn focus(&self, a: &Json) -> R<String> {
        let (obj, _) = self.resolve(req_i(a, "id")?).await?;
        // Qt only honours GrabFocus when the element exposes a "Set Focus"
        // action, so try both before falling back to a click.
        let (ifaces, actions) = tokio::join!(self.a11y.interfaces(&obj), self.a11y.actions(&obj));
        if !ifaces.iter().any(|i| i == "org.a11y.atspi.Component") {
            return Err("element cannot take focus".into());
        }
        let focus_wait = Duration::from_millis(50);
        if self.a11y.grab_focus(&obj).await && self.focused(&obj, focus_wait).await {
            return Ok("ok (grab_focus)".into());
        }
        let names: Vec<String> = actions.iter().map(|n| n.to_lowercase().replace(' ', "")).collect();
        if let Some(i) = names.iter().position(|n| n == "setfocus") {
            let _ = self.a11y.do_action(&obj, i as i32).await;
            if self.focused(&obj, focus_wait).await {
                return Ok("ok (set focus action)".into());
            }
        }
        self.require_active_allowed().await?;
        let (x, y) = self.element_center(&obj).await?;
        self.pointer(x as i64, y as i64, "left", 1, 0).await?;
        if self.focused(&obj, Duration::from_millis(150)).await {
            return Ok(format!("ok (clicked at {x},{y})"));
        }
        Ok(format!("clicked at {x},{y} but element did not report focus"))
    }

    async fn click(&self, a: &Json) -> R<String> {
        let (obj, _) = self.resolve(req_i(a, "id")?).await?;
        let button = arg_s(a, "button", "left");
        let clicks = arg_i(a, "clicks", 1);
        self.require_active_allowed().await?;
        let (x, y) = self.element_center(&obj).await?;
        self.pointer(x as i64, y as i64, &button, clicks, 0).await?;
        Ok(format!("clicked {button} x{clicks} at {x},{y}"))
    }

    async fn key(&self, a: &Json) -> R<String> {
        let combo = req_s(a, "combo")?;
        let repeat = arg_i(a, "repeat", 1);
        input::parse_combo(&combo)?;
        let app = self.require_active_allowed().await?;
        for _ in 0..repeat.clamp(1, 100) {
            self.input.press_combo(&combo).await?;
        }
        Ok(format!("sent {combo} x{repeat} to {app}"))
    }

    async fn type_text(&self, a: &Json) -> R<String> {
        let text = req_s(a, "text")?;
        let app = self.require_active_allowed().await?;
        // Paste instead of typing key by key: works with any layout and Unicode.
        let mut child = tokio::process::Command::new("wl-copy")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| format!("wl-copy: {e}"))?;
        let mut stdin = child.stdin.take().ok_or("wl-copy: no stdin")?;
        stdin.write_all(text.as_bytes()).await.map_err(|e| format!("wl-copy: {e}"))?;
        drop(stdin);
        let status = child.wait().await.map_err(|e| format!("wl-copy: {e}"))?;
        if !status.success() {
            return Err(format!("wl-copy failed: {status}"));
        }
        // wl-copy's parent exits only once the compositor has the selection.
        self.input.press_combo("ctrl+v").await?;
        Ok(format!("typed {} chars into {app}", text.chars().count()))
    }

    async fn mouse(&self, a: &Json) -> R<String> {
        let (x, y) = (req_i(a, "x")?, req_i(a, "y")?);
        let app = self.require_active_allowed().await?;
        self.pointer(x, y, &arg_s(a, "button", "left"), arg_i(a, "clicks", 1),
                     arg_i(a, "scroll", 0)).await?;
        Ok(format!("pointer at ({x},{y}) in {app}"))
    }

    async fn frame_active(&self, caption: &str) -> bool {
        let want = norm_title(caption);
        let a11y = &self.a11y;
        for (_, _, windows) in a11y.apps_with_windows().await {
            for w in windows {
                let (name, active) = tokio::join!(a11y.name(&w), a11y.has_state(&w, atspi::ACTIVE));
                if active && norm_title(&name) == want {
                    return true;
                }
            }
        }
        false
    }

    async fn activate_window(&self, a: &Json) -> R<String> {
        let query = req_s(a, "query")?;
        let caption = self.kwin.activate(&query, &self.allowed()).await?;
        if !caption.is_empty() {
            // KWin switches at once; the toolkit reports ACTIVE a moment later.
            wait_for(Duration::from_millis(500), || self.frame_active(&caption)).await;
        }
        let app = self.a11y.active_app().await;
        Ok(format!("active app now: {}", if app.is_empty() { "unknown" } else { &app }))
    }

    async fn screenshot(&self) -> R<String> {
        self.require_active_allowed().await?;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        let path = std::env::temp_dir().join(format!("desktop-mcp-{nanos}.png"));
        let _ = tokio::process::Command::new("spectacle")
            .args(["-b", "-n", "-a", "-o"]).arg(&path)
            .output().await;
        Ok(if path.exists() { path.display().to_string() } else { "screenshot failed".into() })
    }

    async fn batch(&self, a: &Json) -> R<String> {
        let steps = a.get("steps").and_then(Json::as_array)
            .ok_or("missing array argument 'steps'")?;
        let mut out = Vec::new();
        for (i, step) in steps.iter().enumerate() {
            let tool = step.get("tool").and_then(Json::as_str).unwrap_or("");
            let args = step.get("args").cloned().unwrap_or_else(|| json!({}));
            if tool == "batch" {
                return Err("batch cannot contain batch".into());
            }
            match self.call(tool, &args).await {
                Ok(text) => out.push(format!("[{}] {tool}: {text}", i + 1)),
                Err(e) => {
                    out.push(format!("[{}] {tool} failed: {e}", i + 1));
                    if i + 1 < steps.len() {
                        out.push(format!("stopped; {} step(s) not run", steps.len() - i - 1));
                    }
                    return Err(out.join("\n"));
                }
            }
        }
        Ok(out.join("\n"))
    }

    fn call<'a>(&'a self, name: &'a str, a: &'a Json)
        -> Pin<Box<dyn Future<Output = R<String>> + 'a>> {
        Box::pin(async move {
            match name {
                "apps" => self.apps().await,
                "tree" => self.tree(a).await,
                "find" => self.find(a).await,
                "act" => self.act(a).await,
                "set_text" => self.set_text(a).await,
                "get_text" => self.get_text(a).await,
                "focus" => self.focus(a).await,
                "click" => self.click(a).await,
                "key" => self.key(a).await,
                "type_text" => self.type_text(a).await,
                "mouse" => self.mouse(a).await,
                "activate_window" => self.activate_window(a).await,
                "screenshot" => self.screenshot().await,
                "batch" => self.batch(a).await,
                _ => Err(format!("Unknown tool: {name}")),
            }
        })
    }
}

// ---------------------------------------------------------------- MCP

fn schema(params: &[(&str, &str, Option<Json>)]) -> Json {
    let mut props = serde_json::Map::new();
    let mut required = Vec::new();
    for (name, kind, default) in params {
        let mut p = json!({ "type": kind });
        match default {
            Some(d) => { p["default"] = d.clone(); }
            None => required.push(json!(name)),
        }
        props.insert(name.to_string(), p);
    }
    let mut s = json!({ "type": "object", "properties": props });
    if !required.is_empty() {
        s["required"] = Json::Array(required);
    }
    s
}

fn tools() -> Json {
    let id = ("id", "integer", None);
    let t = |name: &str, desc: &str, input: Json| json!({
        "name": name, "description": desc, "inputSchema": input });
    let mut batch_schema = schema(&[("steps", "array", None)]);
    batch_schema["properties"]["steps"]["items"] = json!({
        "type": "object",
        "properties": { "tool": { "type": "string" }, "args": { "type": "object" } },
        "required": ["tool"],
    });
    json!([
        t("apps", "List applications with their top-level windows.\nMarks which are allowed and which window is active.", schema(&[])),
        t("tree", "Return the accessibility tree as `[id] role \"name\" {states} actions=…`.\nPass `app` (name from `apps`) for its windows, or `root_id` to expand a subtree.\nIds are used by act/set_text/get_text/focus/click.",
          schema(&[("app", "string", Some(json!(""))), ("root_id", "integer", Some(json!(0))),
                   ("max_depth", "integer", Some(json!(12))), ("max_nodes", "integer", Some(json!(300))),
                   ("include_hidden", "boolean", Some(json!(false)))])),
        t("find", "Search an allowed app for elements whose name, text or role contains `query`\n(case-insensitive). Optionally filter by exact role name as shown by tree.",
          schema(&[("query", "string", None), ("app", "string", None), ("role", "string", Some(json!(""))),
                   ("max_results", "integer", Some(json!(30)))])),
        t("act", "Perform an accessibility action on an element (default: its first action,\nusually click/press/invoke). Use the action names listed by tree/find.",
          schema(&[id.clone(), ("action", "string", Some(json!("")))])),
        t("set_text", "Replace the contents of an editable text element.",
          schema(&[id.clone(), ("text", "string", None)])),
        t("get_text", "Read the full text of an element (documents, fields, labels).",
          schema(&[id.clone(), ("limit", "integer", Some(json!(20000)))])),
        t("focus", "Move keyboard focus to an element. Tries the accessibility focus request\nfirst, then a real click on the element's centre (the element's window must\nbe active). Reports which method worked.",
          schema(std::slice::from_ref(&id))),
        t("click", "Real pointer click on an element's centre, for elements without actions.\nThe element's window must be active.",
          schema(&[id.clone(), ("button", "string", Some(json!("left"))), ("clicks", "integer", Some(json!(1)))])),
        t("key", "Press a key or chord in the active window, e.g. \"ctrl+s\", \"alt+tab\", \"enter\",\n\"f5\", \"left\". Only sent when the active window belongs to an allowed app.",
          schema(&[("combo", "string", None), ("repeat", "integer", Some(json!(1)))])),
        t("type_text", "Type text into the focused element of the active window. Works with any\nkeyboard layout and Unicode. On Linux this goes through the clipboard and\noverwrites it.",
          schema(&[("text", "string", None)])),
        t("mouse", "Fallback pointer input in screen coordinates: move, then click\n(`clicks`=0 to only move) and/or scroll (positive = down).",
          schema(&[("x", "integer", None), ("y", "integer", None), ("button", "string", Some(json!("left"))),
                   ("clicks", "integer", Some(json!(1))), ("scroll", "integer", Some(json!(0)))])),
        t("activate_window", "Bring an allowed app's window to the front by title or app-name substring.",
          schema(&[("query", "string", None)])),
        t("screenshot", "Last resort only: capture the active window to a PNG and return its path\n(read it with the Read tool). Prefer tree/find.", schema(&[])),
        t("batch", "Run several tool calls in order in one round trip, stopping at the first\nerror. `steps` is a list of {\"tool\": name, \"args\": {...}}, e.g.\n[{\"tool\": \"activate_window\", \"args\": {\"query\": \"Kate\"}},\n {\"tool\": \"key\", \"args\": {\"combo\": \"ctrl+end\"}},\n {\"tool\": \"type_text\", \"args\": {\"text\": \"hello\"}}].\nIds from tree/find made earlier can be used; ids printed inside the same\nbatch cannot.",
          batch_schema),
    ])
}

const PROTOCOL_VERSIONS: [&str; 4] = ["2025-06-18", "2025-03-26", "2024-11-05", "2025-11-25"];

async fn handle(server: &Server, msg: &Json) -> Option<Json> {
    let id = msg.get("id")?.clone();
    let method = msg.get("method").and_then(Json::as_str).unwrap_or("");
    let params = msg.get("params").cloned().unwrap_or(Json::Null);
    let result = match method {
        "initialize" => {
            let asked = params["protocolVersion"].as_str().unwrap_or("");
            let version = if PROTOCOL_VERSIONS.contains(&asked) { asked } else { PROTOCOL_VERSIONS[0] };
            json!({
                "protocolVersion": version,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": { "name": "desktop", "version": env!("CARGO_PKG_VERSION") },
            })
        }
        "ping" => json!({}),
        "tools/list" => json!({ "tools": tools() }),
        "tools/call" => {
            let name = params["name"].as_str().unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
            let (text, is_error) = match server.call(name, &args).await {
                Ok(t) => (t, false),
                Err(e) => (format!("Error executing tool {name}: {e}"), true),
            };
            json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
        }
        _ => {
            return Some(json!({ "jsonrpc": "2.0", "id": id,
                "error": { "code": -32601, "message": format!("Method not found: {method}") } }));
        }
    };
    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("desktop-mcp: {e}");
        std::process::exit(1);
    }
}

async fn run() -> R<()> {
    let session = zbus::Connection::session().await.map_err(|e| format!("session bus: {e}"))?;
    let a11y = A11y::connect(&session).await?;
    let kwin = KWin::new(session.clone()).await?;
    let input = Arc::new(Input::default());
    // Creating virtual input devices takes a moment; do it while the client
    // is still connecting so the first key/click is not delayed.
    let warm = input.clone();
    tokio::task::spawn_blocking(move || warm.warm_up());
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let server = Server {
        session,
        a11y,
        kwin,
        input,
        refs: Mutex::new(Refs { map: HashMap::new(), next: 1 }),
        allow_file: home.join(".config/desktop-mcp/allow.txt"),
    };
    server.allowed();

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();
    while let Some(line) = lines.next_line().await.map_err(|e| e.to_string())? {
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Json>(&line) {
            Ok(msg) => handle(&server, &msg).await,
            Err(e) => Some(json!({ "jsonrpc": "2.0", "id": null,
                "error": { "code": -32700, "message": format!("Parse error: {e}") } })),
        };
        if let Some(reply) = reply {
            let mut out = reply.to_string();
            out.push('\n');
            stdout.write_all(out.as_bytes()).await.map_err(|e| e.to_string())?;
            stdout.flush().await.map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}
