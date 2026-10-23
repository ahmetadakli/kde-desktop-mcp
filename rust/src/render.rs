//! Tree and search output over already-fetched nodes. Pure functions: when a
//! node they need is missing they return it, the caller fetches and retries.
//! The format matches the Python server line for line.

use std::collections::HashMap;

use crate::atspi::{self, Info, Obj};

pub const BORING_ROLES: [&str; 6] =
    ["filler", "panel", "section", "unknown", "redundant object", "grouping"];
pub const TRUNCATED: &str = "… truncated; narrow with find or a deeper root id";

pub type Cache = HashMap<Obj, Info>;

/// A rendered line: either a node (gets an id when printed) or plain text.
#[derive(Debug, PartialEq)]
pub enum Line {
    Node { depth: usize, obj: Obj },
    Raw(String),
}

fn get<'a>(cache: &'a Cache, objs: &[Obj]) -> Result<Vec<&'a Info>, Vec<Obj>> {
    let missing: Vec<Obj> = objs.iter().filter(|o| !cache.contains_key(o)).cloned().collect();
    if missing.is_empty() {
        Ok(objs.iter().map(|o| &cache[o]).collect())
    } else {
        Err(missing)
    }
}

fn first_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Single-line text preview (Python `Node.text(limit)`).
pub fn text_preview(info: &Info, limit: usize) -> String {
    info.text.as_deref().map(|t| first_chars(t, limit).replace('\n', " ")).unwrap_or_default()
}

pub fn describe(info: &Info, id: u64) -> String {
    let name = first_chars(&info.name.replace('\n', " "), 80);
    let role = if info.role.is_empty() { "?" } else { &info.role };
    let mut out = format!("[{id}] {role}");
    if !name.is_empty() {
        out.push(' ');
        out.push_str(&atspi::json_str(&name));
    }
    let txt = text_preview(info, 80);
    if !txt.is_empty() && txt != name {
        out.push_str(" text=");
        out.push_str(&atspi::json_str(&txt));
    }
    let states = state_labels(info);
    if !states.is_empty() {
        out.push_str(&format!(" {{{}}}", states.join(",")));
    }
    if !info.actions.is_empty() {
        let names: Vec<&str> =
            info.actions.iter().map(String::as_str).filter(|a| !a.is_empty()).collect();
        out.push_str(&format!(" actions={}", names.join(",")));
    }
    out
}

pub fn state_labels(info: &Info) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = [
        (atspi::FOCUSED, "focused"),
        (atspi::CHECKED, "checked"),
        (atspi::SELECTED, "selected"),
        (atspi::EXPANDED, "expanded"),
        (atspi::ACTIVE, "active"),
        (atspi::EDITABLE, "editable"),
    ]
    .into_iter()
    .filter(|(st, _)| info.has(*st))
    .map(|(_, label)| label)
    .collect();
    if info.states_known && !info.has(atspi::ENABLED) {
        out.push("disabled");
    }
    out
}

pub struct WalkOpts {
    pub max_depth: usize,
    pub budget: usize,
    pub only_showing: bool,
}

/// Python `walk`: unnamed, actionless nodes of boring roles are skipped and
/// their children lifted to the same depth.
pub fn walk(cache: &Cache, children: &[Obj], depth: usize, opts: &WalkOpts,
            lines: &mut Vec<Line>) -> Result<(), Vec<Obj>> {
    if lines.len() >= opts.budget || depth > opts.max_depth {
        return Ok(());
    }
    let infos = get(cache, children)?;
    for (c, info) in children.iter().zip(infos) {
        if lines.len() >= opts.budget {
            lines.push(Line::Raw(TRUNCATED.into()));
            return Ok(());
        }
        if opts.only_showing && !info.has(atspi::SHOWING) {
            continue;
        }
        let interesting = !BORING_ROLES.contains(&info.role.as_str())
            || !info.name.is_empty()
            || !info.actions.is_empty();
        if interesting {
            lines.push(Line::Node { depth, obj: c.clone() });
            walk(cache, &info.children, depth + 1, opts, lines)?;
        } else {
            walk(cache, &info.children, depth, opts, lines)?;
        }
    }
    Ok(())
}

pub const FIND_TEXT_LIMIT: usize = 200;
pub const FIND_MAX_SEEN: usize = 20000;

/// Python `find`: depth-first, hidden subtrees skipped, case-insensitive
/// substring match over name, text and role.
pub fn find(cache: &Cache, roots: &[Obj], query: &str, role: &str,
            max_results: usize) -> Result<Vec<Obj>, Vec<Obj>> {
    let q = query.to_lowercase();
    let mut hits = Vec::new();
    // None is the synthetic root whose children are `roots`.
    let mut stack: Vec<Option<&Obj>> = vec![None];
    let mut seen = 0;
    while let Some(node) = stack.pop() {
        if hits.len() >= max_results || seen >= FIND_MAX_SEEN {
            break;
        }
        seen += 1;
        let kids: &[Obj] = match node {
            None => roots,
            Some(o) => &cache[o].children,
        };
        let infos = get(cache, kids)?;
        for (c, info) in kids.iter().zip(infos).rev() {
            if !info.has(atspi::SHOWING) {
                continue;
            }
            stack.push(Some(c));
            if !role.is_empty() && info.role != role {
                continue;
            }
            let hay = format!("{} {} {}", info.name, text_preview(info, FIND_TEXT_LIMIT),
                              info.role).to_lowercase();
            if hay.contains(&q) {
                hits.push(c.clone());
            }
        }
    }
    Ok(hits)
}

