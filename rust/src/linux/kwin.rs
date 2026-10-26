//! KWin over the session bus: scripting for the active window geometry,
//! the screen layout and raising windows, and ScreenShot2 for window
//! captures.
//!
//! KWin scripts cannot return values over D-Bus, so a script calls back into
//! an object this process exports on its own unique bus name.

use std::collections::HashMap;
use std::io::Read;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::Value as Json;
use tokio::sync::{Mutex, mpsc};
use zbus::Connection;
use zbus::zvariant::{Fd, OwnedValue, Value};

use crate::backend::{R, Screen, WindowChange, WindowGeo};

const SINK_PATH: &str = "/org/desktopmcp/sink";
const QUERY_TIMEOUT: Duration = Duration::from_secs(3);
/// How long the outputs' bounding box is reused for mapping pointer moves.
/// Outputs rarely change, and every element-based pointer tool refreshes it
/// for free (the active-window script reports it), so only `mouse` and
/// `drag` by coordinates rely on it for this long after a monitor change.
const SCREEN_BOX_TTL: Duration = Duration::from_secs(60);
const SHOT_PATH: &str = "/org/kde/KWin/ScreenShot2";
const SHOT_IFACE: &str = "org.kde.KWin.ScreenShot2";
/// KWin 6.8 serves ScreenShot2 to any client on the session bus. Before
/// that (dropped in KWin commit 0ea31ae7), each call looked up the caller's
/// pid, resolved /proc/<pid>/exe and searched the installed applications
/// (ksycoca) for a .desktop file whose Exec= program has that canonical
/// path; the call was refused with this error unless the file's
/// X-KDE-DBUS-Restricted-Interfaces listed org.kde.KWin.ScreenShot2
/// (KWIN_SCREENSHOT_NO_PERMISSION_CHECKS=1 in KWin's environment disabled
/// the check). On such a KWin `screenshot` falls back to spectacle; to grant
/// the faster path once, install by hand, then run `kbuildsycoca6`:
///
/// ```text
/// ~/.local/share/applications/desktop-mcp.desktop
/// [Desktop Entry]
/// Type=Application
/// Name=desktop-mcp
/// Exec=/absolute/path/to/rust/target/release/desktop-mcp
/// NoDisplay=true
/// X-KDE-DBUS-Restricted-Interfaces=org.kde.KWin.ScreenShot2
/// ```
pub const SHOT_NOT_AUTHORIZED: &str = "org.kde.KWin.ScreenShot2.Error.NoAuthorized";

/// A capture as KWin sent it: `format` is a QImage::Format value.
pub struct RawImage {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: u32,
    pub data: Vec<u8>,
}

/// A window's frame in logical px, (x, y, width, height), and its
/// ScreenShot2 handle when KWin gave one.
pub struct Frame {
    pub area: (f64, f64, f64, f64),
    pub handle: Option<String>,
}

/// (x0, y0, x1, y1) in logical px.
type ScreenBox = (i32, i32, i32, i32);

/// Which apps the window tools may touch: the allowlist, plus the pids of
/// AT-SPI apps sorted by their name against it. A window's pid decides
/// first, so an app whose window class differs from its AT-SPI name
/// (Ferdium: `electron`) still matches, and a blocked app stays blocked
/// whatever its class. Windows of other pids fall back to class matching.
#[derive(Debug, Default, PartialEq)]
pub struct Allow {
    pub names: Vec<String>,
    pub pids: Vec<u32>,
    pub blocked_pids: Vec<u32>,
}

impl Allow {
    /// `apps` holds (pid, AT-SPI app name). A pid with several apps is
    /// blocked when any of them is; unnamed apps say nothing.
    pub fn new(allowed: &[String], apps: &[(u32, String)]) -> Self {
        let mut pids = Vec::new();
        let mut blocked_pids = Vec::new();
        for (pid, name) in apps {
            let name = name.trim().to_lowercase();
            if name.is_empty() {
                continue;
            }
            if allowed.contains(&name) {
                pids.push(*pid);
            } else {
                blocked_pids.push(*pid);
            }
        }
        pids.retain(|p| !blocked_pids.contains(p));
        pids.sort_unstable();
        pids.dedup();
        blocked_pids.sort_unstable();
        blocked_pids.dedup();
        Allow {
            names: allowed.to_vec(),
            pids,
            blocked_pids,
        }
    }

