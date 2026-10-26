//! Hyprland's request socket, spoken directly instead of spawning `hyprctl`.

use std::path::PathBuf;

use serde_json::Value as Json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use crate::backend::{R, Screen, WindowChange, WindowGeo};

/// Where `move_window` state=minimize parks a window: Hyprland has no
/// minimize, so a special workspace nothing shows unless toggled.
pub const MINIMIZED_WORKSPACE: &str = "special:minimized";

/// Separates the replies of a `[[BATCH]]` request.
const BATCH_SEP: &str = "\n\n\n";

pub struct Hypr {
    socket: PathBuf,
}

impl Hypr {
    /// The running instance's socket; None outside a Hyprland session.
    pub fn from_env() -> Option<Self> {
        let sig = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok()?;
        let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap_or_default();
        // Hyprland 0.40 moved the sockets from /tmp into XDG_RUNTIME_DIR.
        let candidates = [
            PathBuf::from(runtime)
                .join("hypr")
                .join(&sig)
                .join(".socket.sock"),
            PathBuf::from("/tmp/hypr").join(&sig).join(".socket.sock"),
        ];
        let socket = candidates
            .iter()
            .find(|p| p.exists())
            .unwrap_or(&candidates[0])
            .clone();
        Some(Hypr { socket })
    }

    async fn request(&self, cmd: &str) -> R<String> {
        let mut s = UnixStream::connect(&self.socket)
            .await
            .map_err(|e| format!("hyprland socket {}: {e}", self.socket.display()))?;
        s.write_all(cmd.as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        let mut out = String::new();
        s.read_to_string(&mut out)
            .await
            .map_err(|e| e.to_string())?;
        Ok(out)
    }

    /// `hyprctl -j <args>`.
    pub async fn json(&self, args: &str) -> R<Json> {
        let raw = self.request(&format!("j/{args}")).await?;
        if raw.trim().is_empty() {
            return Ok(Json::Null);
        }
        serde_json::from_str(&raw).map_err(|e| format!("hyprctl {args}: {e}: {raw}"))
    }

    /// `hyprctl dispatch <args>`.
    pub async fn dispatch(&self, args: &str) -> R<()> {
        let out = self.request(&format!("dispatch {args}")).await?;
        match out.trim() {
            "ok" | "" => Ok(()),
            other => Err(format!("hyprctl dispatch {args}: {other}")),
        }
    }

    /// Several requests in one round trip (`hyprctl --batch`); one reply
    /// per request. Requests must not contain ';'.
    pub async fn batch(&self, cmds: &[String]) -> R<Vec<String>> {
        if let [one] = cmds {
            return Ok(vec![self.request(one).await?]);
        }
        let raw = self
            .request(&format!("[[BATCH]]{}", cmds.join(";")))
            .await?;
        Ok(raw.split(BATCH_SEP).map(str::to_string).collect())
    }

    /// `hyprctl --batch "dispatch a; dispatch b"`; Err names the first that
    /// failed.
    pub async fn dispatch_all(&self, args: &[String]) -> R<()> {
        if args.is_empty() {
            return Ok(());
        }
        let cmds: Vec<String> = args.iter().map(|a| format!("dispatch {a}")).collect();
        let replies = self.batch(&cmds).await?;
        for (i, a) in args.iter().enumerate() {
            match replies.get(i).map(|r| r.trim()) {
                Some("ok" | "") => {}
                Some(other) => return Err(format!("hyprctl dispatch {a}: {other}")),
                None => return Err(format!("hyprctl dispatch {a}: no reply")),
            }
        }
        Ok(())
    }

    /// Clients, monitors and the active window in one round trip.
    pub async fn snapshot(&self) -> R<Snapshot> {
        let cmds = ["j/clients", "j/monitors", "j/activewindow"].map(String::from);
        let replies = self.batch(&cmds).await?;
        let parse = |i: usize| -> R<Json> {
            let raw = replies.get(i).map(|r| r.trim()).unwrap_or("");
            if raw.is_empty() {
                return Ok(Json::Null);
            }
            serde_json::from_str(raw).map_err(|e| format!("hyprctl {}: {e}: {raw}", cmds[i]))
        };
        let active = parse(2)?;
        Ok(Snapshot {
            clients: parse(0)?,
            monitors: parse(1)?,
            active: active["address"].as_str().unwrap_or("").to_string(),
        })
    }

    /// The focused client, or None.
    pub async fn active_window(&self) -> R<Option<Json>> {
        let aw = self.json("activewindow").await?;
        Ok(aw
            .get("address")
            .and_then(Json::as_str)
            .filter(|a| !a.is_empty())
            .map(|_| aw.clone()))
    }
}

/// What `Hypr::snapshot` read: `clients -j`, `monitors -j` and the active
/// window's address ("" when none).
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub clients: Json,
    pub monitors: Json,
    pub active: String,
}

