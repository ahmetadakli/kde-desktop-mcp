//! Windows backend: UI Automation for reading and acting, SendInput for the
//! keyboard and pointer. Same semantics as the former Python backend
//! (backends/windows.py, removed after 4cd2632).
//!
//! App names are executable names without ".exe" (e.g. "notepad"). The
//! process is per-monitor DPI aware, as `uiautomation` made the Python
//! server, so UIA bounds and SetCursorPos use the same physical pixels.

mod clipboard;
mod input;
mod launch;
mod logic;
mod uia;
mod wm;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use windows::Win32::Media::{timeBeginPeriod, timeEndPeriod};
use windows::Win32::UI::Accessibility::IUIAutomationElement;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE, SetProcessDpiAwarenessContext,
};

use crate::backend::{
    Activated, AppEntry, Backend, Cache, Check, Clip, FileDialog, Info, Launched, NO_AREA, Page, R,
    Screen, Shot, WindowChange, WindowEntry, WindowGeo,
};
use crate::delivery::nothing_sent;
use crate::launch_pick::{self as pick, Found};
use logic::Props;
use uia::{Elem, Uia};

const DEFAULT_ALLOW: &str = "\
# One app (executable name without .exe) per line, case-insensitive.
# See `apps` tool output. Keyboard/mouse input is only sent when the
# foreground window belongs to one of these.
notepad
firefox
chrome
msedge
winword
excel
";
const APPS_NOTE: &str = "App names are executable names without .exe. Store apps hosted by \
ApplicationFrameHost are listed and reached under their own executable (e.g. \
'calculatorapp'); 'applicationframehost' in the allowlist allows those it does not name. Windows of elevated (admin) \
apps cannot receive input from a non-elevated server.";
const BORING: &[&str] = &["pane", "group", "custom"];
const KEY_HOLD: Duration = Duration::from_millis(8);
const KEY_GAP: Duration = Duration::from_millis(8);
const POINTER_SETTLE: Duration = Duration::from_millis(10);
const BUTTON_HOLD: Duration = Duration::from_millis(15);
const BUTTON_GAP: Duration = Duration::from_millis(30);
const FOCUS_WAIT: Duration = Duration::from_millis(50);
const ACTIVATE_WAIT: Duration = Duration::from_millis(500);
/// How long an app may take to read a paste before the clipboard is left
/// holding the typed text.
const PASTE_READ_WAIT: Duration = Duration::from_secs(1);
/// Text this short is typed key by key: no clipboard round trip.
const KEYSTROKE_MAX: usize = 1;
/// Pause between the characters typed key by key: a SendInput burst of
/// KEYEVENTF_UNICODE events makes Win11 Notepad drop and repeat them.
const CHAR_GAP: Duration = Duration::from_millis(5);
/// How long a minimized or maximized window may take to restore before it
/// is moved or resized.
const RESTORE_WAIT: Duration = Duration::from_millis(500);

/// The executables of the installed Store apps ApplicationFrameHost hosts,
/// read once in the background (`warm_up`).
static INSTALLED_HOSTED: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();

pub struct WindowsBackend {
    uia: Uia,
    clip: clipboard::Clipboard,
}