    /// OK_APP's test outside KWin (Hyprland): by pid first, else by a
    /// class containing an allowlist entry.
    pub fn permits(&self, pid: u32, classes: &[&str]) -> bool {
        if self.blocked_pids.contains(&pid) {
            return false;
        }
        if self.pids.contains(&pid) {
            return true;
        }
        classes.iter().any(|c| {
            let c = c.to_lowercase();
            self.names.iter().any(|a| c.contains(a.as_str()))
        })
    }

    /// OK_APP with this allowlist filled in.
    fn ok_app_js(&self) -> String {
        OK_APP
            .replace("%ALLOW%", &Json::from(self.names.clone()).to_string())
            .replace("%ALLOW_PIDS%", &Json::from(self.pids.clone()).to_string())
            .replace(
                "%BLOCK_PIDS%",
                &Json::from(self.blocked_pids.clone()).to_string(),
            )
    }
}

struct Sink {
    tx: mpsc::UnboundedSender<(String, String)>,
}

#[zbus::interface(name = "org.desktopmcp.Sink")]
impl Sink {
    fn report(&self, id: String, data: String) {
        let _ = self.tx.send((id, data));
    }
}

pub struct KWin {
    session: Connection,
    rx: Mutex<mpsc::UnboundedReceiver<(String, String)>>,
    seq: AtomicU64,
    screen: Mutex<Option<(Instant, ScreenBox)>>,
}

/// Whether a window belongs to an allowed app: by its pid's AT-SPI app name
/// when it has one (the name tree and find use), else by its class or
/// resource name containing an allowlist entry.
const OK_APP: &str = r#"
const allow = %ALLOW%;
const allowPids = %ALLOW_PIDS%;
const blockPids = %BLOCK_PIDS%;
function okApp(w) {
  if (blockPids.includes(w.pid)) return false;
  if (allowPids.includes(w.pid)) return true;
  const cls = (w.resourceClass || "").toLowerCase();
  const name = (w.resourceName || "").toLowerCase();
  return allow.some(a => cls.includes(a) || name.includes(a));
}
"#;

/// Raises the first window of an allowed app whose caption or class contains
/// the query; evaluates to its caption and pid as JSON, "" when none
/// matched.
const ACTIVATE: &str = r#"(() => { %OK_APP%
const q = %QUERY%.toLowerCase();
for (const w of workspace.windowList()) {
  const cls = (w.resourceClass || "").toLowerCase();
  if (okApp(w) && (w.caption.toLowerCase().includes(q) || cls.includes(q))) {
    workspace.activeWindow = w;
    return JSON.stringify({caption: w.caption, pid: w.pid});
  }
}
return "";
})()"#;

/// Shared by WINDOWS and SET_WINDOW: windows of allowed apps whose caption
/// or class contains the query, and their geometry.
const WINDOW_JS: &str = r#"%OK_APP%
const q = %QUERY%.toLowerCase();
function matching() {
  return workspace.windowList().filter(w => (w.normalWindow || w.dialog) && okApp(w)
    && (!q || w.caption.toLowerCase().includes(q)
        || (w.resourceClass || "").toLowerCase().includes(q)));
}
function geo(w) {
  const g = w.frameGeometry;
  const m = workspace.clientArea(KWin.MaximizeArea, w);
  return {title: w.caption, app: w.resourceClass || "", x: g.x, y: g.y,
    width: g.width, height: g.height, active: w === workspace.activeWindow,
    minimized: w.minimized, output: w.output ? w.output.name : "", pid: w.pid,
    maximized: g.x === m.x && g.y === m.y && g.width === m.width && g.height === m.height};
}
"#;

const WINDOWS: &str = "(() => { %JS% return JSON.stringify(matching().map(geo)); })()";

/// Applies %CHANGE% ({x, y, width, height, state}; null fields kept) to the
/// first matching window; evaluates to its new geometry, "" when none.
const SET_WINDOW: &str = r#"(() => { %JS%
const c = %CHANGE%;
const w = matching()[0];
if (!w) return "";
if (c.state === "minimize") w.minimized = true;
if (c.state === "maximize") { w.minimized = false; w.setMaximize(true, true); }
if (c.state === "restore") { w.minimized = false; w.setMaximize(false, false); }
if (c.x !== null || c.y !== null || c.width !== null || c.height !== null) {
  w.setMaximize(false, false);
  const g = w.frameGeometry;
  w.frameGeometry = {x: c.x ?? g.x, y: c.y ?? g.y,
    width: c.width ?? g.width, height: c.height ?? g.height};
}
return JSON.stringify(geo(w));
})()"#;

