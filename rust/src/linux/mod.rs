//! Linux backend: AT-SPI for reading and acting, uinput for keyboard and
//! buttons, and the compositor (KWin or Hyprland) for windows and the cursor.

mod atspi;
mod hyprland;
mod input;
mod kwin;
mod roles;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value as Json;
use tokio::io::AsyncWriteExt;
use zbus::zvariant::OwnedObjectPath;

use crate::backend::{AppEntry, Backend, Cache, Info, R, WindowEntry};
use atspi::{A11y, Obj};
use hyprland::Hypr;
use input::Input;
use kwin::KWin;


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
const APPS_NOTE_HYPRLAND: &str = "Apps started before accessibility was enabled may need a \
restart (Chromium/Electron: --force-renderer-accessibility). On Hyprland, Qt apps may need \
QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1 and the at-spi bus must be running in the session.";
const BORING: &[&str] = &["filler", "panel", "section", "unknown", "redundant object", "grouping"];
const ACTIVATE_WAIT: Duration = Duration::from_millis(500);
const APP_ROOT: &str = "/org/a11y/atspi/accessible/root";

enum Compositor {
    Kde(KWin),
    Hyprland(Hypr),
}

pub struct LinuxBackend {
    session: zbus::Connection,
    a11y: A11y,
    comp: Compositor,
    input: Arc<Input>,
    /// Process id per AT-SPI bus name (stable for a connection's lifetime).
    pids: Mutex<HashMap<String, Option<u32>>>,
}

fn which(cmd: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|d| d.join(cmd).is_file())
    })
}

async fn poll<F, Fut>(limit: Duration, mut check: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let start = std::time::Instant::now();
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

/// Paste instead of typing key by key: works with any layout and Unicode.
async fn wl_copy(text: &str) -> R<()> {
    let mut child = tokio::process::Command::new("wl-copy")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("wl-copy: {e}"))?;
    let mut stdin = child.stdin.take().ok_or("wl-copy: no stdin")?;
    stdin.write_all(text.as_bytes()).await.map_err(|e| format!("wl-copy: {e}"))?;
    drop(stdin);
    // wl-copy's parent exits only once the compositor has the selection.
    let status = child.wait().await.map_err(|e| format!("wl-copy: {e}"))?;
    if !status.success() {
        return Err(format!("wl-copy failed: {status}"));
    }
    Ok(())
}

/// A new, empty file only this user can read, for a screenshot. Creating it
/// exclusively means a planted file or symlink in /tmp makes this fail
/// instead of being written through.
fn temp_png() -> Option<PathBuf> {
    use std::os::unix::fs::OpenOptionsExt;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let path = std::env::temp_dir().join(format!("desktop-mcp-{}-{nanos}.png", std::process::id()));
    std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path).ok()?;
    Some(path)
}

impl LinuxBackend {
    /// Hyprland when HYPRLAND_INSTANCE_SIGNATURE is set, otherwise KDE;
    /// DESKTOP_MCP_BACKEND=kde|hyprland overrides.
    pub async fn new() -> R<Self> {
        let session = zbus::Connection::session().await.map_err(|e| format!("session bus: {e}"))?;
        let a11y = A11y::connect(&session).await?;
        let forced = std::env::var("DESKTOP_MCP_BACKEND").unwrap_or_default().to_lowercase();
        let hypr = match forced.as_str() {
            "hyprland" => Some(Hypr::from_env().ok_or("HYPRLAND_INSTANCE_SIGNATURE is not set")?),
            "kde" => None,
            "" => Hypr::from_env(),
            other => return Err(format!("unknown DESKTOP_MCP_BACKEND '{other}' (kde, hyprland)")),
        };
        let comp = match hypr {
            Some(h) => Compositor::Hyprland(h),
            None => Compositor::Kde(KWin::new(session.clone()).await?),
        };
        let absolute = matches!(comp, Compositor::Kde(_));
        Ok(LinuxBackend { session, a11y, comp, input: Arc::new(Input::new(absolute)),
                          pids: Mutex::new(HashMap::new()) })
    }

    fn app_of(o: &Obj) -> Obj {
        Obj::new(&o.bus, OwnedObjectPath::try_from(APP_ROOT).unwrap())
    }

    async fn pid(&self, bus: &str) -> Option<u32> {
        if let Some(p) = self.pids.lock().unwrap().get(bus) {
            return *p;
        }
        let p = self.a11y.pid(bus).await;
        self.pids.lock().unwrap().insert(bus.to_string(), p);
        p
    }

    // ------------------------------------------------------------ Hyprland

