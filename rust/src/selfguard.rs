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
}

/// Why a window counts as the agent's own.
#[derive(Clone, Debug, PartialEq)]
pub enum Own {
    /// The window of an ancestor process that is no terminal (Claude
    /// Desktop running the server).
    Window(Proc),
    /// A terminal whose tab in front runs the agent.
    Tab(Proc, String),
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

    /// The ancestor with this pid.
    pub fn ancestor(&self, pid: u32) -> Option<&Proc> {
        (pid != 0)
            .then(|| self.chain.iter().find(|p| p.pid == pid))
            .flatten()
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
    /// count.
    pub fn front_owner(&self, pid: u32, view: &TermView) -> Option<Own> {
        let p = self.ancestor(pid)?;
        if let Some(t) = &view.shown {
            return self.ours(t).then(|| Own::Tab(p.clone(), t.clone()));
        }
        if !view.ptys.is_empty() {
            if view.ptys.iter().all(|t| self.ours(t)) {
                return Some(Own::AllTabs(p.clone()));
            }
            return view
                .ptys
                .iter()
                .any(|t| self.ours(t))
                .then(|| Own::SomeTab(p.clone()));
        }
        (!TAB_HOSTS.contains(&p.name.as_str())).then(|| Own::Window(p.clone()))
    }

    /// Whether an element of `pid`'s app, named `name`, is part of the
    /// agent's client itself (Claude Desktop's own buttons): an ancestor
    /// without terminals. A terminal's own elements (its tabs, menus) are
    /// not, except the entries that send input into its terminals
    /// (`input_entry`: Paste, Copy Input To) while one runs the agent.
    pub fn element_owner(&self, pid: u32, view: &TermView, name: &str) -> Option<Own> {
        let p = self.ancestor(pid)?;
        if !view.ptys.is_empty() {
            return (input_entry(name) && view.ptys.iter().any(|t| self.ours(t)))
                .then(|| Own::SomeTab(p.clone()));
        }
        (!TAB_HOSTS.contains(&p.name.as_str())).then(|| Own::Window(p.clone()))
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
        Own::Window(p) | Own::Tab(p, _) | Own::AllTabs(p) | Own::SomeTab(p) => &p.name,
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
        Own::Window(p) | Own::Tab(p, _) | Own::AllTabs(p) => &p.name,
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
        Own::Window(p) | Own::Tab(p, _) | Own::AllTabs(p) | Own::SomeTab(p) => &p.name,
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
            tabs: None,
        }
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