const SCREENS_JS: &str = "workspace.screens.map(o => ({name: o.name, \
     x: o.geometry.x, y: o.geometry.y, width: o.geometry.width, \
     height: o.geometry.height, scale: o.devicePixelRatio}))";

/// The outputs and the matching windows in one script run.
const SCREENS_AND_WINDOWS: &str = "(() => { %JS% return JSON.stringify(\
     {screens: %SCREENS%, windows: matching().map(geo)}); })()";

const GEOMETRY: &str = "JSON.stringify((() => { const w = workspace.activeWindow; if (!w) return null; \
     const g = w.clientGeometry; const s = workspace.virtualScreenGeometry; \
     return {x: g.x, y: g.y, width: g.width, height: g.height, caption: w.caption, \
     cls: w.resourceClass, screen: {x: s.x, y: s.y, width: s.width, height: s.height}}; })())";

/// The active window's frame and its internal id, the handle ScreenShot2's
/// CaptureWindow takes ("{uuid}").
const FRAME: &str = "JSON.stringify((() => { const w = workspace.activeWindow; if (!w) return null; \
     const g = w.frameGeometry; return {x: g.x, y: g.y, width: g.width, height: g.height, \
     id: String(w.internalId)}; })())";

/// The active window's caption and process, and the window it is a
/// transient (dialog) for. A portal dialog gets its parent from the window
/// handle the requesting app exported (xdg-foreign), not from its title.
const ACTIVE_PARENT: &str = "JSON.stringify((() => { const w = workspace.activeWindow; if (!w) return null; \
     const p = w.transientFor; return {caption: w.caption, pid: w.pid, \
     parent: p ? {pid: p.pid, class: p.resourceClass || '', caption: p.caption} : null}; })())";

const VIRTUAL_SCREEN: &str = "JSON.stringify((() => { const g = workspace.virtualScreenGeometry; \
     return {x: g.x, y: g.y, width: g.width, height: g.height}; })())";

impl KWin {
    pub async fn new(session: Connection) -> R<Self> {
        let (tx, rx) = mpsc::unbounded_channel();
        session
            .object_server()
            .at(SINK_PATH, Sink { tx })
            .await
            .map_err(|e| e.to_string())?;
        Ok(KWin {
            session,
            rx: Mutex::new(rx),
            seq: AtomicU64::new(0),
            screen: Mutex::new(None),
        })
    }

    async fn call<T>(
        &self,
        path: &str,
        iface: &str,
        method: &str,
        args: &(impl serde::Serialize + zbus::zvariant::DynamicType),
    ) -> R<T>
    where
        T: for<'d> serde::Deserialize<'d> + zbus::zvariant::Type,
    {
        let reply = self
            .session
            .call_method(Some("org.kde.KWin"), path, Some(iface), method, args)
            .await
            .map_err(|e| format!("KWin {method}: {e}"))?;
        reply.body().deserialize().map_err(|e| e.to_string())
    }

    /// Evaluate a JS expression inside KWin and return its string value.
    pub async fn query(&self, js_expr: &str) -> R<Option<String>> {
        let me = self
            .session
            .unique_name()
            .ok_or("no unique bus name")?
            .to_string();
        let id = format!(
            "{}-{}",
            std::process::id(),
            self.seq.fetch_add(1, Ordering::Relaxed)
        );
        let script = format!(
            "const result = String({js_expr});\ncallDBus({}, {}, 'org.desktopmcp.Sink', 'Report', {}, result);\n",
            Json::from(me),
            Json::from(SINK_PATH),
            Json::from(id.as_str())
        );
        let file = write_script(&id, &script).await?;
        let plugin = format!("desktop-mcp-{id}");
        let result = self.run_script(&file, &plugin, &id).await;
        let _ = tokio::fs::remove_file(&file).await;
        result
    }

