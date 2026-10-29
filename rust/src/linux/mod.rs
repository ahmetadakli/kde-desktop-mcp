//! Linux backend: AT-SPI for reading and acting, uinput for keyboard and
//! buttons, and the compositor (KWin or Hyprland) for windows and the cursor.

mod atspi;
mod clipboard;
mod hyprland;
mod image;
pub mod indicator;
mod indicator_draw;
mod input;
mod kwin;
mod launch;
mod roles;
mod takeover;
mod terminals;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value as Json;
use tokio::io::AsyncWriteExt;
use zbus::zvariant::OwnedObjectPath;

use crate::backend::{
    Activated, AppEntry, Backend, Cache, Check, Clip, Fetch, FileDialog, Focused, Info, Launched,
    NO_FILE_DIALOG, R, Screen, Shot, WindowChange, WindowEntry, WindowGeo,
};
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
const BORING: &[&str] = &[
    "filler",
    "panel",
    "section",
    "unknown",
    "redundant object",
    "grouping",
];
const ACTIVATE_WAIT: Duration = Duration::from_millis(500);
const NOT_ACTIVE: &str = "element's window is not active; call activate_window first";
/// How old the active-app check before `type_text` may be for the
/// accessibility insert to trust it.
const INSERT_ACTIVE_AGE: Duration = Duration::from_secs(1);
/// How long the app in front may take to read a paste after ctrl+v before
/// the clipboard is left holding the pasted text instead of being put back.
/// Apps read it while handling the key, within a few ms when idle; one that
/// reads later would get the restored clipboard and paste the user's text.
const PASTE_READ_WAIT: Duration = Duration::from_secs(1);
/// How long a focus request may take to show in the element's state.
/// Qt reports focus about 3 ms after a GrabFocus that works.
const FOCUS_POLL: Duration = Duration::from_millis(20);
const APP_ROOT: &str = "/org/a11y/atspi/accessible/root";
const NOT_A_FILE_DIALOG: &str = "the active window is not a KDE file dialog";
/// Roles holding rows of data (files, places) or tool buttons, never the
/// dialog's form fields: not walked when looking for them.
const DIALOG_SKIP: [&str; 6] = ["list", "tree", "table", "tree table", "tool bar", "menu"];
/// Nodes looked at when searching a dialog for its fields (KFileWidget's
/// form has about 60 outside the skipped lists).
const DIALOG_MAX_NODES: usize = 400;

/// KFileWidget's file name field: the line edit of its location combo box
/// (the "Name:" row at the bottom), not the one of the URL navigator at the
/// top. Qt builds accessible ids from object and class names, so this does
/// not depend on the language.
fn is_file_name_field(id: &str) -> bool {
    id.contains("KFileWidget")
        && id.ends_with("KUrlComboBox.KLineEdit")
        && !id.contains("KUrlNavigator")
}

/// One of KFileWidget's dialog buttons (Open/Save, Cancel).
fn is_file_dialog_button(id: &str) -> bool {
    id.contains("KFileWidget") && id.ends_with(".QPushButton")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum FocusMethod {
    GrabFocus,
    SetFocusAction,
}

/// Failures in a row of a focus method, on elements of one role in one app,
/// after which `try_focus` skips it there (Qt widgets mostly ignore both
/// methods, and each try costs a poll).
const FOCUS_FAILS_TO_SKIP: u32 = 2;
/// Skips after which a skipped method is tried once more, so an app or
/// role where it works again gets it back.
const FOCUS_RETRY_AFTER: u32 = 8;

/// What each `try_focus` method did so far, per (app bus, role). Keyed by
/// role, so one element that cannot take focus (a label, a disabled row)
/// does not turn a method off for the whole app.
#[derive(Default)]
struct FocusRecord {
    map: HashMap<(String, String, FocusMethod), Tried>,
}

#[derive(Clone, Copy, Debug, Default)]
struct Tried {
    /// Failures since the last success.
    fails: u32,
    /// Tries skipped since the last one made.
    skipped: u32,
}

impl FocusRecord {
    /// Whether to try `m` on an element of `role` in the app on `bus`;
    /// counts a skip when not.
    fn should_try(&mut self, bus: &str, role: &str, m: FocusMethod) -> bool {
        let Some(t) = self.map.get_mut(&(bus.to_string(), role.to_string(), m)) else {
            return true;
        };
        if t.fails < FOCUS_FAILS_TO_SKIP || t.skipped >= FOCUS_RETRY_AFTER {
            t.skipped = 0;
            return true;
        }
        t.skipped += 1;
        false
    }

    fn note(&mut self, bus: &str, role: &str, m: FocusMethod, worked: bool) {
        let t = self
            .map
            .entry((bus.to_string(), role.to_string(), m))
            .or_default();
        t.fails = if worked { 0 } else { t.fails.saturating_add(1) };
    }
}

enum Compositor {
    Kde(KWin),
    Hyprland(Hypr),
}

pub struct LinuxBackend {
    session: zbus::Connection,
    a11y: Arc<A11y>,
    comp: Compositor,
    input: Arc<Input>,
    /// Process id per AT-SPI bus name (stable for a connection's lifetime).
    pids: Mutex<HashMap<String, Option<u32>>>,
    /// Toolkit name per AT-SPI bus name.
    toolkits: Mutex<HashMap<String, String>>,
    /// What each `try_focus` method did so far, per AT-SPI bus and role.
    focus_record: Mutex<FocusRecord>,
    /// KWin refused ScreenShot2; use spectacle from now on.
    shot_unauthorized: AtomicBool,
    indicator: tokio::sync::Mutex<IndicatorProc>,
    /// The user's input and the stop shortcut (watched from `warm_up`).
    takeover: Arc<crate::takeover::Takeover>,
}

/// The `--indicator` child process, started on the first action.
#[derive(Default)]
struct IndicatorProc {
    child: Option<(
        tokio::process::Child,
        tokio::process::ChildStdin,
        tokio::io::Lines<tokio::io::BufReader<tokio::process::ChildStdout>>,
    )>,
    /// Turned off by DESKTOP_MCP_INDICATOR=0, or after the child failed.
    off: bool,
}

/// How long a screenshot waits for the indicator to clear the screen.
const INDICATOR_HIDE_WAIT: Duration = Duration::from_millis(300);

impl IndicatorProc {
    fn spawn(&mut self) -> bool {
        if self.off {
            return false;
        }
        if self.child.is_some() {
            return true;
        }
        let env_off = std::env::var("DESKTOP_MCP_INDICATOR").is_ok_and(|v| v == "0");
        let child = std::env::current_exe()
            .ok()
            .filter(|_| !env_off)
            .and_then(|exe| {
                tokio::process::Command::new(exe)
                    .arg("--indicator")
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::null())
                    .kill_on_drop(true)
                    .spawn()
                    .ok()
            });
        let Some(mut child) = child else {
            self.off = true;
            return false;
        };
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            self.off = true;
            return false;
        };
        use tokio::io::AsyncBufReadExt;
        let lines = tokio::io::BufReader::new(stdout).lines();
        self.child = Some((child, stdin, lines));
        true
    }

    async fn write(&mut self, line: &str) -> bool {
        let Some((_, stdin, _)) = self.child.as_mut() else {
            return false;
        };
        stdin
            .write_all(format!("{line}\n").as_bytes())
            .await
            .is_ok()
            && stdin.flush().await.is_ok()
    }

    /// Drop a child that stopped answering. It may come back only when it
    /// ended cleanly, i.e. on its idle exit; whether it did.
    fn drop_child(&mut self) -> bool {
        let idle_exit = match self.child.as_mut() {
            Some((child, _, _)) => matches!(child.try_wait(), Ok(Some(s)) if s.success()),
            None => false,
        };
        self.child = None;
        self.off |= !idle_exit;
        idle_exit
    }

    /// Send one command line. A child that cannot take it is dropped; after
    /// its idle exit, with `restart`, a fresh one gets the line. The
    /// indicator stays off after a child failed, or when a fresh one cannot
    /// take the line either.
    async fn send(&mut self, line: &str, restart: bool) -> bool {
        if self.write(line).await {
            return true;
        }
        if !self.drop_child() || !restart {
            return false;
        }
        if self.spawn() && self.write(line).await {
            return true;
        }
        self.child = None;
        self.off = true;
        false
    }
}

