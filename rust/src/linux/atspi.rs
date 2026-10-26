//! AT-SPI over D-Bus, without libatspi.
//!
//! Every attribute is a D-Bus round trip to the application. Instead of asking
//! one at a time, a node's attributes are requested together and whole levels
//! of the tree are fetched concurrently, so the cost is bounded by how fast the
//! application answers rather than by round-trip latency.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use futures_util::stream::{self, StreamExt};
use zbus::Connection;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

use super::roles;
pub use crate::backend::R;

pub type Info = crate::backend::Info<Obj>;

const ACC: &str = "org.a11y.atspi.Accessible";
const PROPS: &str = "org.freedesktop.DBus.Properties";
const NULL_PATH: &str = "/org/a11y/atspi/null";
/// Upper bound for one AT-SPI request. Replies normally take well under
/// 100 ms even for big trees; an app that is blocked (a modal loop inside a
/// handler, a hung process) must not freeze the whole server, since every
/// later request queues behind the stuck one.
const CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
/// How long `do_action` waits for an action's reply; see there.
const ACTION_WAIT: std::time::Duration = std::time::Duration::from_millis(400);
/// Requests kept in flight at once; well under the bus' pending-reply limit.
const IN_FLIGHT: usize = 512;

// AtspiStateType bit positions.
pub const ACTIVE: u32 = 1;
pub const CHECKED: u32 = 4;
pub const EDITABLE: u32 = 7;
pub const ENABLED: u32 = 8;
pub const EXPANDED: u32 = 10;
pub const FOCUSED: u32 = 12;
pub const MULTI_LINE: u32 = 17;
pub const SELECTED: u32 = 23;
pub const SHOWING: u32 = 25;
pub const VISIBLE: u32 = 30;

// AtspiCoordType.
pub const COORD_SCREEN: u32 = 0;
pub const COORD_WINDOW: u32 = 1;

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
    pub fn new(bus: &str, path: OwnedObjectPath) -> Self {
        Obj {
            bus: bus.into(),
            path,
        }
    }
}

fn has(states: u64, state: u32) -> bool {
    states & (1u64 << state) != 0
}

/// `true` when GetActions returned actions but no usable names: all empty,
/// or only the `;` separators of an empty key binding (Firefox).
pub fn action_names_missing(names: &[String]) -> bool {
    !names.is_empty() && names.iter().all(|n| n.chars().all(|c| c == ';'))
}

/// Python `AtspiNode.states()`; None when the state set could not be read.
pub fn state_labels(states: Option<u64>) -> Vec<&'static str> {
    let Some(st) = states else { return Vec::new() };
    let mut out: Vec<&'static str> = [
        (FOCUSED, "focused"),
        (CHECKED, "checked"),
        (SELECTED, "selected"),
        (EXPANDED, "expanded"),
        (ACTIVE, "active"),
        (EDITABLE, "editable"),
    ]
    .into_iter()
    .filter(|(bit, _)| has(st, *bit))
    .map(|(_, label)| label)
    .collect();
    if !has(st, ENABLED) {
        out.push("disabled");
    }
    out
}

pub struct A11y {
    pub conn: Connection,
    /// Last element seen gaining or holding focus: from focus events, and
    /// from any read that found it focused. Only a hint; check before use.
    focus_hint: Mutex<Option<Obj>>,
    /// Bus name of the app `active_app` last found in front, and when.
    active_bus: Mutex<Option<(Arc<str>, Instant)>>,
}

impl A11y {
    pub async fn connect(session: &Connection) -> R<Self> {
        enable(session).await;
        let reply = session
            .call_method(
                Some("org.a11y.Bus"),
                "/org/a11y/bus",
                Some("org.a11y.Bus"),
                "GetAddress",
                &(),
            )
            .await
            .map_err(|e| format!("accessibility bus unavailable: {e}"))?;
        let addr: String = reply.body().deserialize().map_err(err)?;
        let conn = zbus::connection::Builder::address(addr.as_str())
            .map_err(err)?
            .build()
            .await
            .map_err(|e| format!("cannot connect to the accessibility bus: {e}"))?;
        Ok(A11y {
            conn,
            focus_hint: Mutex::new(None),
            active_bus: Mutex::new(None),
        })
    }