    async fn run_script(
        &self,
        file: &std::path::Path,
        plugin: &str,
        id: &str,
    ) -> R<Option<String>> {
        let mut rx = self.rx.lock().await;
        while rx.try_recv().is_ok() {} // reports from earlier, timed-out scripts
        let sid: i32 = self
            .call(
                "/Scripting",
                "org.kde.kwin.Scripting",
                "loadScript",
                &(file.to_string_lossy().as_ref(), plugin),
            )
            .await?;
        if sid < 0 {
            return Err("KWin refused script".into());
        }
        let ran: R<()> = self
            .call(
                &format!("/Scripting/Script{sid}"),
                "org.kde.kwin.Script",
                "run",
                &(),
            )
            .await;
        let data = match ran {
            Ok(()) => {
                let wait = async {
                    while let Some((got, data)) = rx.recv().await {
                        if got == id {
                            return Some(data);
                        }
                    }
                    None
                };
                tokio::time::timeout(QUERY_TIMEOUT, wait)
                    .await
                    .ok()
                    .flatten()
            }
            Err(_) => None,
        };
        let _: R<bool> = self
            .call(
                "/Scripting",
                "org.kde.kwin.Scripting",
                "unloadScript",
                &(plugin,),
            )
            .await;
        ran?;
        Ok(data)
    }

    /// Raise a window; returns its caption and pid, None when nothing
    /// matched.
    pub async fn activate(&self, query: &str, allow: &Allow) -> R<Option<Raised>> {
        let script = ACTIVATE
            .replace("%OK_APP%", &allow.ok_app_js())
            .replace("%QUERY%", &Json::from(query).to_string());
        let raw = self.query(&script).await?.unwrap_or_default();
        if raw.is_empty() {
            return Ok(None);
        }
        let v: Json = serde_json::from_str(&raw).map_err(|e| format!("KWin: {e}"))?;
        Ok(Some(raised_of(&v)))
    }

    /// Client-area origin (logical px) of KWin's active window.
    pub async fn active_window_origin(&self) -> R<(f64, f64)> {
        let raw = self.query(GEOMETRY).await?.unwrap_or_default();
        let geo: Json = serde_json::from_str(&raw).unwrap_or(Json::Null);
        if let Some(b) = screen_box_of(&geo["screen"]) {
            *self.screen.lock().await = Some((Instant::now(), b));
        }
        match (geo["x"].as_f64(), geo["y"].as_f64()) {
            (Some(x), Some(y)) => Ok((x, y)),
            _ => Err("could not read the active window geometry from KWin".into()),
        }
    }

    pub async fn windows(&self, query: &str, allow: &Allow) -> R<Vec<WindowGeo>> {
        let script = WINDOWS.replace("%JS%", &window_js(query, allow));
        let raw = self.query(&script).await?.unwrap_or_default();
        let list: Json = serde_json::from_str(&raw).map_err(|e| format!("KWin: {e}"))?;
        Ok(list
            .as_array()
            .map(|a| a.iter().map(window_geo).collect())
            .unwrap_or_default())
    }

    pub async fn set_window(
        &self,
        query: &str,
        allow: &Allow,
        change: &WindowChange,
    ) -> R<Option<WindowGeo>> {
        let change = serde_json::json!({
            "x": change.x, "y": change.y, "width": change.width,
            "height": change.height, "state": change.state,
        });
        let script = SET_WINDOW
            .replace("%JS%", &window_js(query, allow))
            .replace("%CHANGE%", &change.to_string());
        let raw = self.query(&script).await?.unwrap_or_default();
        if raw.is_empty() {
            return Ok(None);
        }
        let g: Json = serde_json::from_str(&raw).map_err(|e| format!("KWin: {e}"))?;
        Ok(Some(window_geo(&g)))
    }

    pub async fn screens(&self) -> R<Vec<Screen>> {
        let script = format!("JSON.stringify({SCREENS_JS})");
        let raw = self.query(&script).await?.unwrap_or_default();
        let list: Json = serde_json::from_str(&raw).map_err(|e| format!("KWin: {e}"))?;
        Ok(screens_of(&list))
    }

    /// `screens` and `windows` in one script run.
    pub async fn screens_and_windows(
        &self,
        query: &str,
        allow: &Allow,
    ) -> R<(Vec<Screen>, Vec<WindowGeo>)> {
        let script = SCREENS_AND_WINDOWS
            .replace("%JS%", &window_js(query, allow))
            .replace("%SCREENS%", SCREENS_JS);
        let raw = self.query(&script).await?.unwrap_or_default();
        let v: Json = serde_json::from_str(&raw).map_err(|e| format!("KWin: {e}"))?;
        let windows = v["windows"]
            .as_array()
            .map(|a| a.iter().map(window_geo).collect())
            .unwrap_or_default();
        Ok((screens_of(&v["screens"]), windows))
    }

