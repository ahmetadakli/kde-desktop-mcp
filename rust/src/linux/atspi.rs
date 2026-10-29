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
pub use crate::backend::Fetch;

const ACC: &str = "org.a11y.atspi.Accessible";
const PROPS: &str = "org.freedesktop.DBus.Properties";
const NULL_PATH: &str = "/org/a11y/atspi/null";
/// Upper bound for one AT-SPI request. Replies normally take well under
/// 100 ms even for big trees; an app that is blocked (a modal loop inside a
/// handler, a hung process) must not freeze the whole server, since every
/// later request queues behind the stuck one.
const CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
/// How long `do_action` waits for an action's reply at most; see there.
const ACTION_WAIT: std::time::Duration = std::time::Duration::from_millis(400);
/// How long `do_action` still waits for the reply once the app signalled
/// that the action took effect: a handler that returns replies within a
/// few ms of its last event; one that went into a modal loop never does.
const EFFECT_GRACE: std::time::Duration = std::time::Duration::from_millis(25);
/// Events that show an action did something without its reply: a menu or
/// popup shown, a window (dialog) created or activated. Focus changes,
/// which `track_focus` listens to, count too. (event, interface, member,
/// first argument)
const EFFECT_EVENTS: [(&str, &str, &str, Option<&str>); 3] = [
    (
        "object:state-changed:showing",
        "org.a11y.atspi.Event.Object",
        "StateChanged",
        Some("showing"),
    ),
    (
        "window:activate",
        "org.a11y.atspi.Event.Window",
        "Activate",
        None,
    ),
    (
        "window:create",
        "org.a11y.atspi.Event.Window",
        "Create",
        None,
    ),
];
/// Events that can make an element appear, disappear, or match a query
/// differently, besides EFFECT_EVENTS: `wait_for` re-checks on them. A
/// child added or removed (also a window, on an application's root), a
/// name changed, text changed. (event, interface, member, first argument)
const CHANGE_EVENTS: [(&str, &str, &str, Option<&str>); 3] = [
    (
        "object:children-changed",
        "org.a11y.atspi.Event.Object",
        "ChildrenChanged",
        None,
    ),
    (
        "object:property-change:accessible-name",
        "org.a11y.atspi.Event.Object",
        "PropertyChange",
        Some("accessible-name"),
    ),
    (
        "object:text-changed",
        "org.a11y.atspi.Event.Object",
        "TextChanged",
        None,
    ),
];
/// How long `wait_change` lets a burst of events (a dialog building its
/// widgets, a page loading) go on before the waiter re-checks once.
const CHANGE_SETTLE: std::time::Duration = std::time::Duration::from_millis(15);
/// Requests kept in flight at once; well under the bus' pending-reply limit.
const IN_FLIGHT: usize = 512;
/// How long an app's overlay viewport (its largest window) is reused: one
/// tool call fetches in several rounds, and wait_for polls; a window is
/// rarely resized in between.
const VIEW_TTL: std::time::Duration = std::time::Duration::from_secs(2);
const APP_ROOT: &str = "/org/a11y/atspi/accessible/root";

// AtspiStateType bit positions.
pub const ACTIVE: u32 = 1;
pub const BUSY: u32 = 3;
pub const CHECKED: u32 = 4;
pub const EDITABLE: u32 = 7;
pub const ENABLED: u32 = 8;
pub const EXPANDABLE: u32 = 9;
pub const EXPANDED: u32 = 10;
pub const FOCUSED: u32 = 12;
pub const MULTI_LINE: u32 = 17;
pub const SELECTABLE: u32 = 22;
pub const SELECTED: u32 = 23;
pub const SHOWING: u32 = 25;
pub const VISIBLE: u32 = 30;
pub const IS_DEFAULT: u32 = 39;
pub const CHECKABLE: u32 = 41;

// AtspiRelationType: a browser window EMBEDS its current tab's document.
pub const RELATION_EMBEDS: u32 = 13;

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

