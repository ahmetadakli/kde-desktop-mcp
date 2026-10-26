//! Starting apps: resolve a name to an argv through .desktop files or PATH
//! (never a shell), spawn it detached, and pick the window it opens.

use std::collections::HashSet;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::backend::{R, WindowGeo};

/// What to run: argv, the .desktop file it came from and its working dir.
#[derive(Debug, PartialEq)]
pub struct Resolved {
    pub argv: Vec<String>,
    pub desktop: Option<PathBuf>,
    pub dir: Option<PathBuf>,
}

/// The keys of a desktop file's [Desktop Entry] group this needs.
#[derive(Debug, Default, PartialEq)]
pub struct Entry {
    pub name: String,
    pub exec: String,
    pub try_exec: String,
    pub wm_class: String,
    pub icon: String,
    pub path: String,
    pub application: bool,
    pub hidden: bool,
    pub no_display: bool,
}

/// Parse the [Desktop Entry] group; values keep their \s \n \t \\ escapes
/// decoded. Localized keys (Name[de]) are ignored.
pub fn parse_entry(text: &str) -> Entry {
    let mut e = Entry::default();
    let mut in_group = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_group = line == "[Desktop Entry]";
            continue;
        }
        let Some((k, v)) = line.split_once('=').filter(|_| in_group) else {
            continue;
        };
        let v = unescape_value(v.trim());
        match k.trim() {
            "Name" => e.name = v,
            "Exec" => e.exec = v,
            "TryExec" => e.try_exec = v,
            "StartupWMClass" => e.wm_class = v,
            "Icon" => e.icon = v,
            "Path" => e.path = v,
            "Type" => e.application = v == "Application",
            "Hidden" => e.hidden = v == "true",
            "NoDisplay" => e.no_display = v == "true",
            _ => {}
        }
    }
    e
}

/// The string-value escapes of the desktop entry spec.
fn unescape_value(v: &str) -> String {
    let mut out = String::new();
    let mut it = v.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('s') => out.push(' '),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some(o) => out.push(o),
            None => out.push('\\'),
        }
    }
    out
}

