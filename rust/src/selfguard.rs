//! The self-client guard: input never goes to the agent's own client.
//!
//! The server runs below the client that drives it (Claude Code in a
//! terminal tab, Claude Desktop, an editor). Keys typed into that tab or
//! window reach the agent itself: a command typed there runs as if the
//! user had sent it, and a click there can answer the client's own
//! approval prompts. The guard caches the server's ancestor processes and
//! the terminals they run in at start, and the tool layer refuses input
//! when the window in front is one of them, or for a terminal emulator,
//! when the tab in front runs the agent. Other tabs of the same terminal
//! window stay usable.

use std::fmt::Write as _;

/// One ancestor process of the server.
#[derive(Clone, Debug, PartialEq)]
pub struct Proc {
    pub pid: u32,
    /// Lower case, without ".exe".
    pub name: String,
}

/// The processes the agent's client is made of, read once at start.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SelfClient {
    /// The server's ancestors, nearest first, up to (not including) the
    /// session's own processes (systemd, the compositor, explorer).
    pub chain: Vec<Proc>,
    /// The pseudo-terminals ("pts/3") they run in.
    pub ttys: Vec<String>,
    /// The server runs under a background daemon of its client (Claude
    /// Code's `claude daemon`): the interactive client showing it attaches
    /// over a socket and is no ancestor, so every terminal tab running
    /// such a client counts as the agent's own (which one shows this
    /// session cannot be told).
    pub daemon: bool,
    /// Under a daemon: the executables of its processes (absolute paths)
    /// and the names they go by, which its interactive clients share.
    pub client_exes: Vec<String>,
    pub client_names: Vec<String>,
}

/// A terminal tab running an interactive client of the agent's kind, for
/// doctor.
#[derive(Clone, Debug, PartialEq)]
pub struct ClientTab {
    /// The client's terminal ("pts/0").
    pub tty: String,
    /// The terminal emulator holding it (konsole), and its pid.
    pub terminal: Proc,
}

/// What a window's process shows of its terminals.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TermView {
    /// Every pseudo-terminal the process and its children hold.
    pub ptys: Vec<String>,
    /// The terminal of the tab in front, when the emulator says (Konsole
    /// over D-Bus).
    pub shown: Option<String>,
    /// The terminals of this window's tabs, when the emulator says.
    pub tabs: Option<Vec<String>>,
    /// The terminals among `ptys` running an interactive client of the
    /// agent's kind (`SelfClient::is_client`), read under a daemon only.
    pub clients: Vec<String>,
    /// The name of the window's process ("konsole"), when read.
    pub name: String,
}

/// Why a window counts as the agent's own.
#[derive(Clone, Debug, PartialEq)]
pub enum Own {
    /// The window of an ancestor process that is no terminal (Claude
    /// Desktop running the server).
    Window(Proc),
    /// A terminal whose tab in front runs the agent.
    Tab(Proc, String),
    /// A terminal whose tab in front runs an interactive client of the
    /// daemon the agent runs under (`SelfClient::daemon`).
    ClientTab(Proc, String),
    /// A terminal whose tabs all run the agent (the tab in front unknown).
    AllTabs(Proc),
    /// A terminal one of whose tabs runs the agent, the tab in front
    /// unknown (a terminal other than Konsole, or Konsole not answering).
    SomeTab(Proc),
}

/// Processes that start everything in a session: an ancestor chain stops
/// below them, as their windows (panels, the desktop, the file manager on
/// Windows) are no part of the client.
const SESSION_ROOTS: &[&str] = &[
    "systemd",
    "init",
    "kwin_wayland",
    "kwin_wayland_wrapper",
    "kwin_x11",
    "plasmashell",
    "krunner",
    "ksmserver",
    "kded5",
    "kded6",
    "startplasma-wayland",
    "startplasma-x11",
    "plasma_session",
    "hyprland",
    "sway",
    "gnome-shell",
    "sddm",
    "sddm-helper",
    "login",
    "sshd",
    "dbus-daemon",
    "dbus-broker",
    "uwsm",
    "explorer",
    "svchost",
    "services",
    "wininit",
    "winlogon",
    "userinit",
    "sihost",
    "runtimebroker",
];

/// Apps whose one window holds several terminals or a terminal beside
/// other work, whose tab or pane in front cannot be read (Windows Terminal,
/// editors with a terminal panel). An ancestor among them only counts when
/// its terminals are known to be the agent's.
const TAB_HOSTS: &[&str] = &[
    "windowsterminal",
    "code",
    "code - insiders",
    "code-insiders",
    "codium",
    "vscodium",
    "cursor",
    "windsurf",
    "wezterm-gui",
    "tabby",
    "hyper",
];