    /// KWin's active window with its transient parent, see ACTIVE_PARENT;
    /// None without an active window.
    pub async fn active_parent(&self) -> R<Option<Json>> {
        let raw = self.query(ACTIVE_PARENT).await?.unwrap_or_default();
        let v: Json = serde_json::from_str(&raw).map_err(|e| format!("KWin: {e}"))?;
        Ok((!v.is_null()).then_some(v))
    }

    /// Frame (with title bar) of KWin's active window in logical px, and
    /// the window's handle for `capture_window`.
    pub async fn active_frame(&self) -> Option<Frame> {
        let raw = self.query(FRAME).await.ok()??;
        let g: Json = serde_json::from_str(&raw).ok()?;
        Some(Frame {
            area: (
                g["x"].as_f64()?,
                g["y"].as_f64()?,
                g["width"].as_f64()?,
                g["height"].as_f64()?,
            ),
            handle: g["id"]
                .as_str()
                .filter(|id| id.starts_with('{'))
                .map(str::to_string),
        })
    }

    /// Capture a window (with its title bar, without the shadow) through
    /// ScreenShot2: the one with this `active_frame` handle, so the image
    /// matches the frame read with it even if focus moved since, or else
    /// whichever is active. KWin writes the raw pixels into a pipe after
    /// replying with their layout.
    pub async fn capture_window(&self, handle: Option<&str>) -> R<RawImage> {
        let (mut reader, writer) = std::io::pipe().map_err(|e| format!("pipe: {e}"))?;
        // Read concurrently: KWin blocks on a full pipe until it is drained.
        let pixels = tokio::task::spawn_blocking(move || {
            let mut buf = Vec::new();
            reader.read_to_end(&mut buf).map(|_| buf)
        });
        let options = HashMap::from([
            ("include-decoration", Value::from(true)),
            ("include-shadow", Value::from(false)),
            ("include-cursor", Value::from(false)),
            ("native-resolution", Value::from(true)),
        ]);
        let fd = Fd::from(&writer);
        let bus = Some("org.kde.KWin");
        let (method, reply) = match handle {
            Some(h) => {
                let body = (h, options, fd);
                let m = "CaptureWindow";
                (
                    m,
                    self.session
                        .call_method(bus, SHOT_PATH, Some(SHOT_IFACE), m, &body)
                        .await,
                )
            }
            None => {
                let body = (options, fd);
                let m = "CaptureActiveWindow";
                (
                    m,
                    self.session
                        .call_method(bus, SHOT_PATH, Some(SHOT_IFACE), m, &body)
                        .await,
                )
            }
        };
        // KWin holds its own duplicate; ours must go for the read to end.
        drop(writer);
        let reply = match reply {
            Ok(r) => r,
            Err(zbus::Error::MethodError(name, msg, _)) => {
                return Err(format!("{}: {}", name.as_str(), msg.unwrap_or_default()));
            }
            Err(e) => return Err(format!("KWin {method}: {e}")),
        };
        let meta: HashMap<String, OwnedValue> =
            reply.body().deserialize().map_err(|e| e.to_string())?;
        let num = |k: &str| {
            meta.get(k)
                .and_then(|v| u32::try_from(v).ok())
                .ok_or_else(|| format!("ScreenShot2 reply lacks '{k}'"))
        };
        let kind = meta
            .get("type")
            .and_then(|v| String::try_from(v.try_clone().ok()?).ok());
        if kind.as_deref().unwrap_or("raw") != "raw" {
            return Err(format!("ScreenShot2 sent unsupported type {kind:?}"));
        }
        let (width, height, stride, format) = (
            num("width")?,
            num("height")?,
            num("stride")?,
            num("format")?,
        );
        let data = pixels
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| format!("pipe: {e}"))?;
        if (data.len() as u64) < u64::from(stride) * u64::from(height) {
            return Err(format!(
                "ScreenShot2 sent {} of {} bytes",
                data.len(),
                u64::from(stride) * u64::from(height)
            ));
        }
        Ok(RawImage {
            width,
            height,
            stride,
            format,
            data,
        })
    }

    /// Bounding box (x0, y0, x1, y1) of all enabled outputs in logical px.
    pub async fn screen_box(&self) -> (i32, i32, i32, i32) {
        let mut cached = self.screen.lock().await;
        if let Some((at, b)) = *cached
            && at.elapsed() < SCREEN_BOX_TTL
        {
            return b;
        }
        let b = match self.virtual_screen().await {
            Some(b) => b,
            None => read_screen_box().await.unwrap_or((0, 0, 1920, 1080)),
        };
        *cached = Some((Instant::now(), b));
        b
    }
}