    /// AT-SPI app name per process id, from one registry scan; the first
    /// app wins when a process registered several.
    async fn app_names_by_pid(&self) -> HashMap<i64, String> {
        let mut out = HashMap::new();
        for (app, name, _) in self.a11y.apps_with_windows().await {
            if let Some(pid) = self.pid(&app.bus).await {
                out.entry(i64::from(pid)).or_insert(name);
            }
        }
        out
    }

    /// Prefer the AT-SPI name so one allowlist works for every Linux
    /// backend; fall back to the window class for apps without accessibility.
    fn client_app_name(names: &HashMap<i64, String>, client: &Json) -> String {
        let by_pid = client["pid"].as_i64().and_then(|p| names.get(&p)).filter(|n| !n.is_empty());
        by_pid.map(String::as_str).unwrap_or(client["class"].as_str().unwrap_or("")).to_lowercase()
    }

    async fn same_window(&self, frame: &Obj, client: &Json) -> bool {
        let app = Self::app_of(frame);
        if self.pid(&app.bus).await.map(i64::from) != client["pid"].as_i64() {
            return false;
        }
        if self.a11y.children(&app).await.len() <= 1 {
            return true;
        }
        hyprland::norm(&self.a11y.name(frame).await)
            == hyprland::norm(client["title"].as_str().unwrap_or(""))
    }

    // ------------------------------------------------------------ KDE

    async fn kde_frame_active(&self, caption: &str) -> bool {
        let want = hyprland::norm(caption);
        let a11y = &self.a11y;
        for (_, _, windows) in a11y.apps_with_windows().await {
            for w in windows {
                let (name, active) = tokio::join!(a11y.name(&w), a11y.has_state(&w, atspi::ACTIVE));
                if active && hyprland::norm(&name) == want {
                    return true;
                }
            }
        }
        false
    }
}

impl Backend for LinuxBackend {
    type Id = Obj;

    fn allow_file(&self) -> PathBuf {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        home.join(".config/desktop-mcp/allow.txt")
    }

