//! Tree and search output over already-fetched nodes. Pure functions: when a
//! node they need is missing they return it, the caller fetches and retries.
//! The traversal matches the former Python server, so the "Python ..."
//! notes below name the function each one reproduces; the output is more
//! compact (one-space indent, runs of plain siblings on one line, Qt's
//! "Set Focus" action left out).

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

/// Actions worth listing. Qt gives nearly every widget "Set Focus"; the
/// `focus` tool uses it on its own, so listing it only costs tokens.
pub fn shown_actions<Id>(info: &Info<Id>) -> Vec<&str> {
    info.actions.iter().map(String::as_str)
        .filter(|a| !a.is_empty() && !a.eq_ignore_ascii_case("set focus"))
        .collect()
}

fn short_name<Id>(info: &Info<Id>) -> String {
    first_chars(&info.name.replace('\n', " "), 80)
}

pub fn describe<Id>(info: &Info<Id>, id: u64) -> String {
    let name = short_name(info);
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
    let actions = shown_actions(info);
    if !actions.is_empty() {
        out.push_str(&format!(" actions={}", actions.join(",")));
    }
    out
}

/// An element described by its role, name and actions alone: no text or
/// states. Such siblings with the same role and actions share one line.
fn plain<Id>(info: &Info<Id>) -> bool {
    let txt = text_preview(info, 80);
    info.states.is_empty() && (txt.is_empty() || txt == short_name(info))
}

/// One line of tree/find output, with its id assigned.
pub enum Printed<'a, Id> {
    Raw(String),
    Node { depth: usize, rid: u64, info: &'a Info<Id> },
}

/// Shortest run of plain sibling leaves printed on one line.
const MIN_RUN: usize = 3;

/// Join printed lines, one space of indent per level. Runs of plain sibling
/// leaves of one role and the same actions (long file lists, menus, rows of
/// check boxes) share a line:
/// `list item ×3: [8] "a" [9] "b" [10] "c"`,
/// `check box ×2 actions=Toggle: [11] "Bold" [12]`.
pub fn format<Id>(items: &[Printed<Id>]) -> String {
    // A node is a leaf here when the next line is not deeper.
    let leaf = |i: usize, depth: usize| match items.get(i + 1) {
        Some(Printed::Node { depth: d, .. }) => *d <= depth,
        _ => true,
    };
    let mut out = Vec::new();
    let mut i = 0;
    while i < items.len() {
        let (depth, rid, info) = match &items[i] {
            Printed::Raw(s) => {
                out.push(s.clone());
                i += 1;
                continue;
            }
            Printed::Node { depth, rid, info } => (*depth, *rid, *info),
        };
        let mut j = i;
        while let Some(Printed::Node { depth: d, info: next, .. }) = items.get(j) {
            if *d != depth || next.role != info.role || !plain(next) || !leaf(j, depth)
                || shown_actions(next) != shown_actions(info)
            {
                break;
            }
            j += 1;
        }
        let indent = " ".repeat(depth);
        if j - i >= MIN_RUN {
            let role = if info.role.is_empty() { "?" } else { &info.role };
            let actions = shown_actions(info);
            let actions = if actions.is_empty() { String::new() } else { format!(" actions={}", actions.join(",")) };
            let members: Vec<String> = items[i..j].iter().map(|it| match it {
                Printed::Node { rid, info, .. } => match short_name(info) {
                    n if n.is_empty() => format!("[{rid}]"),
                    n => format!("[{rid}] {}", json_str(&n)),
                },
                Printed::Raw(_) => unreachable!(),
            }).collect();
            out.push(format!("{indent}{role} ×{}{actions}: {}", j - i, members.join(" ")));
            i = j;
        } else {
            out.push(format!("{indent}{}", describe(info, rid)));
            i += 1;
        }
    }
    out.join("\n")
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
            // Once, not once per open level.
            if !matches!(lines.last(), Some(Line::Raw(s)) if s == TRUNCATED) {
                lines.push(Line::Raw(TRUNCATED.into()));
            }
            return Ok(());
        }
        if opts.only_showing && !info.showing {
            continue;
        }
        let interesting = !opts.boring_roles.contains(&info.role.as_str())
            || !info.name.is_empty()
            || !shown_actions(info).is_empty();
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

