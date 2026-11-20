//! Konsole's tabs over D-Bus, read only: which session a window shows and
//! the shell behind each, so the self-client guard can tell the agent's
//! own tab from the others in one Konsole window.

use std::time::Duration;

use zbus::Connection;

const WINDOW: &str = "org.kde.konsole.Window";
const SESSION: &str = "org.kde.konsole.Session";
/// The longest a whole lookup may take; Konsole answers in a few ms.
const WAIT: Duration = Duration::from_millis(400);

/// The tabs of one Konsole window: the shell pid of the session in front
/// and of every session.
#[derive(Clone, Debug, PartialEq)]
pub struct Tabs {
    pub shown: u32,
    pub all: Vec<u32>,
}

async fn call<B, T>(
    conn: &Connection,
    dest: &str,
    path: &str,
    iface: &str,
    method: &str,
    body: &B,
) -> Option<T>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
    T: for<'d> zbus::zvariant::DynamicDeserialize<'d>,
{
    let reply = conn
        .call_method(Some(dest), path, Some(iface), method, body)
        .await
        .ok()?;
    reply.body().deserialize::<T>().ok()
}

async fn bus<B, T>(conn: &Connection, method: &str, body: &B) -> Option<T>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
    T: for<'d> zbus::zvariant::DynamicDeserialize<'d>,
{
    call(
        conn,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
        method,
        body,
    )
    .await
}

/// Process id of the owner of bus name `name`; None without an owner (so
/// nothing is ever started by D-Bus activation).
pub async fn owner_pid(conn: &Connection, name: &str) -> Option<u32> {
    if !bus::<_, bool>(conn, "NameHasOwner", &(name,)).await? {
        return None;
    }
    bus(conn, "GetConnectionUnixProcessID", &(name,)).await
}

/// The bus name of Konsole process `pid`: `org.kde.konsole-<pid>`, else
/// the plain name when `pid` owns it (a single-instance Konsole).
async fn service(conn: &Connection, pid: u32) -> Option<String> {
    for name in [format!("org.kde.konsole-{pid}"), "org.kde.konsole".into()] {
        if owner_pid(conn, &name).await == Some(pid) {
            return Some(name);
        }
    }
    None
}

/// Child node names in introspection XML (`<node name="1"/>`).
fn child_nodes(xml: &str) -> Vec<String> {
    xml.split("<node name=\"")
        .skip(1)
        .filter_map(|rest| rest.split('"').next())
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .collect()
}

/// Which window shows `caption`: the one whose session in front has a
/// title the caption starts with ("~ : claude" in "~ : claude — Konsole");
/// the only window needs no title. None when it stays unclear.
fn pick(windows: &[(String, String)], caption: Option<&str>) -> Option<usize> {
    if windows.len() == 1 {
        return Some(0);
    }
    let caption = caption?.trim();
    let hits: Vec<usize> = windows
        .iter()
        .enumerate()
        .filter(|(_, (_, title))| !title.trim().is_empty() && caption.starts_with(title.trim()))
        .map(|(i, _)| i)
        .collect();
    match hits.as_slice() {
        [one] => Some(*one),
        _ => None,
    }
}

async fn shell_pid(conn: &Connection, svc: &str, session: &str) -> Option<u32> {
    let pid: i32 = call(
        conn,
        svc,
        &format!("/Sessions/{session}"),
        SESSION,
        "processId",
        &(),
    )
    .await?;
    u32::try_from(pid).ok().filter(|p| *p > 0)
}

async fn read_tabs(
    conn: &Connection,
    pid: u32,
    caption: impl AsyncFnOnce() -> Option<String>,
) -> Option<Tabs> {
    let svc = service(conn, pid).await?;
    let xml: String = call(
        conn,
        &svc,
        "/Windows",
        "org.freedesktop.DBus.Introspectable",
        "Introspect",
        &(),
    )
    .await?;
    // Each window's object path and the title of its session in front.
    let mut windows = Vec::new();
    for n in child_nodes(&xml) {
        let path = format!("/Windows/{n}");
        let Some(cur) = call::<_, i32>(conn, &svc, &path, WINDOW, "currentSession", &()).await
        else {
            continue;
        };
        let title: String = call(
            conn,
            &svc,
            &format!("/Sessions/{cur}"),
            SESSION,
            "title",
            &(1i32,),
        )
        .await
        .unwrap_or_default();
        windows.push((path, title, cur));
    }
    let named: Vec<(String, String)> = windows
        .iter()
        .map(|(p, t, _)| (p.clone(), t.clone()))
        .collect();
    let i = match named.len() {
        1 => 0,
        _ => pick(&named, caption().await.as_deref())?,
    };
    let (path, _, cur) = &windows[i];
    let shown = shell_pid(conn, &svc, &cur.to_string()).await?;
    let list: Vec<String> = call(conn, &svc, path, WINDOW, "sessionList", &())
        .await
        .unwrap_or_default();
    let mut all = Vec::new();
    for s in list {
        if let Some(p) = shell_pid(conn, &svc, &s).await {
            all.push(p);
        }
    }
    Some(Tabs { shown, all })
}

/// The tabs of Konsole process `pid`'s window: the one whose caption
/// `caption` gives when Konsole has several windows. None when `pid` is
/// no Konsole, or the window or its tab in front cannot be told.
pub async fn tabs(
    conn: &Connection,
    pid: u32,
    caption: impl AsyncFnOnce() -> Option<String>,
) -> Option<Tabs> {
    tokio::time::timeout(WAIT, read_tabs(conn, pid, caption))
        .await
        .ok()
        .flatten()
}

/// The Konsole process and shell of the session this server was started
/// in, from the variables Konsole sets in each tab
/// (KONSOLE_DBUS_SERVICE, KONSOLE_DBUS_SESSION).
pub async fn own_session(conn: &Connection) -> Option<(u32, u32)> {
    let svc = std::env::var("KONSOLE_DBUS_SERVICE").ok()?;
    let path = std::env::var("KONSOLE_DBUS_SESSION").ok()?;
    let session = path.strip_prefix("/Sessions/")?.to_string();
    let read = async {
        let konsole = owner_pid(conn, &svc).await?;
        let shell = shell_pid(conn, &svc, &session).await?;
        Some((konsole, shell))
    };
    tokio::time::timeout(WAIT, read).await.ok().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_nodes_come_from_introspection() {
        let xml = r#"<node><interface name="x"/><node name="1"/><node name="12"/></node>"#;
        assert_eq!(child_nodes(xml), vec!["1", "12"]);
        assert!(child_nodes("<node/>").is_empty());
    }

    #[test]
    fn the_window_is_picked_by_its_tab_title() {
        let one = [("/Windows/1".to_string(), "~ : claude".to_string())];
        assert_eq!(pick(&one, None), Some(0));
        let two = [
            ("/Windows/1".to_string(), "~ : claude".to_string()),
            ("/Windows/2".to_string(), "~ : fish".to_string()),
        ];
        assert_eq!(pick(&two, Some("~ : fish — Konsole")), Some(1));
        assert_eq!(pick(&two, Some("~ : claude — Konsole")), Some(0));
        assert_eq!(pick(&two, Some("other — Konsole")), None);
        assert_eq!(pick(&two, None), None);
        let same = [
            ("/Windows/1".to_string(), "~ : fish".to_string()),
            ("/Windows/2".to_string(), "~ : fish".to_string()),
        ];
        assert_eq!(pick(&same, Some("~ : fish — Konsole")), None);
    }
}
