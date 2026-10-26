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
    AppEntry, Backend, Cache, Check, Clip, FileDialog, Info, Launched, NO_FILE_DIALOG,
    NO_WINDOW_CONTROL, R, Screen, Shot, WindowChange, WindowEntry, WindowGeo,
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
/// How old the active-app check before `type_text` may be for the
/// accessibility insert to trust it.
const INSERT_ACTIVE_AGE: Duration = Duration::from_secs(1);
/// How long the in-process clipboard may take to become the selection.
const CLIPBOARD_WAIT: Duration = Duration::from_millis(250);
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

#[derive(Clone, Copy, Debug, Default)]
struct Tried {
    failed: bool,
    worked: bool,
}

impl Tried {
    /// Skip a method that failed in this app and never worked there (Qt
    /// widgets mostly ignore both, and each try costs a poll).
    fn skip(self) -> bool {
        self.failed && !self.worked
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
    /// What each `try_focus` method did so far, per AT-SPI bus name.
    focus_record: Mutex<HashMap<(String, FocusMethod), Tried>>,
    /// KWin refused ScreenShot2; use spectacle from now on.
    shot_unauthorized: AtomicBool,
    indicator: tokio::sync::Mutex<IndicatorProc>,
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

    /// Send one command line; a child that cannot take it is dropped and
    /// the indicator stays off.
    async fn send(&mut self, line: &str) -> bool {
        let Some((_, stdin, _)) = self.child.as_mut() else {
            return false;
        };
        let sent = stdin
            .write_all(format!("{line}\n").as_bytes())
            .await
            .is_ok()
            && stdin.flush().await.is_ok();
        if !sent {
            self.child = None;
            self.off = true;
        }
        sent
    }
}

impl LinuxBackend {
    /// Clear the indicator off the screen before a capture, waiting for it
    /// to confirm (briefly; a stuck indicator is dropped).
    async fn hide_indicator(&self) {
        let mut ind = self.indicator.lock().await;
        if ind.child.is_none() || !ind.send("hide").await {
            return;
        }
        let answered = match ind.child.as_mut() {
            Some((_, _, lines)) => {
                tokio::time::timeout(INDICATOR_HIDE_WAIT, lines.next_line()).await
            }
            None => return,
        };
        if !matches!(answered, Ok(Ok(Some(ref l))) if l == "hidden") {
            ind.child = None;
            ind.off = true;
        }
    }
}

/// Fill in the terminals of each window from /proc (off the event loop).
async fn with_terminals(list: &mut [WindowGeo]) {
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

/// How often `launch` looks for the new window.
const LAUNCH_POLL: Duration = Duration::from_millis(150);
/// How long `launch` waits for a new window before taking the app's changed
/// active window, or after the started process ended its active one
/// (handed to a running instance).
const HANDOFF_GRACE: Duration = Duration::from_millis(1500);

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

/// Own the clipboard in-process through the Wayland data-control protocol;
/// a background thread serves pastes until another client takes it over.
/// Returns once the compositor has the new selection, so a paste sent
/// afterwards cannot get the previous one. `secret` adds KDE's password
/// hint (kept when putting back a password the user had copied).
fn own_clipboard(text: &[u8], secret: bool) -> R<()> {
    use std::io::Read;
    use wl_clipboard_rs::{copy, paste};
    let mut options = copy::Options::new();
    options.sensitive(secret);
    options
        .copy(copy::Source::Bytes(text.into()), copy::MimeType::Text)
        .map_err(|e| format!("clipboard: {e}"))?;
    // The selection request is flushed by the serving thread; read the
    // clipboard back until it is ours (if it held the same text already,
    // a paste gives the right text either way).
    let start = std::time::Instant::now();
    loop {
        let got = paste::get_contents(
            paste::ClipboardType::Regular,
            paste::Seat::Unspecified,
            paste::MimeType::Text,
        );
        let mut buf = Vec::new();
        if let Ok((mut pipe, _)) = got
            && pipe.read_to_end(&mut buf).is_ok()
            && buf == text
        {
            return Ok(());
        }
        if start.elapsed() >= CLIPBOARD_WAIT {
            return Err("clipboard: selection did not take".into());
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Make `text` the clipboard again after a paste; a note when that failed.
async fn put_back(text: Vec<u8>, secret: bool) -> String {
    match tokio::task::spawn_blocking(move || own_clipboard(&text, secret)).await {
        Ok(Ok(())) => String::new(),
        Ok(Err(e)) => format!("; the clipboard could not be put back ({e})"),
        Err(e) => format!("; the clipboard could not be put back ({e})"),
    }
}

/// Paste instead of typing key by key: works with any layout and Unicode.
/// In-process when the compositor offers data-control, else through wl-copy.
async fn set_clipboard(text: &str) -> R<()> {
    let owned = text.as_bytes().to_vec();
    let done = tokio::task::spawn_blocking(move || own_clipboard(&owned, false)).await;
    if matches!(done, Ok(Ok(()))) {
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
        if absolute {
            // Without it `type_text` falls back to pasting more often.
            let _ = a11y.track_focus().await;
        }
        Ok(LinuxBackend {
            session,
            a11y,
            comp,
            input: Arc::new(Input::new(absolute)),
            pids: Mutex::new(HashMap::new()),
            toolkits: Mutex::new(HashMap::new()),
            focus_record: Mutex::new(HashMap::new()),
            shot_unauthorized: AtomicBool::new(false),
            indicator: tokio::sync::Mutex::new(IndicatorProc::default()),
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

    /// The allowlist plus pids of AT-SPI apps sorted against it, for the
    /// KWin window scripts.
    async fn kwin_allow(&self, allowed: &[String]) -> kwin::Allow {
        let mut apps = Vec::new();
        for (app, name, _) in self.a11y.apps_with_windows().await {
            if let Some(pid) = self.pid(&app.bus).await {
                apps.push((pid, name));
            }
        }
        kwin::Allow::new(allowed, &apps)
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

    // ------------------------------------------------------------ focus

    fn focus_tried(&self, id: &Obj, m: FocusMethod) -> Tried {
        self.focus_record
            .lock()
            .unwrap()
            .get(&(id.bus.to_string(), m))
            .copied()
            .unwrap_or_default()
    }

    fn focus_note(&self, id: &Obj, m: FocusMethod, worked: bool) {
        let mut rec = self.focus_record.lock().unwrap();
        let t = rec.entry((id.bus.to_string(), m)).or_default();
        if worked {
            t.worked = true;
        } else {
            t.failed = true;
        }
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

    async fn focused(&self) -> Option<Obj> {
        // Focus events keep the hint current; check it still holds focus.
        // Only report it inside the app in front, which the caller checked
        // is allowed: a stale hint from another app must not leak or get an id.
        let a11y = &self.a11y;
        let hint = a11y.focus_hint()?;
        self.active_app().await;
        if a11y.recent_active_bus(INSERT_ACTIVE_AGE).as_deref() != Some(&*hint.bus) {
            return None;
        }
        a11y.has_state(&hint, atspi::FOCUSED).await.then_some(hint)
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
        let wait = FOCUS_POLL;
        if !self.focus_tried(id, FocusMethod::GrabFocus).skip() {
            let ok = self.a11y.grab_focus(id).await && poll(wait, || self.is_focused(id)).await;
            self.focus_note(id, FocusMethod::GrabFocus, ok);
            if ok {
                return Ok(Some("grab_focus".into()));
            }
        }
        let names: Vec<String> = actions
            .iter()
            .map(|n| n.to_lowercase().replace(' ', ""))
            .collect();
        if let Some(i) = names.iter().position(|n| n == "setfocus")
            && !self.focus_tried(id, FocusMethod::SetFocusAction).skip()
        {
            let _ = self.a11y.do_action(id, i as i32).await;
            let ok = poll(wait, || self.is_focused(id)).await;
            self.focus_note(id, FocusMethod::SetFocusAction, ok);
            if ok {
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
                    kwin.active_window_origin()
                );
                if !frame_active {
                    return Err(NOT_ACTIVE.into());
                }
                let (x, y, w, h) = ext?;
                if w <= 0 || h <= 0 {
                    return Err(no_area());
                }
                let (gx, gy) = origin?;
                Ok((
                    (gx + x as f64 + w as f64 / 2.0) as i32,
                    (gy + y as f64 + h as f64 / 2.0) as i32,
                ))
            }
            Compositor::Hyprland(h) => {
                let (aw, frame) = tokio::join!(h.active_window(), a11y.frame_of(id));
                let (Some(aw), Some(frame)) = (aw?, frame) else {
                    return Err(NOT_ACTIVE.into());
                };
                if !self.same_window(&frame, &aw).await {
                    return Err(NOT_ACTIVE.into());
                }
                if aw["xwayland"].as_bool().unwrap_or(false) {
                    // X11 clients report real screen coordinates.
                    let (x, y, w, hh) = a11y.extents(id, atspi::COORD_SCREEN).await?;
                    if w <= 0 || hh <= 0 {
                        return Err(no_area());
                    }
                    return Ok((
                        (x as f64 + w as f64 / 2.0) as i32,
                        (y as f64 + hh as f64 / 2.0) as i32,
                    ));
                }
                // Wayland clients report surface-relative extents; add the window origin.
                let (x, y, w, hh) = a11y.extents(id, atspi::COORD_WINDOW).await?;
                if w <= 0 || hh <= 0 {
                    return Err(no_area());
                }
                let ox = aw["at"][0].as_f64().unwrap_or(0.0);
                let oy = aw["at"][1].as_f64().unwrap_or(0.0);
                Ok((
                    (ox + x as f64 + w as f64 / 2.0) as i32,
                    (oy + y as f64 + hh as f64 / 2.0) as i32,
                ))
            }
        }
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
        let mut ind = self.indicator.lock().await;
        if ind.spawn() {
            ind.send(&format!("show {}", label.replace('\n', " ")))
                .await;
        }
    }

    async fn move_pointer(&self, x: i64, y: i64) -> R<()> {
        {
            let mut ind = self.indicator.lock().await;
            if ind.child.is_some() {
                ind.send(&format!("point {x} {y}")).await;
            }
        }
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
        Ok(())
    }

    async fn button(&self, button: &str, down: bool) -> R<()> {
        self.input.button(button, down).await
    }

    async fn activate_window(&self, query: &str, allowed: &[String]) -> R<()> {
        match &self.comp {
            Compositor::Kde(kwin) => {
                let allow = self.kwin_allow(allowed).await;
                let caption = kwin.activate(query, &allow).await?;
                if caption.is_empty() {
                    return Err(format!("no window of an allowed app matches '{query}'"));
                }
                // KWin switches at once; the toolkit reports ACTIVE a moment later.
                poll(ACTIVATE_WAIT, || self.kde_frame_active(&caption)).await;
            }
            Compositor::Hyprland(h) => {
                let q = query.to_lowercase();
                let (clients, names) = tokio::join!(h.json("clients"), self.app_names_by_pid());
                let clients = clients?;
                for c in clients.as_array().into_iter().flatten() {
                    if !c["mapped"].as_bool().unwrap_or(true)
                        || c["hidden"].as_bool().unwrap_or(false)
                    {
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
                            h.active_window()
                                .await
                                .ok()
                                .flatten()
                                .is_some_and(|aw| aw["address"].as_str() == Some(addr.as_str()))
                        })
                        .await;
                        return Ok(());
                    }
                }
            }
        }
        Ok(())
    }

    async fn windows(&self, query: &str, allowed: &[String]) -> R<Vec<WindowGeo>> {
        match &self.comp {
            Compositor::Kde(kwin) => {
                let mut list = kwin.windows(query, &self.kwin_allow(allowed).await).await?;
                with_terminals(&mut list).await;
                Ok(list)
            }
            Compositor::Hyprland(_) => Err(NO_WINDOW_CONTROL.into()),
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
            Compositor::Hyprland(_) => Err(NO_WINDOW_CONTROL.into()),
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
        let kwin = match &self.comp {
            Compositor::Kde(k) => Some(k),
            Compositor::Hyprland(_) => None,
        };
        // Windows already open, to tell a new one of a running instance.
        let before = match kwin {
            Some(k) => k
                .windows("", &self.kwin_allow(allowed).await)
                .await
                .unwrap_or_default(),
            None => Vec::new(),
        };
        let (pid, exit) = launch::spawn(&r)?;
        let mut out = Launched {
            command: launch::describe(&r),
            pid,
            ..Launched::default()
        };
        let Some(kwin) = kwin else {
            out.note = format!("not waiting for its window: {NO_WINDOW_CONTROL}");
            return Ok(out);
        };
        if wait.is_zero() {
            out.note = "not waiting for its window (wait_ms=0)".into();
            return Ok(out);
        }
        let start = std::time::Instant::now();
        let mut tree = std::collections::HashSet::from([pid]);
        let mut ended: Option<std::time::Instant> = None;
        loop {
            // Children seen once stay in the tree, so apps that fork and
            // let the parent exit are still matched by pid.
            let roots = tree.clone();
            tree = tokio::task::spawn_blocking(move || {
                let stats = terminals::read_stats();
                launch::descendants(&roots, stats.iter().map(|(p, s)| (*p, s.ppid)))
            })
            .await
            .unwrap_or_else(|_| std::collections::HashSet::from([pid]));
            // Rebuilt each round: the new app joins AT-SPI after it starts.
            let allow = self.kwin_allow(allowed).await;
            let now = kwin.windows("", &allow).await?;
            let status = *exit.lock().unwrap();
            if status.is_some() && ended.is_none() {
                ended = Some(std::time::Instant::now());
            }
            let settled = ended.is_some_and(|t| t.elapsed() >= HANDOFF_GRACE);
            let stage = if settled && status.is_some_and(|s| s.success()) {
                launch::Stage::Ended
            } else if start.elapsed() >= HANDOFF_GRACE {
                launch::Stage::Settled
            } else {
                launch::Stage::Starting
            };
            if let Some((w, how)) = launch::pick_window(app, &before, &now, &tree, stage) {
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
            if let Some(s) = status.filter(|s| !s.success() && settled) {
                return Err(format!(
                    "{} (pid {pid}) exited with {s} without opening a window",
                    out.command
                ));
            }
            if start.elapsed() >= wait {
                return Ok(out);
            }
            tokio::time::sleep(LAUNCH_POLL).await;
        }
    }

    async fn screens(&self) -> Vec<Screen> {
        match &self.comp {
            Compositor::Kde(kwin) => kwin.screens().await.unwrap_or_default(),
            Compositor::Hyprland(_) => Vec::new(),
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
                    detail: screens
                        .iter()
                        .map(|s| {
                            format!(
                                "{} at ({},{}) {}x{} scale {}",
                                s.name, s.x, s.y, s.width, s.height, s.scale
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("; "),
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
            }
        }
        out
    }

    async fn screenshot(&self) -> Option<Shot> {
        self.hide_indicator().await;
        let area;
        let mut cmd = match &self.comp {
            Compositor::Kde(kwin) => {
                area = kwin.active_frame().await;
                if !self.shot_unauthorized.load(Ordering::Relaxed) {
                    match kwin.capture_active_window().await {
                        Ok(raw) => {
                            let (path, file) = temp_png()?;
                            let mut out = std::io::BufWriter::new(file);
                            let written = image::write_png(&raw, &mut out).and_then(|()| {
                                std::io::Write::flush(&mut out).map_err(|e| e.to_string())
                            });
                            if written.is_ok() {
                                return keep_if_written(path).map(|path| Shot { path, area });
                            }
                            let _ = std::fs::remove_file(&path);
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
        keep_if_written(path).map(|path| Shot { path, area })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
}