impl KWin {
    /// Bounding box of all outputs from KWin itself, in logical px: one
    /// script run instead of starting kscreen-doctor.
    async fn virtual_screen(&self) -> Option<ScreenBox> {
        let raw = self.query(VIRTUAL_SCREEN).await.ok()??;
        screen_box_of(&serde_json::from_str(&raw).ok()?)
    }
}

/// Where KWin reads a script from: the private runtime directory when
/// there is one, else the temp directory.
fn script_dir() -> std::path::PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .filter(|d| d.is_absolute() && d.is_dir())
        .unwrap_or_else(std::env::temp_dir)
}

/// Write a script to a new file only this user can read: never through a
/// file or symlink planted at that name.
async fn write_script(id: &str, script: &str) -> R<std::path::PathBuf> {
    use tokio::io::AsyncWriteExt;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = script_dir().join(format!("desktop-mcp-{id}-{nanos}.js"));
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .await
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let written = async {
        file.write_all(script.as_bytes()).await?;
        file.flush().await
    }
    .await;
    if let Err(e) = written {
        let _ = tokio::fs::remove_file(&path).await;
        return Err(format!("{}: {e}", path.display()));
    }
    Ok(path)
}

/// (x0, y0, x1, y1) of a `{x, y, width, height}` rectangle with an area.
fn screen_box_of(g: &Json) -> Option<ScreenBox> {
    let (x, y) = (g["x"].as_f64()?, g["y"].as_f64()?);
    let (w, h) = (g["width"].as_f64()?, g["height"].as_f64()?);
    (w > 0.0 && h > 0.0).then_some((x as i32, y as i32, (x + w) as i32, (y + h) as i32))
}

fn window_js(query: &str, allow: &Allow) -> String {
    WINDOW_JS
        .replace("%OK_APP%", &allow.ok_app_js())
        .replace("%QUERY%", &Json::from(query).to_string())
}

/// The window ACTIVATE raised.
#[derive(Debug, PartialEq)]
pub struct Raised {
    pub caption: String,
    pub pid: u32,
}

fn raised_of(v: &Json) -> Raised {
    Raised {
        caption: v["caption"].as_str().unwrap_or("").to_string(),
        pid: v["pid"]
            .as_u64()
            .and_then(|p| u32::try_from(p).ok())
            .unwrap_or(0),
    }
}

fn screens_of(list: &Json) -> Vec<Screen> {
    list.as_array()
        .into_iter()
        .flatten()
        .map(|o| Screen {
            name: o["name"].as_str().unwrap_or("").to_string(),
            x: o["x"].as_f64().unwrap_or(0.0),
            y: o["y"].as_f64().unwrap_or(0.0),
            width: o["width"].as_f64().unwrap_or(0.0),
            height: o["height"].as_f64().unwrap_or(0.0),
            scale: o["scale"].as_f64().unwrap_or(1.0),
        })
        .collect()
}

fn window_geo(g: &Json) -> WindowGeo {
    let num = |k: &str| g[k].as_f64().unwrap_or(0.0);
    let text = |k: &str| g[k].as_str().unwrap_or("").to_string();
    WindowGeo {
        title: text("title"),
        app: text("app"),
        x: num("x"),
        y: num("y"),
        width: num("width"),
        height: num("height"),
        active: g["active"].as_bool().unwrap_or(false),
        minimized: g["minimized"].as_bool().unwrap_or(false),
        maximized: g["maximized"].as_bool().unwrap_or(false),
        output: text("output"),
        pid: g["pid"]
            .as_u64()
            .and_then(|p| u32::try_from(p).ok())
            .unwrap_or(0),
        terminals: Vec::new(),
    }
}