/// Terminal multiplexers, whose server runs apart from the terminal
/// showing it (name prefixes: "tmux: server", "screen").
const MULTIPLEXERS: &[&str] = &["tmux", "screen", "zellij", "abduco", "dtach"];

/// Words of terminal menu entries that send input into a terminal
/// (`fold`ed): Paste, Paste Selection, Copy Input To, in English and
/// Turkish.
const INPUT_ENTRIES: &[&str] = &[
    "paste",
    "yapıştır",
    "copy input",
    "girdiyi kopyala",
    "send input",
    "broadcast",
];

/// Whether a terminal's element named `name` sends input into its
/// terminals (Konsole's Edit > Paste, Edit > Copy Input To).
pub fn input_entry(name: &str) -> bool {
    let n = crate::render::fold(name);
    INPUT_ENTRIES
        .iter()
        .any(|w| n.contains(&crate::render::fold(w)))
}

/// Subcommands and flags of Claude Code's background processes: its
/// daemon, the pty hosts and spare sessions it keeps. They are no
/// interactive client.
#[cfg_attr(windows, allow(dead_code))]
const BACKGROUND: &[&str] = &[
    "daemon",
    "bg-pty-host",
    "bg-spare",
    "--bg-pty-host",
    "--bg-spare",
];

/// The words of a command line: a process that renamed itself may hold its
/// whole command line in its first argument ("claude bg-spare --bg-spare
/// /tmp/a.sock").
#[cfg_attr(windows, allow(dead_code))]
fn words(argv: &[String]) -> impl Iterator<Item = &str> {
    argv.iter().flat_map(|a| a.split_whitespace())
}

/// The name a command line goes by: its first word's file name.
#[cfg_attr(windows, allow(dead_code))]
pub fn argv_name(argv: &[String]) -> String {
    norm(
        words(argv)
            .next()
            .and_then(|w| w.rsplit('/').next())
            .unwrap_or_default(),
    )
}

/// Whether a command line runs a background part of the agent's client
/// (`claude daemon run`, `claude bg-pty-host`, `claude bg-spare`).
#[cfg_attr(windows, allow(dead_code))]
pub fn background_cmd(argv: &[String]) -> bool {
    let mut w = words(argv).skip(1);
    w.next().is_some_and(|first| BACKGROUND.contains(&first))
        || w.any(|a| a.starts_with("--bg-") && BACKGROUND.contains(&a))
}

/// The `self-client` directive that turns the guard off.
pub const OPT_OUT: &str = "self-client: allow";

/// `parent`, the parent pid a process `child` records, unless that pid now
/// names a process started after the child (Windows: the parent died and
/// its pid was reused); 0 then, which ends a walk. Start times (any unit)
/// that cannot be read keep the parent.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn live_parent(parent: u32, child: Option<u64>, parent_started: Option<u64>) -> u32 {
    match (child, parent_started) {
        (Some(c), Some(p)) if p > c => 0,
        _ => parent,
    }
}

/// A process name as the guard compares it.
pub fn norm(name: &str) -> String {
    let n = name.trim().to_lowercase();
    match n.strip_suffix(".exe") {
        Some(stem) => stem.to_string(),
        None => n,
    }
}

impl SelfClient {
    /// Walk up from the server's parent `start`. `read` gives a process'
    /// parent, name and terminal ("pts/N", None for none); the walk stops
    /// at pid 1, a session root, a process it cannot read, or a loop.
    pub fn walk(start: u32, read: impl Fn(u32) -> Option<(u32, String, Option<String>)>) -> Self {
        let mut me = SelfClient::default();
        let mut pid = start;
        while pid > 1 && me.chain.len() < 64 && !me.chain.iter().any(|p| p.pid == pid) {
            let Some((parent, name, tty)) = read(pid) else {
                break;
            };
            let name = norm(&name);
            if SESSION_ROOTS.contains(&name.as_str()) {
                break;
            }
            if let Some(t) = tty
                && !me.ttys.contains(&t)
            {
                me.ttys.push(t);
            }
            me.chain.push(Proc { pid, name });
            pid = parent;
        }
        me
    }