/// A mapped, shown client and what `move_window` needs to change it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Client {
    /// `app` is the window class.
    pub geo: WindowGeo,
    pub address: String,
    pub initial_class: String,
    pub floating: bool,
    /// Hyprland's internal fullscreen mode: 0 none, 1 maximized,
    /// 2 fullscreen, 3 both.
    pub fullscreen: i64,
    pub monitor: i64,
}

impl Client {
    /// Whether the title or class contains `q` (lower-case; "" matches).
    pub fn matches(&self, q: &str) -> bool {
        q.is_empty()
            || [&self.geo.title, &self.geo.app, &self.initial_class]
                .iter()
                .any(|s| s.to_lowercase().contains(q))
    }
}

/// A monitor's logical size: Hyprland reports the mode's pixels, before
/// the scale and the transform (odd transforms turn it by 90 degrees).
pub fn logical_size(width: f64, height: f64, scale: f64, transform: i64) -> (f64, f64) {
    let scale = if scale > 0.0 { scale } else { 1.0 };
    let (w, h) = ((width / scale).round(), (height / scale).round());
    if transform % 2 == 1 { (h, w) } else { (w, h) }
}

/// The enabled monitors of `monitors -j` in logical px, the layout
/// coordinates clients and `movewindowpixel` use.
pub fn screens_of(monitors: &Json) -> Vec<Screen> {
    monitors
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| !m["disabled"].as_bool().unwrap_or(false))
        .map(|m| {
            let num = |k: &str| m[k].as_f64().unwrap_or(0.0);
            let scale = m["scale"].as_f64().filter(|s| *s > 0.0).unwrap_or(1.0);
            let (width, height) = logical_size(
                num("width"),
                num("height"),
                scale,
                m["transform"].as_i64().unwrap_or(0),
            );
            Screen {
                name: m["name"].as_str().unwrap_or("").to_string(),
                x: num("x"),
                y: num("y"),
                width,
                height,
                scale,
            }
        })
        .collect()
}

/// The workspace a monitor shows (not its special one).
pub fn monitor_workspace(monitors: &Json, id: i64) -> Option<i64> {
    monitors
        .as_array()?
        .iter()
        .find(|m| m["id"].as_i64() == Some(id))?["activeWorkspace"]["id"]
        .as_i64()
}

/// Mapped clients that are not hidden (a group's background tabs), in
/// Hyprland's order. `at`/`size` are already logical px. Hyprland has no
/// minimize: a client on a special workspace no monitor shows counts as
/// minimized.
pub fn clients_of(snap: &Snapshot) -> Vec<Client> {
    let monitors = snap.monitors.as_array().map(Vec::as_slice).unwrap_or(&[]);
    let output = |id: i64| {
        monitors
            .iter()
            .find(|m| m["id"].as_i64() == Some(id))
            .and_then(|m| m["name"].as_str())
            .unwrap_or("")
            .to_string()
    };
    let shown_special = |ws: i64| {
        monitors
            .iter()
            .any(|m| m["specialWorkspace"]["id"].as_i64() == Some(ws))
    };
    snap.clients
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| {
            c["mapped"].as_bool().unwrap_or(true) && !c["hidden"].as_bool().unwrap_or(false)
        })
        .filter_map(|c| {
            let address = c["address"].as_str().filter(|a| !a.is_empty())?.to_string();
            let pair = |k: &str, i: usize| c[k][i].as_f64().unwrap_or(0.0);
            let text = |k: &str| c[k].as_str().unwrap_or("").to_string();
            let ws = c["workspace"]["id"].as_i64().unwrap_or(0);
            let special = ws < 0
                || c["workspace"]["name"]
                    .as_str()
                    .is_some_and(|n| n.starts_with("special"));
            let fullscreen = c["fullscreen"].as_i64().unwrap_or(0);
            let monitor = c["monitor"].as_i64().unwrap_or(-1);
            Some(Client {
                geo: WindowGeo {
                    title: text("title"),
                    app: text("class"),
                    x: pair("at", 0),
                    y: pair("at", 1),
                    width: pair("size", 0),
                    height: pair("size", 1),
                    active: address == snap.active,
                    minimized: special && !shown_special(ws),
                    maximized: fullscreen != 0,
                    output: output(monitor),
                    pid: c["pid"]
                        .as_i64()
                        .and_then(|p| u32::try_from(p).ok())
                        .unwrap_or(0),
                    terminals: Vec::new(),
                },
                initial_class: text("initialClass"),
                floating: c["floating"].as_bool().unwrap_or(false),
                fullscreen,
                monitor,
                address,
            })
        })
        .collect()
}

