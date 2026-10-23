//! Tree and search output over already-fetched nodes. Pure functions: when a
//! node they need is missing they return it, the caller fetches and retries.
//! The format matches the Python server line for line.

use std::hash::Hash;

use serde_json::Value as Json;

use crate::backend::{Cache, Info};

pub const TRUNCATED: &str = "… truncated; narrow with find or a deeper root id";
pub const FIND_TEXT_LIMIT: usize = 200;
pub const FIND_MAX_SEEN: usize = 20000;

/// A rendered line: either a node (gets an id when printed) or plain text.
#[derive(Debug, PartialEq)]
pub enum Line<Id> {
    Node { depth: usize, id: Id },
    Raw(String),
}

fn get<'a, Id: Clone + Eq + Hash>(cache: &'a Cache<Id>, ids: &[Id])
                                  -> Result<Vec<&'a Info<Id>>, Vec<Id>> {
    let missing: Vec<Id> = ids.iter().filter(|o| !cache.contains_key(o)).cloned().collect();
    if missing.is_empty() {
        Ok(ids.iter().map(|o| &cache[o]).collect())
    } else {
        Err(missing)
    }
}

fn first_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Python-compatible `json.dumps(s, ensure_ascii=False)`.
pub fn json_str(s: &str) -> String {
    Json::String(s.to_string()).to_string()
}

pub fn text_preview<Id>(info: &Info<Id>, limit: usize) -> String {
    info.text.as_deref().map(|t| first_chars(t, limit)).unwrap_or_default()
}

pub fn describe<Id>(info: &Info<Id>, id: u64) -> String {
    let name = first_chars(&info.name.replace('\n', " "), 80);
    let role = if info.role.is_empty() { "?" } else { &info.role };
    let mut out = format!("[{id}] {role}");
    if !name.is_empty() {
        out.push(' ');
        out.push_str(&json_str(&name));
    }
    let txt = text_preview(info, 80);
    if !txt.is_empty() && txt != name {
        out.push_str(" text=");
        out.push_str(&json_str(&txt));
    }
    if !info.states.is_empty() {
        out.push_str(&format!(" {{{}}}", info.states.join(",")));
    }
    if !info.actions.is_empty() {
        let names: Vec<&str> =
            info.actions.iter().map(String::as_str).filter(|a| !a.is_empty()).collect();
        out.push_str(&format!(" actions={}", names.join(",")));
    }
    out
}

pub struct WalkOpts {
    pub max_depth: usize,
    pub budget: usize,
    pub only_showing: bool,
    pub boring_roles: &'static [&'static str],
}

/// Python `walk`: unnamed, actionless nodes of boring roles are skipped and
/// their children lifted to the same depth.
pub fn walk<Id: Clone + Eq + Hash>(cache: &Cache<Id>, children: &[Id], depth: usize,
                                   opts: &WalkOpts, lines: &mut Vec<Line<Id>>)
                                   -> Result<(), Vec<Id>> {
    if lines.len() >= opts.budget || depth > opts.max_depth {
        return Ok(());
    }
    let infos = get(cache, children)?;
    for (c, info) in children.iter().zip(infos) {
        if lines.len() >= opts.budget {
            lines.push(Line::Raw(TRUNCATED.into()));
            return Ok(());
        }
        if opts.only_showing && !info.showing {
            continue;
        }
        let interesting = !opts.boring_roles.contains(&info.role.as_str())
            || !info.name.is_empty()
            || !info.actions.is_empty();
        if interesting {
            lines.push(Line::Node { depth, id: c.clone() });
            walk(cache, &info.children, depth + 1, opts, lines)?;
        } else {
            walk(cache, &info.children, depth, opts, lines)?;
        }
    }
    Ok(())
}