    /// Count process `pid` and its terminal as the client's too: the
    /// terminal tab the client was started in, found by other means
    /// (Konsole's session variables) when the client outlived the process
    /// that started it.
    pub fn add(&mut self, pid: u32, name: &str, tty: Option<String>) {
        if pid > 1 && self.ancestor(pid).is_none() {
            self.chain.push(Proc {
                pid,
                name: norm(name),
            });
        }
        if let Some(t) = tty
            && !self.ttys.contains(&t)
        {
            self.ttys.push(t);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.chain.is_empty()
    }

    /// Mark the server as running under a background daemon of its client
    /// when `env_bg` says so (CLAUDE_CODE_SESSION_KIND=bg) or one of its
    /// ancestors is such a process (`background_cmd`), and remember what
    /// those processes run (`exe`: an ancestor's executable and command
    /// line), so `is_client` can tell the interactive clients.
    #[cfg_attr(windows, allow(dead_code))]
    pub fn mark_daemon(
        &mut self,
        env_bg: bool,
        exe: impl Fn(u32) -> Option<(String, Vec<String>)>,
    ) {
        for p in &self.chain {
            let Some((path, argv)) = exe(p.pid) else {
                continue;
            };
            if !background_cmd(&argv) {
                continue;
            }
            self.daemon = true;
            if !path.is_empty() && !self.client_exes.contains(&path) {
                self.client_exes.push(path);
            }
            let name = argv_name(&argv);
            if !name.is_empty() && !self.client_names.contains(&name) {
                self.client_names.push(name);
            }
        }
        if env_bg {
            self.daemon = true;
        }
        if self.daemon && self.client_names.is_empty() {
            self.client_names.push("claude".into());
        }
    }

    /// Whether a process running `exe` with command line `argv` is an
    /// interactive client of the daemon the server runs under: the same
    /// executable as the daemon's (or another version beside it, as
    /// updates install them), or the same name, and no background part.
    #[cfg_attr(windows, allow(dead_code))]
    pub fn is_client(&self, exe: &str, argv: &[String]) -> bool {
        if !self.daemon || background_cmd(argv) {
            return false;
        }
        let dir = |e: &str| {
            std::path::Path::new(e)
                .parent()
                .filter(|d| d.file_name().is_some_and(|n| n == "versions"))
                .map(std::path::Path::to_path_buf)
        };
        self.client_exes
            .iter()
            .any(|c| c == exe || (dir(c).is_some() && dir(c) == dir(exe)))
            || self.client_names.contains(&argv_name(argv))
    }

    /// The ancestor with this pid.
    pub fn ancestor(&self, pid: u32) -> Option<&Proc> {
        (pid != 0)
            .then(|| self.chain.iter().find(|p| p.pid == pid))
            .flatten()
    }

    /// Whether the window of `pid` may be the agent's own, so its
    /// terminals are worth reading: an ancestor's, or under a daemon any
    /// window (a terminal running one of its clients).
    pub fn may_own(&self, pid: u32) -> bool {
        pid != 0 && (self.daemon || self.ancestor(pid).is_some())
    }

    fn ours(&self, tty: &str) -> bool {
        self.ttys.iter().any(|t| t == tty)
    }

    /// Whether input to the window of `pid` in front, showing `view`,
    /// would reach the agent: an ancestor's window that is no terminal, or
    /// a terminal whose tab in front runs the agent. When the tab in front
    /// cannot be read (kitty, foot's server, gnome-terminal, Konsole not
    /// answering or with two windows titled the same), any of its
    /// terminals running the agent counts: one process holds the tabs of
    /// all its windows, so another tab elsewhere says nothing about this
    /// one. A tab host whose tabs are unknown (Windows Terminal) does not
    /// count. Under a daemon, a tab running an interactive client counts
    /// as the agent's own too, in any terminal (`view.clients`).
    pub fn front_owner(&self, pid: u32, view: &TermView) -> Option<Own> {
        let (p, ancestor) = self.window_proc(pid, view)?;
        if let Some(t) = &view.shown {
            if self.ours(t) {
                return Some(Own::Tab(p, t.clone()));
            }
            return self
                .client_tty(view, t)
                .then(|| Own::ClientTab(p, t.clone()));
        }
        if !view.ptys.is_empty() {
            let ours = |t: &String| self.ours(t) || self.client_tty(view, t);
            if view.ptys.iter().all(ours) {
                return Some(Own::AllTabs(p));
            }
            return view.ptys.iter().any(ours).then_some(Own::SomeTab(p));
        }
        (ancestor && !TAB_HOSTS.contains(&p.name.as_str())).then_some(Own::Window(p))
    }

    /// The process of the window of `pid` as the guard names it, and
    /// whether it is an ancestor; None when the window is none of the
    /// agent's: no ancestor, nor a terminal running a client of the
    /// daemon the agent runs under.
    fn window_proc(&self, pid: u32, view: &TermView) -> Option<(Proc, bool)> {
        if let Some(p) = self.ancestor(pid) {
            return Some((p.clone(), true));
        }
        (pid != 0 && self.daemon && !view.clients.is_empty()).then(|| {
            let name = match view.name.trim() {
                "" => "terminal".to_string(),
                n => norm(n),
            };
            (Proc { pid, name }, false)
        })
    }

    /// Whether terminal `tty` of `view` runs a client of the daemon.
    fn client_tty(&self, view: &TermView, tty: &str) -> bool {
        self.daemon && view.clients.iter().any(|t| t == tty)
    }

    /// Whether an element of `pid`'s app, named `name`, is part of the
    /// agent's client itself (Claude Desktop's own buttons): an ancestor
    /// without terminals. A terminal's own elements (its tabs, menus) are
    /// not, except the entries that send input into its terminals
    /// (`input_entry`: Paste, Copy Input To) while one runs the agent.
    pub fn element_owner(&self, pid: u32, view: &TermView, name: &str) -> Option<Own> {
        let (p, ancestor) = self.window_proc(pid, view)?;
        if !view.ptys.is_empty() {
            return (input_entry(name)
                && view
                    .ptys
                    .iter()
                    .any(|t| self.ours(t) || self.client_tty(view, t)))
            .then_some(Own::SomeTab(p));
        }
        (ancestor && !TAB_HOSTS.contains(&p.name.as_str())).then_some(Own::Window(p))
    }

    /// Whether closing the window of `pid` would close the agent: an
    /// ancestor's, unless its tabs are known and none runs the agent.
    pub fn close_owner(&self, pid: u32, view: &TermView) -> Option<Own> {
        let p = self.ancestor(pid)?;
        match &view.tabs {
            Some(tabs) => tabs
                .iter()
                .any(|t| self.ours(t))
                .then(|| Own::AllTabs(p.clone())),
            None => Some(Own::Window(p.clone())),
        }
    }

    /// A terminal multiplexer among the client's processes (tmux, screen):
    /// its server left the terminal that shows it, which is then no
    /// ancestor, so the guard cannot see that window.
    pub fn multiplexer(&self) -> Option<&str> {
        self.chain
            .iter()
            .map(|p| p.name.as_str())
            .find(|n| MULTIPLEXERS.contains(&n.split([':', ' ']).next().unwrap_or("")))
    }

    /// The doctor line: the client's processes, outermost first, and their
    /// terminals.
    pub fn describe(&self) -> String {
        if self.chain.is_empty() {
            return "no client process found above this server".into();
        }
        let mut s = self
            .chain
            .iter()
            .rev()
            .map(|p| format!("{} ({})", p.name, p.pid))
            .collect::<Vec<_>>()
            .join(" > ");
        if !self.ttys.is_empty() {
            let _ = write!(s, " on {}", self.ttys.join(", "));
        }
        if self.daemon {
            s.push_str(
                "; it runs under a background daemon, so input to every terminal tab \
                 running an interactive client of it is refused",
            );
        }
        if let Some(m) = self.multiplexer() {
            let _ = write!(
                s,
                "; WARNING: the agent runs in {m}, so the terminal window showing it is \
                 unknown and input to that window is not refused"
            );
        }
        s
    }
}

/// The doctor text for the client tabs found ("claude UI on pts/0 in
/// konsole (2409)").
pub fn describe_tabs(name: &str, tabs: &[ClientTab]) -> String {
    if tabs.is_empty() {
        return "no terminal tab runs one now".into();
    }
    tabs.iter()
        .map(|t| {
            format!(
                "{name} UI on {} in {} ({})",
                t.tty, t.terminal.name, t.terminal.pid
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The refusal for input that `own` says would reach the agent, in
/// `title` ("" when unknown). `allow_file` names the allowlist.
pub fn refusal(own: &Own, title: &str, allow_file: &str) -> String {
    let win = match title.trim() {
        "" => String::new(),
        t => format!(" '{t}'"),
    };
    let (what, instead) = match own {
        Own::Tab(p, tty) => (
            format!(
                "the window in front{win} is {}'s tab running this agent ({tty})",
                p.name
            ),
            "for input meant for another app, pass window=<title or app> to bring it to \
             the front; for another tab of this terminal, bring it to the front first \
             (act on it in the tab bar: find role='page tab' in that app), or ask the user \
             to switch tabs",
        ),
        Own::ClientTab(p, tty) => (
            format!(
                "the window in front{win} is {}'s tab running an interactive client ({tty}) \
                 of the background daemon this agent runs under; which client shows this \
                 agent cannot be told, so none gets input",
                p.name
            ),
            "for input meant for another app, pass window=<title or app> to bring it to \
             the front; for another tab of this terminal, bring it to the front first \
             (act on it in the tab bar: find role='page tab' in that app), or ask the user \
             to switch tabs",
        ),
        Own::SomeTab(p) => (
            format!(
                "the window in front{win} is {}'s, one of whose terminals runs this agent, \
                 and its tab in front cannot be told",
                p.name
            ),
            "for input meant for another app, pass window=<title or app> to bring it to \
             the front; else ask the user",
        ),
        Own::AllTabs(p) => (
            format!(
                "the window in front{win} is {}'s, and every terminal in it runs this agent",
                p.name
            ),
            "open or pick another terminal window for that input, or ask the user",
        ),
        Own::Window(p) => (
            format!(
                "the window in front{win} belongs to {} ({}), the client running this agent",
                p.name, p.pid
            ),
            "bring the window meant for the input to the front (window=<title or app>); \
             the agent's own client is never driven",
        ),
    };
    format!(
        "{what}: input there would reach the agent itself; {instead}. Only the user can \
         turn this off ('{OPT_OUT}' in {allow_file})"
    )
}

/// The refusal for pointer input at `point`, outside the window in front,
/// over the agent's own window `title` (`own`, as `refusal` reads it).
pub fn point_refusal(own: &Own, point: (i64, i64), title: &str, allow_file: &str) -> String {
    let name = match own {
        Own::Window(p)
        | Own::Tab(p, _)
        | Own::ClientTab(p, _)
        | Own::AllTabs(p)
        | Own::SomeTab(p) => &p.name,
    };
    format!(
        "({},{}) is outside the window in front, over {name}'s window '{title}', which runs \
         this agent: a press there would reach the agent itself; pick a point inside the \
         window meant for the input, or bring it to the front (window=<title or app>). Only \
         the user can turn this off ('{OPT_OUT}' in {allow_file})",
        point.0, point.1
    )
}

/// The refusal for an element of the agent's own client.
pub fn element_refusal(own: &Own, allow_file: &str) -> String {
    let name = match own {
        Own::SomeTab(p) => {
            return format!(
                "that entry of {} sends input into its terminals, one of which runs this \
                 agent: the input could reach the agent itself. Only the user can turn this \
                 off ('{OPT_OUT}' in {allow_file})",
                p.name
            );
        }
        Own::Window(p) | Own::Tab(p, _) | Own::ClientTab(p, _) | Own::AllTabs(p) => &p.name,
    };
    format!(
        "that element belongs to {name}, the client running this agent: acting on it would \
         drive the agent itself (its prompts, its approval buttons). Only the user can turn \
         this off ('{OPT_OUT}' in {allow_file})"
    )
}

/// The refusal for closing the agent's own window.
pub fn close_refusal(own: &Own, title: &str, allow_file: &str) -> String {
    let name = match own {
        Own::Window(p)
        | Own::Tab(p, _)
        | Own::ClientTab(p, _)
        | Own::AllTabs(p)
        | Own::SomeTab(p) => &p.name,
    };
    format!(
        "'{title}' is {name}'s window, which runs this agent: closing it would end the \
         agent. Only the user can turn this off ('{OPT_OUT}' in {allow_file})"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn p(pid: u32, name: &str) -> Proc {
        Proc {
            pid,
            name: name.into(),
        }
    }

    /// Claude Code in a Konsole tab, as on 2026-11-20: konsole > fish >
    /// claude > (daemon > pty host > spare) > desktop-mcp.
    fn konsole() -> SelfClient {
        let procs: HashMap<u32, (u32, &str, Option<&str>)> = HashMap::from([
            (61209, (60611, "desktop-mcp", Some("pts/2"))),
            (60611, (60592, "claude", Some("pts/2"))),
            (60592, (60552, "claude", None)),
            (60552, (60418, "claude", None)),
            (60418, (60349, "claude", Some("pts/0"))),
            (60349, (60338, "fish", Some("pts/0"))),
            (60338, (2409, "konsole", None)),
            (2409, (1, "systemd", None)),
        ]);
        SelfClient::walk(60611, |pid| {
            procs
                .get(&pid)
                .map(|(pp, n, t)| (*pp, n.to_string(), t.map(str::to_string)))
        })
    }

    fn view(ptys: &[&str], shown: Option<&str>) -> TermView {
        TermView {
            ptys: ptys.iter().map(|t| t.to_string()).collect(),
            shown: shown.map(str::to_string),
            ..TermView::default()
        }
    }

    fn argv(line: &str) -> Vec<String> {
        vec![line.to_string()]
    }

    const EXE: &str = "/home/u/.local/share/claude/versions/2.1.330";

    /// Claude Code as a background job, as on 2026-11-22: claude daemon
    /// run (a child of systemd --user) > claude bg-pty-host > claude
    /// bg-spare on pts/6 > desktop-mcp. The user looks at `claude` (the
    /// interactive client, no ancestor) in Konsole's tab pts/0.
    fn daemon(env_bg: bool) -> SelfClient {
        let mut me = SelfClient::walk(95355, |pid| match pid {
            95355 => Some((95346, "claude".into(), Some("pts/6".into()))),
            95346 => Some((94198, "claude".into(), None)),
            94198 => Some((3504, "claude".into(), None)),
            _ => Some((1, "systemd".into(), None)),
        });
        me.mark_daemon(env_bg, |pid| {
            let line = match pid {
                95355 => "claude bg-spare --bg-spare /tmp/cc/769e.claim.sock",
                95346 => "claude bg-pty-host --bg-pty-host /tmp/cc/769e.pty.sock 200 50",
                94198 => "/home/u/.local/bin/claude daemon run --origin transient",
                _ => return None,
            };
            Some((EXE.to_string(), argv(line)))
        });
        me
    }

    #[test]
    fn under_a_daemon_every_tab_running_its_client_is_refused() {
        let me = daemon(false);
        assert!(me.daemon);
        assert_eq!(me.client_exes, vec![EXE]);
        assert_eq!(me.client_names, vec!["claude"]);
        assert!(me.describe().starts_with(
            "claude (94198) > claude (95346) > claude (95355) on pts/6; it runs under a \
             background daemon"
        ));
        // The interactive client: same executable, another version beside
        // it, or `claude` by name; not the daemon's own parts.
        assert!(me.is_client(EXE, &argv("claude")));
        assert!(me.is_client(
            "/home/u/.local/share/claude/versions/2.1.331",
            &["/x/y".to_string()]
        ));
        assert!(me.is_client("/opt/claude/bin/claude", &argv("claude")));
        assert!(!me.is_client(EXE, &argv("claude bg-spare --bg-spare /tmp/a")));
        assert!(!me.is_client(EXE, &["claude".into(), "daemon".into(), "run".into()]));
        assert!(!me.is_client("/usr/bin/fish", &argv("/bin/fish")));
        assert!(!me.is_client(
            "/usr/lib/claude-desktop/claude-desktop",
            &argv("/usr/lib/claude-desktop/claude-desktop")
        ));
        // Konsole (pid 2409, no ancestor) with the client on pts/0 in front.
        let kv = TermView {
            clients: vec!["pts/0".into()],
            name: "konsole".into(),
            tabs: Some(vec!["pts/0".into(), "pts/5".into()]),
            ..view(&["pts/0", "pts/5"], Some("pts/0"))
        };
        let own = me.front_owner(2409, &kv);
        assert_eq!(
            own,
            Some(Own::ClientTab(p(2409, "konsole"), "pts/0".into()))
        );
        let r = refusal(&own.unwrap(), "~ : claude — Konsole", "f");
        assert!(
            r.contains("is konsole's tab running an interactive client (pts/0)"),
            "{r}"
        );
        assert!(r.contains("'self-client: allow' in f"), "{r}");
        // The user's other tab stays usable; with the tab in front unknown,
        // refused while one runs a client.
        let other = TermView {
            shown: Some("pts/5".into()),
            ..kv.clone()
        };
        assert_eq!(me.front_owner(2409, &other), None);
        let unknown = TermView {
            shown: None,
            ..kv.clone()
        };
        assert_eq!(
            me.front_owner(2409, &unknown),
            Some(Own::SomeTab(p(2409, "konsole")))
        );
        // Paste into that window is refused, its other entries are not;
        // closing it only closes the client, not the agent.
        assert!(me.element_owner(2409, &other, "Paste").is_some());
        assert_eq!(me.element_owner(2409, &other, "New Tab"), None);
        assert_eq!(me.close_owner(2409, &kv), None);
        // A terminal without a client, or any other window.
        let plain = TermView {
            clients: vec![],
            ..kv.clone()
        };
        assert_eq!(me.front_owner(2409, &plain), None);
        assert_eq!(me.front_owner(7000, &TermView::default()), None);
        // Not under a daemon (the client an ancestor): other agents' tabs
        // are theirs, not refused.
        let mut fg = konsole();
        fg.mark_daemon(false, |_| Some((EXE.into(), argv("claude"))));
        assert!(!fg.daemon);
        assert!(!fg.is_client(EXE, &argv("claude")));
        assert_eq!(
            fg.front_owner(
                7000,
                &TermView {
                    clients: vec!["pts/0".into()],
                    ..view(&["pts/0"], Some("pts/0"))
                }
            ),
            None
        );
        // The environment alone (CLAUDE_CODE_SESSION_KIND=bg) is enough.
        let mut env = SelfClient::walk(5, |_| None);
        env.mark_daemon(true, |_| None);
        assert!(env.daemon);
        assert!(env.is_client("/bin/claude", &argv("claude")));
        assert_eq!(
            describe_tabs(
                "claude",
                &[ClientTab {
                    tty: "pts/0".into(),
                    terminal: p(2409, "konsole")
                }]
            ),
            "claude UI on pts/0 in konsole (2409)"
        );
        assert_eq!(describe_tabs("claude", &[]), "no terminal tab runs one now");
    }

    #[test]
    fn the_walk_stops_below_the_session_and_keeps_each_terminal_once() {
        let me = konsole();
        assert_eq!(me.chain.len(), 6);
        assert_eq!(me.chain[0], p(60611, "claude"));
        assert_eq!(me.chain[5], p(60338, "konsole"));
        assert_eq!(me.multiplexer(), None);
        assert_eq!(me.ttys, vec!["pts/2", "pts/0"]);
        assert_eq!(
            me.describe(),
            "konsole (60338) > fish (60349) > claude (60418) > claude (60552) > \
             claude (60592) > claude (60611) on pts/2, pts/0"
        );
        // Windows names lose ".exe"; explorer ends the chain.
        let procs = HashMap::from([
            (10, (20, "pwsh.exe")),
            (20, (30, "WindowsTerminal.exe")),
            (30, (40, "explorer.exe")),
        ]);
        let me = SelfClient::walk(10, |pid| {
            procs.get(&pid).map(|(pp, n)| (*pp, n.to_string(), None))
        });
        assert_eq!(me.chain, vec![p(10, "pwsh"), p(20, "windowsterminal")]);
        // A loop or an unreadable process ends it too.
        let me = SelfClient::walk(5, |pid| {
            Some((if pid == 5 { 6 } else { 5 }, "x".into(), None))
        });
        assert_eq!(me.chain.len(), 2);
        assert!(SelfClient::walk(5, |_| None).is_empty());
        // In tmux: its server is a child of systemd, the terminal unknown.
        let me = SelfClient::walk(10, |pid| match pid {
            10 => Some((20, "claude".into(), Some("pts/3".into()))),
            20 => Some((30, "tmux: server".into(), None)),
            _ => Some((1, "systemd".into(), None)),
        });
        assert_eq!(me.multiplexer(), Some("tmux: server"));
        assert!(me.describe().contains("WARNING: the agent runs in tmux"));
    }

    #[test]
    fn only_the_tab_running_the_agent_is_refused() {
        let me = konsole();
        let all = ["pts/0", "pts/2", "pts/5"];
        // The incident: the Claude Code tab in front.
        assert_eq!(
            me.front_owner(60338, &view(&all, Some("pts/0"))),
            Some(Own::Tab(p(60338, "konsole"), "pts/0".into()))
        );
        // The user's own fish tab, for sudo commands, works.
        assert_eq!(me.front_owner(60338, &view(&all, Some("pts/5"))), None);
        // Tab unknown (kitty, foot's server, Konsole not answering): refused
        // while any of its terminals runs the agent.
        assert_eq!(
            me.front_owner(60338, &view(&all, None)),
            Some(Own::SomeTab(p(60338, "konsole")))
        );
        assert_eq!(
            me.front_owner(60338, &view(&["pts/5", "pts/7"], None)),
            None
        );
        assert_eq!(
            me.front_owner(60338, &view(&["pts/0", "pts/2"], None)),
            Some(Own::AllTabs(p(60338, "konsole")))
        );
        // Another Konsole process, or any other app.
        assert_eq!(me.front_owner(7000, &view(&["pts/0"], Some("pts/0"))), None);
        assert_eq!(me.front_owner(0, &view(&[], None)), None);
        // Its tabs and menus (elements) stay usable, but not the entries
        // that send input into its terminals; closing the window does not.
        assert_eq!(
            me.element_owner(60338, &view(&all, None), "Yeni Sekme"),
            None
        );
        for entry in ["Paste", "Yapıştır", "Copy Input To", "Paste Selection"] {
            assert_eq!(
                me.element_owner(60338, &view(&all, None), entry),
                Some(Own::SomeTab(p(60338, "konsole"))),
                "{entry}"
            );
        }
        assert_eq!(
            me.element_owner(60338, &view(&["pts/5"], None), "Paste"),
            None
        );
        assert!(me.close_owner(60338, &view(&all, None)).is_some());
        let other_window = TermView {
            tabs: Some(vec!["pts/7".into()]),
            ..view(&all, Some("pts/7"))
        };
        assert_eq!(me.close_owner(60338, &other_window), None);
    }

    #[test]
    fn a_reused_parent_pid_ends_the_walk() {
        // Claude.exe (started at 50) by an Update.exe since gone, its pid
        // now Notepad's (started at 90).
        assert_eq!(live_parent(7, Some(50), Some(90)), 0);
        assert_eq!(live_parent(7, Some(50), Some(10)), 7);
        assert_eq!(live_parent(7, None, Some(90)), 7);
        assert_eq!(live_parent(7, Some(50), None), 7);
    }

    #[test]
    fn a_client_window_is_refused_but_tab_hosts_only_when_known() {
        // Claude Desktop runs the server: its window and elements.
        let desktop = SelfClient::walk(11742, |pid| match pid {
            11742 => Some((10936, "disclaimer".into(), None)),
            10936 => Some((2409, "claude-desktop".into(), None)),
            _ => Some((1, "systemd".into(), None)),
        });
        let none = TermView::default();
        let own = Some(Own::Window(p(10936, "claude-desktop")));
        assert_eq!(desktop.front_owner(10936, &none), own);
        assert_eq!(desktop.element_owner(10936, &none, "Send"), own);
        assert_eq!(desktop.close_owner(10936, &none), own);
        // Windows Terminal: its tab in front cannot be read.
        let wt = SelfClient::walk(10, |pid| match pid {
            10 => Some((20, "claude.exe".into(), None)),
            20 => Some((30, "pwsh.exe".into(), None)),
            30 => Some((40, "WindowsTerminal.exe".into(), None)),
            _ => None,
        });
        assert_eq!(wt.front_owner(30, &none), None);
        assert_eq!(wt.element_owner(30, &none, "Paste"), None);
        assert!(
            wt.close_owner(30, &none).is_some(),
            "closing ends the agent"
        );
        // A classic console window reports its shell.
        assert_eq!(wt.front_owner(20, &none), Some(Own::Window(p(20, "pwsh"))));
    }

    #[test]
    fn refusals_say_what_to_do_instead() {
        let own = Own::Tab(p(60338, "konsole"), "pts/0".into());
        let r = refusal(&own, "~ : claude — Konsole", "/a/allow.txt");
        assert!(r.starts_with("the window in front '~ : claude"), "{r}");
        assert!(r.contains("'~ : claude — Konsole' is konsole's tab running this agent (pts/0)"));
        assert!(r.contains("role='page tab'"), "{r}");
        assert!(r.contains("pass window=<title or app>"), "{r}");
        assert!(r.contains("'self-client: allow' in /a/allow.txt"), "{r}");
        let r = refusal(&Own::SomeTab(p(9, "kitty")), "claude", "f");
        assert!(
            r.contains("is kitty's, one of whose terminals runs this agent"),
            "{r}"
        );
        let r = element_refusal(&Own::SomeTab(p(9, "konsole")), "f");
        assert!(r.starts_with("that entry of konsole sends input"), "{r}");
        let r = element_refusal(&Own::Window(p(1, "claude-desktop")), "f");
        assert!(
            r.starts_with("that element belongs to claude-desktop"),
            "{r}"
        );
        let r = close_refusal(&Own::Window(p(1, "claude-desktop")), "Claude", "f");
        assert!(r.contains("closing it would end the agent"), "{r}");
    }
}