/// Split an Exec value into words: spaces separate, double quotes group and
/// inside them a backslash escapes `"`, `` ` ``, `$` and `\`.
pub fn split_exec(exec: &str) -> R<Vec<String>> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let (mut quoted, mut started) = (false, false);
    let mut it = exec.chars();
    while let Some(c) = it.next() {
        match c {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            '\\' if quoted => match it.next() {
                Some(n) => cur.push(n),
                None => return Err("Exec ends inside an escape".into()),
            },
            ' ' | '\t' if !quoted => {
                if started {
                    words.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            _ => {
                cur.push(c);
                started = true;
            }
        }
    }
    if quoted {
        return Err("Exec has an unterminated quote".into());
    }
    if started {
        words.push(cur);
    }
    Ok(words)
}

/// Expand field codes in Exec words: the first file/URL code (%f %F %u %U)
/// becomes `args`, other file codes vanish, %i the icon, %c the name, %k
/// the file, %% a percent sign; deprecated codes are dropped. `args` go at
/// the end when there was no file code. Words left empty are removed.
pub fn expand_exec(words: &[String], args: &[String], e: &Entry, file: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut placed = false;
    for w in words {
        if matches!(w.as_str(), "%f" | "%F" | "%u" | "%U") {
            if !placed {
                out.extend(args.iter().cloned());
                placed = true;
            }
            continue;
        }
        if w == "%i" {
            if !e.icon.is_empty() {
                out.extend(["--icon".to_string(), e.icon.clone()]);
            }
            continue;
        }
        let mut s = String::new();
        let mut it = w.chars();
        while let Some(c) = it.next() {
            if c != '%' {
                s.push(c);
                continue;
            }
            match it.next() {
                Some('%') => s.push('%'),
                Some('c') => s.push_str(&e.name),
                Some('k') => s.push_str(file),
                // %f inside a word, %i inside a word, deprecated codes.
                Some(_) | None => {}
            }
        }
        if !s.is_empty() {
            out.push(s);
        }
    }
    if !placed {
        out.extend(args.iter().cloned());
    }
    out
}

/// How well a desktop entry matches `app`, lower is better; None for no
/// match. File name, then its last dotted part (org.kde.kate), then the
/// binary Exec runs (flatpak's --command), StartupWMClass and Name.
pub fn match_rank(app: &str, stem: &str, e: &Entry) -> Option<u8> {
    let app = app.to_lowercase();
    let last = |s: &str| s.rsplit('.').next().unwrap_or("").to_lowercase();
    let stem_l = stem.to_lowercase();
    let words = split_exec(&e.exec).unwrap_or_default();
    let rank = if stem_l == app {
        0
    } else if last(&stem_l) == app {
        1
    } else if exec_binary(&words).is_some_and(|b| b == app) {
        2
    } else if !e.wm_class.is_empty()
        && (e.wm_class.to_lowercase() == app || last(&e.wm_class) == app)
    {
        3
    } else if e.name.to_lowercase() == app {
        4
    } else {
        return None;
    };
    // Prefer entries shown in menus over helper entries for the same app.
    Some(rank * 2 + u8::from(e.no_display))
}

/// Lower-case name of the program Exec runs, looking through `env VAR=x`
/// and `flatpak run --command=x`.
fn exec_binary(words: &[String]) -> Option<String> {
    let base = |w: &str| Path::new(w).file_name()?.to_str().map(str::to_lowercase);
    let mut rest = words.iter().map(String::as_str);
    let mut prog = base(rest.next()?)?;
    if prog == "env" {
        let w = rest.find(|w| !w.contains('=') && !w.starts_with('-'))?;
        prog = base(w)?;
    }
    if prog == "flatpak" {
        let words: Vec<&str> = rest.collect();
        if let Some(c) = words.iter().find_map(|w| w.strip_prefix("--command=")) {
            return base(c);
        }
        let id = words.iter().skip(1).find(|w| !w.starts_with('-'))?;
        return id.rsplit('.').next().map(str::to_lowercase);
    }
    Some(prog)
}

/// `applications` directories in XDG precedence order.
fn application_dirs() -> Vec<PathBuf> {
    let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty());
    let home = var("HOME").map(PathBuf::from).unwrap_or_default();
    let mut dirs = vec![
        var("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/share")),
    ];
    let data = var("XDG_DATA_DIRS").unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    dirs.extend(std::env::split_paths(&data));
    dirs.into_iter().map(|d| d.join("applications")).collect()
}

/// Desktop files in `dirs` (and their direct subdirectories) as
/// (desktop id, path); an id found earlier shadows later ones.
fn desktop_files(dirs: &[PathBuf]) -> Vec<(String, PathBuf)> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for dir in dirs {
        let mut found = Vec::new();
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                let prefix = e.file_name().to_string_lossy().into_owned();
                for s in std::fs::read_dir(&p).into_iter().flatten().flatten() {
                    let name = s.file_name().to_string_lossy().into_owned();
                    found.push((format!("{prefix}-{name}"), s.path()));
                }
            } else {
                found.push((e.file_name().to_string_lossy().into_owned(), p));
            }
        }
        found.sort();
        for (id, p) in found {
            if id.ends_with(".desktop") && seen.insert(id.clone()) {
                out.push((id, p));
            }
        }
    }
    out
}

/// A program on PATH (or an absolute path) that is an executable file.
fn on_path(cmd: &str) -> Option<PathBuf> {
    let exe = |p: &Path| {
        use std::os::unix::fs::PermissionsExt;
        p.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    if cmd.contains('/') {
        let p = PathBuf::from(cmd);
        return exe(&p).then_some(p);
    }
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|d| d.join(cmd))
        .find(|p| exe(p))
}