    fn default_allow(&self) -> &'static str {
        DEFAULT_ALLOW
    }

    fn apps_note(&self) -> &'static str {
        match self.comp {
            Compositor::Kde(_) => APPS_NOTE,
            Compositor::Hyprland(_) => APPS_NOTE_HYPRLAND,
        }
    }

    fn boring_roles(&self) -> &'static [&'static str] {
        BORING
    }

    async fn enable(&self) {
        atspi::enable(&self.session).await;
    }

    fn warm_up(&self) {
        let input = self.input.clone();
        std::thread::spawn(move || input.warm_up());
    }

    async fn list_apps(&self) -> Vec<AppEntry> {
        let apps = self.a11y.apps_with_windows().await;
        let active_client = match &self.comp {
            Compositor::Hyprland(h) => h.active_window().await.ok().flatten(),
            Compositor::Kde(_) => None,
        };
        let a11y = &self.a11y;
        let details = futures_util::future::join_all(apps.iter().map(|(_, _, ws)| {
            futures_util::future::join_all(ws.iter().map(|w| async {
                let (kind, title, active) = tokio::join!(
                    a11y.role(w), a11y.name(w), async {
                        match (&self.comp, &active_client) {
                            (Compositor::Kde(_), _) => a11y.has_state(w, atspi::ACTIVE).await,
                            (Compositor::Hyprland(_), Some(c)) => self.same_window(w, c).await,
                            (Compositor::Hyprland(_), None) => false,
                        }
                    });
                WindowEntry { kind, title: title.trim().to_string(), active }
            }))
        }))
        .await;
        apps.into_iter().zip(details)
            .map(|((_, name, _), windows)| AppEntry { name, windows })
            .collect()
    }

    async fn app_windows(&self, app: &str) -> Option<Vec<Obj>> {
        let mut found = false;
        let mut windows = Vec::new();
        for (_, n, ws) in self.a11y.apps_with_windows().await {
            if n.to_lowercase() == app {
                found = true;
                windows.extend(ws);
            }
        }
        found.then_some(windows)
    }

    async fn active_app(&self) -> String {
        match &self.comp {
            Compositor::Kde(_) => self.a11y.active_app().await,
            Compositor::Hyprland(h) => match h.active_window().await.ok().flatten() {
                Some(c) => Self::client_app_name(&self.app_names_by_pid().await, &c),
                None => String::new(),
            },
        }
    }

    async fn info(&self, id: &Obj, text_limit: usize) -> Info<Obj> {
        self.a11y.info(id, text_limit).await
    }

    async fn prefetch(&self, cache: &mut Cache<Obj>, roots: Vec<Obj>, only_showing: bool,
                      text_limit: usize, limit: usize) {
        self.a11y.prefetch(cache, roots, only_showing, text_limit, limit).await
    }

    async fn actions(&self, id: &Obj) -> Vec<String> {
        self.a11y.actions(id).await
    }

    async fn do_action(&self, id: &Obj, index: usize) -> R<bool> {
        self.a11y.do_action(id, index as i32).await
    }

    async fn full_text(&self, id: &Obj, limit: usize) -> R<String> {
        if self.a11y.interfaces(id).await.iter().any(|i| i == "org.a11y.atspi.Text") {
            self.a11y.text(id, limit.min(i32::MAX as usize) as i32).await
        } else {
            Ok(self.a11y.name(id).await)
        }
    }

    async fn set_text(&self, id: &Obj, text: &str) -> R<bool> {
        if !self.a11y.interfaces(id).await.iter().any(|i| i == "org.a11y.atspi.EditableText") {
            return Err("element is not editable text; use focus + type".into());
        }
        self.a11y.set_text(id, text).await
    }

    async fn is_focused(&self, id: &Obj) -> bool {
        self.a11y.has_state(id, atspi::FOCUSED).await
    }

    async fn try_focus(&self, id: &Obj) -> R<Option<String>> {
        // Qt only honours GrabFocus when the element exposes a "Set Focus"
        // action, so try both before the caller falls back to a click.
        let (ifaces, actions) = tokio::join!(self.a11y.interfaces(id), self.a11y.actions(id));
        if !ifaces.iter().any(|i| i == "org.a11y.atspi.Component") {
            return Err("element cannot take focus".into());
        }
        let wait = Duration::from_millis(50);
        if self.a11y.grab_focus(id).await && poll(wait, || self.is_focused(id)).await {
            return Ok(Some("grab_focus".into()));
        }
        let names: Vec<String> = actions.iter().map(|n| n.to_lowercase().replace(' ', "")).collect();
        if let Some(i) = names.iter().position(|n| n == "setfocus") {
            let _ = self.a11y.do_action(id, i as i32).await;
            if poll(wait, || self.is_focused(id)).await {
                return Ok(Some("set focus action".into()));
            }
        }
        Ok(None)
    }

    async fn element_center(&self, id: &Obj) -> R<(i32, i32)> {
        const NOT_ACTIVE: &str = "element's window is not active; call activate_window first";
        let a11y = &self.a11y;
        let no_area = || "element has no on-screen area".to_string();
        match &self.comp {
            Compositor::Kde(kwin) => {
                // AT-SPI extents are relative to the toplevel surface on
                // Wayland, so add the active window's client origin.
                let (frame_active, ext, origin) = tokio::join!(
                    async {
                        match a11y.frame_of(id).await {
                            Some(f) => a11y.has_state(&f, atspi::ACTIVE).await,
                            None => false,
                        }
                    },
                    a11y.extents(id, atspi::COORD_WINDOW),
                    kwin.active_window_origin());
                if !frame_active {
                    return Err(NOT_ACTIVE.into());
                }
                let (x, y, w, h) = ext?;
                if w <= 0 || h <= 0 {
                    return Err(no_area());
                }
                let (gx, gy) = origin?;
                Ok(((gx + x as f64 + w as f64 / 2.0) as i32, (gy + y as f64 + h as f64 / 2.0) as i32))
            }
            Compositor::Hyprland(h) => {
                let (aw, frame) = tokio::join!(h.active_window(), a11y.frame_of(id));
                let (Some(aw), Some(frame)) = (aw?, frame) else { return Err(NOT_ACTIVE.into()) };
                if !self.same_window(&frame, &aw).await {
                    return Err(NOT_ACTIVE.into());
                }
                if aw["xwayland"].as_bool().unwrap_or(false) {
                    // X11 clients report real screen coordinates.
                    let (x, y, w, hh) = a11y.extents(id, atspi::COORD_SCREEN).await?;
                    if w <= 0 || hh <= 0 {
                        return Err(no_area());
                    }
                    return Ok(((x as f64 + w as f64 / 2.0) as i32, (y as f64 + hh as f64 / 2.0) as i32));
                }
                // Wayland clients report surface-relative extents; add the window origin.
                let (x, y, w, hh) = a11y.extents(id, atspi::COORD_WINDOW).await?;
                if w <= 0 || hh <= 0 {
                    return Err(no_area());
                }
                let ox = aw["at"][0].as_f64().unwrap_or(0.0);
                let oy = aw["at"][1].as_f64().unwrap_or(0.0);
                Ok(((ox + x as f64 + w as f64 / 2.0) as i32, (oy + y as f64 + hh as f64 / 2.0) as i32))
            }
        }
    }

    fn check_combo(&self, combo: &str) -> R<()> {
        input::parse_combo(combo).map(|_| ())
    }

    async fn press_combo(&self, combo: &str) -> R<()> {
        self.input.press_combo(combo).await
    }

    async fn type_text(&self, text: &str) -> R<()> {
        if matches!(self.comp, Compositor::Hyprland(_)) && which("wtype") {
            // wtype uses the virtual-keyboard protocol: Unicode-safe and
            // leaves the clipboard alone.
            for args in hyprland::wtype_commands(text) {
                let status = tokio::process::Command::new("wtype").args(&args)
                    .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
                    .status().await.map_err(|e| format!("wtype: {e}"))?;
                if !status.success() {
                    return Err(format!("wtype failed: {status}"));
                }
            }
            return Ok(());
        }
        wl_copy(text).await?;
        self.input.press_combo("ctrl+v").await
    }

    async fn pointer(&self, x: i64, y: i64, button: &str, clicks: i64, scroll: i64) -> R<()> {
        input::button_code(button)?;
        match &self.comp {
            Compositor::Kde(kwin) => {
                let (x0, y0, x1, y1) = kwin.screen_box().await;
                let ax = ((x - x0 as i64) * input::ABS_MAX as i64 / (x1 - x0).max(1) as i64) as i32;
                let ay = ((y - y0 as i64) * input::ABS_MAX as i64 / (y1 - y0).max(1) as i64) as i32;
                self.input.move_abs(ax, ay).await?;
            }
            Compositor::Hyprland(h) => {
                // Hyprland places the cursor exactly; uinput only supplies buttons.
                h.dispatch(&format!("movecursor {x} {y}")).await?;
                self.input.settle().await;
            }
        }
        self.input.buttons(button, clicks, scroll).await
    }

    async fn activate_window(&self, query: &str, allowed: &[String]) -> R<()> {
        match &self.comp {
            Compositor::Kde(kwin) => {
                let caption = kwin.activate(query, allowed).await?;
                if !caption.is_empty() {
                    // KWin switches at once; the toolkit reports ACTIVE a moment later.
                    poll(ACTIVATE_WAIT, || self.kde_frame_active(&caption)).await;
                }
            }
            Compositor::Hyprland(h) => {
                let q = query.to_lowercase();
                let (clients, names) = tokio::join!(h.json("clients"), self.app_names_by_pid());
                let clients = clients?;
                for c in clients.as_array().into_iter().flatten() {
                    if !c["mapped"].as_bool().unwrap_or(true) || c["hidden"].as_bool().unwrap_or(false) {
                        continue;
                    }
                    let app = Self::client_app_name(&names, c);
                    if !allowed.contains(&app) {
                        continue;
                    }
                    let title = c["title"].as_str().unwrap_or("").to_lowercase();
                    let class = c["class"].as_str().unwrap_or("").to_lowercase();
                    if title.contains(&q) || app.contains(&q) || class.contains(&q) {
                        let addr = c["address"].as_str().unwrap_or("").to_string();
                        h.dispatch(&format!("focuswindow address:{addr}")).await?;
                        poll(ACTIVATE_WAIT, || async {
                            h.active_window().await.ok().flatten()
                                .is_some_and(|aw| aw["address"].as_str() == Some(addr.as_str()))
                        }).await;
                        return Ok(());
                    }
                }
            }
        }
        Ok(())
    }

    async fn screenshot(&self) -> Option<String> {
        let mut cmd = match &self.comp {
            Compositor::Kde(_) => {
                let mut c = tokio::process::Command::new("spectacle");
                c.args(["-b", "-n", "-a", "-o"]);
                c
            }
            Compositor::Hyprland(h) => {
                let aw = h.active_window().await.ok().flatten()?;
                if !which("grim") {
                    return None;
                }
                let geo = format!("{},{} {}x{}", aw["at"][0], aw["at"][1], aw["size"][0], aw["size"][1]);
                let mut c = tokio::process::Command::new("grim");
                c.args(["-g", &geo]);
                c
            }
        };
        let path = temp_png()?;
        let _ = cmd.arg(&path).output().await;
        // The file exists from the start; it only counts once written.
        if std::fs::metadata(&path).is_ok_and(|m| m.len() > 0) {
            Some(path.display().to_string())
        } else {
            let _ = std::fs::remove_file(&path);
            None
        }
    }
}