impl LinuxBackend {
    /// Clear the indicator off the screen before a capture, waiting for it
    /// to confirm (briefly; a stuck indicator is dropped).
    async fn hide_indicator(&self) {
        let mut ind = self.indicator.lock().await;
        // A child that exited shows nothing: no need for a new one.
        if ind.child.is_none() || !ind.send("hide", false).await {
            return;
        }
        let answered = match ind.child.as_mut() {
            Some((_, _, lines)) => {
                tokio::time::timeout(INDICATOR_HIDE_WAIT, lines.next_line()).await
            }
            None => return,
        };
        match answered {
            Ok(Ok(Some(l))) if l == "hidden" => {}
            // Closed: it may just have reached its idle exit.
            Ok(Ok(None)) => {
                ind.drop_child();
            }
            _ => {
                ind.child = None;
                ind.off = true;
            }
        }
    }
}

/// `tree` and every process below it now, read from /proc (off the event
/// loop). Processes seen once stay in it, so apps that fork and let the
/// parent exit are still matched by pid.
async fn descendants_of(tree: &std::collections::HashSet<u32>) -> std::collections::HashSet<u32> {
    let roots = tree.clone();
    tokio::task::spawn_blocking(move || {
        let stats = terminals::read_stats();
        launch::descendants(&roots, stats.iter().map(|(p, s)| (*p, s.ppid)))
    })
    .await
    .unwrap_or_else(|_| tree.clone())
}

