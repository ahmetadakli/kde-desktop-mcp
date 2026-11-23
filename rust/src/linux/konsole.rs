//! Konsole's tabs over D-Bus: which session a window shows and the shell
//! behind each, so the self-client guard can tell the agent's own tab from
//! the others in one Konsole window, and `show` to bring a tab to the
//! front of its window (activate_window's `terminal`). Only services that
//! have an owner are called, so D-Bus never starts a Konsole.

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

/// A tab of a Konsole window: what `show` needs to bring it to the front.
#[derive(Clone, Debug, PartialEq)]
pub struct Tab {
    pub service: String,
    /// The window's object path ("/Windows/1").
    pub window: String,
    pub session: i32,
    /// Title of the tab the window shows now, to tell its window among
    /// several ("" when Konsole has one window).
    pub front: String,
    /// The tab is the one shown.
    pub shown: bool,
}

/// One Konsole window as read for `locate`: object path, title of the
/// tab in front, that tab's session, and every session with its pty.
type WindowTabs = (String, String, i32, Vec<(i32, Option<String>)>);

/// The tab of `windows` whose shell is on pty `tty`.
fn locate(service: &str, windows: &[WindowTabs], tty: &str) -> Option<Tab> {
    let one = windows.len() == 1;
    windows.iter().find_map(|(path, title, cur, sessions)| {
        let (session, _) = sessions.iter().find(|(_, t)| t.as_deref() == Some(tty))?;
        Some(Tab {
            service: service.to_string(),
            window: path.clone(),
            session: *session,
            front: if one { String::new() } else { title.clone() },
            shown: session == cur,
        })
    })
}

async fn read_tab(
    conn: &Connection,
    pid: u32,
    tty: &str,
    tty_of: impl Fn(u32) -> Option<String>,
) -> Option<Tab> {
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
    let mut windows: Vec<WindowTabs> = Vec::new();
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
        let list: Vec<String> = call(conn, &svc, &path, WINDOW, "sessionList", &())
            .await
            .unwrap_or_default();
        let mut sessions = Vec::new();
        for s in list {
            let Ok(id) = s.parse::<i32>() else { continue };
            let tty = shell_pid(conn, &svc, &s).await.and_then(&tty_of);
            sessions.push((id, tty));
        }
        windows.push((path, title, cur, sessions));
    }
    locate(&svc, &windows, tty)
}

/// The tab of Konsole process `pid` whose shell is on pty `tty` ("pts/3"),
/// `tty_of` giving a process' pty. None when `pid` is no Konsole or no
/// tab of it is on `tty`.
pub async fn tab(
    conn: &Connection,
    pid: u32,
    tty: &str,
    tty_of: impl Fn(u32) -> Option<String>,
) -> Option<Tab> {
    tokio::time::timeout(WAIT, read_tab(conn, pid, tty, tty_of))
        .await
        .ok()
        .flatten()
}

/// Bring `tab` to the front of its window and check that it is.
pub async fn show(conn: &Connection, tab: &Tab) -> Result<(), String> {
    let run = async {
        // The service must still be there: never start one.
        owner_pid(conn, &tab.service)
            .await
            .ok_or("Konsole is gone")?;
        conn.call_method(
            Some(tab.service.as_str()),
            tab.window.as_str(),
            Some(WINDOW),
            "setCurrentSession",
            &(tab.session,),
        )
        .await
        .map_err(|e| format!("Konsole did not switch tabs: {e}"))?;
        let cur: Option<i32> = call(
            conn,
            &tab.service,
            &tab.window,
            WINDOW,
            "currentSession",
            &(),
        )
        .await;
        match cur {
            Some(c) if c == tab.session => Ok(()),
            _ => Err("Konsole did not switch tabs".to_string()),
        }
    };
    tokio::time::timeout(WAIT, run)
        .await
        .unwrap_or_else(|_| Err("Konsole did not answer".into()))
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

    /// Read only, against the live session bus: the tab of Konsole
    /// DESKTOP_MCP_KONSOLE_PID on pty DESKTOP_MCP_TTY ("pts/3").
    #[test]
    #[ignore]
    fn tab_live() {
        let var = |n: &str| std::env::var(n).unwrap_or_default();
        let (Ok(pid), tty) = (
            var("DESKTOP_MCP_KONSOLE_PID").parse::<u32>(),
            var("DESKTOP_MCP_TTY"),
        ) else {
            eprintln!("skipped: set DESKTOP_MCP_KONSOLE_PID and DESKTOP_MCP_TTY");
            return;
        };
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let conn = Connection::session().await.unwrap();
            let tab = tab(&conn, pid, &tty, super::super::terminals::tty_of).await;
            println!("{tty}: {tab:?}");
            assert!(tab.is_some());
        });
    }

    #[test]
    fn a_tab_is_found_by_its_pty_in_any_window() {
        let pts = |n: u32| Some(format!("pts/{n}"));
        let windows: Vec<WindowTabs> = vec![
            (
                "/Windows/1".into(),
                "~ : claude".into(),
                1,
                vec![(1, pts(0)), (3, pts(5))],
            ),
            (
                "/Windows/2".into(),
                "~/src : fish".into(),
                4,
                vec![(4, pts(7)), (6, None)],
            ),
        ];
        let tab = locate("org.kde.konsole-9", &windows, "pts/5").unwrap();
        assert_eq!(
            tab,
            Tab {
                service: "org.kde.konsole-9".into(),
                window: "/Windows/1".into(),
                session: 3,
                front: "~ : claude".into(),
                shown: false,
            }
        );
        let shown = locate("k", &windows, "pts/7").unwrap();
        assert_eq!((shown.window.as_str(), shown.shown), ("/Windows/2", true));
        assert_eq!(locate("k", &windows, "pts/9"), None);
        // With one window, which one it is needs no title.
        let one = locate("k", &windows[..1], "pts/0").unwrap();
        assert_eq!((one.front.as_str(), one.shown), ("", true));
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
