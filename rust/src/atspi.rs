//! AT-SPI over D-Bus, without libatspi.
//!
//! Every attribute is a D-Bus round trip to the application. Instead of asking
//! one at a time, a node's attributes are requested together and whole levels
//! of the tree are fetched concurrently, so the cost is bounded by how fast the
//! application answers rather than by round-trip latency.

use std::collections::HashMap;
use std::sync::Arc;

use futures_util::stream::{self, StreamExt};
use serde_json::Value as Json;
use zbus::Connection;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

use crate::roles;

const ACC: &str = "org.a11y.atspi.Accessible";
const PROPS: &str = "org.freedesktop.DBus.Properties";
const NULL_PATH: &str = "/org/a11y/atspi/null";
/// Requests kept in flight at once; well under the bus' pending-reply limit.
const IN_FLIGHT: usize = 512;

// AtspiStateType bit positions.
pub const ACTIVE: u32 = 1;
pub const CHECKED: u32 = 4;
pub const EDITABLE: u32 = 7;
pub const ENABLED: u32 = 8;
pub const EXPANDED: u32 = 10;
pub const FOCUSED: u32 = 12;
pub const SELECTED: u32 = 23;
pub const SHOWING: u32 = 25;

// AtspiCoordType.
pub const COORD_WINDOW: u32 = 1;

pub type R<T> = Result<T, String>;

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// One accessible object: the owning connection's bus name and its path.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Obj {
    pub bus: Arc<str>,
    pub path: OwnedObjectPath,
}

impl Obj {
    fn new(bus: &str, path: OwnedObjectPath) -> Self {
        Obj { bus: bus.into(), path }
    }
}

/// Everything tree rendering and search read about a node.
#[derive(Clone, Debug, Default)]
pub struct Info {
    pub role: String,
    pub name: String,
    pub states: u64,
    /// False when the state set could not be read (no "disabled" then).
    pub states_known: bool,
    pub actions: Vec<String>,
    /// First characters of the Text interface, newlines kept; None without it.
    pub text: Option<String>,
    pub children: Vec<Obj>,
}

impl Info {
    pub fn has(&self, state: u32) -> bool {
        self.states & (1u64 << state) != 0
    }
}

pub struct A11y {
    pub conn: Connection,
}

impl A11y {
    pub async fn connect(session: &Connection) -> R<Self> {
        enable(session).await;
        let reply = session
            .call_method(Some("org.a11y.Bus"), "/org/a11y/bus", Some("org.a11y.Bus"),
                         "GetAddress", &())
            .await
            .map_err(|e| format!("accessibility bus unavailable: {e}"))?;
        let addr: String = reply.body().deserialize().map_err(err)?;
        let conn = zbus::connection::Builder::address(addr.as_str())
            .map_err(err)?
            .build()
            .await
            .map_err(|e| format!("cannot connect to the accessibility bus: {e}"))?;
        Ok(A11y { conn })
    }

    async fn call<T>(&self, o: &Obj, iface: &str, method: &str,
                     args: &(impl serde::Serialize + zbus::zvariant::DynamicType)) -> R<T>
    where
        T: for<'d> serde::Deserialize<'d> + zbus::zvariant::Type,
    {
        let reply = self
            .conn
            .call_method(Some(o.bus.as_ref()), &o.path, Some(iface), method, args)
            .await
            .map_err(err)?;
        reply.body().deserialize().map_err(err)
    }

    async fn prop(&self, o: &Obj, iface: &str, name: &str) -> R<OwnedValue> {
        self.call(o, PROPS, "Get", &(iface, name)).await
    }

    pub async fn name(&self, o: &Obj) -> String {
        match self.prop(o, ACC, "Name").await {
            Ok(v) => String::try_from(v).unwrap_or_default(),
            Err(_) => String::new(),
        }
    }

    pub async fn role(&self, o: &Obj) -> String {
        match self.call::<u32>(o, ACC, "GetRole", &()).await {
            Ok(r) if r != roles::EXTENDED && (r as usize) < roles::NAMES.len() => {
                roles::NAMES[r as usize].to_string()
            }
            _ => self.call(o, ACC, "GetRoleName", &()).await.unwrap_or_default(),
        }
    }

    pub async fn states(&self, o: &Obj) -> Option<u64> {
        let words = self.call::<Vec<u32>>(o, ACC, "GetState", &()).await.ok()?;
        Some(words.iter().take(2).enumerate()
            .fold(0, |acc, (i, w)| acc | (u64::from(*w) << (32 * i))))
    }

    pub async fn has_state(&self, o: &Obj, state: u32) -> bool {
        self.states(o).await.unwrap_or(0) & (1u64 << state) != 0
    }

    pub async fn interfaces(&self, o: &Obj) -> Vec<String> {
        self.call(o, ACC, "GetInterfaces", &()).await.unwrap_or_default()
    }

    pub async fn children(&self, o: &Obj) -> Vec<Obj> {
        let kids: Vec<(String, OwnedObjectPath)> =
            self.call(o, ACC, "GetChildren", &()).await.unwrap_or_default();
        kids.into_iter()
            .filter(|(_, p)| p.as_str() != NULL_PATH)
            .map(|(b, p)| Obj::new(&b, p))
            .collect()
    }

    pub async fn parent(&self, o: &Obj) -> Option<Obj> {
        let v = self.prop(o, ACC, "Parent").await.ok()?;
        let (bus, path) = <(String, OwnedObjectPath)>::try_from(v).ok()?;
        (path.as_str() != NULL_PATH).then(|| Obj::new(&bus, path))
    }