    async fn call<T>(
        &self,
        o: &Obj,
        iface: &str,
        method: &str,
        args: &(impl serde::Serialize + zbus::zvariant::DynamicType),
    ) -> R<T>
    where
        T: for<'d> serde::Deserialize<'d> + zbus::zvariant::Type,
    {
        let reply = self
            .conn
            .call_method(Some(o.bus.as_ref()), &o.path, Some(iface), method, args);
        let reply = tokio::time::timeout(CALL_TIMEOUT, reply)
            .await
            .map_err(|_| {
                format!("{method}: no reply from the application within {CALL_TIMEOUT:?}")
            })?
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
            _ => self
                .call(o, ACC, "GetRoleName", &())
                .await
                .unwrap_or_default(),
        }
    }

    pub async fn states(&self, o: &Obj) -> Option<u64> {
        let words = self.call::<Vec<u32>>(o, ACC, "GetState", &()).await.ok()?;
        Some(
            words
                .iter()
                .take(2)
                .enumerate()
                .fold(0, |acc, (i, w)| acc | (u64::from(*w) << (32 * i))),
        )
    }

    pub async fn has_state(&self, o: &Obj, state: u32) -> bool {
        let st = self.states(o).await.unwrap_or(0);
        self.note_states(o, st);
        has(st, state)
    }

    fn note_states(&self, o: &Obj, states: u64) {
        if has(states, FOCUSED) {
            self.note_focus(o.clone());
        }
    }

    pub fn note_focus(&self, o: Obj) {
        *self.focus_hint.lock().unwrap() = Some(o);
    }

    pub fn focus_hint(&self) -> Option<Obj> {
        self.focus_hint.lock().unwrap().clone()
    }

    /// Bus of the app `active_app` found in front, if it looked within `max_age`.
    pub fn recent_active_bus(&self, max_age: std::time::Duration) -> Option<Arc<str>> {
        let seen = self.active_bus.lock().unwrap().clone();
        seen.filter(|(_, at)| at.elapsed() <= max_age)
            .map(|(bus, _)| bus)
    }

    /// Ask toolkits to emit focus changes (Qt only sends events someone
    /// registered for), and keep `focus_hint` current from them. The
    /// registration ends with this connection.
    pub async fn track_focus(self: &Arc<Self>) -> R<()> {
        use futures_util::StreamExt;
        let rule = zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .interface("org.a11y.atspi.Event.Object")
            .map_err(err)?
            .member("StateChanged")
            .map_err(err)?
            .add_arg("focused")
            .map_err(err)?
            .build();
        let mut stream = zbus::MessageStream::for_match_rule(rule, &self.conn, Some(256))
            .await
            .map_err(err)?;
        let args = ("object:state-changed:focused", Vec::<String>::new(), "");
        let register = self.conn.call_method(
            Some("org.a11y.atspi.Registry"),
            "/org/a11y/atspi/registry",
            Some("org.a11y.atspi.Registry"),
            "RegisterEvent",
            &args,
        );
        tokio::time::timeout(CALL_TIMEOUT, register)
            .await
            .map_err(|_| "RegisterEvent: no reply".to_string())?
            .map_err(err)?;
        let me = Arc::downgrade(self);
        tokio::spawn(async move {
            while let Some(Ok(msg)) = stream.next().await {
                let Some(a11y) = me.upgrade() else { break };
                let header = msg.header();
                let (Some(sender), Some(path)) = (header.sender(), header.path()) else {
                    continue;
                };
                let gained = msg.body().deserialize::<(String, i32, i32, OwnedValue)>()
                    .map(|(_, d1, _, _)| d1)
                    .or_else(|_| msg.body()
                        .deserialize::<(String, i32, i32, OwnedValue, HashMap<String, OwnedValue>)>()
                        .map(|(_, d1, _, _, _)| d1));
                if gained == Ok(1) {
                    a11y.note_focus(Obj::new(sender.as_str(), path.clone().into()));
                }
            }
        });
        Ok(())
    }

    /// Toolkit of the application owning `bus` ("Qt", "GTK", "Gecko", ...).
    pub async fn toolkit(&self, bus: &str) -> String {
        let root = Obj::new(
            bus,
            OwnedObjectPath::try_from("/org/a11y/atspi/accessible/root").unwrap(),
        );
        match self
            .prop(&root, "org.a11y.atspi.Application", "ToolkitName")
            .await
        {
            Ok(v) => String::try_from(v).unwrap_or_default(),
            Err(_) => String::new(),
        }
    }

    pub async fn interfaces(&self, o: &Obj) -> Vec<String> {
        self.call(o, ACC, "GetInterfaces", &())
            .await
            .unwrap_or_default()
    }

    pub async fn children(&self, o: &Obj) -> Vec<Obj> {
        let kids: Vec<(String, OwnedObjectPath)> = self
            .call(o, ACC, "GetChildren", &())
            .await
            .unwrap_or_default();
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
        let list: R<Vec<(String, String, String)>> = self
            .call(o, "org.a11y.atspi.Action", "GetActions", &())
            .await;
        let names: Vec<String> = list
            .map(|l| l.into_iter().map(|(name, _, _)| name).collect())
            .unwrap_or_default();
        if !action_names_missing(&names) {
            return names;
        }
        // Firefox answers GetActions with ";;" (an empty key binding) in every
        // field; GetName(i), which libatspi uses, has the real names.
        futures_util::future::join_all((0..names.len() as i32).map(|i| async move {
            self.call::<String>(o, "org.a11y.atspi.Action", "GetName", &(i,))
                .await
        }))
        .await
        .into_iter()
        .map(|name| name.unwrap_or_default())
        .collect()
    }

    /// Run action `index`. Some toolkits answer only once the action is
    /// over: Qt runs a menu button's QMenu::exec() inside the DoAction
    /// handler, so the reply comes when the menu closes. Without a reply
    /// within ACTION_WAIT the action counts as sent (the menu or dialog it
    /// opened is the expected result) and the late reply is dropped.
    pub async fn do_action(&self, o: &Obj, index: i32) -> R<bool> {
        match tokio::time::timeout(
            ACTION_WAIT,
            self.call(o, "org.a11y.atspi.Action", "DoAction", &(index,)),
        )
        .await
        {
            Ok(done) => done,
            Err(_) => Ok(true),
        }
    }

    /// Up to `limit` characters of the Text interface, or None without it.
    pub async fn text(&self, o: &Obj, limit: i32) -> R<String> {
        let count = self
            .prop(o, "org.a11y.atspi.Text", "CharacterCount")
            .await?;
        let count = i32::try_from(count).map_err(err)?;
        self.call(
            o,
            "org.a11y.atspi.Text",
            "GetText",
            &(0i32, count.min(limit)),
        )
        .await
    }

    pub async fn set_text(&self, o: &Obj, text: &str) -> R<bool> {
        self.call(
            o,
            "org.a11y.atspi.EditableText",
            "SetTextContents",
            &(text,),
        )
        .await
    }

    pub async fn caret(&self, o: &Obj) -> R<i32> {
        i32::try_from(self.prop(o, "org.a11y.atspi.Text", "CaretOffset").await?).map_err(err)
    }

    pub async fn char_count(&self, o: &Obj) -> R<i32> {
        i32::try_from(
            self.prop(o, "org.a11y.atspi.Text", "CharacterCount")
                .await?,
        )
        .map_err(err)
    }

    /// (start, end) of the first selection, None without one.
    pub async fn selection(&self, o: &Obj) -> R<Option<(i32, i32)>> {
        let n: i32 = self
            .call(o, "org.a11y.atspi.Text", "GetNSelections", &())
            .await?;
        if n <= 0 {
            return Ok(None);
        }
        let (start, end): (i32, i32) = self
            .call(o, "org.a11y.atspi.Text", "GetSelection", &(0i32,))
            .await?;
        Ok((start != end).then_some((start.min(end), start.max(end))))
    }

    pub async fn set_caret(&self, o: &Obj, offset: i32) -> R<bool> {
        self.call(o, "org.a11y.atspi.Text", "SetCaretOffset", &(offset,))
            .await
    }

    pub async fn insert_text(&self, o: &Obj, offset: i32, text: &str, length: i32) -> R<bool> {
        self.call(
            o,
            "org.a11y.atspi.EditableText",
            "InsertText",
            &(offset, text, length),
        )
        .await
    }

    pub async fn delete_text(&self, o: &Obj, start: i32, end: i32) -> R<bool> {
        self.call(
            o,
            "org.a11y.atspi.EditableText",
            "DeleteText",
            &(start, end),
        )
        .await
    }

    pub async fn grab_focus(&self, o: &Obj) -> bool {
        self.call(o, "org.a11y.atspi.Component", "GrabFocus", &())
            .await
            .unwrap_or(false)
    }

    pub async fn extents(&self, o: &Obj, coord: u32) -> R<(i32, i32, i32, i32)> {
        self.call(o, "org.a11y.atspi.Component", "GetExtents", &(coord,))
            .await
    }

    /// All attributes of one node, requested together.
    pub async fn info(&self, o: &Obj, text_limit: usize) -> Info {
        self.info_flagged(o, text_limit).await.0
    }

    /// `info`, and whether the node is visible but not showing: an overlay
    /// container that toolkits (Firefox's popover layer) never mark showing,
    /// or content scrolled out of view.
    async fn info_flagged(&self, o: &Obj, text_limit: usize) -> (Info, bool) {
        let (role, name, states, ifaces, children) = tokio::join!(
            self.role(o),
            self.name(o),
            self.states(o),
            self.interfaces(o),
            self.children(o)
        );
        if let Some(st) = states {
            self.note_states(o, st);
        }
        let has_iface = |i: &str| ifaces.iter().any(|x| x == i);
        let (actions, text) = tokio::join!(
            async {
                if has_iface("org.a11y.atspi.Action") {
                    self.actions(o).await
                } else {
                    Vec::new()
                }
            },
            async {
                if has_iface("org.a11y.atspi.Text") {
                    let limit = text_limit.min(i32::MAX as usize) as i32;
                    Some(
                        self.text(o, limit)
                            .await
                            .unwrap_or_default()
                            .replace('\n', " "),
                    )
                } else {
                    None
                }
            }
        );
        let raw = states.unwrap_or(0);
        // Background browser tabs are visible-not-showing documents too;
        // only containers inside a page qualify.
        let hidden_visible = !has(raw, SHOWING)
            && has(raw, VISIBLE)
            && !OVERLAY_EXCLUDED_ROLES.contains(&role.as_str())
            && has_iface("org.a11y.atspi.Component");
        let info = Info {
            role,
            name,
            showing: has(raw, SHOWING),
            states: state_labels(states),
            actions,
            text,
            children,
        };
        (info, hidden_visible)
    }

    /// Size of the largest window among the frames of `roots`, in window
    /// coordinates; None when no frame answers.
    async fn viewport(&self, roots: &[Obj]) -> Option<(i32, i32)> {
        let mut best: Option<(i32, i32)> = None;
        for r in roots.iter().take(8) {
            let Some(f) = self.frame_of(r).await else {
                continue;
            };
            if let Ok((_, _, w, h)) = self.extents(&f, COORD_WINDOW).await
                && w > 0
                && h > 0
            {
                best = Some(best.map_or((w, h), |(bw, bh)| (bw.max(w), bh.max(h))));
            }
        }
        best
    }

    /// Breadth-first fetch below `roots` into `cache`, all of a level at once.
    /// Children of hidden nodes are skipped when `only_showing`. Stops after
    /// roughly `limit` new nodes.
    pub async fn prefetch(
        &self,
        cache: &mut HashMap<Obj, Info>,
        roots: Vec<Obj>,
        only_showing: bool,
        text_limit: usize,
        limit: usize,
    ) {
        let mut level: Vec<Obj> = roots
            .into_iter()
            .filter(|o| !cache.contains_key(o))
            .collect();
        let mut fetched = 0;
        let view = tokio::sync::OnceCell::new();
        let first: Vec<Obj> = level.clone();
        let view = LazyView {
            cell: view,
            a11y: self,
            roots: first,
        };
        while !level.is_empty() && fetched < limit {
            level.sort_by(|a, b| a.path.as_str().cmp(b.path.as_str()));
            level.dedup();
            let view = &view;
            let infos: Vec<(Obj, Info)> = stream::iter(level)
                .map(|o| async move {
                    let (mut info, hidden_visible) = self.info_flagged(&o, text_limit).await;
                    // A visible node that is not showing yet lies inside the
                    // window holds an overlay (a web page's popup menu): treat
                    // it as showing so its showing children are reached.
                    if hidden_visible
                        && let Some((vw, vh)) = view.get().await
                        && let Ok((x, y, w, h)) = self.extents(&o, COORD_WINDOW).await
                    {
                        info.showing = overlaps((x, y, w, h), (*vw, *vh));
                    }
                    (o, info)
                })
                .buffered(IN_FLIGHT)
                .collect()
                .await;
            fetched += infos.len();
            let mut next = Vec::new();
            for (o, info) in infos {
                if !only_showing || info.showing {
                    next.extend(
                        info.children
                            .iter()
                            .filter(|c| !cache.contains_key(c))
                            .cloned(),
                    );
                }
                cache.insert(o, info);
            }
            level = next;
        }
    }

    pub async fn applications(&self) -> Vec<Obj> {
        let root = Obj::new(
            "org.a11y.atspi.Registry",
            OwnedObjectPath::try_from("/org/a11y/atspi/accessible/root").unwrap(),
        );
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
        let found = apps
            .iter()
            .zip(flags)
            .find(|(_, f)| f.iter().any(|x| *x))
            .map(|(a, _)| a);
        *self.active_bus.lock().unwrap() =
            found.map(|(app, _, _)| (app.bus.clone(), Instant::now()));
        found.map(|(_, name, _)| name.clone()).unwrap_or_default()
    }

    /// Process id of the connection owning `bus` on the accessibility bus.
    pub async fn pid(&self, bus: &str) -> Option<u32> {
        let reply = self
            .conn
            .call_method(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                Some("org.freedesktop.DBus"),
                "GetConnectionUnixProcessID",
                &(bus,),
            )
            .await
            .ok()?;
        reply.body().deserialize().ok()
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

/// Roles never treated as an overlay: whole documents (background tabs),
/// windows, and Firefox's auto-hidden menu bar.
const OVERLAY_EXCLUDED_ROLES: [&str; 9] = [
    "menu bar",
    "document web",
    "document frame",
    "internal frame",
    "embedded",
    "frame",
    "window",
    "dialog",
    "application",
];

/// The viewport size, looked up only when a node needs it.
struct LazyView<'a> {
    cell: tokio::sync::OnceCell<Option<(i32, i32)>>,
    a11y: &'a A11y,
    roots: Vec<Obj>,
}