/// Dispatchers that apply `change` to `c`, for one batch. `back` is the
/// workspace a minimized client returns to (its monitor's). Hyprland's
/// fullscreenstate only acts on the focused window, so maximize and
/// leaving it focus the client first. Geometry needs a floating window:
/// a tiled one is floated, and then takes every field (the tiled size for
/// those not given) since floating places it anew. A resize is followed
/// by a move, as Hyprland keeps the centre and not the corner.
pub fn change_commands(c: &Client, change: &WindowChange, back: Option<i64>) -> R<Vec<String>> {
    let addr = format!("address:{}", c.address);
    let mut cmds = Vec::new();
    let mut fullscreen = c.fullscreen != 0;
    let unminimize = |cmds: &mut Vec<String>| -> R<()> {
        if c.geo.minimized {
            let ws = back.ok_or("cannot tell which workspace to restore the window to")?;
            cmds.push(format!("movetoworkspacesilent {ws},{addr}"));
        }
        Ok(())
    };
    let leave_fullscreen = |cmds: &mut Vec<String>, fullscreen: &mut bool| {
        if *fullscreen {
            cmds.push(format!("focuswindow {addr}"));
            cmds.push("fullscreenstate 0 0".into());
            *fullscreen = false;
        }
    };
    match change.state.as_deref() {
        Some("minimize") => {
            if !c.geo.minimized {
                cmds.push(format!(
                    "movetoworkspacesilent {MINIMIZED_WORKSPACE},{addr}"
                ));
            }
        }
        Some("maximize") => {
            unminimize(&mut cmds)?;
            cmds.push(format!("focuswindow {addr}"));
            cmds.push("fullscreenstate 1 1".into());
            fullscreen = true;
        }
        Some("restore") => {
            unminimize(&mut cmds)?;
            leave_fullscreen(&mut cmds, &mut fullscreen);
        }
        _ => {}
    }
    let geometry = [change.x, change.y, change.width, change.height];
    if geometry.iter().any(Option::is_some) {
        leave_fullscreen(&mut cmds, &mut fullscreen);
        let g = &c.geo;
        let float = !c.floating;
        if float {
            cmds.push(format!("setfloating {addr}"));
        }
        let resize = float || change.width.is_some() || change.height.is_some();
        if resize {
            let w = change.width.unwrap_or(g.width).round().max(1.0);
            let h = change.height.unwrap_or(g.height).round().max(1.0);
            cmds.push(format!("resizewindowpixel exact {w} {h},{addr}"));
        }
        // Hyprland resizes a floating window around its centre: move it
        // back so the top-left corner stays, as on KDE.
        if resize || change.x.is_some() || change.y.is_some() {
            let x = change.x.unwrap_or(g.x).round();
            let y = change.y.unwrap_or(g.y).round();
            cmds.push(format!("movewindowpixel exact {x} {y},{addr}"));
        }
    }
    Ok(cmds)
}