/// Python `repr()` of a str, for window titles in `apps`.
pub fn py_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') { '"' } else { '\'' };
    let mut out = String::from(quote);
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || (0x7f..0xa0).contains(&(c as u32)) => {
                out.push_str(&format!("\\x{:02x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::OwnedObjectPath;

    fn obj(p: &str) -> Obj {
        Obj { bus: ":1.1".into(), path: OwnedObjectPath::try_from(p).unwrap() }
    }

    fn node(role: &str, name: &str, states: &[u32], kids: &[&str]) -> Info {
        Info {
            role: role.into(),
            name: name.into(),
            states: states.iter().fold(0, |a, s| a | (1 << s)),
            states_known: true,
            children: kids.iter().map(|k| obj(k)).collect(),
            ..Default::default()
        }
    }

    const SHOWN: &[u32] = &[atspi::SHOWING, atspi::ENABLED];

    fn sample() -> Cache {
        let mut c = Cache::new();
        c.insert(obj("/w"), node("frame", "Win", &[atspi::SHOWING, atspi::ENABLED, atspi::ACTIVE], &["/p"]));
        c.insert(obj("/p"), node("panel", "", SHOWN, &["/b1", "/b2", "/h"]));
        let mut b1 = node("push button", "Kaydet", SHOWN, &[]);
        b1.actions = vec!["Press".into(), "".into()];
        c.insert(obj("/b1"), b1);
        let mut b2 = node("text", "line\nbreak", &[atspi::SHOWING, atspi::FOCUSED], &[]);
        b2.text = Some("içerik\n\"x\"".into());
        c.insert(obj("/b2"), b2);
        c.insert(obj("/h"), node("push button", "hidden", &[atspi::ENABLED], &[]));
        c
    }

    #[test]
    fn walk_lifts_boring_nodes_and_skips_hidden() {
        let cache = sample();
        let mut lines = Vec::new();
        let opts = WalkOpts { max_depth: 12, budget: 300, only_showing: true };
        walk(&cache, &[obj("/w")], 1, &opts, &mut lines).unwrap();
        let got: Vec<(usize, &str)> = lines.iter().map(|l| match l {
            Line::Node { depth, obj } => (*depth, obj.path.as_str()),
            Line::Raw(_) => (0, "raw"),
        }).collect();
        assert_eq!(got, vec![(1, "/w"), (2, "/b1"), (2, "/b2")]);
    }

    #[test]
    fn walk_reports_missing_nodes_and_truncates() {
        let mut cache = sample();
        cache.remove(&obj("/b2"));
        let opts = WalkOpts { max_depth: 12, budget: 300, only_showing: true };
        let missing = walk(&cache, &[obj("/w")], 1, &opts, &mut Vec::new()).unwrap_err();
        assert_eq!(missing, vec![obj("/b2")]);

        let cache = sample();
        let mut lines = Vec::new();
        let opts = WalkOpts { max_depth: 12, budget: 2, only_showing: true };
        walk(&cache, &[obj("/w")], 1, &opts, &mut lines).unwrap();
        assert_eq!(lines.last(), Some(&Line::Raw(TRUNCATED.into())));
    }

    #[test]
    fn describe_matches_python_format() {
        let cache = sample();
        assert_eq!(describe(&cache[&obj("/b1")], 7), "[7] push button \"Kaydet\" actions=Press");
        assert_eq!(describe(&cache[&obj("/b2")], 8),
                   "[8] text \"line break\" text=\"içerik \\\"x\\\"\" {focused,disabled}");
        assert_eq!(describe(&cache[&obj("/w")], 1), "[1] frame \"Win\" {active}");
    }

    #[test]
    fn find_is_depth_first_with_reversed_siblings() {
        let cache = sample();
        let hits = find(&cache, &[obj("/w")], "", "", 10).unwrap();
        let paths: Vec<&str> = hits.iter().map(|o| o.path.as_str()).collect();
        assert_eq!(paths, vec!["/w", "/p", "/b2", "/b1"]);
        let hits = find(&cache, &[obj("/w")], "KAYDET", "push button", 10).unwrap();
        assert_eq!(hits, vec![obj("/b1")]);
    }

    #[test]
    fn repr_matches_python() {
        assert_eq!(py_repr("a — b"), "'a — b'");
        assert_eq!(py_repr("it's"), "\"it's\"");
        assert_eq!(py_repr("a'b\"c"), "'a\\'b\"c'");
        assert_eq!(py_repr("x\ty"), "'x\\ty'");
    }
}
