//! KWin scripting over the session bus: active window geometry and raising
//! windows, plus the screen layout from kscreen-doctor.
//!
//! KWin scripts cannot return values over D-Bus, so a script calls back into
//! an object this process exports on its own unique bus name.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::Value as Json;
use tokio::sync::{Mutex, mpsc};
use zbus::Connection;

use crate::atspi::R;

const SINK_PATH: &str = "/org/desktopmcp/sink";
const QUERY_TIMEOUT: Duration = Duration::from_secs(3);
const SCREEN_BOX_TTL: Duration = Duration::from_secs(5);

/// (x0, y0, x1, y1) in logical px.
type ScreenBox = (i32, i32, i32, i32);

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

/// Raises the first window of an allowed app whose caption or class contains
/// the query; evaluates to its caption, "" when none matched.
const ACTIVATE: &str = r#"(() => {
const allow = %ALLOW%;
const q = %QUERY%.toLowerCase();
for (const w of workspace.windowList()) {
  const cls = (w.resourceClass || "").toLowerCase();
  const name = (w.resourceName || "").toLowerCase();
  const okApp = allow.some(a => cls.includes(a) || name.includes(a));
  if (okApp && (w.caption.toLowerCase().includes(q) || cls.includes(q))) {
    workspace.activeWindow = w;
    return w.caption;
  }
}
return "";
})()"#;

const GEOMETRY: &str = "JSON.stringify((() => { const w = workspace.activeWindow; if (!w) return null; \
     const g = w.clientGeometry; return {x: g.x, y: g.y, width: g.width, \
     height: g.height, caption: w.caption, cls: w.resourceClass}; })())";

impl KWin {
    pub async fn new(session: Connection) -> R<Self> {
        let (tx, rx) = mpsc::unbounded_channel();
        session.object_server().at(SINK_PATH, Sink { tx }).await.map_err(|e| e.to_string())?;
        Ok(KWin { session, rx: Mutex::new(rx), seq: AtomicU64::new(0), screen: Mutex::new(None) })
    }

    async fn call<T>(&self, path: &str, iface: &str, method: &str,
                     args: &(impl serde::Serialize + zbus::zvariant::DynamicType)) -> R<T>
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
        let me = self.session.unique_name().ok_or("no unique bus name")?.to_string();
        let id = format!("{}-{}", std::process::id(), self.seq.fetch_add(1, Ordering::Relaxed));
        let script = format!(
            "const result = String({js_expr});\ncallDBus({}, {}, 'org.desktopmcp.Sink', 'Report', {}, result);\n",
            Json::from(me), Json::from(SINK_PATH), Json::from(id.as_str()));
        let file = std::env::temp_dir().join(format!("desktop-mcp-{id}.js"));
        tokio::fs::write(&file, script).await.map_err(|e| e.to_string())?;
        let plugin = format!("desktop-mcp-{id}");
        let result = self.run_script(&file, &plugin, &id).await;
        let _ = tokio::fs::remove_file(&file).await;
        result
    }

    async fn run_script(&self, file: &std::path::Path, plugin: &str, id: &str) -> R<Option<String>> {
        let mut rx = self.rx.lock().await;
        while rx.try_recv().is_ok() {} // reports from earlier, timed-out scripts
        let sid: i32 = self
            .call("/Scripting", "org.kde.kwin.Scripting", "loadScript",
                  &(file.to_string_lossy().as_ref(), plugin))
            .await?;
        if sid < 0 {
            return Err("KWin refused script".into());
        }
        let ran: R<()> = self
            .call(&format!("/Scripting/Script{sid}"), "org.kde.kwin.Script", "run", &())
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
                tokio::time::timeout(QUERY_TIMEOUT, wait).await.ok().flatten()
            }
            Err(_) => None,
        };
        let _: R<bool> = self
            .call("/Scripting", "org.kde.kwin.Scripting", "unloadScript", &(plugin,))
            .await;
        ran?;
        Ok(data)
    }

    /// Raise a window; returns its caption, or "" when nothing matched.
    pub async fn activate(&self, query: &str, allowed: &[String]) -> R<String> {
        let script = ACTIVATE
            .replace("%ALLOW%", &Json::from(allowed.to_vec()).to_string())
            .replace("%QUERY%", &Json::from(query).to_string());
        Ok(self.query(&script).await?.unwrap_or_default())
    }

    /// Client-area origin (logical px) of KWin's active window.
    pub async fn active_window_origin(&self) -> R<(f64, f64)> {
        let raw = self.query(GEOMETRY).await?.unwrap_or_default();
        let geo: Json = serde_json::from_str(&raw).unwrap_or(Json::Null);
        match (geo["x"].as_f64(), geo["y"].as_f64()) {
            (Some(x), Some(y)) => Ok((x, y)),
            _ => Err("could not read the active window geometry from KWin".into()),
        }
    }

    /// Bounding box (x0, y0, x1, y1) of all enabled outputs in logical px.
    pub async fn screen_box(&self) -> (i32, i32, i32, i32) {
        let mut cached = self.screen.lock().await;
        if let Some((at, b)) = *cached
            && at.elapsed() < SCREEN_BOX_TTL {
                return b;
            }
        let b = read_screen_box().await.unwrap_or((0, 0, 1920, 1080));
        *cached = Some((Instant::now(), b));
        b
    }
}

async fn read_screen_box() -> Option<(i32, i32, i32, i32)> {
    let out = tokio::process::Command::new("kscreen-doctor").arg("-j").output().await.ok()?;
    let doc: Json = serde_json::from_slice(&out.stdout).ok()?;
    let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for o in doc["outputs"].as_array()? {
        if !o["enabled"].as_bool().unwrap_or(false) {
            continue;
        }
        let sc = o["scale"].as_f64().filter(|s| *s > 0.0).unwrap_or(1.0);
        let (w, h) = (o["size"]["width"].as_f64()? / sc, o["size"]["height"].as_f64()? / sc);
        let (px, py) = (o["pos"]["x"].as_f64()?, o["pos"]["y"].as_f64()?);
        x0 = x0.min(px);
        y0 = y0.min(py);
        x1 = x1.max(px + w);
        y1 = y1.max(py + h);
    }
    (x0 <= x1).then_some((x0 as i32, y0 as i32, x1 as i32, y1 as i32))
}
