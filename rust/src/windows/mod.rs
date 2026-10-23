//! Windows backend: UI Automation for reading and acting, SendInput for the
//! keyboard and pointer. Same semantics as backends/windows.py.
//!
//! App names are executable names without ".exe" (e.g. "notepad"). The
//! process is per-monitor DPI aware, as `uiautomation` makes the Python
//! server, so UIA bounds and SetCursorPos use the same physical pixels.

mod input;
mod logic;
mod uia;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::Accessibility::IUIAutomationElement;

use crate::backend::{AppEntry, Backend, Cache, Info, R, WindowEntry};
use input::ProcNames;
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
const APPS_NOTE: &str = "App names are executable names without .exe. Store apps may run \
inside 'applicationframehost'. Windows of elevated (admin) apps cannot receive input from a \
non-elevated server.";
const BORING: &[&str] = &["pane", "group", "custom"];
const KEY_HOLD: Duration = Duration::from_millis(8);
const KEY_GAP: Duration = Duration::from_millis(8);
const POINTER_SETTLE: Duration = Duration::from_millis(10);
const BUTTON_HOLD: Duration = Duration::from_millis(15);
const BUTTON_GAP: Duration = Duration::from_millis(30);
const FOCUS_WAIT: Duration = Duration::from_millis(50);
const ACTIVATE_WAIT: Duration = Duration::from_millis(500);

pub struct WindowsBackend {
    uia: Uia,
    procs: ProcNames,
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
    Info { role: String::new(), name: String::new(), states: Vec::new(), showing: false,
           actions: Vec::new(), text: None, children: Vec::new() }
}

impl WindowsBackend {
    pub fn new() -> R<Self> {
        unsafe {
            // Fails when already set (e.g. by a manifest); either way is fine.
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE);
        }
        Ok(WindowsBackend { uia: Uia::new()?, procs: ProcNames::default() })
    }

    /// Info of an element whose properties (and, from a subtree request,
    /// children) are in its UIA cache. Only TextPattern text is read live.
    fn info_of(&self, el: &IUIAutomationElement, p: &Props, fg: i32, text_limit: usize,
               children: Vec<Elem>) -> Info<Elem> {
        let text = match p.value_preview(text_limit) {
            Some(t) => Some(t),
            None if p.wants_document_text() => self.uia.document_text(el, text_limit)
                .map(|t| logic::one_line(&t, text_limit)),
            None => None,
        };
        Info {
            role: p.role().to_string(),
            name: p.name.clone(),
            states: p.states(fg),
            showing: !p.offscreen,
            actions: p.actions().into_iter().map(String::from).collect(),
            text: text.filter(|t| !t.is_empty()),
            children,
        }
    }

    fn foreground_i32() -> i32 {
        input::hwnd_i32(input::foreground())
    }
}

impl Backend for WindowsBackend {
    type Id = Elem;

    fn allow_file(&self) -> PathBuf {
        let base = std::env::var_os("APPDATA").filter(|v| !v.is_empty()).map(PathBuf::from)
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
            let name = self.procs.name(p.pid);
            if name.is_empty() {
                continue;
            }
            apps.entry(name).or_default().push(WindowEntry {
                kind: p.role().to_string(),
                title: p.name.trim().to_string(),
                active: p.hwnd != 0 && p.hwnd == fg,
            });
        }
        apps.into_iter().map(|(name, windows)| AppEntry { name, windows }).collect()
    }

    async fn app_windows(&self, app: &str) -> Option<Vec<Elem>> {
        // UIA has no application object: group top-level windows by executable.
        let windows: Vec<Elem> = self.uia.top_level().into_iter()
            .filter(|(_, p)| self.procs.name(p.pid) == app)
            .map(|(e, _)| e)
            .collect();
        (!windows.is_empty()).then_some(windows)
    }

    async fn active_app(&self) -> String {
        self.procs.of_hwnd(input::foreground())
    }

    async fn info(&self, id: &Elem, text_limit: usize) -> Info<Elem> {
        let Some(el) = self.uia.fetch_subtree(&id.el) else { return dead() };
        let p = uia::props(&el);
        let kids = Uia::cached_children(&el);
        self.info_of(&el, &p, Self::foreground_i32(), text_limit, kids)
    }

    async fn prefetch(&self, cache: &mut Cache<Elem>, roots: Vec<Elem>, only_showing: bool,
                      text_limit: usize, limit: usize) {
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
        self.uia.fetch(&id.el).map(|p| p.actions().into_iter().map(String::from).collect())
            .unwrap_or_default()
    }

    async fn do_action(&self, id: &Elem, index: usize) -> R<bool> {
        let names = self.uia.fetch(&id.el).map(|p| p.actions()).unwrap_or_default();
        let name = names.get(index).ok_or("action index out of range")?;
        Ok(self.uia.perform(&id.el, name))
    }

    async fn full_text(&self, id: &Elem, limit: usize) -> R<String> {
        let Some(p) = self.uia.fetch(&id.el) else { return Ok(String::new()) };
        if p.text && let Some(t) = self.uia.document_text(&id.el, limit) {
            return Ok(logic::unix_newlines(&t));
        }
        if p.value && let Some(v) = self.uia.value(&id.el) {
            return Ok(v.chars().take(limit).collect());
        }
        Ok(p.name)
    }

    async fn set_text(&self, id: &Elem, text: &str) -> R<bool> {
        let vp = self.uia.writable_value(&id.el)
            .ok_or("element has no writable value; use focus + type_text")?;
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

    async fn element_center(&self, id: &Elem) -> R<(i32, i32)> {
        self.uia.bounds(&id.el)
            .and_then(|(l, t, r, b)| logic::center(l, t, r, b))
            .ok_or_else(|| "element has no on-screen area".into())
    }

    fn check_combo(&self, combo: &str) -> R<()> {
        logic::parse_combo(combo).map(|_| ())
    }

    async fn press_combo(&self, combo: &str) -> R<()> {
        let (downs, ups) = logic::combo_events(&logic::parse_combo(combo)?);
        input::send_keys(&downs)?;
        tokio::time::sleep(KEY_HOLD).await;
        input::send_keys(&ups)?;
        tokio::time::sleep(KEY_GAP).await;
        Ok(())
    }

    async fn type_text(&self, text: &str) -> R<()> {
        // KEYEVENTF_UNICODE sends characters independent of the keyboard
        // layout and leaves the clipboard alone.
        for batch in logic::unicode_batches(text) {
            input::send_keys(&batch)?;
        }
        Ok(())
    }

    async fn pointer(&self, x: i64, y: i64, button: &str, clicks: i64, scroll: i64) -> R<()> {
        let (down, up) = input::button_flags(button)?;
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

    async fn activate_window(&self, query: &str, allowed: &[String]) -> R<()> {
        let q = query.to_lowercase();
        for (_, p) in self.uia.top_level() {
            let app = self.procs.name(p.pid);
            if !allowed.contains(&app) {
                continue;
            }
            if (p.name.to_lowercase().contains(&q) || app.contains(&q)) && p.hwnd != 0 {
                let h = input::hwnd_from_i32(p.hwnd);
                input::raise(h)?;
                poll(ACTIVATE_WAIT, || input::foreground() == h).await;
                return Ok(());
            }
        }
        Ok(())
    }

    async fn screenshot(&self) -> Option<String> {
        input::screenshot()
    }
}