pub fn has(states: u64, state: u32) -> bool {
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

/// Width and height of an app's largest window; None when unknown.
type Viewport = Option<(i32, i32)>;

pub struct A11y {
    pub conn: Connection,
    /// Last element seen gaining or holding focus: from focus events, and
    /// from any read that found it focused. Only a hint; check before use.
    focus_hint: Mutex<Option<Obj>>,
    /// Bus name of the app `active_app` last found in front, and when.
    active_bus: Mutex<Option<(Arc<str>, Instant)>>,
    /// Overlay viewport per application bus, and when it was measured.
    views: tokio::sync::Mutex<HashMap<Arc<str>, (Instant, Viewport)>>,
    /// Bus names of apps that just sent one of EFFECT_EVENTS or a focus
    /// change; only fed while a `do_action` listens.
    effects: tokio::sync::broadcast::Sender<Arc<str>>,
    /// Which apps changed lately (CHANGE_EVENTS, EFFECT_EVENTS, an app
    /// leaving the bus), for `wait_change`.
    changes: Changes,
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
            views: tokio::sync::Mutex::new(HashMap::new()),
            effects: tokio::sync::broadcast::channel(64).0,
            changes: Changes::default(),
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

    /// Toolkit's id for the element (Qt: the object names of the widget
    /// path, e.g. `QApplication.FileDialog.KFileWidget...`); "" when none.
    pub async fn accessible_id(&self, o: &Obj) -> String {
        match self.prop(o, ACC, "AccessibleId").await {
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

    /// Record that the app on `bus` is in front, as `active_app` does when
    /// it finds it there.
    pub fn note_active_bus(&self, bus: Arc<str>) {
        *self.active_bus.lock().unwrap() = Some((bus, Instant::now()));
    }

    /// Ask toolkits to emit focus changes (Qt only sends events someone
    /// registered for), and keep `focus_hint` current from them. The
    /// registration ends with this connection.
    pub async fn track_focus(self: &Arc<Self>) -> R<()> {
        self.listen(
            "object:state-changed:focused",
            "org.a11y.atspi.Event.Object",
            "StateChanged",
            Some("focused"),
            |a11y, sender, path, msg| {
                a11y.note_effect(sender);
                let gained = msg
                    .body()
                    .deserialize::<(String, i32, i32, OwnedValue)>()
                    .map(|(_, d1, _, _)| d1)
                    .or_else(|_| {
                        msg.body()
                            .deserialize::<(
                                String,
                                i32,
                                i32,
                                OwnedValue,
                                HashMap<String, OwnedValue>,
                            )>()
                            .map(|(_, d1, _, _, _)| d1)
                    });
                if gained == Ok(1) {
                    a11y.note_focus(Obj::new(sender, path.clone().into()));
                }
            },
        )
        .await
    }

    /// Listen for EFFECT_EVENTS, so `do_action` can tell an action that
    /// opened a menu or dialog (and has not replied) from one still
    /// running. One registration per event for the connection's lifetime:
    /// registering per action would race the app learning of it.
    pub async fn track_actions(self: &Arc<Self>) -> R<()> {
        let all = EFFECT_EVENTS.map(|(event, iface, member, arg0)| {
            let window = event.starts_with("window:");
            self.listen(event, iface, member, arg0, move |a11y, sender, _, _| {
                a11y.note_effect(sender);
                a11y.changes.note(sender, window);
            })
        });
        futures_util::future::join_all(all)
            .await
            .into_iter()
            .collect()
    }

    /// Register `event` with the AT-SPI registry and run `on` (with the
    /// sender's bus name and the object path) for each matching signal,
    /// until this A11y is dropped. The registration ends with the connection.
    async fn listen(
        self: &Arc<Self>,
        event: &str,
        iface: &str,
        member: &str,
        arg0: Option<&str>,
        on: impl Fn(&A11y, &str, &zbus::zvariant::ObjectPath<'_>, &zbus::Message) + Send + 'static,
    ) -> R<()> {
        let mut rule = zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .interface(iface)
            .map_err(err)?
            .member(member)
            .map_err(err)?;
        if let Some(arg) = arg0 {
            rule = rule.add_arg(arg).map_err(err)?;
        }
        let stream = zbus::MessageStream::for_match_rule(rule.build(), &self.conn, Some(1024))
            .await
            .map_err(err)?;
        let args = (event, Vec::<String>::new(), "");
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
        self.dispatch(stream, on);
        Ok(())
    }

    /// Run `on` for each message of `stream` until this A11y is dropped.
    /// `on` must be quick: a stream that is not read holds up the whole
    /// connection once its queue is full.
    fn dispatch(
        self: &Arc<Self>,
        mut stream: zbus::MessageStream,
        on: impl Fn(&A11y, &str, &zbus::zvariant::ObjectPath<'_>, &zbus::Message) + Send + 'static,
    ) {
        use futures_util::StreamExt;
        let me = Arc::downgrade(self);
        tokio::spawn(async move {
            while let Some(Ok(msg)) = stream.next().await {
                let Some(a11y) = me.upgrade() else { break };
                let header = msg.header();
                if let (Some(sender), Some(path)) = (header.sender(), header.path()) {
                    on(&a11y, sender.as_str(), path, &msg);
                }
            }
        });
    }

    /// Listen for CHANGE_EVENTS and for apps leaving the bus, so
    /// `wait_change` can wake a `wait_for` when the app it watches changed
    /// instead of polling it. Registered once for the connection's
    /// lifetime, like `track_actions`; the registration ends with it.
    pub async fn track_changes(self: &Arc<Self>) -> R<()> {
        let all = CHANGE_EVENTS.map(|(event, iface, member, arg0)| {
            self.listen(event, iface, member, arg0, |a11y, sender, path, _| {
                // A child of an app's root is a window; of the registry's
                // desktop (same path), an app.
                let windows = path.as_str() == APP_ROOT;
                a11y.changes.note(sender, windows);
            })
        });
        let gone = async {
            // A bus daemon signal: no registration needed.
            let rule = zbus::MatchRule::builder()
                .msg_type(zbus::message::Type::Signal)
                .sender("org.freedesktop.DBus")
                .map_err(err)?
                .interface("org.freedesktop.DBus")
                .map_err(err)?
                .member("NameOwnerChanged")
                .map_err(err)?
                .build();
            let stream = zbus::MessageStream::for_match_rule(rule, &self.conn, Some(256))
                .await
                .map_err(err)?;
            self.dispatch(stream, |a11y, _, _, msg| {
                if let Ok((name, _, new)) = msg.body().deserialize::<(String, String, String)>()
                    && name.starts_with(':')
                    && new.is_empty()
                {
                    a11y.changes.forget(&name);
                }
            });
            R::Ok(())
        };
        let (events, gone) = tokio::join!(futures_util::future::join_all(all), gone);
        events.into_iter().collect::<R<()>>()?;
        gone?;
        self.changes
            .live
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    /// A mark for `wait_change`, taken before a check so that changes
    /// during the check count.
    pub fn change_mark(&self) -> u64 {
        self.changes.mark()
    }

    /// Wait until an app on one of `buses` (or the set of apps or windows)
    /// changed after `mark`, and the burst of events settled, or until
    /// `timeout`. True on a change. Without change events (registering
    /// them failed), one poll interval.
    pub async fn wait_change(
        &self,
        buses: &[&str],
        mark: u64,
        timeout: std::time::Duration,
    ) -> bool {
        if !self.changes.live.load(std::sync::atomic::Ordering::SeqCst) {
            tokio::time::sleep(timeout.min(std::time::Duration::from_millis(50))).await;
            return true;
        }
        self.changes.wait(buses, mark, timeout, CHANGE_SETTLE).await
    }

    /// Whether change events are registered (`wait_change` hears of them).
    pub fn changes_live(&self) -> bool {
        self.changes.live.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn note_effect(&self, bus: &str) {
        if self.effects.receiver_count() > 0 {
            let _ = self.effects.send(bus.into());
        }
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

    /// The first target of `o`'s relation of `kind` (AtspiRelationType),
    /// None without one.
    pub async fn relation(&self, o: &Obj, kind: u32) -> Option<Obj> {
        let set: Vec<(u32, Vec<(String, OwnedObjectPath)>)> =
            self.call(o, ACC, "GetRelationSet", &()).await.ok()?;
        set.into_iter()
            .filter(|(k, _)| *k == kind)
            .flat_map(|(_, targets)| targets)
            .find(|(_, p)| p.as_str() != NULL_PATH)
            .map(|(b, p)| Obj::new(&b, p))
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
    /// handler, so the reply comes when the menu closes. Such an action
    /// counts as sent (the menu or dialog it opened is the expected result)
    /// once the app signals an effect (EFFECT_EVENTS, focus) and EFFECT_GRACE
    /// passes without a reply, or after ACTION_WAIT without either. The
    /// late reply is dropped.
    pub async fn do_action(&self, o: &Obj, index: i32) -> R<bool> {
        // Subscribed before the call is sent, so no effect is missed.
        let effects = self.effects.subscribe();
        let args = (index,);
        let reply = self.call(o, "org.a11y.atspi.Action", "DoAction", &args);
        await_action(reply, effects, &o.bus, ACTION_WAIT, EFFECT_GRACE).await
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

    /// Role and name, requested together.
    pub async fn identity(&self, o: &Obj) -> (String, String) {
        tokio::join!(self.role(o), self.name(o))
    }

    /// All attributes of one node, requested together.
    pub async fn info(&self, o: &Obj, text_limit: usize) -> Info {
        self.info_flagged(o, text_limit, true).await.0
    }

    /// `info`, and whether the node is visible but not showing: an overlay
    /// container that toolkits (Firefox's popover layer) never mark showing,
    /// or content scrolled out of view. Action names only `with_actions`.
    async fn info_flagged(&self, o: &Obj, text_limit: usize, with_actions: bool) -> (Info, bool) {
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
                if with_actions && has_iface("org.a11y.atspi.Action") {
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
            operable: [SELECTABLE, CHECKABLE, EXPANDABLE]
                .iter()
                .any(|s| has(raw, *s)),
        };
        (info, hidden_visible)
    }

    /// Size of the largest window of the app on `bus`, in window
    /// coordinates; None when no window answers. Measured once per VIEW_TTL.
    async fn viewport(&self, bus: &Arc<str>) -> Viewport {
        // Held while measuring, so concurrent nodes measure once.
        let mut views = self.views.lock().await;
        if let Some((at, view)) = views.get(bus)
            && at.elapsed() < VIEW_TTL
        {
            return *view;
        }
        let app = Obj::new(bus, OwnedObjectPath::try_from(APP_ROOT).unwrap());
        let windows = self.children(&app).await;
        let sizes = futures_util::future::join_all(windows.iter().take(8).map(|w| async move {
            let (role, extents) = tokio::join!(self.role(w), self.extents(w, COORD_WINDOW));
            match (role.as_str(), extents) {
                ("frame" | "window" | "dialog", Ok((_, _, w, h))) if w > 0 && h > 0 => Some((w, h)),
                _ => None,
            }
        }))
        .await;
        let best = sizes
            .into_iter()
            .flatten()
            .reduce(|(bw, bh), (w, h)| (bw.max(w), bh.max(h)));
        views.insert(bus.clone(), (Instant::now(), best));
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
        let f = Fetch {
            only_showing,
            text_limit,
            limit,
            actions: true,
            stop_roles: &[],
        };
        self.prefetch_with(cache, roots, &f).await
    }

    /// `prefetch` reading what `f` asks for.
    pub async fn prefetch_with(&self, cache: &mut HashMap<Obj, Info>, roots: Vec<Obj>, f: &Fetch) {
        let mut level: Vec<Obj> = roots
            .into_iter()
            .filter(|o| !cache.contains_key(o))
            .collect();
        let mut fetched = 0;
        while !level.is_empty() && fetched < f.limit {
            level.sort_by(|a, b| a.path.as_str().cmp(b.path.as_str()));
            level.dedup();
            let infos: Vec<(Obj, Info)> = stream::iter(level)
                .map(|o| async move {
                    let (mut info, hidden_visible) =
                        self.info_flagged(&o, f.text_limit, f.actions).await;
                    // A visible node that is not showing yet lies inside the
                    // window holds an overlay (a web page's popup menu): treat
                    // it as showing so its showing children are reached. Only
                    // a walk over showing nodes needs to know.
                    if f.only_showing
                        && hidden_visible
                        && let Some((vw, vh)) = self.viewport(&o.bus).await
                        && let Ok((x, y, w, h)) = self.extents(&o, COORD_WINDOW).await
                    {
                        info.showing = overlaps((x, y, w, h), (vw, vh));
                    }
                    (o, info)
                })
                .buffered(IN_FLIGHT)
                .collect()
                .await;
            fetched += infos.len();
            let mut next = Vec::new();
            for (o, info) in infos {
                if (!f.only_showing || info.showing) && !f.stop_roles.contains(&info.role.as_str())
                {
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

    /// Read the action names of `ids` (fetched without them) into `cache`.
    pub async fn fill_actions(&self, cache: &mut HashMap<Obj, Info>, ids: &[Obj]) {
        let names: Vec<Vec<String>> = stream::iter(ids)
            .map(|o| self.actions(o))
            .buffered(IN_FLIGHT)
            .collect()
            .await;
        for (o, names) in ids.iter().zip(names) {
            if let Some(info) = cache.get_mut(o) {
                info.actions = names;
            }
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

    /// Nearest frame/window/dialog ancestor (or self). Role and parent of
    /// each level are asked together: one round trip per level instead of
    /// two (Firefox elements sit 15-25 levels deep); the parent read at the
    /// frame itself is the only extra call.
    pub async fn frame_of(&self, o: &Obj) -> Option<Obj> {
        let mut cur = Some(o.clone());
        while let Some(c) = cur {
            let (role, parent) = tokio::join!(self.role(&c), self.parent(&c));
            if matches!(role.as_str(), "frame" | "window" | "dialog") {
                return Some(c);
            }
            cur = parent;
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

/// Change counters per application bus, fed by the event listeners and
/// read by waiters. Every event bumps one global sequence number and
/// stores it for its bus; a waiter compares the numbers of the buses it
/// watches with the mark it took before its check. Bursts coalesce: a
/// waiter wakes once per change it has not seen, however many events came,
/// and events of other apps never wake it.
#[derive(Default)]
struct Changes {
    seq: std::sync::atomic::AtomicU64,
    /// Last change per bus. Dropped when the bus' owner leaves; a waiter
    /// for a bus without an entry adds one.
    buses: Mutex<HashMap<Arc<str>, tokio::sync::watch::Sender<u64>>>,
    /// Last change to the set of apps or of any app's windows: watched by
    /// every waiter, as the app waited for may not be running yet, or may
    /// open a window from another process.
    windows: WatchCell,
    /// The events above are registered and listened to.
    live: std::sync::atomic::AtomicBool,
}

/// A watch channel that starts at 0, for `#[derive(Default)]`.
struct WatchCell(tokio::sync::watch::Sender<u64>);

impl Default for WatchCell {
    fn default() -> Self {
        WatchCell(tokio::sync::watch::channel(0).0)
    }
}

impl Changes {
    fn mark(&self) -> u64 {
        self.seq.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Something on `bus` changed; `windows`: its windows, or the set of apps.
    fn note(&self, bus: &str, windows: bool) {
        let n = self.seq.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        let mut buses = self.buses.lock().unwrap();
        match buses.get(bus) {
            Some(tx) => {
                tx.send_replace(n);
            }
            None => {
                buses.insert(bus.into(), tokio::sync::watch::channel(n).0);
            }
        }
        drop(buses);
        if windows {
            self.windows.0.send_replace(n);
        }
    }

    /// The app on `bus` left: wake its waiters (a window is gone) and
    /// drop its entry.
    fn forget(&self, bus: &str) {
        let n = self.seq.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        if let Some(tx) = self.buses.lock().unwrap().remove(bus) {
            tx.send_replace(n);
        }
        self.windows.0.send_replace(n);
    }

    /// See `A11y::wait_change`; `settle` is the time a burst gets.
    async fn wait(
        &self,
        buses: &[&str],
        mark: u64,
        timeout: std::time::Duration,
        settle: std::time::Duration,
    ) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut watches: Vec<_> = {
            let mut map = self.buses.lock().unwrap();
            buses
                .iter()
                .map(|bus| {
                    map.entry((*bus).into())
                        .or_insert_with(|| tokio::sync::watch::channel(0).0)
                        .subscribe()
                })
                .collect()
        };
        watches.push(self.windows.0.subscribe());
        // Changes since the mark, before subscribing, are in the values.
        let seen = watches.iter().any(|w| *w.borrow() > mark);
        if !seen {
            let any =
                futures_util::future::select_all(watches.iter_mut().map(|w| Box::pin(w.changed())));
            if tokio::time::timeout_at(deadline, any).await.is_err() {
                return false;
            }
        }
        // Let the burst finish, so one check sees all of it.
        tokio::time::sleep_until((tokio::time::Instant::now() + settle).min(deadline)).await;
        true
    }
}

/// The reply of an action, or Ok(true) once it is overdue: `grace` after
/// `effects` named `bus`, or after `wait` in any case.
async fn await_action(
    reply: impl std::future::Future<Output = R<bool>>,
    mut effects: tokio::sync::broadcast::Receiver<Arc<str>>,
    bus: &str,
    wait: std::time::Duration,
    grace: std::time::Duration,
) -> R<bool> {
    use tokio::sync::broadcast::error::RecvError;
    let deadline = tokio::time::Instant::now() + wait;
    let mut reply = std::pin::pin!(reply);
    let mut listening = true;
    loop {
        tokio::select! {
            done = &mut reply => return done,
            ev = effects.recv(), if listening => match ev {
                Ok(from) if *from == *bus => break,
                Ok(_) | Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => listening = false,
            },
            _ = tokio::time::sleep_until(deadline) => return Ok(true),
        }
    }
    let until = (tokio::time::Instant::now() + grace).min(deadline);
    tokio::time::timeout_at(until, reply)
        .await
        .unwrap_or(Ok(true))
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

    /// `await_action` on a reply that comes after `reply_ms` with `value`,
    /// while `effect` (bus, after ms) is sent: (result, elapsed ms).
    fn run_action(reply_ms: u64, value: bool, effect: Option<(&str, u64)>) -> (R<bool>, u128) {
        use std::time::Duration;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        rt.block_on(async {
            let (tx, rx) = tokio::sync::broadcast::channel::<Arc<str>>(8);
            if let Some((bus, ms)) = effect {
                let bus: Arc<str> = bus.into();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                    let _ = tx.send(bus);
                    // Keep the sender alive past the wait.
                    tokio::time::sleep(Duration::from_secs(2)).await;
                });
            }
            let reply = async move {
                tokio::time::sleep(Duration::from_millis(reply_ms)).await;
                Ok(value)
            };
            let start = std::time::Instant::now();
            let out = await_action(
                reply,
                rx,
                ":1.42",
                Duration::from_millis(400),
                Duration::from_millis(25),
            )
            .await;
            (out, start.elapsed().as_millis())
        })
    }

    #[test]
    fn a_quick_reply_is_returned_as_is() {
        let (out, ms) = run_action(5, false, None);
        assert_eq!(out, Ok(false));
        assert!(ms < 100, "{ms} ms");
        // An effect does not cut a reply that follows within the grace.
        let (out, _) = run_action(15, false, Some((":1.42", 5)));
        assert_eq!(out, Ok(false));
    }

    #[test]
    fn an_effect_of_the_target_app_ends_the_wait_after_a_grace() {
        // A menu opened (reply only when it closes): sent after the grace.
        let (out, ms) = run_action(5000, false, Some((":1.42", 10)));
        assert_eq!(out, Ok(true));
        assert!((30..200).contains(&ms), "{ms} ms");
        // Events of other apps do not count: the full wait.
        let (out, ms) = run_action(5000, false, Some((":1.7", 10)));
        assert_eq!(out, Ok(true));
        assert!(ms >= 390, "{ms} ms");
        // Nobody listening at all: the full wait, too.
        let (out, ms) = run_action(5000, false, None);
        assert_eq!(out, Ok(true));
        assert!(ms >= 390, "{ms} ms");
    }

    /// `Changes::wait` for `buses` with a 300 ms timeout and 15 ms settle,
    /// while `events` (bus, windows, after ms) are noted: (changed, ms).
    fn run_wait(
        changes: Arc<Changes>,
        buses: &[&str],
        events: Vec<(&'static str, bool, u64)>,
    ) -> (bool, u128) {
        use std::time::Duration;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        rt.block_on(async {
            let mark = changes.mark();
            let feed = changes.clone();
            tokio::spawn(async move {
                for (bus, windows, ms) in events {
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                    feed.note(bus, windows);
                }
            });
            let start = std::time::Instant::now();
            let changed = changes
                .wait(
                    buses,
                    mark,
                    Duration::from_millis(300),
                    Duration::from_millis(15),
                )
                .await;
            (changed, start.elapsed().as_millis())
        })
    }

    #[test]
    fn a_change_of_the_watched_app_ends_the_wait_once_it_settled() {
        let changes = Arc::new(Changes::default());
        let (changed, ms) = run_wait(changes.clone(), &[":1.5"], vec![(":1.5", false, 20)]);
        assert!(changed);
        assert!((30..150).contains(&ms), "{ms} ms");
        // A burst wakes the waiter once; once it is over, a new mark sees
        // nothing left over.
        let burst = (0..20).map(|_| (":1.5", false, 0)).collect();
        let (changed, ms) = run_wait(changes.clone(), &[":1.5"], burst);
        assert!(changed && ms < 150, "{ms} ms");
        let (changed, ms) = run_wait(changes, &[":1.5"], vec![]);
        assert!(!changed && ms >= 290, "{ms} ms");
    }

    #[test]
    fn changes_of_other_apps_do_not_wake_the_waiter() {
        let changes = Arc::new(Changes::default());
        let others = (0..30).map(|i| (":1.7", false, 5 + i % 2)).collect();
        let (changed, ms) = run_wait(changes, &[":1.5", ":1.6"], others);
        assert!(!changed);
        assert!(ms >= 290, "{ms} ms");
    }

    #[test]
    fn a_change_between_mark_and_wait_is_not_missed() {
        let changes = Changes::default();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        // Before the mark: old news.
        changes.note(":1.5", false);
        let mark = changes.mark();
        // During the check, before the waiter subscribed.
        changes.note(":1.5", false);
        let wait = |mark| {
            changes.wait(
                &[":1.5"],
                mark,
                std::time::Duration::from_millis(200),
                std::time::Duration::from_millis(5),
            )
        };
        let start = std::time::Instant::now();
        assert!(rt.block_on(wait(mark)));
        assert!(start.elapsed().as_millis() < 100);
        let start = std::time::Instant::now();
        assert!(!rt.block_on(wait(changes.mark())));
        assert!(start.elapsed().as_millis() >= 190);
    }

    #[test]
    fn window_changes_and_apps_leaving_wake_every_waiter() {
        let changes = Arc::new(Changes::default());
        // An app not running yet (no buses): a window or app appearing on
        // any bus counts, other changes do not.
        let (changed, _) = run_wait(changes.clone(), &[], vec![(":1.9", false, 10)]);
        assert!(!changed);
        let (changed, ms) = run_wait(changes.clone(), &[], vec![(":1.9", true, 10)]);
        assert!(changed && ms < 150, "{ms} ms");
        // The watched app leaving the bus ends the wait, too.
        let feed = changes.clone();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let start = std::time::Instant::now();
        let changed = rt.block_on(async {
            let mark = feed.mark();
            let gone = feed.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                gone.forget(":1.5");
            });
            feed.wait(
                &[":1.5"],
                mark,
                std::time::Duration::from_millis(300),
                std::time::Duration::from_millis(5),
            )
            .await
        });
        assert!(changed && start.elapsed().as_millis() < 150);
        assert!(!changes.buses.lock().unwrap().contains_key(":1.5"));
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