/// Fill in the terminals of each window from /proc (off the event loop);
/// no scan without windows.
async fn with_terminals(list: &mut [WindowGeo]) {
    if list.is_empty() {
        return;
    }
    let pids: Vec<u32> = list.iter().map(|w| w.pid).collect();
    let found = tokio::task::spawn_blocking(move || {
        let stats = terminals::read_stats();
        pids.into_iter()
            .map(|pid| terminals::terminals_of(pid, &stats, terminals::describe))
            .collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default();
    for (w, t) in list.iter_mut().zip(found) {
        w.terminals = t;
    }
}

/// Which of the windows read after raising one (title, ACTIVE) is the
/// raised one in front: an ACTIVE one titled like KWin's `caption`, or the
/// app's only window once ACTIVE. With several windows a title is needed:
/// the one that was in front may still report ACTIVE for a moment.
fn active_index(caption: &str, seen: &[(String, bool)]) -> Option<usize> {
    let want = hyprland::norm(caption);
    seen.iter()
        .position(|(name, active)| *active && hyprland::norm(name) == want)
        .or_else(|| (seen.len() == 1 && seen[0].1).then_some(0))
}

/// The doctor's line about the outputs.
fn screens_line(screens: &[Screen]) -> String {
    screens
        .iter()
        .map(|s| {
            format!(
                "{} at ({},{}) {}x{} scale {}",
                s.name, s.x, s.y, s.width, s.height, s.scale
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn which(cmd: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|d| d.join(cmd).is_file()))
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

/// Make `text` the clipboard again after a paste; a note when that failed.
async fn put_back(text: Vec<u8>, secret: bool) -> String {
    match clipboard::own(&text, secret).await {
        Ok(()) => String::new(),
        Err(e) => format!("; the clipboard could not be put back ({e})"),
    }
}

/// Paste instead of typing key by key: works with any layout and Unicode.
/// In-process when the compositor offers data-control, else through wl-copy.
async fn set_clipboard(text: &str) -> R<()> {
    if clipboard::own(text.as_bytes(), false).await.is_ok() {
        return Ok(());
    }
    wl_copy(text).await
}

async fn wl_copy(text: &str) -> R<()> {
    let mut child = tokio::process::Command::new("wl-copy")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("wl-copy: {e}"))?;
    let mut stdin = child.stdin.take().ok_or("wl-copy: no stdin")?;
    stdin
        .write_all(text.as_bytes())
        .await
        .map_err(|e| format!("wl-copy: {e}"))?;
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
fn temp_png() -> Option<(PathBuf, std::fs::File)> {
    use std::os::unix::fs::OpenOptionsExt;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = std::env::temp_dir().join(format!("desktop-mcp-{}-{nanos}.png", std::process::id()));
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .ok()?;
    Some((path, file))
}

/// The file only counts once something was written into it.
fn keep_if_written(path: PathBuf) -> Option<String> {
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 0) {
        Some(path.display().to_string())
    } else {
        let _ = std::fs::remove_file(&path);
        None
    }
}

impl LinuxBackend {
    /// Hyprland when HYPRLAND_INSTANCE_SIGNATURE is set, otherwise KDE;
    /// DESKTOP_MCP_BACKEND=kde|hyprland overrides.
    pub async fn new() -> R<Self> {
        let session = zbus::Connection::session()
            .await
            .map_err(|e| format!("session bus: {e}"))?;
        let a11y = Arc::new(A11y::connect(&session).await?);
        let forced = std::env::var("DESKTOP_MCP_BACKEND")
            .unwrap_or_default()
            .to_lowercase();
        let hypr = match forced.as_str() {
            "hyprland" => Some(Hypr::from_env().ok_or("HYPRLAND_INSTANCE_SIGNATURE is not set")?),
            "kde" => None,
            "" => Hypr::from_env(),
            other => {
                return Err(format!(
                    "unknown DESKTOP_MCP_BACKEND '{other}' (kde, hyprland)"
                ));
            }
        };
        let comp = match hypr {
            Some(h) => Compositor::Hyprland(h),
            None => Compositor::Kde(KWin::new(session.clone()).await?),
        };
        let absolute = matches!(comp, Compositor::Kde(_));
        // Focus events keep the hint `focused` reports current (on KDE,
        // without it `type_text` also falls back to pasting more often).
        // The others let `act` return as soon as a menu or dialog opened,
        // and `wait_for` wait for the app to change rather than poll it.
        let _ = tokio::join!(
            a11y.track_focus(),
            a11y.track_actions(),
            a11y.track_changes()
        );
        Ok(LinuxBackend {
            session,
            a11y,
            comp,
            input: Arc::new(Input::new(absolute)),
            pids: Mutex::new(HashMap::new()),
            toolkits: Mutex::new(HashMap::new()),
            focus_record: Mutex::new(FocusRecord::default()),
            shot_unauthorized: AtomicBool::new(false),
            indicator: tokio::sync::Mutex::new(IndicatorProc::default()),
            takeover: Arc::new(crate::takeover::Takeover::from_env()),
        })
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

    /// AT-SPI apps with their process id (None when unknown), name and
    /// top-level windows, from one registry scan.
    async fn apps_with_pids(&self) -> Vec<(Option<u32>, Obj, String, Vec<Obj>)> {
        let apps = self.a11y.apps_with_windows().await;
        let pids =
            futures_util::future::join_all(apps.iter().map(|(app, _, _)| self.pid(&app.bus))).await;
        pids.into_iter()
            .zip(apps)
            .map(|(pid, (app, name, windows))| (pid, app, name, windows))
            .collect()
    }

    /// The allowlist plus pids of AT-SPI apps sorted against it, for the
    /// KWin window scripts.
    fn allow_of(allowed: &[String], apps: &[(Option<u32>, Obj, String, Vec<Obj>)]) -> kwin::Allow {
        let named: Vec<(u32, String)> = apps
            .iter()
            .filter_map(|(pid, _, name, _)| Some(((*pid)?, name.clone())))
            .collect();
        kwin::Allow::new(allowed, &named)
    }

    async fn kwin_allow(&self, allowed: &[String]) -> kwin::Allow {
        Self::allow_of(allowed, &self.apps_with_pids().await)
    }

    // ------------------------------------------------------------ Hyprland

    /// AT-SPI app name per process id, from one registry scan; the first
    /// app wins when a process registered several.
    async fn app_names_by_pid(&self) -> HashMap<i64, String> {
        Self::names_by_pid(&self.apps_with_pids().await)
    }

    fn names_by_pid(apps: &[(Option<u32>, Obj, String, Vec<Obj>)]) -> HashMap<i64, String> {
        let mut out = HashMap::new();
        for (pid, _, name, _) in apps {
            if let Some(pid) = pid {
                out.entry(i64::from(*pid)).or_insert_with(|| name.clone());
            }
        }
        out
    }

    /// What `activate_window` reports for the Hyprland client it focused:
    /// the AT-SPI app of its process and the title of its AT-SPI window, as
    /// `list_apps` matches them (the only window, else by title), so the
    /// title compares equal to `list_apps`' later; the client's own class
    /// and title when no AT-SPI app or window matches.
    async fn hypr_activated(
        &self,
        apps: &[(Option<u32>, Obj, String, Vec<Obj>)],
        names: &HashMap<i64, String>,
        client: &Json,
    ) -> Activated {
        let pid = client["pid"].as_i64();
        let title = client["title"].as_str().unwrap_or("");
        let want = hyprland::norm(title);
        for (p, _, name, windows) in apps {
            if p.map(i64::from) != pid || windows.is_empty() {
                continue;
            }
            let titles =
                futures_util::future::join_all(windows.iter().map(|w| self.a11y.name(w))).await;
            let found = if titles.len() == 1 {
                titles.into_iter().next()
            } else {
                titles.into_iter().find(|t| hyprland::norm(t) == want)
            };
            if let Some(t) = found {
                return Activated {
                    app: name.clone(),
                    title: t.trim().to_string(),
                };
            }
        }
        Activated {
            app: Self::client_app_name(names, client),
            title: title.trim().to_string(),
        }
    }

    /// Screen centre of `id` when its window is `active` (Hyprland's
    /// `activewindow`). Wayland clients report extents relative to their
    /// window, which Hyprland places at `at`: the client area, without
    /// borders. Toolkits that draw their own shadow and count it in the
    /// window extents (GTK3) come out off by the shadow's width.
    async fn hypr_center(&self, active: Option<&Json>, id: &Obj) -> R<(i32, i32)> {
        let a11y = &self.a11y;
        let no_area = || "element has no on-screen area".to_string();
        let Some(aw) = active else {
            return Err(NOT_ACTIVE.into());
        };
        let Some(frame) = a11y.frame_of(id).await else {
            return Err(NOT_ACTIVE.into());
        };
        if !self.same_window(&frame, aw).await {
            return Err(NOT_ACTIVE.into());
        }
        let xwayland = aw["xwayland"].as_bool().unwrap_or(false);
        // X11 clients report real screen coordinates.
        let coords = if xwayland {
            atspi::COORD_SCREEN
        } else {
            atspi::COORD_WINDOW
        };
        let (x, y, w, h) = a11y.extents(id, coords).await?;
        if w <= 0 || h <= 0 {
            return Err(no_area());
        }
        let (ox, oy) = if xwayland {
            (0.0, 0.0)
        } else {
            (
                aw["at"][0].as_f64().unwrap_or(0.0),
                aw["at"][1].as_f64().unwrap_or(0.0),
            )
        };
        Ok((
            (ox + x as f64 + w as f64 / 2.0) as i32,
            (oy + y as f64 + h as f64 / 2.0) as i32,
        ))
    }

    /// Prefer the AT-SPI name so one allowlist works for every Linux
    /// backend; fall back to the window class for apps without accessibility.
    fn client_app_name(names: &HashMap<i64, String>, client: &Json) -> String {
        let by_pid = client["pid"]
            .as_i64()
            .and_then(|p| names.get(&p))
            .filter(|n| !n.is_empty());
        by_pid
            .map(String::as_str)
            .unwrap_or(client["class"].as_str().unwrap_or(""))
            .to_lowercase()
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

    /// Hyprland's monitors, and the clients of allowed apps whose title or
    /// class contains `query`, from one batched request. Like KWin's
    /// windows: a client's pid sorted by its AT-SPI app name decides, else
    /// its class.
    async fn hypr_clients(
        &self,
        h: &Hypr,
        query: &str,
        allowed: &[String],
    ) -> R<(hyprland::Snapshot, Vec<hyprland::Client>)> {
        let (snap, allow) = tokio::join!(h.snapshot(), self.kwin_allow(allowed));
        let snap = snap?;
        let q = query.to_lowercase();
        let list = hyprland::clients_of(&snap)
            .into_iter()
            .filter(|c| allow.permits(c.geo.pid, &[&c.geo.app, &c.initial_class]) && c.matches(&q))
            .collect();
        Ok((snap, list))
    }

    /// Every window of an allowed app, without terminals (launch's poll).
    async fn all_windows(&self, allowed: &[String]) -> R<Vec<WindowGeo>> {
        self.windows_quick("", allowed).await
    }

    // ------------------------------------------------------------ focus

    fn focus_should_try(&self, id: &Obj, role: &str, m: FocusMethod) -> bool {
        self.focus_record
            .lock()
            .unwrap()
            .should_try(&id.bus, role, m)
    }

    fn focus_note(&self, id: &Obj, role: &str, m: FocusMethod, worked: bool) {
        self.focus_record
            .lock()
            .unwrap()
            .note(&id.bus, role, m, worked);
    }

    // ------------------------------------------------------------ KDE

    async fn toolkit(&self, bus: &str) -> String {
        if let Some(t) = self.toolkits.lock().unwrap().get(bus) {
            return t.clone();
        }
        let t = self.a11y.toolkit(bus).await;
        self.toolkits
            .lock()
            .unwrap()
            .insert(bus.to_string(), t.clone());
        t
    }

    /// Paste `text` with ctrl+v through the clipboard, then put back the
    /// text the clipboard held, once the app in front has read the paste
    /// (restoring earlier would let it paste the old text). Images, files
    /// and other non-text contents are not put back; the note says so.
    async fn paste(&self, text: &str) -> R<String> {
        let saved = clipboard::save().await;
        let Ok(mut source) = clipboard::PasteSource::start(text).await else {
            // No data-control: wl-copy, and no way to see the paste read.
            set_clipboard(text).await?;
            self.input.press_combo("ctrl+v").await?;
            return Ok("; the clipboard now holds the typed text".into());
        };
        let since = std::time::Instant::now();
        if let Err(e) = self.input.press_combo("ctrl+v").await {
            // Nothing was pasted: safe to put the clipboard back now.
            if let clipboard::Saved::Text { bytes, secret } = saved {
                put_back(bytes, secret).await;
            }
            return Err(e);
        }
        let read = source.wait_read(since, PASTE_READ_WAIT).await;
        Ok(match saved {
            clipboard::Saved::Empty => String::new(),
            clipboard::Saved::Nothing(why) => {
                format!("; the clipboard now holds the typed text (not put back: {why})")
            }
            clipboard::Saved::Text { .. } if !read => "; the paste was not read within 1 s, so \
                 the clipboard was not put back"
                .into(),
            clipboard::Saved::Text { bytes, secret } => put_back(bytes, secret).await,
        })
    }

    /// Type by inserting at the caret of the focused element through
    /// AT-SPI, like a paste but without the clipboard or key events: a
    /// selection is replaced, newlines and any Unicode go in as they are,
    /// and calls arrive in order because each insert is synchronous. As with
    /// a paste, no auto-indent, auto-brackets or completion run; unlike a
    /// paste, replacing a selection takes two undo steps, only the primary
    /// cursor of a multi-cursor edit gets the text, overwrite mode and
    /// Kate's vi normal mode are bypassed.
    ///
    /// Only for Qt multi-line editors (Kate's view, QTextEdit,
    /// QPlainTextEdit), whose InsertText edits through a text cursor; Qt
    /// line edits implement it with setText, which drops their undo history
    /// and skips textEdited, so they get the paste. The focused element is
    /// known from focus events and from reads that found it focused.
    /// Ok(false) when it does not apply and nothing was changed.
    async fn insert_at_caret(&self, text: &str) -> R<bool> {
        let a11y = &self.a11y;
        let Some(target) = a11y.focus_hint() else {
            return Ok(false);
        };
        // `type_text` runs right after the caller checked the active app.
        if a11y.recent_active_bus(INSERT_ACTIVE_AGE).as_deref() != Some(&*target.bus) {
            return Ok(false);
        }
        // Qt's offsets and lengths count UTF-16 units.
        let len = text.encode_utf16().count();
        let Ok(len) = i32::try_from(len) else {
            return Ok(false);
        };
        let (toolkit, states, ifaces, count, selection, caret) = tokio::join!(
            self.toolkit(&target.bus),
            a11y.states(&target),
            a11y.interfaces(&target),
            a11y.char_count(&target),
            a11y.selection(&target),
            a11y.caret(&target)
        );
        let st = states.unwrap_or(0);
        let bit = |b: u32| st & (1u64 << b) != 0;
        if toolkit != "Qt"
            || !bit(atspi::FOCUSED)
            || !bit(atspi::EDITABLE)
            || !bit(atspi::MULTI_LINE)
            || !ifaces.iter().any(|i| i == "org.a11y.atspi.EditableText")
        {
            return Ok(false);
        }
        let (Ok(count), Ok(selection), Ok(caret)) = (count, selection, caret) else {
            return Ok(false);
        };
        // Typing replaces the selection.
        let (at, removed) = match selection {
            Some((start, end)) => {
                if !a11y.delete_text(&target, start, end).await.unwrap_or(false) {
                    return Ok(false);
                }
                (start, end - start)
            }
            None => (caret, 0),
        };
        let inserted = a11y
            .insert_text(&target, at, text, len)
            .await
            .unwrap_or(false);
        // Normally `len`; an editor may normalise text (line endings).
        let grown = a11y.char_count(&target).await? - (count - removed);
        if grown <= 0 {
            if grown == 0 && !inserted {
                // Nothing went in (read-only document?): let the paste try.
                return Ok(false);
            }
            return Err("inserting text through accessibility gave an unexpected result".into());
        }
        // Later calls continue at the caret, so it must follow the text.
        if a11y.caret(&target).await.ok() != Some(at + grown) {
            a11y.set_caret(&target, at + grown).await?;
        }
        Ok(true)
    }

    /// The file name field and default button of a KDE file dialog window,
    /// read level by level below `window` (showing nodes only).
    async fn file_dialog_parts(&self, window: &Obj) -> (Option<Obj>, Option<Obj>) {
        let a11y = &self.a11y;
        let (mut field, mut accept) = (None, None);
        let mut level = vec![window.clone()];
        let mut seen = 0;
        while !level.is_empty() && seen < DIALOG_MAX_NODES && (field.is_none() || accept.is_none())
        {
            seen += level.len();
            let nodes = futures_util::future::join_all(level.iter().map(|o| async move {
                let (role, states, id, kids) = tokio::join!(
                    a11y.role(o),
                    a11y.states(o),
                    a11y.accessible_id(o),
                    a11y.children(o)
                );
                (o.clone(), role, states.unwrap_or(0), id, kids)
            }))
            .await;
            level = Vec::new();
            for (o, role, st, id, kids) in nodes {
                if !atspi::has(st, atspi::SHOWING) {
                    continue;
                }
                if field.is_none() && atspi::has(st, atspi::EDITABLE) && is_file_name_field(&id) {
                    field = Some(o.clone());
                }
                if accept.is_none()
                    && atspi::has(st, atspi::IS_DEFAULT)
                    && is_file_dialog_button(&id)
                {
                    accept = Some(o.clone());
                }
                if !DIALOG_SKIP.contains(&role.as_str()) {
                    level.extend(kids);
                }
            }
        }
        (field, accept)
    }

    /// Whether a top-level window of `app` (an app root) is ACTIVE.
    async fn app_active(&self, app: &Obj) -> bool {
        let a11y = &self.a11y;
        let windows = a11y.children(app).await;
        futures_util::future::join_all(windows.iter().map(|w| a11y.states(w)))
            .await
            .into_iter()
            .any(|st| atspi::has(st.unwrap_or(0), atspi::ACTIVE))
    }

    /// Whether the window holding `id` (its nearest frame, window or
    /// dialog ancestor) is ACTIVE.
    async fn frame_active(&self, id: &Obj) -> bool {
        match self.a11y.frame_of(id).await {
            Some(f) => self.a11y.has_state(&f, atspi::ACTIVE).await,
            None => false,
        }
    }

    /// After KWin raised the window `raised`: wait until the toolkit
    /// reports it ACTIVE, reading only the windows of the AT-SPI apps of its
    /// process (`apps` is the scan the allowlist was built from). None when
    /// no AT-SPI app has that pid (no accessibility, or a sandbox proxy owns
    /// its bus) or the wait ran out.
    async fn kde_wait_active(
        &self,
        raised: &kwin::Raised,
        apps: &[(Option<u32>, Obj, String, Vec<Obj>)],
    ) -> Option<Activated> {
        let mine: Vec<(&Obj, &str, &Obj)> = apps
            .iter()
            .filter(|(pid, ..)| *pid == Some(raised.pid))
            .flat_map(|(_, app, name, ws)| ws.iter().map(move |w| (app, name.as_str(), w)))
            .collect();
        if mine.is_empty() {
            return None;
        }
        let a11y = &self.a11y;
        let start = std::time::Instant::now();
        loop {
            let seen = futures_util::future::join_all(mine.iter().map(|(_, _, w)| async move {
                let (name, st) = tokio::join!(a11y.name(w), a11y.states(w));
                (name, atspi::has(st.unwrap_or(0), atspi::ACTIVE))
            }))
            .await;
            if let Some(i) = active_index(&raised.caption, &seen) {
                let (app, name, _) = mine[i];
                a11y.note_active_bus(app.bus.clone());
                return Some(Activated {
                    app: name.to_string(),
                    title: seen[i].0.trim().to_string(),
                });
            }
            if start.elapsed() >= ACTIVATE_WAIT {
                return None;
            }
            // A round reads only this app's windows: cheap enough to look often.
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

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
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
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
        // In parallel: each device waits DEVICE_SETTLE after creation.
        let input = self.input.clone();
        std::thread::spawn(move || input.warm_up_keyboard());
        let input = self.input.clone();
        std::thread::spawn(move || input.warm_up_pointer());
        takeover::start(
            self.takeover.clone(),
            matches!(self.comp, Compositor::Kde(_)),
            self.session.clone(),
        );
    }

    fn takeover(&self) -> Option<Arc<crate::takeover::Takeover>> {
        Some(self.takeover.clone())
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
                let (kind, title, active) = tokio::join!(a11y.role(w), a11y.name(w), async {
                    match (&self.comp, &active_client) {
                        (Compositor::Kde(_), _) => a11y.has_state(w, atspi::ACTIVE).await,
                        (Compositor::Hyprland(_), Some(c)) => self.same_window(w, c).await,
                        (Compositor::Hyprland(_), None) => false,
                    }
                });
                WindowEntry {
                    kind,
                    title: title.trim().to_string(),
                    active,
                }
            }))
        }))
        .await;
        apps.into_iter()
            .zip(details)
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

    fn change_mark(&self) -> u64 {
        self.a11y.change_mark()
    }

    async fn wait_change(&self, near: &[Obj], mark: u64, timeout: Duration) -> bool {
        let mut buses: Vec<&str> = near.iter().map(|o| o.bus.as_ref()).collect();
        buses.sort_unstable();
        buses.dedup();
        self.a11y.wait_change(&buses, mark, timeout).await
    }

    fn changes_live(&self) -> bool {
        self.a11y.changes_live()
    }

    fn changed_since(&self, near: &[Obj], mark: u64) -> Option<bool> {
        let buses: Vec<&str> = near.iter().map(|o| o.bus.as_ref()).collect();
        self.a11y.changed_since(&buses, mark)
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

    async fn identity(&self, id: &Obj) -> (String, String) {
        self.a11y.identity(id).await
    }

    async fn prefetch(
        &self,
        cache: &mut Cache<Obj>,
        roots: Vec<Obj>,
        only_showing: bool,
        text_limit: usize,
        limit: usize,
    ) {
        self.a11y
            .prefetch(cache, roots, only_showing, text_limit, limit)
            .await
    }

    async fn prefetch_with(&self, cache: &mut Cache<Obj>, roots: Vec<Obj>, f: &Fetch) {
        self.a11y.prefetch_with(cache, roots, f).await
    }

    async fn fill_actions(&self, cache: &mut Cache<Obj>, ids: &[Obj]) {
        self.a11y.fill_actions(cache, ids).await
    }

    async fn embedded_document(&self, window: &Obj) -> Option<Obj> {
        self.a11y.relation(window, atspi::RELATION_EMBEDS).await
    }

    async fn loading(&self, windows: &[Obj]) -> bool {
        let busy = futures_util::future::join_all(windows.iter().map(|w| async move {
            match self.embedded_document(w).await {
                Some(doc) => self.a11y.has_state(&doc, atspi::BUSY).await,
                None => false,
            }
        }))
        .await;
        busy.into_iter().any(|b| b)
    }

    async fn actions(&self, id: &Obj) -> Vec<String> {
        self.a11y.actions(id).await
    }

    async fn do_action(&self, id: &Obj, index: usize) -> R<bool> {
        self.a11y.do_action(id, index as i32).await
    }

    async fn full_text(&self, id: &Obj, limit: usize) -> R<String> {
        if self
            .a11y
            .interfaces(id)
            .await
            .iter()
            .any(|i| i == "org.a11y.atspi.Text")
        {
            self.a11y
                .text(id, limit.min(i32::MAX as usize) as i32)
                .await
        } else {
            Ok(self.a11y.name(id).await)
        }
    }

    async fn set_text(&self, id: &Obj, text: &str) -> R<bool> {
        if !self
            .a11y
            .interfaces(id)
            .await
            .iter()
            .any(|i| i == "org.a11y.atspi.EditableText")
        {
            return Err("element is not editable text; use focus + type".into());
        }
        self.a11y.set_text(id, text).await
    }

    fn app_key(&self, id: &Obj) -> Option<String> {
        // A unique connection name, never reused for another process.
        Some(id.bus.to_string())
    }

    async fn focused(&self) -> Option<Focused<Obj>> {
        // Focus events keep the hint current; check it still holds focus,
        // and that its own app has the active window. The app's name comes
        // from the hint's own bus (a unique connection name, never reused),
        // so the caller checks the right app against the allowlist even
        // when the input moved focus to another app. All of it in one go:
        // two round trips, instead of a registry scan and then a fetch of
        // children and text the note never shows.
        let a11y = &self.a11y;
        let hint = a11y.focus_hint()?;
        let app = Self::app_of(&hint);
        let in_front = async {
            match &self.comp {
                Compositor::Kde(_) => self.app_active(&app).await,
                // Hyprland's active window is not an AT-SPI state: the
                // hint's process (cached per bus) must own the client in front.
                Compositor::Hyprland(h) => {
                    let (pid, aw) = tokio::join!(self.pid(&hint.bus), h.active_window());
                    match (pid, aw) {
                        (Some(pid), Ok(Some(aw))) => aw["pid"].as_i64() == Some(i64::from(pid)),
                        _ => false,
                    }
                }
            }
        };
        let (name, active, (role, label, states, actions)) =
            tokio::join!(a11y.name(&app), in_front, async {
                tokio::join!(
                    a11y.role(&hint),
                    a11y.name(&hint),
                    a11y.states(&hint),
                    // No Action interface: an error, so no actions.
                    a11y.actions(&hint)
                )
            });
        let st = states?;
        if !active || !atspi::has(st, atspi::FOCUSED) || role.is_empty() || name.is_empty() {
            return None;
        }
        let info = Info {
            role,
            name: label,
            states: atspi::state_labels(Some(st)),
            showing: atspi::has(st, atspi::SHOWING),
            actions,
            text: None,
            children: Vec::new(),
            operable: false,
        };
        Some(Focused {
            id: hint,
            info,
            app: name,
        })
    }

    async fn is_focused(&self, id: &Obj) -> bool {
        self.a11y.has_state(id, atspi::FOCUSED).await
    }

    async fn try_focus(&self, id: &Obj) -> R<Option<String>> {
        // Qt only honours GrabFocus when the element exposes a "Set Focus"
        // action, so try both before the caller falls back to a click.
        let (ifaces, actions, role) = tokio::join!(
            self.a11y.interfaces(id),
            self.a11y.actions(id),
            self.a11y.role(id)
        );
        if !ifaces.iter().any(|i| i == "org.a11y.atspi.Component") {
            return Err("element cannot take focus".into());
        }
        let wait = FOCUS_POLL;
        if self.focus_should_try(id, &role, FocusMethod::GrabFocus) {
            let ok = self.a11y.grab_focus(id).await && poll(wait, || self.is_focused(id)).await;
            self.focus_note(id, &role, FocusMethod::GrabFocus, ok);
            if ok {
                return Ok(Some("grab_focus".into()));
            }
        }
        let names: Vec<String> = actions
            .iter()
            .map(|n| n.to_lowercase().replace(' ', ""))
            .collect();
        if let Some(i) = names.iter().position(|n| n == "setfocus")
            && self.focus_should_try(id, &role, FocusMethod::SetFocusAction)
        {
            let _ = self.a11y.do_action(id, i as i32).await;
            let ok = poll(wait, || self.is_focused(id)).await;
            self.focus_note(id, &role, FocusMethod::SetFocusAction, ok);
            if ok {
                return Ok(Some("set focus action".into()));
            }
        }
        Ok(None)
    }

    async fn element_center(&self, id: &Obj) -> R<(i32, i32)> {
        self.element_centers(std::slice::from_ref(id))
            .await
            .pop()
            .unwrap_or_else(|| Err("element has no on-screen area".into()))
    }

    async fn element_centers(&self, ids: &[Obj]) -> Vec<R<(i32, i32)>> {
        let kwin = match &self.comp {
            Compositor::Kde(kwin) => kwin,
            Compositor::Hyprland(h) => {
                // One look at the window in front for all the elements.
                let aw = match h.active_window().await {
                    Ok(aw) => aw,
                    Err(e) => return ids.iter().map(|_| Err(e.clone())).collect(),
                };
                return futures_util::future::join_all(
                    ids.iter().map(|id| self.hypr_center(aw.as_ref(), id)),
                )
                .await;
            }
        };
        let a11y = &self.a11y;
        // AT-SPI extents are relative to the toplevel surface on Wayland,
        // so add the active window's client origin: one KWin script for
        // all the elements.
        let (origin, parts) = tokio::join!(
            kwin.active_window_origin(),
            futures_util::future::join_all(ids.iter().map(|id| async move {
                tokio::join!(self.frame_active(id), a11y.extents(id, atspi::COORD_WINDOW))
            }))
        );
        parts
            .into_iter()
            .map(|(frame_active, ext)| {
                if !frame_active {
                    return Err(NOT_ACTIVE.into());
                }
                let (x, y, w, h) = ext?;
                if w <= 0 || h <= 0 {
                    return Err("element has no on-screen area".into());
                }
                let (gx, gy) = origin.clone()?;
                Ok((
                    (gx + x as f64 + w as f64 / 2.0) as i32,
                    (gy + y as f64 + h as f64 / 2.0) as i32,
                ))
            })
            .collect()
    }

    fn check_combo(&self, combo: &str) -> R<()> {
        input::parse_combo(combo).map(|_| ())
    }

    async fn press_combo(&self, combo: &str) -> R<()> {
        self.input.press_combo(combo).await
    }

    async fn type_text(&self, text: &str) -> R<String> {
        if text.is_empty() {
            return Ok(String::new());
        }
        if matches!(self.comp, Compositor::Hyprland(_)) && which("wtype") {
            // wtype uses the virtual-keyboard protocol: Unicode-safe and
            // leaves the clipboard alone.
            for args in hyprland::wtype_commands(text) {
                let status = tokio::process::Command::new("wtype")
                    .args(&args)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .await
                    .map_err(|e| format!("wtype: {e}"))?;
                if !status.success() {
                    return Err(format!("wtype failed: {status}"));
                }
            }
            return Ok(String::new());
        }
        if matches!(self.comp, Compositor::Kde(_)) && self.insert_at_caret(text).await? {
            return Ok(String::new());
        }
        self.paste(text).await
    }

    async fn clipboard_get(&self, limit: usize) -> R<Clip> {
        let mimes = clipboard::mime_types().await?;
        Ok(match clipboard::classify(&mimes) {
            clipboard::Kind::Empty => Clip::Empty,
            clipboard::Kind::Text { secret: true } => Clip::Secret,
            clipboard::Kind::Text { secret: false } => {
                // A character is at most 4 bytes in UTF-8.
                let (bytes, more) = clipboard::read_text(limit.saturating_mul(4)).await?;
                let (text, cut) = clipboard::cap_chars(&bytes, limit);
                Clip::Text {
                    text,
                    truncated: cut || more,
                }
            }
            clipboard::Kind::Files | clipboard::Kind::Other(_) => Clip::Other(mimes),
        })
    }

    async fn clipboard_set(&self, text: &str) -> R<()> {
        set_clipboard(text).await
    }

    async fn pointer(&self, x: i64, y: i64, button: &str, clicks: i64, scroll: i64) -> R<()> {
        input::button_code(button)?;
        self.move_pointer(x, y).await?;
        self.input.buttons(button, clicks, scroll).await
    }

    async fn indicate(&self, label: &str) {
        // Busy (a screenshot waits for it to hide): skip rather than delay
        // the action.
        let Ok(mut ind) = self.indicator.try_lock() else {
            return;
        };
        if ind.spawn() {
            ind.send(&format!("show {}", label.replace('\n', " ")), true)
                .await;
        }
    }

    async fn move_pointer(&self, x: i64, y: i64) -> R<()> {
        if let Ok(mut ind) = self.indicator.try_lock()
            && ind.child.is_some()
        {
            ind.send(&format!("point {x} {y}"), true).await;
        }
        match &self.comp {
            Compositor::Kde(kwin) => {
                let (x0, y0, x1, y1) = kwin.screen_box().await;
                let ax = ((x - x0 as i64) * input::ABS_MAX as i64 / (x1 - x0).max(1) as i64) as i32;
                let ay = ((y - y0 as i64) * input::ABS_MAX as i64 / (y1 - y0).max(1) as i64) as i32;
                self.input.move_abs(ax, ay).await?;
            }
            Compositor::Hyprland(h) => {
                // Hyprland places the cursor exactly; uinput only supplies
                // buttons. Its main loop handles the request in full (moves
                // the cursor, sends the motion to the surface under it)
                // before it replies, and the same loop reads the uinput
                // buttons through libinput: a button emitted after the reply
                // is handled after the move. The position read back confirms
                // the move took (it is clamped outside every monitor); only
                // an unconfirmed move gets the old fixed wait.
                h.dispatch(&format!("movecursor {x} {y}")).await?;
                if !h
                    .cursor_pos()
                    .await
                    .is_ok_and(|(cx, cy)| (cx - x).abs() <= 1 && (cy - y).abs() <= 1)
                {
                    self.input.settle().await;
                }
            }
        }
        Ok(())
    }

    async fn button(&self, button: &str, down: bool) -> R<()> {
        self.input.button(button, down).await
    }

    async fn key(&self, key: &str, down: bool) -> R<()> {
        self.input.key(key, down).await
    }

    async fn cursor_position(&self) -> R<(i64, i64)> {
        match &self.comp {
            Compositor::Kde(kwin) => kwin.cursor_pos().await,
            Compositor::Hyprland(h) => h.cursor_pos().await,
        }
    }

    async fn activate_window(&self, query: &str, allowed: &[String]) -> R<Option<Activated>> {
        match &self.comp {
            Compositor::Kde(kwin) => {
                let apps = self.apps_with_pids().await;
                let allow = Self::allow_of(allowed, &apps);
                let Some(raised) = kwin.activate(query, &allow).await? else {
                    return Err(format!("no window of an allowed app matches '{query}'"));
                };
                // KWin switches at once; the toolkit reports ACTIVE a moment later.
                if apps.iter().any(|(pid, ..)| *pid == Some(raised.pid)) {
                    return Ok(self.kde_wait_active(&raised, &apps).await);
                }
                // Its process has no AT-SPI app under that pid: by title.
                poll(ACTIVATE_WAIT, || self.kde_frame_active(&raised.caption)).await;
            }
            Compositor::Hyprland(h) => {
                let q = query.to_lowercase();
                let (clients, apps) = tokio::join!(h.json("clients"), self.apps_with_pids());
                let clients = clients?;
                let names = Self::names_by_pid(&apps);
                let found = clients.as_array().into_iter().flatten().find(|c| {
                    if !c["mapped"].as_bool().unwrap_or(true)
                        || c["hidden"].as_bool().unwrap_or(false)
                    {
                        return false;
                    }
                    let app = Self::client_app_name(&names, c);
                    if !allowed.contains(&app) {
                        return false;
                    }
                    let title = c["title"].as_str().unwrap_or("").to_lowercase();
                    let class = c["class"].as_str().unwrap_or("").to_lowercase();
                    title.contains(&q) || app.contains(&q) || class.contains(&q)
                });
                let Some(c) = found else {
                    return Err(format!("no window of an allowed app matches '{query}'"));
                };
                let addr = c["address"].as_str().unwrap_or("");
                h.dispatch(&format!("focuswindow address:{addr}")).await?;
                // Hyprland switches focus while it handles the request, so
                // the first look finds the client in front (measured: every
                // time, 0.05 ms for both requests); the poll only covers a
                // compositor that switches later. The toolkit's own ACTIVE
                // state may lag, but input goes where Hyprland says.
                let in_front = poll(ACTIVATE_WAIT, || async {
                    h.active_window()
                        .await
                        .ok()
                        .flatten()
                        .is_some_and(|aw| aw["address"].as_str() == Some(addr))
                })
                .await;
                if in_front {
                    return Ok(Some(self.hypr_activated(&apps, &names, c).await));
                }
            }
        }
        Ok(None)
    }

    async fn windows(&self, query: &str, allowed: &[String]) -> R<Vec<WindowGeo>> {
        match &self.comp {
            Compositor::Kde(kwin) => {
                let mut list = kwin.windows(query, &self.kwin_allow(allowed).await).await?;
                with_terminals(&mut list).await;
                Ok(list)
            }
            Compositor::Hyprland(h) => {
                let (_, clients) = self.hypr_clients(h, query, allowed).await?;
                let mut list: Vec<WindowGeo> = clients.into_iter().map(|c| c.geo).collect();
                with_terminals(&mut list).await;
                Ok(list)
            }
        }
    }

    async fn windows_quick(&self, query: &str, allowed: &[String]) -> R<Vec<WindowGeo>> {
        match &self.comp {
            Compositor::Kde(kwin) => kwin.windows(query, &self.kwin_allow(allowed).await).await,
            Compositor::Hyprland(h) => Ok(self
                .hypr_clients(h, query, allowed)
                .await?
                .1
                .into_iter()
                .map(|c| c.geo)
                .collect()),
        }
    }

    async fn screens_and_windows(
        &self,
        query: &str,
        allowed: &[String],
    ) -> (Vec<Screen>, R<Vec<WindowGeo>>) {
        match &self.comp {
            Compositor::Kde(kwin) => {
                let allow = self.kwin_allow(allowed).await;
                match kwin.screens_and_windows(query, &allow).await {
                    Ok((screens, mut list)) => {
                        with_terminals(&mut list).await;
                        (screens, Ok(list))
                    }
                    Err(e) => (Vec::new(), Err(e)),
                }
            }
            Compositor::Hyprland(h) => match self.hypr_clients(h, query, allowed).await {
                Ok((snap, clients)) => {
                    let mut list: Vec<WindowGeo> = clients.into_iter().map(|c| c.geo).collect();
                    with_terminals(&mut list).await;
                    (hyprland::screens_of(&snap.monitors), Ok(list))
                }
                Err(e) => (Vec::new(), Err(e)),
            },
        }
    }

    async fn set_window(
        &self,
        query: &str,
        allowed: &[String],
        change: &WindowChange,
    ) -> R<WindowGeo> {
        match &self.comp {
            Compositor::Kde(kwin) => kwin
                .set_window(query, &self.kwin_allow(allowed).await, change)
                .await?
                .ok_or_else(|| format!("no window of an allowed app matches '{query}'")),
            Compositor::Hyprland(h) => {
                let (snap, clients) = self.hypr_clients(h, query, allowed).await?;
                // The active window when it matches, else the first.
                let c = clients
                    .iter()
                    .find(|c| c.geo.active)
                    .or(clients.first())
                    .ok_or_else(|| format!("no window of an allowed app matches '{query}'"))?;
                let back = hyprland::monitor_workspace(&snap.monitors, c.monitor);
                h.dispatch_all(&hyprland::change_commands(c, change, back)?)
                    .await?;
                // Hyprland applies geometry at once; the caller waits for the
                // app (and the state) by re-reading `windows`.
                let snap = h.snapshot().await?;
                hyprland::clients_of(&snap)
                    .into_iter()
                    .find(|n| n.address == c.address)
                    .map(|n| n.geo)
                    .ok_or_else(|| format!("the window '{}' closed", c.geo.title))
            }
        }
    }

    async fn launch(
        &self,
        app: &str,
        args: &[String],
        allowed: &[String],
        wait: Duration,
    ) -> R<Launched> {
        let (name, argv) = (app.to_string(), args.to_vec());
        let r = tokio::task::spawn_blocking(move || launch::resolve(&name, &argv))
            .await
            .map_err(|e| e.to_string())??;
        // Windows already open, to tell a new one of a running instance.
        let before = self.all_windows(allowed).await.unwrap_or_default();
        let (pid, exit) = launch::spawn(&r)?;
        let mut out = Launched {
            command: launch::describe(&r),
            pid,
            ..Launched::default()
        };
        if wait.is_zero() {
            out.note = "not waiting for its window (wait_ms=0)".into();
            return Ok(out);
        }
        let start = std::time::Instant::now();
        let hints = launch::arg_hints(args);
        let mut tree = std::collections::HashSet::from([pid]);
        // Rounds since the tree last grew.
        let mut stable = 0;
        let mut ended: Option<std::time::Instant> = None;
        loop {
            let scanned = launch::tree_scan_due(stable);
            if scanned {
                let grown = descendants_of(&tree).await;
                stable = if grown.len() > tree.len() {
                    0
                } else {
                    stable + 1
                };
                tree = grown;
            } else {
                stable += 1;
            }
            // The allowlist's pids are rebuilt each round: the new app
            // joins AT-SPI after it starts.
            let now = self.all_windows(allowed).await?;
            let status = *exit.lock().unwrap();
            if status.is_some() && ended.is_none() {
                ended = Some(std::time::Instant::now());
            }
            let exited = status.zip(ended).map(|(s, t)| (s.success(), t.elapsed()));
            let stage = launch::stage(start.elapsed(), exited);
            let mut picked = launch::pick_window(app, &before, &now, &tree, stage, &hints);
            if picked
                .as_ref()
                .is_some_and(|(_, how)| *how != launch::Found::Own)
                && !scanned
            {
                // A skipped scan must not call the started app's own window
                // the running instance's.
                tree = descendants_of(&tree).await;
                stable = 0;
                picked = launch::pick_window(app, &before, &now, &tree, stage, &hints);
            }
            if let Some((w, how)) = picked {
                let mut found = vec![w.clone()];
                with_terminals(&mut found).await;
                out.window = found.pop();
                out.note = match how {
                    launch::Found::Own => "",
                    launch::Found::New => "opened by the running instance",
                    launch::Found::Existing => "handed to the running instance",
                }
                .into();
                return Ok(out);
            }
            if let (Some(s), Some((false, since))) = (status, exited)
                && since >= launch::HANDOFF_GRACE
            {
                return Err(format!(
                    "{} (pid {pid}) exited with {s} without opening a window",
                    out.command
                ));
            }
            if start.elapsed() >= wait {
                return Ok(out);
            }
            tokio::time::sleep(launch::poll_interval(start.elapsed())).await;
        }
    }

    async fn screens(&self) -> Vec<Screen> {
        match &self.comp {
            Compositor::Kde(kwin) => kwin.screens().await.unwrap_or_default(),
            Compositor::Hyprland(h) => h
                .json("monitors")
                .await
                .map(|m| hyprland::screens_of(&m))
                .unwrap_or_default(),
        }
    }

    async fn active_file_dialog(&self) -> R<FileDialog<Obj>> {
        let Compositor::Kde(kwin) = &self.comp else {
            return Err(NO_FILE_DIALOG.into());
        };
        let a11y = &self.a11y;
        let (active, apps) = tokio::join!(kwin.active_parent(), a11y.apps_with_windows());
        let active = active?.ok_or("no active window")?;
        // The accessible window KWin has in front: the ACTIVE one of the
        // process KWin names, so the parent it reports is this window's.
        let pid = active["pid"].as_i64();
        let mut found = None;
        'apps: for (app, name, windows) in apps {
            if self.pid(&app.bus).await.map(i64::from) != pid {
                continue;
            }
            for w in windows {
                if a11y.has_state(&w, atspi::ACTIVE).await {
                    found = Some((name, w));
                    break 'apps;
                }
            }
        }
        let (app, window) = found.ok_or(NOT_A_FILE_DIALOG)?;
        if a11y.role(&window).await != "dialog" {
            return Err(NOT_A_FILE_DIALOG.into());
        }
        let (field, accept) = self.file_dialog_parts(&window).await;
        let field = field.ok_or(NOT_A_FILE_DIALOG)?;
        let parent = if active["parent"].is_object() {
            let names = self.app_names_by_pid().await;
            Some(Self::client_app_name(&names, &active["parent"]))
        } else {
            None
        };
        let titles = vec![
            active["caption"].as_str().unwrap_or("").trim().to_string(),
            a11y.name(&window).await.trim().to_string(),
        ];
        Ok(FileDialog {
            window,
            app: app.to_lowercase(),
            titles,
            parent,
            field,
            accept,
        })
    }

    async fn doctor(&self) -> Vec<Check> {
        let mut out = Vec::new();
        let env = |k: &str| std::env::var(k).unwrap_or_default();
        let wayland = env("WAYLAND_DISPLAY");
        out.push(Check {
            name: "session",
            ok: !wayland.is_empty(),
            detail: format!(
                "{} on {} (WAYLAND_DISPLAY={}, XDG_SESSION_TYPE={})",
                match &self.comp {
                    Compositor::Kde(_) => "KDE Plasma",
                    Compositor::Hyprland(_) => "Hyprland",
                },
                if wayland.is_empty() {
                    "no Wayland display"
                } else {
                    "Wayland"
                },
                if wayland.is_empty() {
                    "unset"
                } else {
                    &wayland
                },
                env("XDG_SESSION_TYPE"),
            ),
        });
        let apps = self.a11y.applications().await;
        out.push(Check {
            name: "accessibility (AT-SPI)",
            ok: !apps.is_empty(),
            detail: if apps.is_empty() {
                "no applications registered on the AT-SPI bus; is at-spi2-core running?".into()
            } else {
                format!("{} applications registered", apps.len())
            },
        });
        let input = self.input.clone();
        let (kbd, ptr) = tokio::task::spawn_blocking(move || input.check())
            .await
            .unwrap_or_else(|e| (Err(e.to_string()), Err(e.to_string())));
        for (name, r) in [
            ("virtual keyboard (uinput)", kbd),
            ("virtual pointer (uinput)", ptr),
        ] {
            out.push(Check {
                name,
                ok: r.is_ok(),
                detail: r.err().unwrap_or_else(|| "ready".into()),
            });
        }
        match &self.comp {
            Compositor::Kde(kwin) => {
                let script = kwin.query("1").await;
                out.push(Check {
                    name: "KWin scripting",
                    ok: matches!(&script, Ok(Some(v)) if v == "1"),
                    detail: match script {
                        Ok(Some(_)) => "window queries and activation work".into(),
                        Ok(None) => "script ran but did not answer".into(),
                        Err(e) => e,
                    },
                });
                let screens = kwin.screens().await.unwrap_or_default();
                out.push(Check {
                    name: "screens",
                    ok: !screens.is_empty(),
                    detail: screens_line(&screens),
                });
                let fallback = self.shot_unauthorized.load(Ordering::Relaxed);
                out.push(Check {
                    name: "screenshot",
                    ok: !fallback || which("spectacle"),
                    detail: if fallback {
                        "KWin refused ScreenShot2; using spectacle (slower)".into()
                    } else {
                        "KWin ScreenShot2 (spectacle as fallback)".into()
                    },
                });
            }
            Compositor::Hyprland(h) => {
                let ok = h.json("version").await;
                out.push(Check {
                    name: "hyprctl",
                    ok: ok.is_ok(),
                    detail: ok.err().unwrap_or_else(|| "reachable".into()),
                });
                let screens = self.screens().await;
                out.push(Check {
                    name: "screens",
                    ok: !screens.is_empty(),
                    detail: screens_line(&screens),
                });
            }
        }
        out
    }

    async fn screenshot(&self) -> Option<Shot> {
        self.hide_indicator().await;
        let area;
        let mut cmd = match &self.comp {
            Compositor::Kde(kwin) => {
                // One script run gives the frame and the window's handle, so
                // the capture is of the window the area belongs to.
                let frame = kwin.active_frame().await;
                area = frame.as_ref().map(|f| f.area);
                if !self.shot_unauthorized.load(Ordering::Relaxed) {
                    let handle = frame.as_ref().and_then(|f| f.handle.as_deref());
                    let mut captured = kwin.capture_window(handle).await;
                    // The window closed meanwhile, or the handle was refused:
                    // the active window still beats spectacle.
                    if handle.is_some()
                        && captured
                            .as_ref()
                            .is_err_and(|e| !e.starts_with(kwin::SHOT_NOT_AUTHORIZED))
                    {
                        captured = kwin.capture_window(None).await;
                    }
                    match captured {
                        Ok(raw) => {
                            let (path, file) = temp_png()?;
                            drop(file);
                            return Some(Shot {
                                path: path.display().to_string(),
                                pixels: Some(Box::new(move || image::to_pixels(&raw))),
                                area,
                            });
                        }
                        // Older KWin without the desktop-file grant: stop asking.
                        Err(e) if e.starts_with(kwin::SHOT_NOT_AUTHORIZED) => {
                            self.shot_unauthorized.store(true, Ordering::Relaxed);
                        }
                        Err(_) => {}
                    }
                }
                // spectacle is authorised by its own desktop file.
                let mut c = tokio::process::Command::new("spectacle");
                c.args(["-b", "-n", "-a", "-o"]);
                c
            }
            Compositor::Hyprland(h) => {
                let aw = h.active_window().await.ok().flatten()?;
                area = (|| {
                    Some((
                        aw["at"][0].as_f64()?,
                        aw["at"][1].as_f64()?,
                        aw["size"][0].as_f64()?,
                        aw["size"][1].as_f64()?,
                    ))
                })();
                if !which("grim") {
                    return None;
                }
                let geo = format!(
                    "{},{} {}x{}",
                    aw["at"][0], aw["at"][1], aw["size"][0], aw["size"][1]
                );
                let mut c = tokio::process::Command::new("grim");
                c.args(["-g", &geo]);
                c
            }
        };
        let (path, file) = temp_png()?;
        drop(file);
        let _ = cmd.arg(&path).output().await;
        keep_if_written(path).map(|path| Shot {
            path,
            pixels: None,
            area,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn one_unfocusable_element_does_not_turn_focus_off_for_the_app() {
        let g = FocusMethod::GrabFocus;
        let mut rec = FocusRecord::default();
        assert!(rec.should_try(":1.5", "label", g));
        rec.note(":1.5", "label", g, false);
        // Other roles in the app, and a second try on the role, still go.
        assert!(rec.should_try(":1.5", "push button", g));
        assert!(rec.should_try(":1.5", "label", g));
        rec.note(":1.5", "label", g, false);
        // Failed twice in a row on labels there: skipped, for labels only,
        // in that app only, for that method only.
        assert!(!rec.should_try(":1.5", "label", g));
        assert!(rec.should_try(":1.5", "text", g));
        assert!(rec.should_try(":1.9", "label", g));
        assert!(rec.should_try(":1.5", "label", FocusMethod::SetFocusAction));
    }

    #[test]
    fn a_skipped_focus_method_is_retried_and_a_success_restores_it() {
        let g = FocusMethod::GrabFocus;
        let mut rec = FocusRecord::default();
        rec.note(":1.5", "text", g, false);
        rec.note(":1.5", "text", g, false);
        for _ in 0..FOCUS_RETRY_AFTER {
            assert!(!rec.should_try(":1.5", "text", g));
        }
        // The probe after the skips; it works, so the method is back.
        assert!(rec.should_try(":1.5", "text", g));
        rec.note(":1.5", "text", g, true);
        assert!(rec.should_try(":1.5", "text", g));
        rec.note(":1.5", "text", g, false);
        assert!(
            rec.should_try(":1.5", "text", g),
            "one failure after a success"
        );
        // A failed probe keeps it skipped.
        rec.note(":1.5", "text", g, false);
        assert!(!rec.should_try(":1.5", "text", g));
    }

    #[test]
    fn file_dialog_parts_are_matched_by_qt_ids() {
        // As seen in xdg-desktop-portal-kde and kdialog (KF6).
        let base = "QApplication.FileDialog.KFileWidget.QSplitter.QWidget";
        assert!(is_file_name_field(&format!(
            "{base}.KUrlComboBox.KLineEdit"
        )));
        assert!(is_file_name_field(
            "QApplication.KDEPlatformFileDialog.KFileWidget.QSplitter.QWidget.KUrlComboBox.KLineEdit"
        ));
        // The URL navigator's edit, the filter combo, a search field.
        assert!(!is_file_name_field(&format!(
            "{base}.QWidget.KUrlNavigator.KUrlComboBox.KLineEdit"
        )));
        assert!(!is_file_name_field(&format!(
            "{base}.KFileFilterCombo.KLineEdit"
        )));
        assert!(!is_file_name_field(&format!("{base}.QWidget.QLineEdit")));
        assert!(!is_file_name_field(
            "QApplication.MainWindow.KUrlComboBox.KLineEdit"
        ));
        assert!(is_file_dialog_button(&format!("{base}.QPushButton")));
        assert!(!is_file_dialog_button("QApplication.Settings.QPushButton"));
    }

    /// An indicator whose child is `program` (with nothing to read), already
    /// exited.
    async fn exited_indicator(program: &str) -> IndicatorProc {
        use tokio::io::AsyncBufReadExt;
        let mut child = tokio::process::Command::new(program)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let (stdin, stdout) = (child.stdin.take().unwrap(), child.stdout.take().unwrap());
        let mut lines = tokio::io::BufReader::new(stdout).lines();
        assert!(lines.next_line().await.unwrap().is_none());
        // Wait for the exit without reaping it, as after an idle exit.
        while std::fs::read_to_string(format!("/proc/{}/stat", child.id().unwrap()))
            .is_ok_and(|st| !st.contains(") Z "))
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        IndicatorProc {
            child: Some((child, stdin, lines)),
            off: false,
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn indicator_may_come_back_after_its_idle_exit_only() {
        // Exit code 0 (idle exit): dropped, but not turned off for good.
        let mut ind = exited_indicator("true").await;
        assert!(!ind.send("hide", false).await);
        assert!(ind.child.is_none() && !ind.off);
        // A child that failed keeps the indicator off.
        let mut ind = exited_indicator("false").await;
        assert!(!ind.send("show x", true).await);
        assert!(ind.child.is_none() && ind.off);
        assert!(!ind.spawn());
    }

    #[test]
    fn raised_window_is_found_by_title_or_as_the_only_one() {
        let w = |n: &str, a: bool| (n.to_string(), a);
        // Kate's frame name spaces the dash differently from KWin's caption.
        let seen = [w("a.txt  — Kate", true), w("b.txt  — Kate", true)];
        assert_eq!(active_index("b.txt — Kate", &seen), Some(1));
        // The window that was in front still says ACTIVE: not yet.
        let seen = [w("a.txt — Kate", true), w("b.txt — Kate", false)];
        assert_eq!(active_index("b.txt — Kate", &seen), None);
        // One window: its ACTIVE state is enough, whatever its name.
        assert_eq!(active_index("Mozilla Firefox", &[w("x", true)]), Some(0));
        assert_eq!(active_index("Mozilla Firefox", &[w("x", false)]), None);
        assert_eq!(active_index("x", &[]), None);
    }

    #[test]
    fn client_app_name_prefers_atspi_name_by_pid() {
        let names = HashMap::from([(4242, "kate".to_string()), (6161, "Firefox".to_string())]);
        let kate = json!({"pid": 4242, "class": "org.kde.kate"});
        let firefox = json!({"pid": 6161, "class": "firefox"});
        // No accessibility: fall back to the window class.
        let xterm = json!({"pid": 5151, "class": "XTerm"});
        assert_eq!(LinuxBackend::client_app_name(&names, &kate), "kate");
        assert_eq!(LinuxBackend::client_app_name(&names, &firefox), "firefox");
        assert_eq!(LinuxBackend::client_app_name(&names, &xterm), "xterm");
        assert_eq!(LinuxBackend::client_app_name(&names, &json!({})), "");
    }

    #[test]
    fn names_by_pid_keeps_the_first_app_of_a_process() {
        let app = |bus: &str| Obj::new(bus, OwnedObjectPath::try_from(APP_ROOT).unwrap());
        let apps = vec![
            (Some(7), app(":1.1"), "kate".to_string(), Vec::new()),
            (Some(7), app(":1.2"), "other".to_string(), Vec::new()),
            (None, app(":1.3"), "sandboxed".to_string(), Vec::new()),
        ];
        let names = LinuxBackend::names_by_pid(&apps);
        assert_eq!(names, HashMap::from([(7, "kate".to_string())]));
    }

    /// Read-only timing of the reads an effect fingerprint makes, on the
    /// live desktop: `cargo test --release -- --ignored --nocapture fingerprint_timing`.
    #[test]
    #[ignore]
    fn fingerprint_timing() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let b = LinuxBackend::new().await.unwrap();
            let time = |name: &str, t: std::time::Instant, n: u32| {
                let ms = t.elapsed().as_secs_f64() * 1000.0 / f64::from(n);
                println!("{name}: {ms:.2} ms");
            };
            let n = 20;
            let _ = b.list_apps().await;
            let t = std::time::Instant::now();
            for _ in 0..n {
                std::hint::black_box(b.list_apps().await);
            }
            time("list_apps", t, n);
            let t = std::time::Instant::now();
            for _ in 0..n {
                std::hint::black_box(b.active_app().await);
            }
            time("active_app", t, n);
            let t = std::time::Instant::now();
            for _ in 0..n {
                std::hint::black_box(b.focused().await);
            }
            time("focused", t, n);
            let t = std::time::Instant::now();
            for _ in 0..n {
                std::hint::black_box(tokio::join!(b.list_apps(), b.focused()));
            }
            time("list_apps + focused", t, n);
            let apps = b.list_apps().await;
            println!(
                "{} apps, {} windows",
                apps.len(),
                apps.iter().map(|a| a.windows.len()).sum::<usize>()
            );
        });
    }
}