async fn poll(limit: Duration, mut check: impl FnMut() -> bool) -> bool {
    let start = std::time::Instant::now();
    loop {
        if check() {
            return true;
        }
        if start.elapsed() >= limit {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// What Python renders for an element that is gone: no role, not showing.
fn dead() -> Info<Elem> {
    Info {
        role: String::new(),
        name: String::new(),
        states: Vec::new(),
        showing: false,
        actions: Vec::new(),
        text: None,
        children: Vec::new(),
        operable: false,
    }
}

/// The app of a top-level window, a Store app's frame named after the app
/// it hosts.
fn app_of(p: &Props) -> String {
    input::window_app(input::hwnd_from_i32(p.hwnd), p.pid)
}

/// Allowed apps' windows as `launch` compares them, with a Store app's
/// frame standing for the app it hosts (its name and process), next to the
/// windows as `windows` reports them.
fn launch_windows(allowed: &[String]) -> (Vec<WindowGeo>, Vec<WindowGeo>) {
    wm::list("", allowed)
        .into_iter()
        .map(|w| {
            let mut seen = w.geo.clone();
            if !w.hosted.is_empty() {
                seen.app = w.hosted;
                seen.pid = w.hosted_pid;
            }
            (seen, w.geo)
        })
        .unzip()
}

impl WindowsBackend {
    pub fn new() -> R<Self> {
        unsafe {
            // Fails when already set (e.g. by a manifest); either way is fine.
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE);
        }
        Ok(WindowsBackend {
            uia: Uia::new()?,
            clip: clipboard::Clipboard::default(),
        })
    }

    /// Info of an element whose properties (and, from a subtree request,
    /// children) are in its UIA cache. Only TextPattern text is read live.
    fn info_of(
        &self,
        el: &IUIAutomationElement,
        p: &Props,
        fg: i32,
        text_limit: usize,
        children: Vec<Elem>,
    ) -> Info<Elem> {
        let text = match p.value_preview(text_limit) {
            Some(t) => Some(t),
            None if p.wants_document_text() => self
                .uia
                .document_text(el, text_limit)
                .map(|t| logic::preview(&t, text_limit)),
            None => None,
        };
        // XAML's popup window says it is off screen while the flyout in it
        // shows: a child on screen shows it.
        let showing = !p.offscreen || children.iter().any(|k| uia::cached_onscreen(&k.el));
        Info {
            role: p.role().to_string(),
            name: p.name.clone(),
            states: p.states(fg),
            showing,
            actions: p.actions().into_iter().map(String::from).collect(),
            text: text.filter(|t| !t.is_empty()),
            children,
            operable: p.select || p.toggle || p.expand,
        }
    }

    fn foreground_i32() -> i32 {
        input::hwnd_i32(input::foreground())
    }

    /// Type `text` key by key, one character per SendInput with a pause in
    /// between (KEYEVENTF_UNICODE: any layout, the clipboard untouched).
    async fn keystrokes(&self, text: &str) -> R<()> {
        // The pauses are shorter than the default 15.6 ms timer tick.
        unsafe { timeBeginPeriod(1) };
        let mut r = Ok(());
        for (i, batch) in logic::unicode_per_char(text).iter().enumerate() {
            if i > 0 {
                tokio::time::sleep(CHAR_GAP).await;
            }
            if let Err(e) = input::send_keys(batch) {
                r = Err(if i == 0 {
                    crate::delivery::nothing_sent(e)
                } else {
                    e
                });
                break;
            }
        }
        unsafe { timeEndPeriod(1) };
        r
    }

    /// Paste `text` with ctrl+v through the clipboard, then put back the
    /// text the clipboard held once the app in front has read the paste
    /// (restoring earlier would let it paste the old text). Images, files
    /// and other non-text contents are not put back; the note says so.
    /// When the clipboard cannot be taken, the text is typed key by key.
    async fn paste(&self, text: &str) -> R<String> {
        use crate::trace::time;
        let saved = time("save", self.clip.save()).await;
        if let clipboard::Saved::Busy = saved {
            self.keystrokes(text).await?;
            return Ok(String::new());
        }
        let mut offered = time("offer", self.clip.offer(text)).await;
        // A clipboard monitor read the text right away: offer it again, so
        // the paste's own read shows.
        if offered.is_ok() && self.clip.read_early().await {
            offered = self.clip.offer(text).await;
        }
        if offered.is_err() {
            // The clipboard may have been emptied before the offer failed.
            self.clip.restore(saved).await;
            self.keystrokes(text).await?;
            return Ok(String::new());
        }
        let since = std::time::Instant::now();
        if let Err(e) = time("ctrl+v", self.press_combo("ctrl+v")).await {
            // Nothing was pasted: safe to put the clipboard back now.
            self.clip.restore(saved).await;
            return Err(e);
        }
        let read = time("wait_read", self.clip.wait_read(since, PASTE_READ_WAIT)).await;
        Ok(match saved {
            clipboard::Saved::Busy => String::new(),
            clipboard::Saved::Nothing(why) => {
                format!("; the clipboard now holds the typed text (not put back: {why})")
            }
            // An empty clipboard is emptied again once the paste was read;
            // unread, the typed text stays and nothing was lost.
            clipboard::Saved::Empty if !read => String::new(),
            _ if !read => "; the paste was not read within 1 s, so the clipboard was not put \
                 back"
                .into(),
            saved => time("restore", self.clip.restore(saved)).await,
        })
    }
}

impl Backend for WindowsBackend {
    type Id = Elem;

    fn allow_file(&self) -> PathBuf {
        let base = std::env::var_os("APPDATA")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
            .unwrap_or_default();
        base.join("desktop-mcp").join("allow.txt")
    }

    fn default_allow(&self) -> &'static str {
        DEFAULT_ALLOW
    }

    fn apps_note(&self) -> &'static str {
        APPS_NOTE
    }

    fn confirm_example(&self) -> &'static str {
        "key:notepad"
    }

    fn app_aliases(&self) -> Vec<(String, &'static str)> {
        // The installed ones too (once read), so whether a Store app is
        // allowed does not depend on whether it is running.
        let mut apps = input::hosted_apps();
        for a in INSTALLED_HOSTED.get().into_iter().flatten() {
            if !apps.contains(a) {
                apps.push(a.clone());
            }
        }
        apps.into_iter()
            .filter(|a| a != logic::STORE_FRAME_APP)
            .map(|a| (a, logic::STORE_FRAME_APP))
            .collect()
    }

    fn warm_up(&self) {
        // Reading every package's manifest takes a moment: not on a call.
        std::thread::spawn(|| {
            let _ = INSTALLED_HOSTED.set(launch::installed_hosted_apps());
        });
    }

    fn boring_roles(&self) -> &'static [&'static str] {
        BORING
    }

    async fn enable(&self) {
        // UI Automation is always available.
    }

    async fn list_apps(&self) -> Vec<AppEntry> {
        let fg = Self::foreground_i32();
        let mut apps: BTreeMap<String, Vec<WindowEntry>> = BTreeMap::new();
        for (_, p) in self.uia.top_level() {
            let name = app_of(&p);
            if name.is_empty() {
                continue;
            }
            apps.entry(name).or_default().push(WindowEntry {
                kind: p.role().to_string(),
                title: p.name.trim().to_string(),
                active: p.hwnd != 0 && p.hwnd == fg,
            });
        }
        apps.into_iter()
            .map(|(name, windows)| AppEntry { name, windows })
            .collect()
    }

    async fn app_windows(&self, app: &str) -> Option<Vec<Elem>> {
        // UIA has no application object: group top-level windows by executable.
        let windows: Vec<Elem> = self
            .uia
            .top_level()
            .into_iter()
            // A Store app's frame is the root: its tree has the title bar
            // as well as the app. Only the hosted app's name reaches it, so
            // that app's own allowlist line and confirm rules apply;
            // 'applicationframehost' is left with frames that host none.
            .filter(|(_, p)| app_of(p) == app)
            .map(|(e, _)| e)
            .collect();
        (!windows.is_empty()).then_some(windows)
    }

    async fn active_app(&self) -> String {
        input::hwnd_proc_name(input::foreground())
    }

    async fn info(&self, id: &Elem, text_limit: usize) -> Info<Elem> {
        let Some(el) = self.uia.fetch_subtree(&id.el) else {
            return dead();
        };
        let p = uia::props(&el);
        let kids = Uia::cached_children(&el);
        let mut info = self.info_of(&el, &p, Self::foreground_i32(), text_limit, kids);
        // UIA keeps reading a window that was hidden or destroyed (the
        // closed Open/Save dialog file_dialog waits for).
        if p.hwnd != 0 && !wm::is_shown(input::hwnd_from_i32(p.hwnd)) {
            info.showing = false;
        }
        info
    }

    async fn identity(&self, id: &Elem) -> (String, String) {
        // One element, not its subtree as `info` fetches.
        match self.uia.fetch(&id.el) {
            Some(p) => (p.role().to_string(), p.name),
            None => (String::new(), String::new()),
        }
    }

    async fn prefetch(
        &self,
        cache: &mut Cache<Elem>,
        roots: Vec<Elem>,
        only_showing: bool,
        text_limit: usize,
        limit: usize,
    ) {
        let fg = Self::foreground_i32();
        // (cache key, element whose UIA cache holds its properties and children)
        let mut queue = std::collections::VecDeque::new();
        for root in roots {
            if cache.contains_key(&root) {
                continue;
            }
            if root.subtree {
                let el = root.el.clone();
                queue.push_back((root, el));
            } else {
                // One cross-process call brings the root's whole subtree.
                match self.uia.fetch_subtree(&root.el) {
                    Some(el) => queue.push_back((root, el)),
                    // Gone: render it the way Python renders a dead element.
                    None => {
                        cache.insert(root, dead());
                    }
                }
            }
        }
        let mut added = 0;
        while let Some((key, el)) = queue.pop_front() {
            if cache.contains_key(&key) {
                continue;
            }
            let p = uia::props(&el);
            let kids = Uia::cached_children(&el);
            let info = self.info_of(&el, &p, fg, text_limit, kids);
            // Nodes past the limit stay out of the cache; their `Elem`s
            // still carry the fetched subtree, so asking for them later
            // costs no further cross-process call.
            if !(only_showing && !info.showing) {
                for k in &info.children {
                    queue.push_back((k.clone(), k.el.clone()));
                }
            }
            cache.insert(key, info);
            added += 1;
            if added >= limit {
                break;
            }
        }
    }

    async fn actions(&self, id: &Elem) -> Vec<String> {
        self.uia
            .fetch(&id.el)
            .map(|p| p.actions().into_iter().map(String::from).collect())
            .unwrap_or_default()
    }

    async fn do_action(&self, id: &Elem, index: usize) -> R<bool> {
        let names = self
            .uia
            .fetch(&id.el)
            .map(|p| p.actions())
            .unwrap_or_default();
        let name = names.get(index).ok_or("action index out of range")?;
        Ok(self.uia.perform(&id.el, name))
    }

    async fn full_text(&self, id: &Elem, limit: usize) -> R<String> {
        let Some(p) = self.uia.fetch(&id.el) else {
            return Ok(String::new());
        };
        if p.text
            && let Some(t) = self.uia.document_text(&id.el, limit)
        {
            return Ok(logic::unix_newlines(&t));
        }
        if p.value
            && let Some(v) = self.uia.value(&id.el)
        {
            return Ok(v.chars().take(limit).collect());
        }
        Ok(p.name)
    }

    async fn set_text(&self, id: &Elem, text: &str) -> R<bool> {
        let vp = self
            .uia
            .writable_value(&id.el)
            .ok_or("element has no writable value; use focus + type_text")?;
        if let Some(h) = self.uia.own_handle(&id.el).map(input::hwnd_from_i32)
            && wm::single_line_edit(h)
        {
            return Ok(wm::replace_edit_text(h, text));
        }
        Ok(Uia::set_value(&vp, text))
    }

    async fn is_focused(&self, id: &Elem) -> bool {
        self.uia.has_focus(&id.el)
    }

    async fn try_focus(&self, id: &Elem) -> R<Option<String>> {
        if self.uia.set_focus(&id.el) && poll(FOCUS_WAIT, || self.uia.has_focus(&id.el)).await {
            return Ok(Some("SetFocus".into()));
        }
        Ok(None)
    }

    async fn toolkit_of(&self, id: &Elem) -> String {
        let class = self
            .uia
            .window_handle(&id.el)
            .map(|h| wm::class_name(input::root(input::hwnd_from_i32(h))))
            .unwrap_or_default();
        logic::web_toolkit(&self.uia.framework(&id.el), &class).to_string()
    }

    async fn embedded_document(&self, window: &Elem) -> Option<Elem> {
        // Chromium's page sits deep below its window: one FindFirst.
        if self.toolkit_of(window).await.is_empty() {
            return None;
        }
        self.uia.showing_document(&window.el)
    }

    async fn page(&self, doc: &Elem) -> Option<Page> {
        let p = self.uia.fetch(&doc.el)?;
        if p.role() != "document" || p.offscreen {
            return None;
        }
        Some(Page {
            title: p.name,
            url: self.uia.value(&doc.el).unwrap_or_default(),
            busy: self
                .uia
                .legacy_state(&doc.el)
                .is_some_and(|s| s & logic::STATE_BUSY != 0),
        })
    }

    async fn loading(&self, windows: &[Elem]) -> bool {
        for w in windows {
            if let Some(doc) = self.embedded_document(w).await
                && self.page(&doc).await.is_some_and(|p| p.busy)
            {
                return true;
            }
        }
        false
    }

    async fn scroll_into_view(&self, id: &Elem) -> R<bool> {
        // An element without ScrollItemPattern: the wheel fallback.
        Ok(self.uia.scroll_into_view(&id.el).unwrap_or(false))
    }

    async fn scroll_container(&self, id: &Elem) -> Option<Elem> {
        self.uia.scroll_container(&id.el)
    }

    async fn element_center(&self, id: &Elem) -> R<(i32, i32)> {
        // Like the Linux backends: a click at a background element's
        // coordinates would land on whatever window covers them.
        if let Some(h) = self.uia.window_handle(&id.el)
            && input::root(input::hwnd_from_i32(h)) != input::root(input::foreground())
        {
            return Err("element's window is not active; call activate_window first".into());
        }
        self.uia
            .bounds(&id.el)
            .and_then(|(l, t, r, b)| logic::center(l, t, r, b))
            .ok_or_else(|| NO_AREA.into())
    }

    fn check_combo(&self, combo: &str) -> R<()> {
        logic::parse_combo(combo).map(|_| ())
    }

    async fn press_combo(&self, combo: &str) -> R<()> {
        let (downs, ups) =
            logic::combo_events(&logic::parse_combo(combo).map_err(crate::delivery::nothing_sent)?);
        input::send_keys(&downs)?;
        tokio::time::sleep(KEY_HOLD).await;
        input::send_keys(&ups)?;
        tokio::time::sleep(KEY_GAP).await;
        Ok(())
    }

    async fn type_text(&self, text: &str) -> R<String> {
        if text.chars().count() <= KEYSTROKE_MAX {
            self.keystrokes(text).await?;
            return Ok(String::new());
        }
        self.paste(text).await
    }

    async fn clipboard_get(&self, limit: usize) -> R<Clip> {
        self.clip.get(limit).await
    }

    async fn clipboard_set(&self, text: &str) -> R<()> {
        self.clip.set(text).await
    }

    async fn pointer(&self, x: i64, y: i64, button: &str, clicks: i64, scroll: i64) -> R<()> {
        let (down, up) = input::button_flags(button).map_err(crate::delivery::nothing_sent)?;
        input::set_cursor(x, y)?;
        tokio::time::sleep(POINTER_SETTLE).await;
        for _ in 0..clicks.clamp(0, 3) {
            input::button(down)?;
            tokio::time::sleep(BUTTON_HOLD).await;
            input::button(up)?;
            tokio::time::sleep(BUTTON_GAP).await;
        }
        if scroll != 0 {
            input::wheel(scroll)?;
        }
        Ok(())
    }

    async fn move_pointer(&self, x: i64, y: i64) -> R<()> {
        input::set_cursor(x, y)?;
        tokio::time::sleep(POINTER_SETTLE).await;
        Ok(())
    }

    async fn button(&self, button: &str, down: bool) -> R<()> {
        let (d, u) = input::button_flags(button).map_err(crate::delivery::nothing_sent)?;
        input::button(if down { d } else { u })?;
        tokio::time::sleep(BUTTON_HOLD).await;
        Ok(())
    }

    async fn key(&self, key: &str, down: bool) -> R<()> {
        // One SendInput queue for keyboard and mouse: no settle needed to
        // keep a modifier ordered before a click.
        input::send_keys(&[logic::key_event(
            logic::vk(key).map_err(crate::delivery::nothing_sent)?,
            !down,
        )])?;
        if down {
            tokio::time::sleep(KEY_HOLD).await;
        }
        Ok(())
    }

    async fn cursor_position(&self) -> R<(i64, i64)> {
        input::cursor()
    }

    async fn activate_window(&self, query: &str, allowed: &[String]) -> R<Option<Activated>> {
        let q = query.to_lowercase();
        for (_, p) in self.uia.top_level() {
            let app = app_of(&p);
            if !allowed.contains(&app) {
                continue;
            }
            if (p.name.to_lowercase().contains(&q) || app.contains(&q)) && p.hwnd != 0 {
                let h = input::hwnd_from_i32(p.hwnd);
                input::set_foreground(h);
                if !poll(FOCUS_WAIT, || input::foreground() == h).await {
                    // Refused: retry after an Alt tap, but only send that
                    // key to a foreground app on the allowlist.
                    if allowed.contains(&input::hwnd_proc_name(input::foreground())) {
                        input::alt_tap()?;
                        input::set_foreground(h);
                    }
                    poll(ACTIVATE_WAIT, || input::foreground() == h).await;
                }
                return Ok(None);
            }
        }
        Ok(None)
    }

    fn names_menus(&self) -> bool {
        true
    }

    async fn open_menus(&self, windows: &[Elem]) -> Option<Vec<Elem>> {
        let roots: Vec<IUIAutomationElement> = windows.iter().map(|w| w.el.clone()).collect();
        let mut menus = self.uia.showing_menus(&roots);
        // A Win32 popup menu (#32768) is a top-level window of its own,
        // possibly newer than `windows`.
        let pids: Vec<u32> = windows
            .iter()
            .filter_map(|w| self.uia.fetch(&w.el).map(|p| p.pid))
            .collect();
        for (e, p) in self.uia.top_level() {
            if p.role() == "menu" && pids.contains(&p.pid) && !menus.contains(&e) {
                menus.push(e);
            }
        }
        Some(menus)
    }

    async fn active_file_dialog(&self) -> R<FileDialog<Elem>> {
        // The common item dialog is a #32770 window of the app's own
        // process, owned by the app's window; UIA lists it below its owner,
        // so it is taken from the foreground handle, not the root's children.
        let h = input::foreground();
        let not_one = || nothing_sent(logic::NOT_A_FILE_DIALOG);
        if h.is_invalid() || wm::class_name(h) != logic::DIALOG_CLASS {
            return Err(not_one());
        }
        let parts = self.uia.file_dialog(h).ok_or_else(not_one)?;
        let field = parts.field.ok_or_else(not_one)?;
        Ok(FileDialog {
            window: parts.window,
            app: input::proc_name(input::hwnd_pid(h)).to_lowercase(),
            titles: vec![parts.name],
            parent: None,
            field,
            accept: parts.accept,
        })
    }

    async fn file_dialog_prompt(&self, window: &Elem) -> Option<String> {
        let dialog = input::hwnd_from_i32(self.uia.own_handle(&window.el)?);
        wm::prompt_over(dialog, input::foreground())
    }

    async fn dialogs_of(&self, w: &WindowGeo) -> Vec<Elem> {
        let all = [w.app.clone()];
        match wm::find(w, &all) {
            Some(n) => self.uia.dialogs(n.hwnd, &wm::owned(n.hwnd)),
            None => Vec::new(),
        }
    }

    async fn windows(&self, query: &str, allowed: &[String]) -> R<Vec<WindowGeo>> {
        Ok(wm::list(query, allowed)
            .into_iter()
            .map(|w| w.geo)
            .collect())
    }

    async fn set_window(
        &self,
        query: &str,
        allowed: &[String],
        change: &WindowChange,
    ) -> R<WindowGeo> {
        use windows::Win32::UI::WindowsAndMessaging::{SW_MAXIMIZE, SW_MINIMIZE, SW_RESTORE};
        if change.desktop.is_some() {
            return Err(nothing_sent(
                "virtual desktops cannot be changed on Windows",
            ));
        }
        let mut list = wm::list(query, allowed);
        let active: Vec<bool> = list.iter().map(|w| w.geo.active).collect();
        let Some(i) = logic::pick_window(&active) else {
            return Err(crate::backend::not_one_window(query, &[]));
        };
        let w = list.swap_remove(i);
        let h = w.hwnd;
        let geometry = [change.x, change.y, change.width, change.height]
            .iter()
            .any(Option::is_some);
        let state = change.state.as_deref();
        // Restore until neither minimized nor maximized (a maximized window
        // that was minimized comes back maximized first); true when asked.
        let restore = || async move {
            let mut asked = false;
            for _ in 0..2 {
                if !wm::is_minimized(h) && !wm::is_maximized(h) {
                    break;
                }
                asked = true;
                let minimized = wm::is_minimized(h);
                wm::show(h, SW_RESTORE);
                poll(RESTORE_WAIT, || match minimized {
                    true => !wm::is_minimized(h),
                    false => !wm::is_maximized(h),
                })
                .await;
            }
            asked
        };
        let mut sent = false;
        match state {
            // With a new geometry the window ends up restored (as on KWin).
            Some("maximize") if !geometry => {
                wm::show(h, SW_MAXIMIZE);
                sent = true;
            }
            Some("restore") => sent = restore().await,
            _ => {}
        }
        if geometry {
            // A maximized or minimized window is restored to be moved, as
            // KWin does.
            sent |= restore().await;
            if let Err(e) = wm::place(h, change.x, change.y, change.width, change.height) {
                return Err(match sent {
                    false => nothing_sent(e),
                    true => e,
                });
            }
        }
        // Last, so a move given with it does not restore the window again.
        if state == Some("minimize") {
            wm::show(h, SW_MINIMIZE);
        }
        Ok(wm::reread(h, &w.geo.app))
    }

    async fn close_window(&self, query: &str, allowed: &[String]) -> R<WindowGeo> {
        let list = wm::list(query, allowed);
        let [w] = list.as_slice() else {
            let geos: Vec<WindowGeo> = list.into_iter().map(|w| w.geo).collect();
            return Err(crate::backend::not_one_window(query, &geos));
        };
        wm::close(w.hwnd).map_err(nothing_sent)?;
        Ok(w.geo.clone())
    }

    async fn launch(
        &self,
        app: &str,
        args: &[String],
        allowed: &[String],
        wait: Duration,
    ) -> R<Launched> {
        if app.trim().eq_ignore_ascii_case(logic::STORE_FRAME_APP) {
            // ApplicationFrameHost.exe itself, not a Store app.
            return Err(nothing_sent(
                "applicationframehost only hosts Store apps' frames; launch the Store app by its \
                 own name (e.g. calculatorapp)",
            ));
        }
        let (name, argv) = (app.to_string(), args.to_vec());
        let r = tokio::task::spawn_blocking(move || launch::resolve(&name, &argv))
            .await
            .map_err(|e| e.to_string())?
            // Not found: nothing started.
            .map_err(nothing_sent)?;
        // Windows already open, to tell a new one of a running instance.
        let (before, _) = launch_windows(allowed);
        let (pid, exit) = match &r.target {
            launch::Target::Program(p) => {
                let (pid, exit) = launch::spawn(p, args).map_err(nothing_sent)?;
                (pid, Some(exit))
            }
            launch::Target::Store(aumid) => {
                let (aumid, argv) = (aumid.clone(), args.to_vec());
                let pid = tokio::task::spawn_blocking(move || launch::activate(&aumid, &argv))
                    .await
                    .map_err(|e| e.to_string())?
                    .map_err(nothing_sent)?;
                (pid, None)
            }
        };
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
        let hints = pick::arg_hints(args);
        // A Store app that was running already took the request: its
        // windows are not new, and it counts as a clean hand-off.
        let running = exit.is_none() && before.iter().any(|w| w.pid == pid);
        let mut tree = std::collections::HashSet::new();
        if !running {
            tree.insert(pid);
        }
        let mut ended = running.then_some(start);
        // Rounds since the tree last grew.
        let mut stable = 0;
        loop {
            let scanned = pick::tree_scan_due(stable);
            let grow = |tree: &std::collections::HashSet<u32>| match tree.is_empty() {
                true => tree.clone(),
                false => pick::descendants(tree, launch::parents()),
            };
            if scanned {
                let grown = grow(&tree);
                stable = if grown.len() > tree.len() {
                    0
                } else {
                    stable + 1
                };
                tree = grown;
            } else {
                stable += 1;
            }
            let (now, shown) = launch_windows(allowed);
            let status = exit.as_ref().and_then(|e| *e.lock().unwrap());
            if status.is_some() && ended.is_none() {
                ended = Some(std::time::Instant::now());
            }
            // Starters hand the request on and end, explorer with code 1.
            let exited = match status {
                Some(code) => ended.map(|t| (logic::handoff_exit(Some(code as i32)), t.elapsed())),
                None => ended.map(|t| (true, t.elapsed())),
            };
            let stage = pick::stage(start.elapsed(), exited);
            let mut picked = pick::pick_window(app, &before, &now, &tree, stage, &hints);
            if picked.as_ref().is_some_and(|(_, how)| *how != Found::Own) && !scanned {
                // A skipped scan must not call the started app's own window
                // the running instance's.
                tree = grow(&tree);
                stable = 0;
                picked = pick::pick_window(app, &before, &now, &tree, stage, &hints);
            }
            if let Some((w, how)) = picked {
                let i = now.iter().position(|n| std::ptr::eq(n, w)).unwrap_or(0);
                out.window = shown.into_iter().nth(i);
                out.note = match how {
                    Found::Own => "",
                    Found::New => "opened by the running instance",
                    Found::Existing => "handed to the running instance",
                }
                .into();
                return Ok(out);
            }
            if let (Some(code), Some((false, since))) = (status, exited)
                && since >= pick::HANDOFF_GRACE
            {
                return Err(format!(
                    "{} (pid {pid}) exited with code {} without opening a window",
                    out.command,
                    logic::exit_code(code)
                ));
            }
            if start.elapsed() >= wait {
                return Ok(out);
            }
            tokio::time::sleep(pick::poll_interval(start.elapsed())).await;
        }
    }

    async fn screens(&self) -> Vec<Screen> {
        wm::screens()
    }

    async fn doctor(&self) -> Vec<Check> {
        let screens = wm::screens();
        vec![Check {
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
        }]
    }

    async fn screenshot(&self) -> Option<Shot> {
        // The process is DPI aware: image pixels are screen pixels.
        input::screenshot().map(|(path, pixels, r)| Shot {
            path,
            pixels: Some(Box::new(move || Ok(pixels))),
            area: Some((
                r.left as f64,
                r.top as f64,
                (r.right - r.left) as f64,
                (r.bottom - r.top) as f64,
            )),
        })
    }
}
