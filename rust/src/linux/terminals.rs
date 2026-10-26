//! Terminals behind a window: the pseudo-terminals its processes hold, and
//! what runs in the foreground of each, read from /proc.

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::backend::Terminal;

/// The /proc/<pid>/stat fields this needs.
#[derive(Clone, Debug, PartialEq)]
pub struct Stat {
    pub ppid: u32,
    pub tty_nr: u32,
    /// Foreground process group of the process' terminal.
    pub tpgid: i32,
}

/// Parse /proc/<pid>/stat. The command name is in parentheses and may hold
/// spaces or parentheses itself, so fields are counted after the last ')'.
pub fn parse_stat(line: &str) -> Option<Stat> {
    let rest = &line[line.rfind(')')? + 1..];
    // state ppid pgrp session tty_nr tpgid ...
    let f: Vec<&str> = rest.split_whitespace().collect();
    Some(Stat {
        ppid: f.get(1)?.parse().ok()?,
        tty_nr: f.get(4)?.parse().ok()?,
        tpgid: f.get(5)?.parse().ok()?,
    })
}

/// "pts/N" for a Unix98 pty device number; None for other terminals.
pub fn pts_name(tty_nr: u32) -> Option<String> {
    let major = (tty_nr >> 8) & 0xfff;
    let minor = (tty_nr & 0xff) | ((tty_nr >> 12) & 0xfff00);
    (136..=143)
        .contains(&major)
        .then(|| format!("pts/{}", (major - 136) * 256 + minor))
}

/// Terminals held by `root` and its descendants, with the foreground
/// process of each, given every process' stat and a way to describe a
/// process (command line, working directory).
pub fn terminals_of(
    root: u32,
    stats: &HashMap<u32, Stat>,
    describe: impl Fn(u32) -> (String, String),
) -> Vec<Terminal> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for (&pid, s) in stats {
        children.entry(s.ppid).or_default().push(pid);
    }
    // Every pty some descendant holds, and its foreground group.
    let mut ttys: BTreeMap<u32, i32> = BTreeMap::new();
    let mut seen = HashSet::new();
    let mut stack = vec![root];
    while let Some(pid) = stack.pop() {
        if !seen.insert(pid) {
            continue;
        }
        if let Some(s) = stats.get(&pid)
            && pts_name(s.tty_nr).is_some()
        {
            ttys.entry(s.tty_nr).or_insert(s.tpgid);
        }
        stack.extend(children.get(&pid).into_iter().flatten());
    }
    ttys.into_iter()
        .filter_map(|(tty, fg)| {
            let fg = u32::try_from(fg).ok().filter(|&p| p > 0)?;
            let (command, cwd) = describe(fg);
            Some(Terminal {
                tty: pts_name(tty)?,
                command,
                cwd,
            })
        })
        .collect()
}

/// Read every process' stat from /proc.
pub fn read_stats() -> HashMap<u32, Stat> {
    let mut out = HashMap::new();
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return out;
    };
    for e in dir.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else {
            continue;
        };
        if let Some(s) = std::fs::read_to_string(e.path().join("stat"))
            .ok()
            .and_then(|l| parse_stat(&l))
        {
            out.insert(pid, s);
        }
    }
    out
}

/// Command line (arguments joined by spaces, at most 120 chars) and working
/// directory (with the home directory as ~) of a process.
pub fn describe(pid: u32) -> (String, String) {
    let base = std::path::PathBuf::from(format!("/proc/{pid}"));
    let raw = std::fs::read(base.join("cmdline")).unwrap_or_default();
    let mut command = raw
        .split(|&b| b == 0)
        .filter(|a| !a.is_empty())
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect::<Vec<_>>()
        .join(" ");
    if command.chars().count() > 120 {
        command = command.chars().take(119).collect::<String>() + "…";
    }
    let cwd = std::fs::read_link(base.join("cwd"))
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let home = std::env::var("HOME").unwrap_or_default();
    let cwd = match cwd.strip_prefix(&home) {
        Some(rest) if !home.is_empty() && (rest.is_empty() || rest.starts_with('/')) => {
            format!("~{rest}")
        }
        _ => cwd,
    };
    (command, cwd)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(ppid: u32, tty_nr: u32, tpgid: i32) -> Stat {
        Stat {
            ppid,
            tty_nr,
            tpgid,
        }
    }

    #[test]
    fn stat_fields_come_after_the_last_parenthesis() {
        let line = "4242 (my (odd) prog) S 1 4242 4242 34817 5000 4194304 0 0";
        assert_eq!(parse_stat(line), Some(st(1, 34817, 5000)));
        assert_eq!(parse_stat("garbage"), None);
    }

    #[test]
    fn pty_numbers_name_pts_devices() {
        // makedev(136, 1) and makedev(137, 3) = pts/1 and pts/259.
        assert_eq!(pts_name(136 << 8 | 1).as_deref(), Some("pts/1"));
        assert_eq!(pts_name(137 << 8 | 3).as_deref(), Some("pts/259"));
        // A large minor: makedev(136, 300).
        assert_eq!(
            pts_name((136 << 8) | (300 & 0xff) | ((300 & !0xff) << 12)).as_deref(),
            Some("pts/300")
        );
        // /dev/tty1 (4, 1) and no terminal (0).
        assert_eq!(pts_name(4 << 8 | 1), None);
        assert_eq!(pts_name(0), None);
    }

    #[test]
    fn finds_each_tty_of_a_window_and_its_foreground_process() {
        let (pts1, pts2) = (136 << 8 | 1, 136 << 8 | 2);
        let stats = HashMap::from([
            (100, st(1, 0, -1)),         // konsole, no terminal
            (200, st(100, pts1, 300)),   // zsh on pts/1, `cargo test` in front
            (300, st(200, pts1, 300)),   // cargo test
            (400, st(100, pts2, 400)),   // fish on pts/2, idle
            (900, st(1, pts1 + 5, 900)), // another window's shell
        ]);
        let got = terminals_of(100, &stats, |pid| match pid {
            300 => ("cargo test".into(), "~/kde-desktop-mcp".into()),
            400 => ("fish".into(), "~".into()),
            _ => panic!("described {pid}"),
        });
        assert_eq!(
            got,
            vec![
                Terminal {
                    tty: "pts/1".into(),
                    command: "cargo test".into(),
                    cwd: "~/kde-desktop-mcp".into()
                },
                Terminal {
                    tty: "pts/2".into(),
                    command: "fish".into(),
                    cwd: "~".into()
                },
            ]
        );
        assert!(terminals_of(900_000, &stats, |_| unreachable!()).is_empty());
    }
}