/// Collapse whitespace: toolkits and compositors space titles differently
/// (Kate's AT-SPI frame name has two spaces before the dash, its title one).
pub fn norm(title: &str) -> String {
    title.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `wtype` invocations for `text`, with newlines and tabs as real keys.
pub fn wtype_commands(text: &str) -> Vec<Vec<String>> {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut cmds = Vec::new();
    let mut chunk = String::new();
    for ch in text.chars() {
        if ch == '\n' || ch == '\t' {
            if !chunk.is_empty() {
                cmds.push(vec!["--".into(), std::mem::take(&mut chunk)]);
            }
            cmds.push(vec![
                "-k".into(),
                if ch == '\n' { "Return" } else { "Tab" }.into(),
            ]);
        } else {
            chunk.push(ch);
        }
    }
    if !chunk.is_empty() {
        cmds.push(vec!["--".into(), chunk]);
    }
    cmds
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wtype_splits_newlines_and_tabs() {
        let got = wtype_commands("a b\nc\td\r\n");
        let want: Vec<Vec<&str>> = vec![
            vec!["--", "a b"],
            vec!["-k", "Return"],
            vec!["--", "c"],
            vec!["-k", "Tab"],
            vec!["--", "d"],
            vec!["-k", "Return"],
        ];
        assert_eq!(
            got,
            want.iter()
                .map(|v| v.iter().map(|s| s.to_string()).collect::<Vec<_>>())
                .collect::<Vec<_>>()
        );
    }

    /// Captured from a Hyprland 0.56.2 test instance: HEADLESS-1 1920x1080
    /// at scale 1.5, HEADLESS-2 1920x1080 turned 90 degrees (transform 1);
    /// kcalc maximized and active, "alpha term" floating and minimized to
    /// special:minimized, "beta term" tiled on HEADLESS-2.
    fn fixture() -> Snapshot {
        Snapshot {
            clients: serde_json::from_str(include_str!("testdata/hypr_clients.json")).unwrap(),
            monitors: serde_json::from_str(include_str!("testdata/hypr_monitors.json")).unwrap(),
            active: "0x55f94a3aaeb0".into(),
        }
    }

    fn by_title<'a>(list: &'a [Client], title: &str) -> &'a Client {
        list.iter().find(|c| c.geo.title == title).unwrap()
    }

    #[test]
    fn monitor_sizes_are_logical() {
        assert_eq!(logical_size(1920.0, 1080.0, 1.5, 0), (1280.0, 720.0));
        assert_eq!(logical_size(1920.0, 1080.0, 1.0, 1), (1080.0, 1920.0));
        assert_eq!(logical_size(2560.0, 1600.0, 1.25, 3), (1280.0, 2048.0));
        assert_eq!(logical_size(2560.0, 1600.0, 1.25, 4), (2048.0, 1280.0));
        assert_eq!(logical_size(800.0, 600.0, 0.0, 0), (800.0, 600.0));
        let s = screens_of(&fixture().monitors);
        let want = vec![
            Screen {
                name: "HEADLESS-1".into(),
                x: 0.0,
                y: 0.0,
                width: 1280.0,
                height: 720.0,
                scale: 1.5,
            },
            Screen {
                name: "HEADLESS-2".into(),
                x: 1280.0,
                y: 0.0,
                width: 1080.0,
                height: 1920.0,
                scale: 1.0,
            },
        ];
        assert_eq!(s, want);
        assert_eq!(monitor_workspace(&fixture().monitors, 1), Some(2));
        assert_eq!(monitor_workspace(&fixture().monitors, 7), None);
    }

    #[test]
    fn clients_become_window_geometry() {
        let list = clients_of(&fixture());
        assert_eq!(list.len(), 3);
        let k = by_title(&list, "K Hesap");
        assert_eq!(
            k.geo,
            WindowGeo {
                title: "K Hesap".into(),
                app: "org.kde.kcalc".into(),
                x: 21.0,
                y: 21.0,
                width: 1238.0,
                height: 678.0,
                active: true,
                minimized: false,
                maximized: true,
                output: "HEADLESS-1".into(),
                pid: 526509,
                terminals: Vec::new(),
            }
        );
        assert_eq!((k.fullscreen, k.floating, k.monitor), (1, false, 1));
        let a = by_title(&list, "alpha term");
        assert!(a.geo.minimized && !a.geo.active && !a.geo.maximized && a.floating);
        assert_eq!(
            (a.geo.x, a.geo.y, a.geo.width, a.geo.height),
            (100.0, 50.0, 400.0, 250.0)
        );
        let b = by_title(&list, "beta term");
        assert_eq!(b.geo.output, "HEADLESS-2");
        assert_eq!((b.geo.width, b.geo.height), (1038.0, 1878.0));
        assert!(
            !b.geo.minimized && b.matches("alacritty") && b.matches("beta") && !b.matches("kcalc")
        );
        assert!(b.matches(""));
    }

    #[test]
    fn shown_special_workspace_is_not_minimized_and_hidden_clients_are_skipped() {
        let mut snap = fixture();
        snap.monitors[0]["specialWorkspace"] =
            serde_json::json!({"id": -98, "name": "special:minimized"});
        assert!(!by_title(&clients_of(&snap), "alpha term").geo.minimized);
        snap.clients[0]["hidden"] = Json::Bool(true);
        snap.clients[1]["mapped"] = Json::Bool(false);
        assert_eq!(clients_of(&snap).len(), 1);
        assert!(clients_of(&Snapshot::default()).is_empty());
    }

    fn ask(x: Option<f64>, w: Option<f64>, state: Option<&str>) -> WindowChange {
        WindowChange {
            x,
            width: w,
            state: state.map(str::to_string),
            ..WindowChange::default()
        }
    }

    #[test]
    fn changes_become_dispatchers() {
        let list = clients_of(&fixture());
        let (k, a, b) = (
            by_title(&list, "K Hesap"),
            by_title(&list, "alpha term"),
            by_title(&list, "beta term"),
        );
        let ka = "address:0x55f94a3aaeb0";
        // Tiled: floated first, then every field so it keeps its size.
        assert_eq!(
            change_commands(b, &ask(Some(10.0), None, None), Some(1)).unwrap(),
            vec![
                "setfloating address:0x55f94a444680".to_string(),
                "resizewindowpixel exact 1038 1878,address:0x55f94a444680".into(),
                "movewindowpixel exact 10 21,address:0x55f94a444680".into(),
            ]
        );
        // Floating: only what was asked; a maximized one leaves that first.
        let mut floating = k.clone();
        floating.floating = true;
        assert_eq!(
            change_commands(&floating, &ask(None, Some(500.4), None), None).unwrap(),
            vec![
                format!("focuswindow {ka}"),
                "fullscreenstate 0 0".into(),
                format!("resizewindowpixel exact 500 678,{ka}"),
                format!("movewindowpixel exact 21 21,{ka}"),
            ]
        );
        assert_eq!(
            change_commands(k, &ask(None, None, Some("minimize")), None).unwrap(),
            vec![format!("movetoworkspacesilent special:minimized,{ka}")]
        );
        assert_eq!(
            change_commands(k, &ask(None, None, Some("restore")), None).unwrap(),
            vec![format!("focuswindow {ka}"), "fullscreenstate 0 0".into()]
        );
        assert!(
            change_commands(k, &ask(None, None, Some("maximize")), None)
                .unwrap()
                .ends_with(&["fullscreenstate 1 1".to_string()])
        );
        // Minimized: back to its monitor's workspace.
        let aa = "address:0x55f94a3a5470";
        assert_eq!(
            change_commands(a, &ask(None, None, Some("restore")), Some(2)).unwrap(),
            vec![format!("movetoworkspacesilent 2,{aa}")]
        );
        assert_eq!(
            change_commands(a, &ask(None, None, Some("maximize")), Some(2)).unwrap(),
            vec![
                format!("movetoworkspacesilent 2,{aa}"),
                format!("focuswindow {aa}"),
                "fullscreenstate 1 1".into(),
            ]
        );
        assert!(change_commands(a, &ask(None, None, Some("restore")), None).is_err());
        assert!(
            change_commands(a, &ask(None, None, Some("minimize")), None)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn titles_compare_without_spacing() {
        assert_eq!(norm("notes.txt  — Kate"), norm("notes.txt — Kate"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn speaks_the_socket_protocol() {
        let dir = std::env::temp_dir().join(format!("hypr-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".socket.sock");
        let _ = std::fs::remove_file(&path);
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let mut got = Vec::new();
            for _ in 0..5 {
                let (mut s, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 256];
                let n = s.read(&mut buf).await.unwrap();
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let reply = match req.as_str() {
                    "j/activewindow" => r#"{"address":"0x1","pid":42,"title":"t"}"#,
                    "dispatch movecursor 10 20" => "ok",
                    "[[BATCH]]j/clients;j/monitors;j/activewindow" => {
                        "[{\"address\":\"0x1\"}]\n\n\n[]\n\n\n{\"address\":\"0x1\"}"
                    }
                    "[[BATCH]]dispatch a;dispatch b" => "ok\n\n\nInvalid dispatcher",
                    _ => "Invalid dispatcher",
                };
                s.write_all(reply.as_bytes()).await.unwrap();
                got.push(req);
            }
            got
        });
        let h = Hypr { socket: path };
        assert_eq!(h.active_window().await.unwrap().unwrap()["pid"], 42);
        h.dispatch("movecursor 10 20").await.unwrap();
        assert!(h.dispatch("nope").await.is_err());
        let snap = h.snapshot().await.unwrap();
        assert_eq!(
            (snap.active.as_str(), snap.clients[0]["address"].as_str()),
            ("0x1", Some("0x1"))
        );
        let err = h.dispatch_all(&["a".into(), "b".into()]).await.unwrap_err();
        assert_eq!(err, "hyprctl dispatch b: Invalid dispatcher");
        assert_eq!(
            server.await.unwrap(),
            vec![
                "j/activewindow",
                "dispatch movecursor 10 20",
                "dispatch nope",
                "[[BATCH]]j/clients;j/monitors;j/activewindow",
                "[[BATCH]]dispatch a;dispatch b",
            ]
        );
    }
}
