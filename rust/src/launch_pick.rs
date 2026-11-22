//! Picking the window a started app opened, shared by the platforms'
//! `launch`: the started process' tree, its stage, and the window rules.

use std::collections::HashSet;
use std::time::Duration;

use crate::backend::WindowGeo;

/// argv as one line, quoting words that are empty or hold whitespace.
pub fn command_line(argv: &[String]) -> String {
    let words: Vec<String> = argv
        .iter()
        .map(|w| {
            if w.is_empty() || w.contains(char::is_whitespace) {
                format!("{w:?}")
            } else {
                w.clone()
            }
        })
        .collect();
    words.join(" ")
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
        // The last path part, split at both separators: a Windows path
        // given to a Linux build (tests) or the other way round.
        .filter_map(|a| a.trim_end_matches(['/', '\\']).rsplit(['/', '\\']).next())
        .map(str::to_lowercase)
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
    /// No new window, but the app's window in front is new or changed, or
    /// another of its windows was renamed: a running instance took the
    /// request (e.g. `kate -b file` or `firefox URL` opens a tab).
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
    let named = |w: &WindowGeo| {
        let title = w.title.to_lowercase();
        hints.iter().any(|h| title.contains(h.as_str()))
    };
    if let Some(active) = of_app.iter().copied().find(|w| w.active) {
        let changed = !was_active.contains(&key(active));
        let ok = match stage {
            Stage::Starting => changed && named(active),
            Stage::Settled => changed || named(active),
            Stage::Ended => true,
        };
        if ok {
            return Some((active, Found::Existing));
        }
    }
    // The instance opened it in a window it did not bring to the front
    // (a browser's new tab, the compositor keeping focus where it was):
    // a window of the app renamed since, of a process that had one.
    let pids: HashSet<u32> = before
        .iter()
        .filter(|w| class_matches(w, app))
        .map(|w| w.pid)
        .collect();
    let renamed: Vec<&WindowGeo> = of_app
        .iter()
        .copied()
        .filter(|w| !old.contains(&key(w)) && pids.contains(&w.pid))
        .collect();
    let pick = match stage {
        Stage::Starting => renamed.into_iter().find(|w| named(w)),
        Stage::Settled | Stage::Ended => first(renamed),
    };
    if let Some(w) = pick {
        return Some((w, Found::Existing));
    }
    // Ended cleanly with nothing renamed (the page's title was already
    // shown): the app's only window took it.
    match (stage, of_app.as_slice()) {
        (Stage::Ended, [only]) => Some((only, Found::Existing)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn command_lines_quote_words_with_spaces() {
        let argv = s(&[r"C:\Program Files\x.exe", "a", "", "b c"]);
        assert_eq!(
            command_line(&argv),
            r#""C:\\Program Files\\x.exe" a "" "b c""#
        );
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
        // Windows paths, a folder with a trailing separator, a drive root.
        let paths = arg_hints(&s(&[
            r"C:\Users\x\Belge.TXT",
            r"C:\mcp\Proje\",
            "C:\\",
            "/a/b/",
        ]));
        assert_eq!(paths, s(&["belge.txt", "proje"]));
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
    fn a_tab_opened_in_a_window_left_behind_counts() {
        // `firefox file:///…/live.html` with Firefox running: its window
        // gets the page as a new tab but stays behind the terminal.
        let tree = HashSet::from([50]);
        let hints = arg_hints(&s(&["file:///tmp/smoke/live.html"]));
        assert!(hints.is_empty());
        let term = win("konsole", "~ : claude — Konsole", 7, true);
        let before = [
            term.clone(),
            win("firefox", "Gelen kutusu — Mozilla Firefox", 10, false),
        ];
        let now = [
            term.clone(),
            win(
                "firefox",
                "desktop-mcp canli test — Mozilla Firefox",
                10,
                false,
            ),
        ];
        let pick = |stage| pick_window("firefox", &before, &now, &tree, stage, &hints);
        assert_eq!(pick(Stage::Starting), None);
        assert_eq!(pick(Stage::Settled), Some((&now[1], Found::Existing)));
        assert_eq!(pick(Stage::Ended), Some((&now[1], Found::Existing)));
        // Already showing that title: its only window, once it ended.
        let pick = |stage| pick_window("firefox", &now, &now, &tree, stage, &hints);
        assert_eq!(pick(Stage::Settled), None);
        assert_eq!(pick(Stage::Ended), Some((&now[1], Found::Existing)));
        // With two windows left as they were, none can be told.
        let two = [
            now[1].clone(),
            win("firefox", "Mail — Mozilla Firefox", 10, false),
        ];
        assert_eq!(
            pick_window("firefox", &two, &two, &tree, Stage::Ended, &hints),
            None
        );
        // A renamed window counts while just started only when its title
        // names the file asked for.
        let before = [win("org.kde.kate", "a.txt — Kate", 10, false)];
        let now = [win("org.kde.kate", "notes.txt — Kate", 10, false)];
        let hints = s(&["notes.txt"]);
        assert_eq!(
            pick_window("kate", &before, &now, &tree, Stage::Starting, &hints),
            Some((&now[0], Found::Existing))
        );
        // Another process' window is no rename.
        let now = [win("org.kde.kate", "notes.txt — Kate", 11, false)];
        assert_eq!(
            pick_window("kate", &before, &now, &tree, Stage::Settled, &hints),
            None
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