/// Best desktop entry for `app` in `dirs`, then a PATH binary named `app`.
pub fn resolve_in(app: &str, args: &[String], dirs: &[PathBuf]) -> R<Resolved> {
    let mut best: Option<(u8, PathBuf, Entry, Vec<String>)> = None;
    for (id, path) in desktop_files(dirs) {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let e = parse_entry(&text);
        if !e.application || e.hidden || e.exec.is_empty() {
            continue;
        }
        let stem = id.trim_end_matches(".desktop");
        let Some(rank) = match_rank(app, stem, &e) else {
            continue;
        };
        if best.as_ref().is_some_and(|b| b.0 <= rank) {
            continue;
        }
        if !e.try_exec.is_empty() && on_path(&e.try_exec).is_none() {
            continue;
        }
        let Ok(words) = split_exec(&e.exec) else {
            continue;
        };
        if words.first().is_none_or(|p| on_path(p).is_none()) {
            continue;
        }
        best = Some((rank, path, e, words));
    }
    if let Some((_, path, e, words)) = best {
        let argv = expand_exec(&words, args, &e, &path.to_string_lossy());
        let dir = Some(PathBuf::from(&e.path)).filter(|d| d.is_dir());
        return Ok(Resolved {
            argv,
            desktop: Some(path),
            dir,
        });
    }
    match on_path(app) {
        Some(p) => Ok(Resolved {
            argv: std::iter::once(p.to_string_lossy().into_owned())
                .chain(args.iter().cloned())
                .collect(),
            desktop: None,
            dir: None,
        }),
        None => Err(format!(
            "no .desktop entry or program on PATH named '{app}'"
        )),
    }
}

pub fn resolve(app: &str, args: &[String]) -> R<Resolved> {
    resolve_in(app, args, &application_dirs())
}

unsafe extern "C" {
    fn setsid() -> i32;
}

/// Exit status once the process has ended; it is reaped by a thread, so no
/// zombie is left behind.
pub type Exit = Arc<Mutex<Option<ExitStatus>>>;