/// Fallback when the KWin query fails.
async fn read_screen_box() -> Option<(i32, i32, i32, i32)> {
    let out = tokio::process::Command::new("kscreen-doctor")
        .arg("-j")
        .output()
        .await
        .ok()?;
    let doc: Json = serde_json::from_slice(&out.stdout).ok()?;
    let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for o in doc["outputs"].as_array()? {
        if !o["enabled"].as_bool().unwrap_or(false) {
            continue;
        }
        let sc = o["scale"].as_f64().filter(|s| *s > 0.0).unwrap_or(1.0);
        let (w, h) = (
            o["size"]["width"].as_f64()? / sc,
            o["size"]["height"].as_f64()? / sc,
        );
        let (px, py) = (o["pos"]["x"].as_f64()?, o["pos"]["y"].as_f64()?);
        x0 = x0.min(px);
        y0 = y0.min(py);
        x1 = x1.max(px + w);
        y1 = y1.max(py + h);
    }
    (x0 <= x1).then_some((x0 as i32, y0 as i32, x1 as i32, y1 as i32))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn scripts_are_written_to_new_private_files() {
        use std::os::unix::fs::PermissionsExt;
        let path = write_script("test", "1 + 1").await.unwrap();
        assert!(path.starts_with(script_dir()));
        let meta = std::fs::symlink_metadata(&path).unwrap();
        assert!(meta.is_file());
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "1 + 1");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn allow_sorts_pids_by_atspi_name() {
        let apps = [
            (10, "electron".to_string()), // Ferdium: class is not "electron"
            (20, "Kate".to_string()),
            (30, "konsole".to_string()),
            (40, String::new()),
        ];
        let allow = Allow::new(&names(&["electron", "kate"]), &apps);
        assert_eq!(allow.pids, vec![10, 20]);
        assert_eq!(allow.blocked_pids, vec![30]);
        assert_eq!(allow.names, names(&["electron", "kate"]));
    }

    #[test]
    fn allow_blocks_pid_with_any_blocked_app() {
        let apps = [
            (10, "electron".to_string()),
            (10, "secret-vault".to_string()),
            (10, "electron".to_string()),
        ];
        let allow = Allow::new(&names(&["electron"]), &apps);
        assert!(allow.pids.is_empty());
        assert_eq!(allow.blocked_pids, vec![10]);
    }

    #[test]
    fn permits_by_pid_then_class() {
        let allow = Allow::new(
            &names(&["electron", "alacritty"]),
            &[(10, "electron".into()), (30, "konsole".into())],
        );
        assert!(allow.permits(10, &["Ferdium"]));
        assert!(!allow.permits(30, &["Alacritty"]));
        assert!(allow.permits(99, &["", "Alacritty"]));
        assert!(!allow.permits(99, &["org.kde.kcalc"]));
    }

    #[test]
    fn scripts_get_allowlist_and_pids() {
        let allow = Allow::new(
            &names(&["electron"]),
            &[(10, "electron".into()), (30, "konsole".into())],
        );
        let js = window_js("Ferdium", &allow);
        assert!(js.contains(r#"const allow = ["electron"];"#));
        assert!(js.contains("const allowPids = [10];"));
        assert!(js.contains("const blockPids = [30];"));
        assert!(js.contains(r#"const q = "Ferdium".toLowerCase();"#));
        assert!(!js.contains('%'));
        let act = ACTIVATE.replace("%OK_APP%", &allow.ok_app_js());
        assert!(act.contains("const allowPids = [10];") && act.contains("okApp(w)"));
        let both = SCREENS_AND_WINDOWS
            .replace("%JS%", &js)
            .replace("%SCREENS%", SCREENS_JS);
        assert!(!both.contains('%') && both.contains("workspace.screens.map"));
    }

    #[test]
    fn reads_raised_window_and_screens() {
        let v: Json = serde_json::from_str(r#"{"caption": "a — Kate", "pid": 42}"#).unwrap();
        let want = Raised {
            caption: "a — Kate".into(),
            pid: 42,
        };
        assert_eq!(raised_of(&v), want);
        let v: Json = serde_json::from_str(
            r#"{"screens": [{"name": "eDP-1", "x": 0, "y": 0, "width": 1536,
                "height": 864, "scale": 1.25}], "windows": []}"#,
        )
        .unwrap();
        let s = screens_of(&v["screens"]);
        assert_eq!(s.len(), 1);
        assert_eq!(
            (s[0].name.as_str(), s[0].width, s[0].scale),
            ("eDP-1", 1536.0, 1.25)
        );
        assert!(screens_of(&Json::Null).is_empty());
    }
}