    pub async fn actions(&self, o: &Obj) -> Vec<String> {
        let list: R<Vec<(String, String, String)>> =
            self.call(o, "org.a11y.atspi.Action", "GetActions", &()).await;
        list.map(|l| l.into_iter().map(|(name, _, _)| name).collect()).unwrap_or_default()
    }

    pub async fn do_action(&self, o: &Obj, index: i32) -> R<bool> {
        self.call(o, "org.a11y.atspi.Action", "DoAction", &(index,)).await
    }

    /// Up to `limit` characters of the Text interface, or None without it.
    pub async fn text(&self, o: &Obj, limit: i32) -> R<String> {
        let count = self.prop(o, "org.a11y.atspi.Text", "CharacterCount").await?;
        let count = i32::try_from(count).map_err(err)?;
        self.call(o, "org.a11y.atspi.Text", "GetText", &(0i32, count.min(limit))).await
    }

    pub async fn set_text(&self, o: &Obj, text: &str) -> R<bool> {
        self.call(o, "org.a11y.atspi.EditableText", "SetTextContents", &(text,)).await
    }

    pub async fn grab_focus(&self, o: &Obj) -> bool {
        self.call(o, "org.a11y.atspi.Component", "GrabFocus", &()).await.unwrap_or(false)
    }

    pub async fn extents(&self, o: &Obj, coord: u32) -> R<(i32, i32, i32, i32)> {
        self.call(o, "org.a11y.atspi.Component", "GetExtents", &(coord,)).await
    }

    /// All attributes of one node, requested together.
    pub async fn info(&self, o: &Obj, text_limit: i32) -> Info {
        let (role, name, states, ifaces, children) = tokio::join!(
            self.role(o), self.name(o), self.states(o), self.interfaces(o), self.children(o));
        let has = |i: &str| ifaces.iter().any(|x| x == i);
        let (actions, text) = tokio::join!(
            async {
                if has("org.a11y.atspi.Action") { self.actions(o).await } else { Vec::new() }
            },
            async {
                if has("org.a11y.atspi.Text") {
                    Some(self.text(o, text_limit).await.unwrap_or_default())
                } else {
                    None
                }
            });
        Info { role, name, states: states.unwrap_or(0), states_known: states.is_some(),
               actions, text, children }
    }

    /// Breadth-first fetch below `roots` into `cache`, all of a level at once.
    /// Children of hidden nodes are skipped when `only_showing`. Stops after
    /// roughly `limit` new nodes.
    pub async fn prefetch(&self, cache: &mut HashMap<Obj, Info>, roots: Vec<Obj>,
                          only_showing: bool, text_limit: i32, limit: usize) {
        let mut level: Vec<Obj> = roots.into_iter().filter(|o| !cache.contains_key(o)).collect();
        let mut fetched = 0;
        while !level.is_empty() && fetched < limit {
            level.sort_by(|a, b| a.path.as_str().cmp(b.path.as_str()));
            level.dedup();
            let infos: Vec<(Obj, Info)> = stream::iter(level)
                .map(|o| async move {
                    let info = self.info(&o, text_limit).await;
                    (o, info)
                })
                .buffered(IN_FLIGHT)
                .collect()
                .await;
            fetched += infos.len();
            let mut next = Vec::new();
            for (o, info) in infos {
                if !only_showing || info.has(SHOWING) {
                    next.extend(info.children.iter().filter(|c| !cache.contains_key(c)).cloned());
                }
                cache.insert(o, info);
            }
            level = next;
        }
    }

    pub async fn applications(&self) -> Vec<Obj> {
        let root = Obj::new("org.a11y.atspi.Registry",
                            OwnedObjectPath::try_from("/org/a11y/atspi/accessible/root").unwrap());
        self.children(&root).await
    }

    /// (app, app name, windows) for every registered application.
    pub async fn apps_with_windows(&self) -> Vec<(Obj, String, Vec<Obj>)> {
        let apps = self.applications().await;
        stream::iter(apps)
            .map(|a| async move {
                let (name, windows) = tokio::join!(self.name(&a), self.children(&a));
                (a, name, windows)
            })
            .buffered(IN_FLIGHT)
            .collect()
            .await
    }

    /// Name of the first app with an ACTIVE top-level window, "" if none.
    pub async fn active_app(&self) -> String {
        let apps = self.apps_with_windows().await;
        let flags: Vec<Vec<bool>> = stream::iter(&apps)
            .map(|(_, _, ws)| async move {
                futures_util::future::join_all(ws.iter().map(|w| self.has_state(w, ACTIVE))).await
            })
            .buffered(IN_FLIGHT)
            .collect()
            .await;
        apps.iter().zip(flags)
            .find(|(_, f)| f.iter().any(|x| *x))
            .map(|((_, name, _), _)| name.clone())
            .unwrap_or_default()
    }

    /// Nearest frame/window/dialog ancestor (or self).
    pub async fn frame_of(&self, o: &Obj) -> Option<Obj> {
        let mut cur = Some(o.clone());
        while let Some(c) = cur {
            if matches!(self.role(&c).await.as_str(), "frame" | "window" | "dialog") {
                return Some(c);
            }
            cur = self.parent(&c).await;
        }
        None
    }
}

/// Ask the session to turn the accessibility layer on (idempotent).
pub async fn enable(session: &Connection) {
    let _ = session
        .call_method(Some("org.a11y.Bus"), "/org/a11y/bus", Some(PROPS), "Set",
                     &("org.a11y.Status", "IsEnabled", zbus::zvariant::Value::from(true)))
        .await;
}

/// Python-compatible `json.dumps(s, ensure_ascii=False)`.
pub fn json_str(s: &str) -> String {
    Json::String(s.to_string()).to_string()
}