/// Start `r` in its own session with no stdio, so it outlives the server.
pub fn spawn(r: &Resolved) -> R<(u32, Exit)> {
    let mut cmd = Command::new(&r.argv[0]);
    cmd.args(&r.argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    match &r.dir {
        Some(d) => {
            cmd.current_dir(d);
        }
        None => {
            if let Some(h) = std::env::var_os("HOME") {
                cmd.current_dir(h);
            }
        }
    }
    // SAFETY: setsid is async-signal-safe and touches no memory of ours.
    unsafe {
        cmd.pre_exec(|| {
            setsid();
            Ok(())
        });
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("could not start {}: {e}", r.argv[0]))?;
    let pid = child.id();
    let exit: Exit = Arc::new(Mutex::new(None));
    let done = exit.clone();
    std::thread::spawn(move || {
        if let Ok(st) = child.wait() {
            *done.lock().unwrap() = Some(st);
        }
    });
    Ok((pid, exit))
}

/// `roots` and every process below them, from (pid, parent pid) pairs.
pub fn descendants(
    roots: &HashSet<u32>,
    parents: impl IntoIterator<Item = (u32, u32)>,
) -> HashSet<u32> {
    let pairs: Vec<(u32, u32)> = parents.into_iter().collect();
    let mut out = roots.clone();
    loop {
        let before = out.len();
        for &(p, pp) in &pairs {
            if out.contains(&pp) {
                out.insert(p);
            }
        }
        if out.len() == before {
            return out;
        }
    }
}

/// argv as one line, quoting words with spaces, plus the desktop file.
pub fn describe(r: &Resolved) -> String {
    let words: Vec<String> = r
        .argv
        .iter()
        .map(|w| {
            if w.is_empty() || w.contains(char::is_whitespace) {
                format!("{w:?}")
            } else {
                w.clone()
            }
        })
        .collect();
    let mut s = words.join(" ");
    if let Some(name) = r.desktop.as_ref().and_then(|d| d.file_name()) {
        s.push_str(&format!(" (from {})", name.to_string_lossy()));
    }
    s
}

/// Whether a window's KWin class belongs to `app`, as the allowlist
/// matches it in KWin scripts.
fn class_matches(w: &WindowGeo, app: &str) -> bool {
    w.app.to_lowercase().contains(app)
}

/// How often `launch` looks for the new window while one usually appears
/// (FAST_PHASE after the start), and after that.
const FAST_POLL: Duration = Duration::from_millis(50);
const SLOW_POLL: Duration = Duration::from_millis(150);
const FAST_PHASE: Duration = Duration::from_secs(2);
/// How long `launch` waits for a new window before taking the app's active
/// window when it did not change, and before calling a failed exit final.
pub const HANDOFF_GRACE: Duration = Duration::from_millis(1500);

/// Pause before the next look for the window.
pub fn poll_interval(since_start: Duration) -> Duration {
    if since_start < FAST_PHASE {
        FAST_POLL
    } else {
        SLOW_POLL
    }
}

/// How far the start has come: `exit` is (succeeded, time since) once the
/// started process ended. A clean exit right away means it handed the
/// request to a running instance, so a changed window counts from then on;
/// an unchanged one only after HANDOFF_GRACE, in case the instance is
/// still opening a new window.
pub fn stage(since_start: Duration, exit: Option<(bool, Duration)>) -> Stage {
    match exit {
        Some((true, since)) if since >= HANDOFF_GRACE => Stage::Ended,
        Some((true, _)) => Stage::Settled,
        _ if since_start >= HANDOFF_GRACE => Stage::Settled,
        _ => Stage::Starting,
    }
}

/// Whether to read /proc again for the started process' descendants after
/// `stable` rounds in which the tree did not grow: every round at first,
/// then every fourth (a candidate window of another pid forces a read).
pub fn tree_scan_due(stable: u32) -> bool {
    stable < 3 || stable.is_multiple_of(4)
}

/// Lower-case file names among `args` (not options, not URLs): a running
/// instance that took the request usually shows them in its title.
pub fn arg_hints(args: &[String]) -> Vec<String> {
    args.iter()
        .filter(|a| !a.starts_with('-') && !a.contains("://"))
        .filter_map(|a| Path::new(a).file_name()?.to_str().map(str::to_lowercase))
        // Too short to tell a title apart.
        .filter(|n| n.chars().count() >= 3)
        .collect()
}

/// How a launched app's window was found.
#[derive(Debug, PartialEq)]
pub enum Found {
    /// Opened by the started process or one of its children.
    Own,
    /// A new window of the app from another process (a running instance).
    New,
    /// No new window, but the app's window in front is new or changed: a
    /// running instance took the request (e.g. `kate -b file` opens a tab).
    Existing,
}

/// How far the start has come when looking for its window.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Stage {
    /// Just started: its own or a new window counts, and the app's active
    /// window once it changed to a title naming one of the files.
    Starting,
    /// It ended cleanly, or a while passed: also the app's active window
    /// when it changed or names one of the files.
    Settled,
    /// It ended cleanly a while ago: the app's active window.
    Ended,
}