/// The element a query names: depth-first in document order over visible
/// elements, the first whose name equals `query` (case-insensitive) wins,
/// else the first whose name, text or role contains it. Ok(None) when
/// nothing matches.
pub fn locate<Id: Clone + Eq + Hash>(cache: &Cache<Id>, roots: &[Id], query: &str,
                                     role: &str) -> Result<Option<Id>, Vec<Id>> {
    let q = query.trim().to_lowercase();
    let mut first_partial = None;
    let mut stack: Vec<&Id> = Vec::new();
    let infos = get(cache, roots)?;
    stack.extend(roots.iter().zip(infos).filter(|(_, i)| i.showing).map(|(r, _)| r).rev());
    let mut seen = 0;
    while let Some(id) = stack.pop() {
        seen += 1;
        if seen > FIND_MAX_SEEN {
            break;
        }
        let info = &cache[id];
        if role.is_empty() || info.role == role {
            if info.name.trim().to_lowercase() == q {
                return Ok(Some(id.clone()));
            }
            if first_partial.is_none() {
                let hay = format!("{} {} {}", info.name, text_preview(info, FIND_TEXT_LIMIT),
                                  info.role).to_lowercase();
                if hay.contains(&q) {
                    first_partial = Some(id.clone());
                }
            }
        }
        let kids = get(cache, &info.children)?;
        stack.extend(info.children.iter().zip(kids).filter(|(_, k)| k.showing).map(|(c, _)| c).rev());
    }
    Ok(first_partial)
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
        assert_eq!(lines.iter().filter(|l| matches!(l, Line::Raw(_))).count(), 1);
    }

    #[test]
    fn describe_matches_python_format() {
        let cache = sample();
        assert_eq!(describe(&cache["/b1"], 7), "[7] push button \"Kaydet\" actions=Press");
        assert_eq!(describe(&cache["/b2"], 8),
                   "[8] text \"line break\" text=\"içerik \\\"x\\\"\" {focused,disabled}");
        assert_eq!(describe(&cache["/w"], 1), "[1] frame \"Win\" {active}");
        let mut f = node("filler", "", true, &[], &[]);
        f.actions = vec!["Set Focus".into()];
        assert_eq!(describe(&f, 2), "[2] filler");
    }

    #[test]
    fn set_focus_alone_does_not_make_a_container_interesting() {
        let mut cache = sample();
        cache.get_mut("/p").unwrap().actions = vec!["Set Focus".into()];
        let mut lines = Vec::new();
        walk(&cache, &["/w"], 1, &opts(300), &mut lines).unwrap();
        assert_eq!(lines.len(), 3); // /w, /b1, /b2: the panel is still lifted
    }

    #[test]
    fn format_folds_runs_of_plain_sibling_leaves() {
        let items: Vec<Info<&str>> = ["a", "b", "c", "d"].iter()
            .map(|n| node("list item", n, true, &[], &[])).collect();
        let focused = node("list item", "e", true, &["focused"], &[]);
        let list = node("list", "files", true, &[], &[]);
        let printed = vec![
            Printed::Raw("dolphin".into()),
            Printed::Node { depth: 1, rid: 1, info: &list },
            Printed::Node { depth: 2, rid: 2, info: &focused },
            Printed::Node { depth: 2, rid: 3, info: &items[0] },
            Printed::Node { depth: 2, rid: 4, info: &items[1] },
            Printed::Node { depth: 2, rid: 5, info: &items[2] },
            Printed::Node { depth: 1, rid: 6, info: &items[3] },
        ];
        assert_eq!(format(&printed), "dolphin\n [1] list \"files\"\n  [2] list item \"e\" {focused}\n  \
             list item ×3: [3] \"a\" [4] \"b\" [5] \"c\"\n [6] list item \"d\"");
        // Same role and actions fold too, unnamed members as bare ids.
        let boxes: Vec<Info<&str>> = ["Bold", "", "Italic"].iter().map(|n| {
            let mut b = node("check box", n, true, &[], &[]);
            b.actions = vec!["Toggle".into(), "Set Focus".into()];
            b
        }).collect();
        let printed: Vec<Printed<&str>> = boxes.iter().enumerate()
            .map(|(k, info)| Printed::Node { depth: 0, rid: k as u64 + 1, info }).collect();
        assert_eq!(format(&printed), "check box ×3 actions=Toggle: [1] \"Bold\" [2] [3] \"Italic\"");
        // Two in a row stay separate lines; a node with children is no leaf.
        let printed = vec![
            Printed::Node { depth: 0, rid: 1, info: &items[0] },
            Printed::Node { depth: 0, rid: 2, info: &items[1] },
            Printed::Node { depth: 0, rid: 3, info: &items[2] },
            Printed::Node { depth: 1, rid: 4, info: &list },
        ];
        assert_eq!(format(&printed),
                   "[1] list item \"a\"\n[2] list item \"b\"\n[3] list item \"c\"\n [4] list \"files\"");
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
    fn locate_prefers_exact_names_in_document_order() {
        let mut cache = sample();
        cache.insert("/b3", node("push button", "Farklı Kaydet", true, &[], &[]));
        cache.get_mut("/p").unwrap().children = vec!["/b3", "/b1", "/b2", "/h"];
        assert_eq!(locate(&cache, &["/w"], "kaydet", "").unwrap(), Some("/b1"));
        assert_eq!(locate(&cache, &["/w"], "farklı", "").unwrap(), Some("/b3"));
        assert_eq!(locate(&cache, &["/w"], "kaydet", "text").unwrap(), None);
        assert_eq!(locate(&cache, &["/w"], "hidden", "").unwrap(), None);
        cache.remove("/b2");
        assert_eq!(locate(&cache, &["/w"], "x", "").unwrap_err(), vec!["/b2"]);
    }

    #[test]
    fn repr_matches_python() {
        assert_eq!(py_repr("a — b"), "'a — b'");
        assert_eq!(py_repr("it's"), "\"it's\"");
        assert_eq!(py_repr("a'b\"c"), "'a\\'b\"c'");
        assert_eq!(py_repr("x\ty"), "'x\\ty'");
    }
}
