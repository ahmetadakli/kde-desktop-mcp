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

use backend::{Backend, Cache, R};
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
        Server { b, refs: Mutex::new(Refs { map: HashMap::new(), next: 1 }), allow_file }
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
            return Err(format!(
                "active window app '{}' is not in {}; activate an allowed window first",
                if name.is_empty() { "unknown" } else { &name }, self.allow_file.display()));
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
        self.b.app_windows(&name.to_lowercase()).await
            .ok_or_else(|| format!("app '{name}' not found in the accessibility tree"))
    }

    async fn print(&self, lines: Vec<Line<B::Id>>, cache: &Cache<B::Id>, app: &str) -> String {
        let mut out = Vec::with_capacity(lines.len());
        for line in lines {
            out.push(match line {
                Line::Raw(s) => Printed::Raw(s),
                Line::Node { depth, id } => {
                    let rid = self.add_ref(&id, app).await;
                    Printed::Node { depth, rid, info: &cache[&id] }
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
            let flag = if allow.contains(&app.name.to_lowercase()) { "allowed" } else { "blocked" };
            lines.push(format!("{} ({flag})", app.name));
            for w in app.windows {
                let act = if w.active { " [active]" } else { "" };
                lines.push(format!("  - {}: {}{act}", w.kind, render::py_repr(&w.title)));
            }
        }
        lines.push(format!("allowlist file: {}", self.allow_file.display()));
        if !self.b.apps_note().is_empty() {
            lines.push(self.b.apps_note().into());
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
        } else {
            let app = arg_s(a, "app", "");
            let windows = self.app_windows(&app).await?;
            (app.clone(), windows, app.to_lowercase())
        };
        let first = (opts.budget * 4).max(1000);
        self.b.prefetch(&mut cache, roots.clone(), opts.only_showing, TREE_TEXT_LIMIT, first).await;
        let lines = loop {
            let mut lines = vec![Line::Raw(header.clone())];
            match render::walk(&cache, &roots, 1, &opts, &mut lines) {
                Ok(()) => break lines,
                Err(missing) => {
                    self.b.prefetch(&mut cache, missing, opts.only_showing, TREE_TEXT_LIMIT,
                                    PREFETCH_MORE).await;
                }
            }
        };
        Ok(self.print(lines, &cache, &app).await)
    }

    async fn find(&self, a: &Json) -> R<String> {
        self.b.enable().await;
        let query = req_s(a, "query")?;
        let app = req_s(a, "app")?;
        let role = arg_s(a, "role", "");
        let max_results = arg_i(a, "max_results", 30).max(0) as usize;
        let windows = self.app_windows(&app).await?;
        let mut cache = Cache::new();
        self.b.prefetch(&mut cache, windows.clone(), true, render::FIND_TEXT_LIMIT,
                        render::FIND_MAX_SEEN).await;
        let hits = loop {
            match render::find(&cache, &windows, &query, &role, max_results) {
                Ok(h) => break h,
                Err(missing) => {
                    self.b.prefetch(&mut cache, missing, true, render::FIND_TEXT_LIMIT,
                                    PREFETCH_MORE).await;
                }
            }
        };
        if hits.is_empty() {
            return Ok("no matches".into());
        }
        let lines = hits.into_iter().map(|id| Line::Node { depth: 0, id }).collect();
        Ok(self.print(lines, &cache, &app.to_lowercase()).await)
    }

    async fn act(&self, a: &Json) -> R<String> {
        let id = self.resolve(req_i(a, "id")?).await?;
        let action = arg_s(a, "action", "");
        let names = self.b.actions(&id).await;
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
        let ok = self.b.do_action(&id, idx).await?;
        Ok(format!("{} -> {}", names[idx], if ok { "ok" } else { "failed" }))
    }

    async fn set_text(&self, a: &Json) -> R<String> {
        let id = self.resolve(req_i(a, "id")?).await?;
        let text = req_s(a, "text")?;
        Ok(if self.b.set_text(&id, &text).await? { "ok" } else { "failed" }.into())
    }

    async fn get_text(&self, a: &Json) -> R<String> {
        let id = self.resolve(req_i(a, "id")?).await?;
        self.b.full_text(&id, arg_i(a, "limit", 20000).max(0) as usize).await
    }

    async fn focus(&self, a: &Json) -> R<String> {
        let id = self.resolve(req_i(a, "id")?).await?;
        if let Some(method) = self.b.try_focus(&id).await? {
            return Ok(format!("ok ({method})"));
        }
        self.require_active_allowed().await?;
        let (x, y) = self.b.element_center(&id).await?;
        self.b.pointer(x as i64, y as i64, "left", 1, 0).await?;
        if wait_for(Duration::from_millis(150), || self.b.is_focused(&id)).await {
            return Ok(format!("ok (clicked at {x},{y})"));
        }
        Ok(format!("clicked at {x},{y} but element did not report focus"))
    }

    async fn click(&self, a: &Json) -> R<String> {
        let id = self.resolve(req_i(a, "id")?).await?;
        let button = arg_s(a, "button", "left");
        let clicks = arg_i(a, "clicks", 1);
        self.require_active_allowed().await?;
        let (x, y) = self.b.element_center(&id).await?;
        self.b.pointer(x as i64, y as i64, &button, clicks, 0).await?;
        Ok(format!("clicked {button} x{clicks} at {x},{y}"))
    }

    async fn key(&self, a: &Json) -> R<String> {
        let combo = req_s(a, "combo")?;
        let repeat = arg_i(a, "repeat", 1);
        self.b.check_combo(&combo)?;
        let app = self.require_active_allowed().await?;
        for _ in 0..repeat.clamp(1, 100) {
            self.b.press_combo(&combo).await?;
        }
        Ok(format!("sent {combo} x{repeat} to {app}"))
    }

    async fn type_text(&self, a: &Json) -> R<String> {
        let text = req_s(a, "text")?;
        let app = self.require_active_allowed().await?;
        self.b.type_text(&text).await?;
        Ok(format!("typed {} chars into {app}", text.chars().count()))
    }

    async fn mouse(&self, a: &Json) -> R<String> {
        let (x, y) = (req_i(a, "x")?, req_i(a, "y")?);
        let app = self.require_active_allowed().await?;
        self.b.pointer(x, y, &arg_s(a, "button", "left"), arg_i(a, "clicks", 1),
                       arg_i(a, "scroll", 0)).await?;
        Ok(format!("pointer at ({x},{y}) in {app}"))
    }

    async fn activate_window(&self, a: &Json) -> R<String> {
        let query = req_s(a, "query")?;
        self.b.activate_window(&query, &self.allowed()).await?;
        let app = self.b.active_app().await;
        Ok(format!("active app now: {}", if app.is_empty() { "unknown" } else { &app }))
    }

    async fn screenshot(&self) -> R<String> {
        self.require_active_allowed().await?;
        Ok(self.b.screenshot().await.unwrap_or_else(|| "screenshot failed".into()))
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

const PROTOCOL_VERSIONS: [&str; 4] = ["2025-06-18", "2025-03-26", "2024-11-05", "2025-11-25"];

async fn handle<B: Backend>(server: &Server<B>, msg: &Json) -> Option<Json> {
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
        "tools/list" => json!({ "tools": tools::tools() }),
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

async fn serve<B: Backend>(backend: B) -> R<()> {
    // Creating virtual input devices takes a moment; do it while the client
    // is still connecting so the first key/click is not delayed.
    backend.warm_up();
    let server = Server::new(backend);
    server.b.enable().await;
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

#[cfg(not(any(target_os = "linux", windows)))]
compile_error!("desktop-mcp supports Linux (KDE Plasma, Hyprland) and Windows only");

#[tokio::main(flavor = "current_thread")]
async fn main() {
    #[cfg(target_os = "linux")]
    let result = match linux::LinuxBackend::new().await {
        Ok(b) => serve(b).await,
        Err(e) => Err(e),
    };
    #[cfg(windows)]
    let result = match windows::WindowsBackend::new() {
        Ok(b) => serve(b).await,
        Err(e) => Err(e),
    };
    if let Err(e) = result {
        eprintln!("desktop-mcp: {e}");
        std::process::exit(1);
    }
}