/// The launched app's window in `now`: one owned by `tree`, else a window of
/// the app that was not in `before` once their number grew, else (by
/// `stage`, and whether its title names one of `hints`, see `arg_hints`)
/// the app's active window.
pub fn pick_window<'a>(
    app: &str,
    before: &[WindowGeo],
    now: &'a [WindowGeo],
    tree: &HashSet<u32>,
    stage: Stage,
    hints: &[String],
) -> Option<(&'a WindowGeo, Found)> {
    let first = |mut v: Vec<&'a WindowGeo>| {
        v.sort_by_key(|w| !w.active);
        v.into_iter().next()
    };
    let own: Vec<_> = now.iter().filter(|w| tree.contains(&w.pid)).collect();
    if let Some(w) = first(own) {
        return Some((w, Found::Own));
    }
    let of_app: Vec<_> = now.iter().filter(|w| class_matches(w, app)).collect();
    let had = before.iter().filter(|w| class_matches(w, app)).count();
    // Identity across polls: KWin gives no stable id, so pid and title.
    let key = |w: &WindowGeo| (w.pid, w.title.clone());
    let old: HashSet<_> = before.iter().map(key).collect();
    let was_active: HashSet<_> = before.iter().filter(|w| w.active).map(key).collect();
    if of_app.len() > had {
        let new: Vec<_> = of_app
            .iter()
            .copied()
            .filter(|w| !old.contains(&key(w)))
            .collect();
        if let Some(w) = first(new) {
            return Some((w, Found::New));
        }
    }
    let active = of_app.into_iter().find(|w| w.active)?;
    let changed = !was_active.contains(&key(active));
    let title = active.title.to_lowercase();
    let named = hints.iter().any(|h| title.contains(h.as_str()));
    let ok = match stage {
        Stage::Starting => changed && named,
        Stage::Settled => changed || named,
        Stage::Ended => true,
    };
    ok.then_some((active, Found::Existing))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn exec_words_split_on_spaces_and_respect_quotes() {
        assert_eq!(
            split_exec(r#"kate -b %U"#).unwrap(),
            s(&["kate", "-b", "%U"])
        );
        assert_eq!(
            split_exec(r#""/opt/My App/run" --x="a \"b\"" "" end"#).unwrap(),
            s(&["/opt/My App/run", r#"--x=a "b""#, "", "end"])
        );
        assert!(split_exec(r#"a "b"#).is_err());
    }

    #[test]
    fn field_codes_become_args_or_vanish() {
        let e = Entry {
            name: "Kate".into(),
            icon: "kate".into(),
            ..Entry::default()
        };
        let args = s(&["/tmp/x.txt", "b"]);
        let words = s(&["kate", "-b", "%U"]);
        assert_eq!(
            expand_exec(&words, &args, &e, "/f.desktop"),
            s(&["kate", "-b", "/tmp/x.txt", "b"])
        );
        // No file code: args go last; %i, %c, %k, %% and old codes.
        let words = s(&["app", "%i", "--title=%c", "%k", "100%%", "%d", "%m"]);
        assert_eq!(
            expand_exec(&words, &args, &e, "/f.desktop"),
            s(&[
                "app",
                "--icon",
                "kate",
                "--title=Kate",
                "/f.desktop",
                "100%",
                "/tmp/x.txt",
                "b"
            ])
        );
        // Flatpak wraps the file code; args stay inside, a second code is dropped.
        let words = s(&["flatpak", "run", "org.x.Y", "@@u", "%u", "@@", "%f"]);
        assert_eq!(
            expand_exec(&words, &s(&["https://a"]), &e, ""),
            s(&["flatpak", "run", "org.x.Y", "@@u", "https://a", "@@"])
        );
    }

    #[test]
    fn desktop_entry_group_and_escapes() {
        let text = "# c\n[Desktop Entry]\nType=Application\nName=Kate\nName[de]=X\n\
                    Exec=kate -b %U\nStartupWMClass=kate\nPath=/a\\sb\nNoDisplay=true\n\
                    [Desktop Action new]\nExec=kate -n\n";
        let e = parse_entry(text);
        assert_eq!(e.name, "Kate");
        assert_eq!(e.exec, "kate -b %U");
        assert_eq!(e.wm_class, "kate");
        assert_eq!(e.path, "/a b");
        assert!(e.application && e.no_display && !e.hidden);
    }

    #[test]
    fn desktop_files_match_by_id_binary_class_or_name() {
        let entry = |exec: &str, class: &str, name: &str| Entry {
            exec: exec.into(),
            wm_class: class.into(),
            name: name.into(),
            application: true,
            ..Entry::default()
        };
        let plain = entry("x", "", "");
        assert_eq!(match_rank("kate", "kate", &plain), Some(0));
        assert_eq!(match_rank("kate", "org.kde.kate", &plain), Some(2));
        let ff = entry("/usr/lib/firefox/firefox %u", "", "");
        assert_eq!(match_rank("firefox", "firefox-esr", &ff), Some(4));
        let env = entry("env GDK_BACKEND=x11 /usr/bin/gimp %U", "", "");
        assert_eq!(match_rank("gimp", "gimp-2", &env), Some(4));
        let fp = entry(
            "/usr/bin/flatpak run --branch=stable --command=firefox org.mozilla.firefox @@u %u @@",
            "",
            "",
        );
        assert_eq!(match_rank("firefox", "flatpak-ff", &fp), Some(4));
        let fp2 = entry("flatpak run org.gnome.Calculator", "", "");
        assert_eq!(match_rank("calculator", "zz", &fp2), Some(4));
        let cls = entry("x", "Telegram", "");
        assert_eq!(match_rank("telegram", "zz", &cls), Some(6));
        let named = entry("x", "", "Dolphin");
        assert_eq!(match_rank("dolphin", "zz", &named), Some(8));
        assert_eq!(
            match_rank("kate", "kwrite", &entry("kwrite", "", "KWrite")),
            None
        );
        let hidden = Entry {
            no_display: true,
            ..plain
        };
        assert_eq!(match_rank("kate", "kate", &hidden), Some(1));
    }

    #[test]
    fn resolves_from_desktop_files_with_precedence() {
        let root = std::env::temp_dir().join(format!("desktop-mcp-launch-{}", std::process::id()));
        let (a, b) = (root.join("a"), root.join("b"));
        std::fs::create_dir_all(a.join("kde4")).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let write = |p: PathBuf, exec: &str| {
            std::fs::write(
                p,
                format!("[Desktop Entry]\nType=Application\nName=T\nExec={exec}\n"),
            )
            .unwrap();
        };
        // `sh` exists everywhere; the earlier dir wins for the same id.
        write(a.join("tool.desktop"), "sh -c true %f");
        write(b.join("tool.desktop"), "sh -c false");
        write(a.join("kde4/other.desktop"), "sh -x");
        write(b.join("gone.desktop"), "/no/such/binary %U");
        let dirs = [a.clone(), b.clone()];
        let r = resolve_in("tool", &s(&["f"]), &dirs).unwrap();
        assert_eq!(r.argv, s(&["sh", "-c", "true", "f"]));
        assert_eq!(r.desktop, Some(a.join("tool.desktop")));
        // A subdirectory prefixes the id.
        assert_eq!(
            resolve_in("kde4-other", &[], &dirs).unwrap().desktop,
            Some(a.join("kde4/other.desktop"))
        );
        // Missing binary: no desktop match, then PATH (`gone` is not on it).
        assert!(resolve_in("gone", &[], &dirs).is_err());
        // `sh` matches an entry by its Exec binary; with none, PATH.
        let sh = resolve_in("sh", &[], &dirs).unwrap();
        assert_eq!(sh.desktop, Some(a.join("kde4/other.desktop")));
        let sh = resolve_in("sh", &s(&["-c", "x"]), &[]).unwrap();
        assert!(
            sh.desktop.is_none() && sh.argv[0].ends_with("/sh"),
            "{sh:?}"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn descendants_follow_parent_links() {
        let pairs = [(11, 10), (12, 11), (13, 1), (14, 12)];
        let got = descendants(&HashSet::from([10]), pairs);
        assert_eq!(got, HashSet::from([10, 11, 12, 14]));
    }

    fn win(app: &str, title: &str, pid: u32, active: bool) -> WindowGeo {
        WindowGeo {
            app: app.into(),
            title: title.into(),
            pid,
            active,
            ..WindowGeo::default()
        }
    }

    #[test]
    fn picks_own_then_new_then_handed_off_window() {
        let tree = HashSet::from([50, 51]);
        let old = win("org.kde.kate", "a.txt — Kate", 10, false);
        let own = win("org.kde.kate", "x.txt — Kate", 51, false);
        let now = [old.clone(), own.clone()];
        let got = pick_window(
            "kate",
            std::slice::from_ref(&old),
            &now,
            &tree,
            Stage::Starting,
            &[],
        );
        assert_eq!(got, Some((&own, Found::Own)));

        // Handed to a running instance that opened a second window.
        let new = win("firefox", "New Tab", 10, true);
        let before = [win("firefox", "Mail", 10, false)];
        let now = [before[0].clone(), new.clone()];
        let got = pick_window("firefox", &before, &now, &tree, Stage::Starting, &[]);
        assert_eq!(got, Some((&new, Found::New)));

        // The running instance's window came to the front with a new title
        // (kate -b opened a tab): only counted once settled.
        let now = [win("firefox", "Mail (1)", 10, true)];
        let pick = |stage| pick_window("firefox", &before, &now, &tree, stage, &[]);
        assert_eq!(pick(Stage::Starting), None);
        assert_eq!(pick(Stage::Settled), Some((&now[0], Found::Existing)));
        assert_eq!(pick(Stage::Ended), Some((&now[0], Found::Existing)));

        // Unchanged and already in front: only once the process ended.
        let before = [win("firefox", "Mail", 10, true)];
        let now = before.clone();
        let pick = |stage| pick_window("firefox", &before, &now, &tree, stage, &[]);
        assert_eq!(pick(Stage::Settled), None);
        assert_eq!(pick(Stage::Ended), Some((&now[0], Found::Existing)));

        // Other apps never count.
        let now = [win("konsole", "x", 99, true)];
        assert_eq!(
            pick_window("kate", &[], &now, &tree, Stage::Ended, &[]),
            None
        );
    }

    #[test]
    fn handoff_counts_as_soon_as_the_title_names_the_file() {
        let tree = HashSet::from([50]);
        let hints = arg_hints(&s(&["-b", "/tmp/dir/Notes.TXT", "https://x/y"]));
        assert_eq!(hints, s(&["notes.txt"]));
        // `kate -b file` keeps running while Kate's window shows the file.
        let before = [win("org.kde.kate", "a.txt — Kate", 10, true)];
        let now = [win("org.kde.kate", "Notes.txt — Kate", 10, true)];
        let pick =
            |stage, hints: &[String]| pick_window("kate", &before, &now, &tree, stage, hints);
        assert_eq!(
            pick(Stage::Starting, &hints),
            Some((&now[0], Found::Existing))
        );
        // A change that names no file waits for an exit or the grace time.
        assert_eq!(pick(Stage::Starting, &s(&["other.txt"])), None);
        assert_eq!(
            pick(Stage::Settled, &s(&["other.txt"])),
            Some((&now[0], Found::Existing))
        );
        // Already in front with the file: not while just started (a new
        // process may still open its own window), once settled.
        let same = [win("org.kde.kate", "Notes.txt — Kate", 10, true)];
        let pick = |stage| pick_window("kate", &same, &same, &tree, stage, &hints);
        assert_eq!(pick(Stage::Starting), None);
        assert_eq!(pick(Stage::Settled), Some((&same[0], Found::Existing)));
        // Its own window still wins over a renamed one of the instance.
        let own = win("org.kde.kate", "Notes.txt — Kate", 50, false);
        let now = [now[0].clone(), own.clone()];
        assert_eq!(
            pick_window("kate", &before, &now, &tree, Stage::Starting, &hints),
            Some((&own, Found::Own))
        );
    }

    #[test]
    fn stage_follows_exit_and_time() {
        let ms = Duration::from_millis;
        assert_eq!(stage(ms(100), None), Stage::Starting);
        assert_eq!(stage(ms(1500), None), Stage::Settled);
        // A clean exit settles at once and ends after the grace time.
        assert_eq!(stage(ms(200), Some((true, ms(0)))), Stage::Settled);
        assert_eq!(stage(ms(1800), Some((true, ms(1500)))), Stage::Ended);
        // A failed exit never makes an unchanged window count.
        assert_eq!(stage(ms(200), Some((false, ms(0)))), Stage::Starting);
        assert_eq!(stage(ms(3000), Some((false, ms(2000)))), Stage::Settled);
        assert_eq!(poll_interval(ms(0)), FAST_POLL);
        assert_eq!(poll_interval(ms(2500)), SLOW_POLL);
    }

    #[test]
    fn tree_is_rescanned_while_it_grows_then_now_and_then() {
        let due: Vec<u32> = (0..10).filter(|n| tree_scan_due(*n)).collect();
        assert_eq!(due, vec![0, 1, 2, 4, 8]);
    }
}