impl LazyView<'_> {
    async fn get(&self) -> &Option<(i32, i32)> {
        self.cell
            .get_or_init(|| self.a11y.viewport(&self.roots))
            .await
    }
}

/// Whether a box in window coordinates has area inside a `w`×`h` window.
fn overlaps((x, y, bw, bh): (i32, i32, i32, i32), (w, h): (i32, i32)) -> bool {
    bw > 0 && bh > 0 && x < w && y < h && x + bw > 0 && y + bh > 0
}

/// Ask the session to turn the accessibility layer on (idempotent).
pub async fn enable(session: &Connection) {
    let _ = session
        .call_method(
            Some("org.a11y.Bus"),
            "/org/a11y/bus",
            Some(PROPS),
            "Set",
            &(
                "org.a11y.Status",
                "IsEnabled",
                zbus::zvariant::Value::from(true),
            ),
        )
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_boxes_must_have_area_inside_the_window() {
        let window = (1536, 779);
        assert!(overlaps((0, 84, 1536, 779), window));
        assert!(overlaps((-10, -10, 20, 20), window));
        // Scrolled out of view, collapsed, or unknown (-1 extents).
        assert!(!overlaps((0, 2000, 300, 40), window));
        assert!(!overlaps((0, -200, 300, 40), window));
        assert!(!overlaps((10, 10, 0, 40), window));
        assert!(!overlaps((-1, -1, -1, -1), window));
    }

    #[test]
    fn firefox_key_bindings_are_not_action_names() {
        let names = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(action_names_missing(&names(&[";;"])));
        assert!(action_names_missing(&names(&[";;", ""])));
        assert!(!action_names_missing(&names(&["Press", ";;"])));
        assert!(!action_names_missing(&names(&["Set Focus"])));
        assert!(!action_names_missing(&[]));
    }
}