/// Python `find`: depth-first, hidden subtrees skipped, case-insensitive
/// substring match over name, text and role.
pub fn find<Id: Clone + Eq + Hash>(cache: &Cache<Id>, roots: &[Id], query: &str, role: &str,
                                   max_results: usize) -> Result<Vec<Id>, Vec<Id>> {
    let q = query.to_lowercase();
    let mut hits = Vec::new();
    // None is the synthetic root whose children are `roots`.
    let mut stack: Vec<Option<&Id>> = vec![None];
    let mut seen = 0;
    while let Some(node) = stack.pop() {
        if hits.len() >= max_results || seen >= FIND_MAX_SEEN {
            break;
        }
        seen += 1;
        let kids: &[Id] = match node {
            None => roots,
            Some(o) => &cache[o].children,
        };
        let infos = get(cache, kids)?;
        for (c, info) in kids.iter().zip(infos).rev() {
            if !info.showing {
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

    const BORING: &[&str] = &["filler", "panel"];

    fn node(role: &str, name: &str, showing: bool, states: &[&'static str], kids: &[&'static str])
            -> Info<&'static str> {
        Info {
            role: role.into(),
            name: name.into(),
            states: states.to_vec(),
            showing,
            children: kids.to_vec(),
            ..Default::default()
        }
    }

    fn sample() -> Cache<&'static str> {
        let mut c = Cache::new();
        c.insert("/w", node("frame", "Win", true, &["active"], &["/p"]));
        c.insert("/p", node("panel", "", true, &[], &["/b1", "/b2", "/h"]));
        let mut b1 = node("push button", "Kaydet", true, &[], &[]);
        b1.actions = vec!["Press".into(), "".into()];
        c.insert("/b1", b1);
        let mut b2 = node("text", "line\nbreak", true, &["focused", "disabled"], &[]);
        b2.text = Some("içerik \"x\"".into());
        c.insert("/b2", b2);
        c.insert("/h", node("push button", "hidden", false, &[], &[]));
        c
    }

    fn opts(budget: usize) -> WalkOpts {
        WalkOpts { max_depth: 12, budget, only_showing: true, boring_roles: BORING }
    }

    #[test]
    fn walk_lifts_boring_nodes_and_skips_hidden() {
        let cache = sample();
        let mut lines = Vec::new();
        walk(&cache, &["/w"], 1, &opts(300), &mut lines).unwrap();
        let got: Vec<(usize, &str)> = lines.iter().map(|l| match l {
            Line::Node { depth, id } => (*depth, *id),
            Line::Raw(_) => (0, "raw"),
        }).collect();
        assert_eq!(got, vec![(1, "/w"), (2, "/b1"), (2, "/b2")]);
    }

    #[test]
    fn walk_reports_missing_nodes_and_truncates() {
        let mut cache = sample();
        cache.remove("/b2");
        let missing = walk(&cache, &["/w"], 1, &opts(300), &mut Vec::new()).unwrap_err();
        assert_eq!(missing, vec!["/b2"]);

        let cache = sample();
        let mut lines = Vec::new();
        walk(&cache, &["/w"], 1, &opts(2), &mut lines).unwrap();
        assert_eq!(lines.last(), Some(&Line::Raw(TRUNCATED.into())));
    }

    #[test]
    fn describe_matches_python_format() {
        let cache = sample();
        assert_eq!(describe(&cache["/b1"], 7), "[7] push button \"Kaydet\" actions=Press");
        assert_eq!(describe(&cache["/b2"], 8),
                   "[8] text \"line break\" text=\"içerik \\\"x\\\"\" {focused,disabled}");
        assert_eq!(describe(&cache["/w"], 1), "[1] frame \"Win\" {active}");
    }

    #[test]
    fn find_is_depth_first_with_reversed_siblings() {
        let cache = sample();
        let hits = find(&cache, &["/w"], "", "", 10).unwrap();
        assert_eq!(hits, vec!["/w", "/p", "/b2", "/b1"]);
        let hits = find(&cache, &["/w"], "KAYDET", "push button", 10).unwrap();
        assert_eq!(hits, vec!["/b1"]);
    }

    #[test]
    fn repr_matches_python() {
        assert_eq!(py_repr("a — b"), "'a — b'");
        assert_eq!(py_repr("it's"), "\"it's\"");
        assert_eq!(py_repr("a'b\"c"), "'a\\'b\"c'");
        assert_eq!(py_repr("x\ty"), "'x\\ty'");
    }
}
