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

/// The fetched nodes among `ids`, in order; the others go to `missing`.
fn get<'a, Id: Clone + Eq + Hash>(
    cache: &'a Cache<Id>,
    ids: &'a [Id],
    missing: &mut Vec<Id>,
) -> Vec<(&'a Id, &'a Info<Id>)> {
    ids.iter()
        .filter_map(|o| match cache.get(o) {
            Some(info) => Some((o, info)),
            None => {
                missing.push(o.clone());
                None
            }
        })
        .collect()
}

/// `Ok(value)` once nothing was missing, else every missing node at once,
/// so the caller fetches them in one round.
fn done<T, Id>(value: T, missing: Vec<Id>) -> Result<T, Vec<Id>> {
    if missing.is_empty() {
        Ok(value)
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

/// Web pages put U+FFFC (OBJECT REPLACEMENT CHARACTER) in a container's
/// text for each child element; it says nothing the tree does not.
const EMBEDDED_OBJECT: char = '\u{FFFC}';

pub fn text_preview<Id>(info: &Info<Id>, limit: usize) -> String {
    info.text
        .as_deref()
        .map(|t| first_chars(&t.replace(EMBEDDED_OBJECT, ""), limit))
        .unwrap_or_default()
}

/// Actions that say nothing about the element itself, so listing them only
/// costs tokens (and would keep unnamed containers from being lifted). Qt
/// gives nearly every widget "Set Focus", which the `focus` tool uses on its
/// own; Firefox gives every element inside a clickable one "clickAncestor".
/// `act` still runs them by name.
const UNLISTED_ACTIONS: [&str; 2] = ["set focus", "clickancestor"];

/// Actions worth listing.
pub fn shown_actions<Id>(info: &Info<Id>) -> Vec<&str> {
    info.actions
        .iter()
        .map(String::as_str)
        .filter(|a| !a.is_empty() && !UNLISTED_ACTIONS.iter().any(|u| a.eq_ignore_ascii_case(u)))
        .collect()
}

/// Actions a role implies: a link jumps, a button presses, a tab switches,
/// a check box toggles, and in web pages any element with a click handler
/// (a section, a panel) clicks. Listing them on every line cost about a
/// tenth of a Firefox tree. Per role, lower case; an element that also has
/// another action gets its full list, so `act`'s default stays the first.
/// Elements of the roles marked `expected` that have no action at all say
/// `actions=none`, so a button `act` cannot press still stands out.
struct Implied {
    roles: &'static [&'static str],
    actions: &'static [&'static str],
    expected: bool,
}

const IMPLIED_ACTIONS: [Implied; 8] = [
    Implied {
        roles: &["push button", "button"],
        actions: &["press", "click"],
        expected: true,
    },
    Implied {
        roles: &["toggle button"],
        actions: &["press", "toggle", "click"],
        expected: true,
    },
    Implied {
        roles: &["link"],
        actions: &["jump", "click"],
        expected: true,
    },
    Implied {
        roles: &["page tab"],
        actions: &["switch", "press", "click"],
        expected: true,
    },
    Implied {
        roles: &["check box", "check menu item"],
        actions: &["toggle", "check", "uncheck", "click"],
        expected: true,
    },
    Implied {
        roles: &["radio button", "radio menu item"],
        actions: &["select", "check", "click"],
        expected: true,
    },
    Implied {
        roles: &["menu item"],
        actions: &["click", "press", "activate"],
        expected: true,
    },
    Implied {
        roles: &[
            "section",
            "panel",
            "frame",
            "internal frame",
            "document web",
            "page tab list",
            "menu",
        ],
        actions: &["click"],
        expected: false,
    },
];

/// Actions printed: none when every one is implied by the role, `none`
/// when a role that implies one has none.
pub fn listed_actions<Id>(info: &Info<Id>) -> Vec<&str> {
    let actions = shown_actions(info);
    let Some(implied) = IMPLIED_ACTIONS
        .iter()
        .find(|i| i.roles.contains(&info.role.as_str()))
    else {
        return actions;
    };
    if actions.is_empty() {
        return if implied.expected {
            vec!["none"]
        } else {
            actions
        };
    }
    if actions
        .iter()
        .all(|a| implied.actions.iter().any(|i| a.eq_ignore_ascii_case(i)))
    {
        Vec::new()
    } else {
        actions
    }
}

fn short_name<Id>(info: &Info<Id>) -> String {
    first_chars(&info.name.replace('\n', " "), 80)
}

/// `role "name"` as `describe` shows it, for messages about an element.
pub fn role_and_name(role: &str, name: &str) -> String {
    let role = if role.is_empty() { "?" } else { role };
    let name = first_chars(&name.replace('\n', " "), 80);
    if name.is_empty() {
        role.to_string()
    } else {
        format!("{role} {}", json_str(&name))
    }
}

pub fn describe<Id>(info: &Info<Id>, id: u64) -> String {
    let name = short_name(info);
    let role = if info.role.is_empty() {
        "?"
    } else {
        &info.role
    };
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
    let actions = listed_actions(info);
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
    Node {
        depth: usize,
        rid: u64,
        info: &'a Info<Id>,
    },
}

/// Shortest run of plain sibling leaves printed on one line.
const MIN_RUN: usize = 3;

/// Join printed lines, one space of indent per level. Runs of plain sibling
/// leaves of one role and the same actions (long file lists, menus, rows of
/// check boxes) share a line:
/// `list item ×3: [8] "a" [9] "b" [10] "c"`,
/// `check box ×2 actions=Toggle: [11] "Bold" [12]`.
pub fn format<Id>(items: &[Printed<Id>]) -> String {
    format_lines(items)
        .into_iter()
        .map(|(line, _)| line)
        .collect::<Vec<_>>()
        .join("\n")
}

/// The lines `format` joins, each with the number of `items` it covers.
fn format_lines<Id>(items: &[Printed<Id>]) -> Vec<(String, usize)> {
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
                out.push((s.clone(), 1));
                i += 1;
                continue;
            }
            Printed::Node { depth, rid, info } => (*depth, *rid, *info),
        };
        let mut j = i;
        while let Some(Printed::Node {
            depth: d,
            info: next,
            ..
        }) = items.get(j)
        {
            if *d != depth
                || next.role != info.role
                || !plain(next)
                || !leaf(j, depth)
                || listed_actions(next) != listed_actions(info)
            {
                break;
            }
            j += 1;
        }
        let indent = " ".repeat(depth);
        if j - i >= MIN_RUN {
            let role = if info.role.is_empty() {
                "?"
            } else {
                &info.role
            };
            let actions = listed_actions(info);
            let actions = if actions.is_empty() {
                String::new()
            } else {
                format!(" actions={}", actions.join(","))
            };
            let members: Vec<String> = items[i..j]
                .iter()
                .map(|it| match it {
                    Printed::Node { rid, info, .. } => match short_name(info) {
                        n if n.is_empty() => format!("[{rid}]"),
                        n => format!("[{rid}] {}", json_str(&n)),
                    },
                    Printed::Raw(_) => unreachable!(),
                })
                .collect();
            out.push((
                format!("{indent}{role} ×{}{actions}: {}", j - i, members.join(" ")),
                j - i,
            ));
            i = j;
        } else {
            out.push((format!("{indent}{}", describe(info, rid)), 1));
            i += 1;
        }
    }
    out
}

/// What `format_capped` printed.
pub struct Capped {
    pub text: String,
    /// Id of the last element printed, where a continuation starts after.
    pub last: Option<u64>,
    /// Elements among `items` left out.
    pub left: usize,
}

/// `format` cut to whole lines of at most `max_chars` characters in all
/// (0: no cap). At least one element line is kept, so paging always moves on.
pub fn format_capped<Id>(items: &[Printed<Id>], max_chars: usize) -> Capped {
    let mut text = String::new();
    let mut chars = 0;
    let mut used = 0;
    let mut nodes = false;
    for (line, n) in format_lines(items) {
        let len = line.chars().count() + usize::from(!text.is_empty());
        if max_chars > 0 && nodes && chars + len > max_chars {
            break;
        }
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&line);
        chars += len;
        nodes |= items[used..used + n]
            .iter()
            .any(|it| matches!(it, Printed::Node { .. }));
        used += n;
    }
    let last = items[..used].iter().rev().find_map(|it| match it {
        Printed::Node { rid, .. } => Some(*rid),
        Printed::Raw(_) => None,
    });
    let left = items[used..]
        .iter()
        .filter(|it| matches!(it, Printed::Node { .. }))
        .count();
    Capped { text, last, left }
}

pub struct WalkOpts {
    pub max_depth: usize,
    pub budget: usize,
    pub only_showing: bool,
    pub boring_roles: &'static [&'static str],
    /// Only elements one can operate (see `interactive`), each with its
    /// nearest named ancestor for context.
    pub interactive: bool,
}

/// How a walk ended.
#[derive(Debug, PartialEq)]
pub struct Walked {
    /// The budget ran out before the end.
    pub truncated: bool,
    /// The `after` element was met (always true without one).
    pub after_found: bool,
}

/// Python `walk`: unnamed, actionless nodes of boring roles are skipped and
/// their children lifted to the same depth (see `Shape` for what else is
/// left out). With `after`, only the lines after that element's line are
/// kept and count against the budget: the next page of a cut tree. On Err,
/// every node the walk could not reach yet, so one fetch brings them all.
pub fn walk<Id: Clone + Eq + Hash>(
    cache: &Cache<Id>,
    children: &[Id],
    depth: usize,
    opts: &WalkOpts,
    after: Option<&Id>,
    lines: &mut Vec<Line<Id>>,
) -> Result<Walked, Vec<Id>> {
    let base = lines.len();
    let mut st = Walk {
        cache,
        opts,
        lines,
        missing: Vec::new(),
        after,
        start: None,
        truncated: false,
    };
    walk_into(&mut st, children, depth);
    let after_found = after.is_none() || st.start.is_some();
    match st.start {
        Some(start) => drop(st.lines.drain(base..start)),
        None if after.is_some() => st.lines.truncate(base),
        None => {}
    }
    let walked = Walked {
        truncated: st.truncated,
        after_found,
    };
    done(walked, st.missing)
}

/// A walk in progress.
struct Walk<'a, Id> {
    cache: &'a Cache<Id>,
    opts: &'a WalkOpts,
    lines: &'a mut Vec<Line<Id>>,
    missing: Vec<Id>,
    /// Lines up to this element's are skipped.
    after: Option<&'a Id>,
    /// Index of the first line after `after`'s, once it was printed.
    start: Option<usize>,
    truncated: bool,
}

impl<Id> Walk<'_, Id> {
    /// Lines that count against the budget.
    fn counted(&self) -> usize {
        match (self.after, self.start) {
            (None, _) => self.lines.len(),
            (Some(_), Some(s)) => self.lines.len().saturating_sub(s),
            (Some(_), None) => 0,
        }
    }

    /// Drop the line at `at`; with `lift`, its subtree (the lines after it)
    /// moves up a level.
    fn remove(&mut self, at: usize, lift: bool) {
        self.lines.remove(at);
        if let Some(s) = self.start
            && s > at
        {
            self.start = Some(s - 1);
        }
        if lift {
            for line in &mut self.lines[at..] {
                if let Line::Node { depth, .. } = line {
                    *depth = depth.saturating_sub(1);
                }
            }
        }
    }
}

/// Roles one operates whatever actions the platform lists (AT-SPI, then
/// UI Automation names).
const INTERACTIVE_ROLES: [&str; 25] = [
    "push button",
    "button",
    "toggle button",
    "link",
    "page tab",
    "check box",
    "check menu item",
    "radio button",
    "radio menu item",
    "menu item",
    "combo box",
    "entry",
    "password text",
    "spin button",
    "slider",
    "hyperlink",
    "checkbox",
    "radiobutton",
    "combobox",
    "tabitem",
    "menuitem",
    "edit",
    "splitbutton",
    "spinner",
    "listitem",
];

/// States that mark an element one operates.
const INTERACTIVE_STATES: [&str; 5] = ["editable", "checked", "selected", "expanded", "focused"];

/// Whether `tree(interactive=true)` lists the element: it has an action
/// worth listing (implied ones included), an interactive role or state, or
/// the platform marks it selectable, checkable or expandable.
/// A container's implied click (a web page's section or panel with a click
/// handler somewhere inside, the page itself) does not count: nearly every
/// page wrapper has one.
pub fn interactive<Id>(info: &Info<Id>) -> bool {
    let role = info.role.as_str();
    let container = IMPLIED_ACTIONS
        .iter()
        .any(|i| !i.expected && i.roles.contains(&role));
    let acts = if container {
        !listed_actions(info).is_empty()
    } else {
        !shown_actions(info).is_empty()
    };
    acts || info.operable
        || INTERACTIVE_ROLES.contains(&role)
        || info.states.iter().any(|s| INTERACTIVE_STATES.contains(s))
}

/// Name compared for `echoes_only_child`: trimmed, whitespace runs as one
/// space, lower case.
fn norm_name(name: &str) -> String {
    name.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Wrapper roles beyond the backend's boring ones: printed only when they
/// carry a name, text, states or actions. Without them they are pure
/// structure (a web page's nameless landmarks and forms, the scroll pane
/// around a page) and their children are lifted, keeping their ids.
const WRAPPER_ROLES: [&str; 8] = [
    "landmark",
    "form",
    "scroll pane",
    "split pane",
    "viewport",
    "paragraph",
    "article",
    "block quote",
];

/// Roles left out when an element of them has nothing to show: no name,
/// text, states, actions or printed children (an empty scroll pane, a
/// separator, a nameless image inside a button, an empty live region).
const EMPTY_LEAF_ROLES: [&str; 13] = [
    "separator",
    "image",
    "icon",
    "alert",
    "status bar",
    "tool bar",
    "menu bar",
    "label",
    "static",
    "heading",
    "filler",
    "panel",
    "section",
];

/// Labels whose only child is the element they name (a web page's `label`
/// around its button or link): the child is printed in their place.
const LABEL_ROLES: [&str; 2] = ["label", "static"];

/// How the walk prints an element.
#[derive(PartialEq)]
enum Shape {
    /// A line of its own, children below it.
    Line,
    /// Left out, its children lifted to its depth.
    Lift,
}

/// Name, text and states that describe an element beyond its role.
fn bare<Id>(info: &Info<Id>) -> bool {
    info.name.trim().is_empty() && info.states.is_empty() && text_preview(info, 80).is_empty()
}

fn shape<Id: Clone + Eq + Hash>(cache: &Cache<Id>, info: &Info<Id>, opts: &WalkOpts) -> Shape {
    let role = info.role.as_str();
    let acts = !shown_actions(info).is_empty();
    if acts {
        return Shape::Line;
    }
    if opts.boring_roles.contains(&role) && info.name.is_empty() {
        return Shape::Lift;
    }
    if WRAPPER_ROLES.contains(&role) && bare(info) {
        return Shape::Lift;
    }
    if LABEL_ROLES.contains(&role) && info.states.is_empty() && plain(info) {
        let name = info.name.trim().to_lowercase();
        let mut kids = info
            .children
            .iter()
            .filter_map(|c| cache.get(c))
            .filter(|k| k.showing || !opts.only_showing);
        if let (Some(kid), None) = (kids.next(), kids.next())
            && !name.is_empty()
            && kid.name.trim().to_lowercase() == name
        {
            return Shape::Lift;
        }
    }
    Shape::Line
}

/// `walk`, going on past missing nodes (the lines are then incomplete and
/// get thrown away) to collect all of them. In an interactive walk, true
/// when a line it kept still wants its nearest named ancestor printed.
fn walk_into<Id: Clone + Eq + Hash>(st: &mut Walk<'_, Id>, children: &[Id], depth: usize) -> bool {
    let (cache, opts) = (st.cache, st.opts);
    let mut wants_context = false;
    if st.truncated || depth > opts.max_depth {
        return false;
    }
    for (c, info) in get(cache, children, &mut st.missing) {
        if st.truncated {
            break;
        }
        if st.counted() >= opts.budget {
            st.truncated = true;
            break;
        }
        if opts.only_showing && !info.showing {
            continue;
        }
        if shape(cache, info, opts) == Shape::Lift {
            wants_context |= walk_into(st, &info.children, depth);
            continue;
        }
        let at = st.lines.len();
        st.lines.push(Line::Node {
            depth,
            id: c.clone(),
        });
        if st.start.is_none() && st.after == Some(c) {
            st.start = Some(at + 1);
        }
        // Whether the children were looked at, not cut by budget or depth.
        let walked = st.counted() < opts.budget && depth < opts.max_depth;
        let kids_want = walk_into(st, &info.children, depth + 1);
        if opts.interactive {
            if interactive(info) {
                wants_context = true;
            } else if !kids_want || info.name.trim().is_empty() || echoes_only_child(st, at, info) {
                // Neither operable nor the context something below needs.
                st.remove(at, true);
                wants_context |= kids_want;
            }
        } else if st.lines.len() == at + 1
            && (info.children.is_empty() || walked)
            && EMPTY_LEAF_ROLES.contains(&info.role.as_str())
            && bare(info)
            && shown_actions(info).is_empty()
        {
            st.remove(at, false);
        }
    }
    wants_context
}

/// Whether the line at `at` (`info`'s) has a single line right below it
/// that carries the same name: a list item or heading named after the one
/// link in it says nothing as context.
fn echoes_only_child<Id: Clone + Eq + Hash>(st: &Walk<'_, Id>, at: usize, info: &Info<Id>) -> bool {
    let Some(Line::Node { depth, .. }) = st.lines.get(at) else {
        return false;
    };
    let mut below = st.lines[at + 1..].iter().filter_map(|l| match l {
        Line::Node { depth: d, id } if *d == depth + 1 => Some(id),
        _ => None,
    });
    match (below.next(), below.next()) {
        (Some(id), None) => st
            .cache
            .get(id)
            .is_some_and(|kid| norm_name(&kid.name) == norm_name(&info.name)),
        _ => false,
    }
}

/// Roles whose name is a window or tab title. A title names the page or
/// document as soon as it starts loading, so it would satisfy a query meant
/// for the content (`wait_for "Inbox"` returning on the tab title before
/// the page is there). Elements of these roles match a query only when no
/// other element does, or when `role` names their role.
pub const TITLE_ROLES: [&str; 3] = ["frame", "window", "page tab"];

/// How an element matches a query.
#[derive(Clone, Copy, PartialEq)]
enum Hit {
    /// The name equals the query (case-insensitive, trimmed).
    Exact,
    /// Name or text contains the query; with `role` given, the role too.
    Partial,
}

/// Whether `info` matches lower-cased query `q` and the exact `role`
/// filter. The role name is searched only when `role` is given: otherwise
/// a short query ("a") would match every frame, panel and alert.
fn matches<Id>(info: &Info<Id>, q: &str, role: &str) -> Option<Hit> {
    if !role.is_empty() && info.role != role {
        return None;
    }
    if info.name.trim().to_lowercase() == q {
        return Some(Hit::Exact);
    }
    let mut hay = format!("{} {}", info.name, text_preview(info, FIND_TEXT_LIMIT));
    if !role.is_empty() {
        hay.push(' ');
        hay.push_str(&info.role);
    }
    hay.to_lowercase().contains(q).then_some(Hit::Partial)
}

/// A window or tab title, which ranks below every other element.
fn is_title<Id>(info: &Info<Id>, role: &str) -> bool {
    role.is_empty() && TITLE_ROLES.contains(&info.role.as_str())
}

/// Python `find`: depth-first, hidden subtrees skipped, case-insensitive
/// substring match over name and text (and role, when `role` is given).
/// Window and tab titles come after all other hits.
pub fn find<Id: Clone + Eq + Hash>(
    cache: &Cache<Id>,
    roots: &[Id],
    query: &str,
    role: &str,
    max_results: usize,
) -> Result<Vec<Id>, Vec<Id>> {
    let q = query.to_lowercase();
    let mut hits = Vec::new();
    let mut titles = Vec::new();
    let mut missing = Vec::new();
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
        for (c, info) in get(cache, kids, &mut missing).into_iter().rev() {
            if !info.showing {
                continue;
            }
            stack.push(Some(c));
            if matches(info, &q, role).is_some() {
                if is_title(info, role) {
                    titles.push(c.clone());
                } else {
                    hits.push(c.clone());
                }
            }
        }
    }
    let room = max_results.saturating_sub(hits.len());
    hits.extend(titles.into_iter().take(room));
    done(hits, missing)
}

/// The visible children of a node (or the visible `roots`), reversed so
/// that popping a stack visits them in document order.
fn push_showing<'a, Id: Clone + Eq + Hash>(
    cache: &'a Cache<Id>,
    ids: &'a [Id],
    stack: &mut Vec<&'a Id>,
    missing: &mut Vec<Id>,
) {
    stack.extend(
        get(cache, ids, missing)
            .into_iter()
            .filter(|(_, i)| i.showing)
            .map(|(c, _)| c)
            .rev(),
    );
}

/// The element a query names: depth-first in document order over visible
/// elements, the first whose name equals `query` (case-insensitive) wins,
/// else the first whose name or text contains it (or role, when `role` is
/// given). Window and tab titles (TITLE_ROLES) count only when nothing else
/// matches, and with `titles` false not at all. Ok(None) when nothing
/// matches.
pub fn locate<Id: Clone + Eq + Hash>(
    cache: &Cache<Id>,
    roots: &[Id],
    query: &str,
    role: &str,
    titles: bool,
) -> Result<Option<Id>, Vec<Id>> {
    let q = query.trim().to_lowercase();
    let mut first_partial = None;
    let mut title_exact = None;
    let mut title_partial = None;
    let mut missing = Vec::new();
    let mut stack: Vec<&Id> = Vec::new();
    push_showing(cache, roots, &mut stack, &mut missing);
    let mut seen = 0;
    while let Some(id) = stack.pop() {
        seen += 1;
        if seen > FIND_MAX_SEEN {
            break;
        }
        let info = &cache[id];
        match (matches(info, &q, role), is_title(info, role)) {
            (Some(Hit::Exact), false) => return done(Some(id.clone()), missing),
            (Some(Hit::Exact), true) if titles => {
                title_exact.get_or_insert(id);
            }
            (Some(Hit::Partial), false) => {
                first_partial.get_or_insert(id);
            }
            (Some(Hit::Partial), true) if titles => {
                title_partial.get_or_insert(id);
            }
            _ => {}
        }
        push_showing(cache, &info.children, &mut stack, &mut missing);
    }
    let best = first_partial.or(title_exact).or(title_partial);
    done(best.cloned(), missing)
}

/// A hash of the visible elements below `roots` (role, name, states,
/// text), in document order, for telling whether a window changed. Nodes
/// not fetched are left out.
pub fn fingerprint<Id: Clone + Eq + Hash>(cache: &Cache<Id>, roots: &[Id]) -> u64 {
    use std::hash::Hasher;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let mut stack: Vec<&Id> = roots.iter().rev().collect();
    let mut seen = 0;
    while let Some(id) = stack.pop() {
        seen += 1;
        if seen > FIND_MAX_SEEN {
            break;
        }
        let Some(info) = cache.get(id) else { continue };
        if !info.showing {
            continue;
        }
        (&info.role, &info.name, &info.states, &info.text).hash(&mut h);
        // Where a subtree ends, so moving a node changes the hash.
        info.children.len().hash(&mut h);
        stack.extend(info.children.iter().rev());
    }
    h.finish()
}

/// Roles of the page or document inside an app window: a browser tab's web
/// page, an office document.
pub const DOCUMENT_ROLES: [&str; 3] = ["document web", "document frame", "document"];

/// The first visible element whose role is one of `roles`, depth-first in
/// document order. Ok(None) when there is none.
pub fn first_with_role<Id: Clone + Eq + Hash>(
    cache: &Cache<Id>,
    roots: &[Id],
    roles: &[&str],
) -> Result<Option<Id>, Vec<Id>> {
    let mut missing = Vec::new();
    let mut stack: Vec<&Id> = Vec::new();
    push_showing(cache, roots, &mut stack, &mut missing);
    let mut seen = 0;
    while let Some(id) = stack.pop() {
        seen += 1;
        if seen > FIND_MAX_SEEN {
            break;
        }
        let info = &cache[id];
        if roles.contains(&info.role.as_str()) {
            return done(Some(id.clone()), missing);
        }
        push_showing(cache, &info.children, &mut stack, &mut missing);
    }
    done(None, missing)
}

/// Roles whose content starts on a line of its own in `container_text`.
const BLOCK_ROLES: [&str; 30] = [
    "paragraph",
    "heading",
    "section",
    "article",
    "block quote",
    "landmark",
    "form",
    "list",
    "list item",
    "description list",
    "description term",
    "description value",
    "table",
    "table row",
    "caption",
    "header",
    "footer",
    "document web",
    "document frame",
    "internal frame",
    "dialog",
    "alert",
    "panel",
    "grouping",
    "tool bar",
    "page tab list",
    "page tab panel",
    "separator",
    "figure",
    "math",
];

/// Whether `container_text` should read `text` (from `full_text`) through
/// the children: a web container's own text has U+FFFC where each child
/// element goes, so a page reads as "￼".
pub fn has_embedded_objects(text: &str) -> bool {
    text.contains(EMBEDDED_OBJECT)
}

/// The text of `root` and its descendants in reading order, at most `limit`
/// characters: each U+FFFC in an element's text is replaced by the text of
/// the child it stands for (children in order), block elements (paragraphs,
/// headings, list items, sections) on lines of their own. An element without
/// text reads as its children, or as its name when it has none (an image's
/// alt text). Nodes need their text read with a limit of at least `limit`.
pub fn container_text<Id: Clone + Eq + Hash>(
    cache: &Cache<Id>,
    root: &Id,
    limit: usize,
) -> Result<String, Vec<Id>> {
    let mut out = String::new();
    let mut missing = Vec::new();
    let mut seen = 0;
    match cache.get(root) {
        Some(info) => read_into(cache, info, limit, &mut out, &mut missing, &mut seen),
        None => missing.push(root.clone()),
    }
    // Tidy: no blank runs, no spaces at line ends, no empty lines.
    let text = out
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    done(first_chars(text.trim_start(), limit), missing)
}

fn new_line(out: &mut String) {
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
}

fn read_into<Id: Clone + Eq + Hash>(
    cache: &Cache<Id>,
    info: &Info<Id>,
    limit: usize,
    out: &mut String,
    missing: &mut Vec<Id>,
    seen: &mut usize,
) {
    *seen += 1;
    if *seen > FIND_MAX_SEEN || (out.len() > limit && out.chars().count() > limit) {
        return;
    }
    let mut kids = info.children.iter();
    // A child goes inline unless its role is a block; between two inline
    // children with nothing in between, a space.
    let mut child = |out: &mut String, id: &Id, after_child: bool| match cache.get(id) {
        Some(k) => {
            let block = BLOCK_ROLES.contains(&k.role.as_str());
            if block {
                new_line(out);
            } else if after_child && !out.ends_with(char::is_whitespace) {
                out.push(' ');
            }
            read_into(cache, k, limit, out, missing, seen);
            if block {
                new_line(out);
            }
        }
        None => missing.push(id.clone()),
    };
    match info.text.as_deref() {
        Some(text) if !text.is_empty() => {
            let mut after_child = false;
            for ch in text.chars() {
                if ch == EMBEDDED_OBJECT {
                    if let Some(id) = kids.next() {
                        child(out, id, after_child);
                    }
                    after_child = true;
                } else {
                    out.push(ch);
                    after_child = false;
                }
            }
            // Children the text did not place (it was cut at the limit).
            for id in kids {
                child(out, id, true);
            }
        }
        _ if info.children.is_empty() => out.push_str(info.name.trim()),
        _ => {
            for id in kids {
                child(out, id, true);
            }
        }
    }
}

/// Python `repr()` of a str, for window titles in `apps`.
pub fn py_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
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

    fn node(
        role: &str,
        name: &str,
        showing: bool,
        states: &[&'static str],
        kids: &[&'static str],
    ) -> Info<&'static str> {
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
        WalkOpts {
            max_depth: 12,
            budget,
            only_showing: true,
            boring_roles: BORING,
            interactive: false,
        }
    }

    #[test]
    fn embedded_object_placeholders_are_not_text() {
        let mut n = node("section", "", true, &[], &[]);
        n.text = Some("\u{FFFC}\u{FFFC}".into());
        assert_eq!(text_preview(&n, 80), "");
        assert_eq!(describe(&n, 4), "[4] section");
        n.text = Some("Prime\u{FFFC} Video".into());
        assert_eq!(text_preview(&n, 80), "Prime Video");
    }

    #[test]
    fn walk_lifts_boring_nodes_and_skips_hidden() {
        let cache = sample();
        let mut lines = Vec::new();
        walk(&cache, &["/w"], 1, &opts(300), None, &mut lines).unwrap();
        let got: Vec<(usize, &str)> = lines
            .iter()
            .map(|l| match l {
                Line::Node { depth, id } => (*depth, *id),
                Line::Raw(_) => (0, "raw"),
            })
            .collect();
        assert_eq!(got, vec![(1, "/w"), (2, "/b1"), (2, "/b2")]);
    }

    #[test]
    fn walk_reports_missing_nodes_and_truncates() {
        let mut cache = sample();
        cache.remove("/b2");
        let missing = walk(&cache, &["/w"], 1, &opts(300), None, &mut Vec::new()).unwrap_err();
        assert_eq!(missing, vec!["/b2"]);

        let cache = sample();
        let mut lines = Vec::new();
        let walked = walk(&cache, &["/w"], 1, &opts(2), None, &mut lines).unwrap();
        assert!(walked.truncated);
        assert_eq!(lines.len(), 2);
        let walked = walk(&cache, &["/w"], 1, &opts(300), None, &mut Vec::new()).unwrap();
        assert_eq!(
            walked,
            Walked {
                truncated: false,
                after_found: true
            }
        );
    }

    #[test]
    fn describe_matches_python_format() {
        let cache = sample();
        // Press is what a push button does: not listed.
        assert_eq!(describe(&cache["/b1"], 7), "[7] push button \"Kaydet\"");
        assert_eq!(
            describe(&cache["/b2"], 8),
            "[8] text \"line break\" text=\"içerik \\\"x\\\"\" {focused,disabled}"
        );
        assert_eq!(describe(&cache["/w"], 1), "[1] frame \"Win\" {active}");
        let mut f = node("filler", "", true, &[], &[]);
        f.actions = vec!["Set Focus".into()];
        assert_eq!(describe(&f, 2), "[2] filler");
        f.actions = vec!["clickAncestor".into()];
        assert_eq!(describe(&f, 2), "[2] filler");
        f.actions = vec!["clickAncestor".into(), "click".into()];
        assert_eq!(describe(&f, 2), "[2] filler actions=click");
    }

    #[test]
    fn set_focus_alone_does_not_make_a_container_interesting() {
        let mut cache = sample();
        cache.get_mut("/p").unwrap().actions = vec!["Set Focus".into()];
        let mut lines = Vec::new();
        walk(&cache, &["/w"], 1, &opts(300), None, &mut lines).unwrap();
        assert_eq!(lines.len(), 3); // /w, /b1, /b2: the panel is still lifted
    }

    #[test]
    fn format_folds_runs_of_plain_sibling_leaves() {
        let items: Vec<Info<&str>> = ["a", "b", "c", "d"]
            .iter()
            .map(|n| node("list item", n, true, &[], &[]))
            .collect();
        let focused = node("list item", "e", true, &["focused"], &[]);
        let list = node("list", "files", true, &[], &[]);
        let printed = vec![
            Printed::Raw("dolphin".into()),
            Printed::Node {
                depth: 1,
                rid: 1,
                info: &list,
            },
            Printed::Node {
                depth: 2,
                rid: 2,
                info: &focused,
            },
            Printed::Node {
                depth: 2,
                rid: 3,
                info: &items[0],
            },
            Printed::Node {
                depth: 2,
                rid: 4,
                info: &items[1],
            },
            Printed::Node {
                depth: 2,
                rid: 5,
                info: &items[2],
            },
            Printed::Node {
                depth: 1,
                rid: 6,
                info: &items[3],
            },
        ];
        assert_eq!(
            format(&printed),
            "dolphin\n [1] list \"files\"\n  [2] list item \"e\" {focused}\n  \
             list item ×3: [3] \"a\" [4] \"b\" [5] \"c\"\n [6] list item \"d\""
        );
        // Same role and actions fold too, unnamed members as bare ids.
        let boxes: Vec<Info<&str>> = ["Bold", "", "Italic"]
            .iter()
            .map(|n| {
                let mut b = node("check box", n, true, &[], &[]);
                b.actions = vec!["Toggle".into(), "Set Focus".into()];
                b
            })
            .collect();
        let printed: Vec<Printed<&str>> = boxes
            .iter()
            .enumerate()
            .map(|(k, info)| Printed::Node {
                depth: 0,
                rid: k as u64 + 1,
                info,
            })
            .collect();
        assert_eq!(
            format(&printed),
            "check box ×3: [1] \"Bold\" [2] [3] \"Italic\""
        );
        // Two in a row stay separate lines; a node with children is no leaf.
        let printed = vec![
            Printed::Node {
                depth: 0,
                rid: 1,
                info: &items[0],
            },
            Printed::Node {
                depth: 0,
                rid: 2,
                info: &items[1],
            },
            Printed::Node {
                depth: 0,
                rid: 3,
                info: &items[2],
            },
            Printed::Node {
                depth: 1,
                rid: 4,
                info: &list,
            },
        ];
        assert_eq!(
            format(&printed),
            "[1] list item \"a\"\n[2] list item \"b\"\n[3] list item \"c\"\n [4] list \"files\""
        );
    }

    #[test]
    fn find_is_depth_first_with_reversed_siblings() {
        let cache = sample();
        // The frame is a window title: after every other hit.
        let hits = find(&cache, &["/w"], "", "", 10).unwrap();
        assert_eq!(hits, vec!["/p", "/b2", "/b1", "/w"]);
        let hits = find(&cache, &["/w"], "KAYDET", "push button", 10).unwrap();
        assert_eq!(hits, vec!["/b1"]);
    }

    #[test]
    fn locate_prefers_exact_names_in_document_order() {
        let mut cache = sample();
        cache.insert("/b3", node("push button", "Farklı Kaydet", true, &[], &[]));
        cache.get_mut("/p").unwrap().children = vec!["/b3", "/b1", "/b2", "/h"];
        assert_eq!(
            locate(&cache, &["/w"], "kaydet", "", true).unwrap(),
            Some("/b1")
        );
        assert_eq!(
            locate(&cache, &["/w"], "farklı", "", true).unwrap(),
            Some("/b3")
        );
        assert_eq!(
            locate(&cache, &["/w"], "kaydet", "text", true).unwrap(),
            None
        );
        assert_eq!(locate(&cache, &["/w"], "hidden", "", true).unwrap(), None);
        cache.remove("/b2");
        assert_eq!(
            locate(&cache, &["/w"], "x", "", true).unwrap_err(),
            vec!["/b2"]
        );
    }

    #[test]
    fn queries_do_not_match_role_names_unless_role_is_given() {
        let cache = sample();
        // "a" is in "frame" and "panel"; only names and text count.
        assert_eq!(
            find(&cache, &["/w"], "a", "", 10).unwrap(),
            vec!["/b2", "/b1"]
        );
        assert_eq!(
            find(&cache, &["/w"], "pan", "", 10).unwrap(),
            Vec::<&str>::new()
        );
        assert_eq!(locate(&cache, &["/w"], "pan", "", true).unwrap(), None);
        // With `role`, the role filter is exact and the role is searched.
        assert_eq!(
            find(&cache, &["/w"], "pan", "panel", 10).unwrap(),
            vec!["/p"]
        );
        assert_eq!(
            find(&cache, &["/w"], "button", "push button", 10).unwrap(),
            vec!["/b1"]
        );
        assert_eq!(
            find(&cache, &["/w"], "butt", "", 10).unwrap(),
            Vec::<&str>::new()
        );
    }

    #[test]
    fn window_and_tab_titles_rank_last() {
        let mut cache = sample();
        cache.insert("/t", node("page tab", "Kaydet", true, &[], &[]));
        cache.get_mut("/p").unwrap().children = vec!["/t", "/b1", "/b2", "/h"];
        // find lists titles after the rest, and drops them first.
        assert_eq!(
            find(&cache, &["/w"], "kaydet", "", 10).unwrap(),
            vec!["/b1", "/t"]
        );
        assert_eq!(find(&cache, &["/w"], "kaydet", "", 1).unwrap(), vec!["/b1"]);
        assert_eq!(
            find(&cache, &["/w"], "kaydet", "page tab", 10).unwrap(),
            vec!["/t"]
        );
        // locate takes a content match over an exact title, a title only
        // when nothing else matches, and no title at all with `titles` off.
        assert_eq!(
            locate(&cache, &["/w"], "kaydet", "", true).unwrap(),
            Some("/b1")
        );
        assert_eq!(
            locate(&cache, &["/w"], "ayde", "", true).unwrap(),
            Some("/b1")
        );
        assert_eq!(
            locate(&cache, &["/w"], "win", "", true).unwrap(),
            Some("/w")
        );
        assert_eq!(locate(&cache, &["/w"], "win", "", false).unwrap(), None);
        assert_eq!(
            locate(&cache, &["/w"], "win", "frame", false).unwrap(),
            Some("/w")
        );
        assert_eq!(
            locate(&cache, &["/w"], "kaydet", "page tab", false).unwrap(),
            Some("/t")
        );
        // An exact title beats a partial one.
        cache.insert("/t2", node("page tab", "Kaydet sekmesi", true, &[], &[]));
        cache.insert("/t3", node("page tab", "Sekme", true, &[], &[]));
        cache.get_mut("/p").unwrap().children = vec!["/t2", "/t3"];
        assert_eq!(
            locate(&cache, &["/w"], "sekme", "", true).unwrap(),
            Some("/t3")
        );
    }

    #[test]
    fn every_missing_node_is_reported_in_one_pass() {
        let mut cache = sample();
        cache.insert("/q", node("panel", "", true, &[], &["/c"]));
        cache.get_mut("/w").unwrap().children = vec!["/p", "/q"];
        cache.remove("/b2");
        // Two branches, each with a node not fetched yet.
        let mut lines = Vec::new();
        let missing = walk(&cache, &["/w"], 1, &opts(300), None, &mut lines).unwrap_err();
        assert_eq!(missing, vec!["/b2", "/c"]);
        let mut missing = find(&cache, &["/w"], "zz", "", 10).unwrap_err();
        missing.sort();
        assert_eq!(missing, vec!["/b2", "/c"]);
        let mut missing = locate(&cache, &["/w"], "zz", "", true).unwrap_err();
        missing.sort();
        assert_eq!(missing, vec!["/b2", "/c"]);
        let mut missing = first_with_role(&cache, &["/w"], &DOCUMENT_ROLES).unwrap_err();
        missing.sort();
        assert_eq!(missing, vec!["/b2", "/c"]);
        // A match found while nodes are missing is not final yet.
        assert!(locate(&cache, &["/w"], "kaydet", "", true).is_err());
        cache.insert("/b2", node("text", "b2", true, &[], &[]));
        cache.insert("/c", node("push button", "Kaydet", true, &[], &[]));
        assert_eq!(
            locate(&cache, &["/w"], "kaydet", "", true).unwrap(),
            Some("/b1")
        );
    }

    #[test]
    fn first_with_role_finds_the_document_in_document_order() {
        let mut cache = sample();
        cache.insert("/d1", node("document web", "page", true, &[], &[]));
        cache.insert("/d2", node("document web", "hidden page", false, &[], &[]));
        cache.get_mut("/p").unwrap().children = vec!["/b1", "/d2", "/d1"];
        assert_eq!(
            first_with_role(&cache, &["/w"], &DOCUMENT_ROLES).unwrap(),
            Some("/d1")
        );
        assert_eq!(first_with_role(&cache, &["/w"], &["table"]).unwrap(), None);
    }

    fn with_actions(mut n: Info<&'static str>, actions: &[&str]) -> Info<&'static str> {
        n.actions = actions.iter().map(|a| a.to_string()).collect();
        n
    }

    #[test]
    fn actions_a_role_implies_are_not_listed() {
        let link = with_actions(node("link", "Home", true, &[], &[]), &["jump"]);
        assert_eq!(describe(&link, 1), "[1] link \"Home\"");
        let tab = with_actions(node("page tab", "A", true, &["selected"], &[]), &["switch"]);
        assert_eq!(describe(&tab, 2), "[2] page tab \"A\" {selected}");
        let sec = with_actions(node("section", "", true, &[], &[]), &["click"]);
        assert_eq!(describe(&sec, 3), "[3] section");
        let boxed = with_actions(node("check box", "Bold", true, &[], &[]), &["Toggle"]);
        assert_eq!(describe(&boxed, 4), "[4] check box \"Bold\"");
        // Any other action and the whole list is shown, in order, so the
        // default (the first) stays visible.
        let menu = with_actions(
            node("push button", "Back", true, &[], &[]),
            &["Show Menu", "Press"],
        );
        assert_eq!(
            describe(&menu, 5),
            "[5] push button \"Back\" actions=Show Menu,Press"
        );
        let both = with_actions(
            node("check box", "Edit", true, &[], &[]),
            &["Toggle", "Press"],
        );
        assert_eq!(
            describe(&both, 6),
            "[6] check box \"Edit\" actions=Toggle,Press"
        );
        // Roles that imply nothing list theirs; a label's click is listed.
        let combo = with_actions(node("combo box", "URL", true, &[], &[]), &["activate"]);
        assert_eq!(
            describe(&combo, 7),
            "[7] combo box \"URL\" actions=activate"
        );
        let label = with_actions(node("label", "Share", true, &[], &[]), &["click"]);
        assert_eq!(describe(&label, 8), "[8] label \"Share\" actions=click");
        // A button without any action says so; a section never does.
        let dead = with_actions(node("button", "Menu", true, &[], &[]), &["Set Focus"]);
        assert_eq!(describe(&dead, 9), "[9] button \"Menu\" actions=none");
        let named = node("section", "Player", true, &[], &[]);
        assert_eq!(describe(&named, 10), "[10] section \"Player\"");
    }

    /// (depth, id) of the walk's node lines.
    fn walked(
        cache: &Cache<&'static str>,
        roots: &[&'static str],
        o: &WalkOpts,
    ) -> Vec<(usize, &'static str)> {
        let mut lines = Vec::new();
        walk(cache, roots, 1, o, None, &mut lines).unwrap();
        lines
            .iter()
            .filter_map(|l| match l {
                Line::Node { depth, id } => Some((*depth, *id)),
                Line::Raw(_) => None,
            })
            .collect()
    }

    /// A Firefox page in miniature: nameless landmarks and forms around
    /// content, labels around same-named buttons, nameless images inside
    /// buttons, empty scroll panes.
    fn web_page() -> Cache<&'static str> {
        let mut c = Cache::new();
        c.insert(
            "/doc",
            with_actions(
                node("document web", "Page", true, &[], &["/lm", "/sp", "/nav"]),
                &["click"],
            ),
        );
        c.insert(
            "/lm",
            node("landmark", "", true, &[], &["/form", "/lab2", "/sec"]),
        );
        c.insert("/form", node("form", "", true, &[], &["/lab"]));
        c.insert("/lab", node("label", "Like", true, &[], &["/btn"]));
        c.insert(
            "/btn",
            with_actions(node("button", "Like", true, &[], &["/img"]), &["press"]),
        );
        c.insert("/img", node("image", "", true, &[], &[]));
        c.insert(
            "/lab2",
            with_actions(node("label", "Share", true, &[], &["/btn2"]), &["click"]),
        );
        c.insert(
            "/btn2",
            with_actions(node("button", "Share", true, &[], &[]), &["press"]),
        );
        c.insert(
            "/sec",
            with_actions(node("section", "", true, &[], &["/sep"]), &["click"]),
        );
        c.insert("/sep", node("separator", "", true, &[], &[]));
        c.insert("/sp", node("scroll pane", "", true, &[], &[]));
        c.insert("/nav", node("landmark", "Navigation", true, &[], &["/p"]));
        c.insert("/p", node("paragraph", "", true, &[], &["/link"]));
        c.insert(
            "/link",
            with_actions(node("link", "Home", true, &[], &[]), &["jump"]),
        );
        c
    }

    #[test]
    fn walk_folds_wrappers_and_drops_empty_leaves() {
        let cache = web_page();
        let o = WalkOpts {
            boring_roles: &["section", "panel"],
            ..opts(300)
        };
        assert_eq!(
            walked(&cache, &["/doc"], &o),
            vec![
                (1, "/doc"),
                // landmark and form lifted; the label merged into its button;
                // the nameless image inside it dropped.
                (2, "/btn"),
                // A label with an action of its own stays.
                (2, "/lab2"),
                (3, "/btn2"),
                // A section with an action stays; its empty separator goes.
                (2, "/sec"),
                // The empty scroll pane is gone; a named landmark stays,
                // the text-free paragraph in it is lifted.
                (2, "/nav"),
                (3, "/link"),
            ]
        );
    }

    #[test]
    fn wrappers_with_something_to_show_are_kept() {
        let mut cache = web_page();
        // Text, states or a differently named child keep them.
        cache.get_mut("/lm").unwrap().text = Some("Intro \u{FFFC}".into());
        cache.get_mut("/form").unwrap().states = vec!["focused"];
        cache.get_mut("/lab").unwrap().name = "Like this".into();
        cache.get_mut("/sp").unwrap().states = vec!["focused"];
        let o = WalkOpts {
            boring_roles: &["section", "panel"],
            ..opts(300)
        };
        let got = walked(&cache, &["/doc"], &o);
        for id in ["/lm", "/form", "/lab", "/btn", "/sp"] {
            assert!(got.iter().any(|(_, i)| *i == id), "{id} missing: {got:?}");
        }
        assert!(!got.iter().any(|(_, i)| *i == "/img"));
    }

    #[test]
    fn empty_leaves_are_kept_when_their_children_were_not_walked() {
        let mut cache = web_page();
        cache.get_mut("/sep").unwrap().children = vec!["/link"];
        // At the depth limit its child is not walked: the separator stays,
        // so its subtree can still be expanded by id.
        let shallow = WalkOpts {
            max_depth: 1,
            ..opts(300)
        };
        assert_eq!(walked(&cache, &["/sep"], &shallow), vec![(1, "/sep")]);
        // Same when the budget ran out on it.
        assert_eq!(walked(&cache, &["/sep"], &opts(1)), vec![(1, "/sep")]);
        assert_eq!(
            walked(&cache, &["/sep"], &opts(300)),
            vec![(1, "/sep"), (2, "/link")]
        );
        cache.get_mut("/sep").unwrap().children = vec![];
        assert_eq!(walked(&cache, &["/sep"], &shallow), vec![]);
    }

    #[test]
    fn find_output_uses_the_same_action_rules() {
        let cache = web_page();
        let printed: Vec<Printed<&str>> = ["/link", "/lab2", "/btn2"]
            .iter()
            .enumerate()
            .map(|(k, id)| Printed::Node {
                depth: 0,
                rid: k as u64 + 1,
                info: &cache[id],
            })
            .collect();
        assert_eq!(
            format(&printed),
            "[1] link \"Home\"\n[2] label \"Share\" actions=click\n[3] button \"Share\""
        );
    }

    fn texted(
        role: &str,
        name: &str,
        text: Option<&str>,
        kids: &[&'static str],
    ) -> Info<&'static str> {
        let mut n = node(role, name, true, &[], kids);
        n.text = text.map(str::to_string);
        n
    }

    /// A page as Firefox exposes it: the document's text is one U+FFFC per
    /// child element; a paragraph's text has its links as U+FFFC.
    fn page_text() -> Cache<&'static str> {
        let mut c = Cache::new();
        c.insert(
            "/doc",
            texted(
                "document web",
                "Page",
                Some("\u{FFFC}\u{FFFC}\u{FFFC}"),
                &["/h", "/p", "/ul"],
            ),
        );
        c.insert("/h", texted("heading", "Title", Some("Title"), &[]));
        c.insert(
            "/p",
            texted(
                "paragraph",
                "",
                Some("Read \u{FFFC} or \u{FFFC}\u{FFFC}."),
                &["/a", "/b", "/img"],
            ),
        );
        c.insert("/a", texted("link", "the docs", Some("the docs"), &[]));
        c.insert("/b", texted("link", "", Some("\u{FFFC}"), &["/bimg"]));
        c.insert("/bimg", texted("image", "logo", None, &[]));
        c.insert("/img", texted("image", "star", None, &[]));
        c.insert(
            "/ul",
            texted("list", "", Some("\u{FFFC}\u{FFFC}"), &["/li1", "/li2"]),
        );
        c.insert("/li1", texted("list item", "", Some("• one"), &[]));
        c.insert("/li2", texted("list item", "", Some("• two"), &[]));
        c
    }

    #[test]
    fn container_text_reads_children_in_place_of_placeholders() {
        let cache = page_text();
        assert!(has_embedded_objects(cache["/doc"].text.as_deref().unwrap()));
        assert_eq!(
            container_text(&cache, &"/doc", 1000).unwrap(),
            "Title\nRead the docs or logo star.\n• one\n• two"
        );
        // Cut at the limit.
        assert_eq!(container_text(&cache, &"/doc", 8).unwrap(), "Title\nRe");
        // A container without text reads as its children.
        let mut cache = page_text();
        cache.get_mut("/ul").unwrap().text = None;
        assert_eq!(
            container_text(&cache, &"/ul", 1000).unwrap(),
            "• one\n• two"
        );
    }

    #[test]
    fn container_text_reports_missing_nodes() {
        let mut cache = page_text();
        cache.remove("/a");
        cache.remove("/li2");
        let mut missing = container_text(&cache, &"/doc", 1000).unwrap_err();
        missing.sort();
        assert_eq!(missing, vec!["/a", "/li2"]);
    }

    #[test]
    fn repr_matches_python() {
        assert_eq!(py_repr("a — b"), "'a — b'");
        assert_eq!(py_repr("it's"), "\"it's\"");
        assert_eq!(py_repr("a'b\"c"), "'a\\'b\"c'");
        assert_eq!(py_repr("x\ty"), "'x\\ty'");
    }

    /// (depth, id) of the node lines of a walk that may start `after`.
    fn walked_after(
        cache: &Cache<&'static str>,
        roots: &[&'static str],
        o: &WalkOpts,
        after: Option<&'static str>,
    ) -> (Vec<(usize, &'static str)>, Walked) {
        let mut lines = Vec::new();
        let w = walk(cache, roots, 1, o, after.as_ref(), &mut lines).unwrap();
        let nodes = lines
            .iter()
            .filter_map(|l| match l {
                Line::Node { depth, id } => Some((*depth, *id)),
                Line::Raw(_) => None,
            })
            .collect();
        (nodes, w)
    }

    #[test]
    fn a_cut_walk_continues_after_its_last_element_to_the_end() {
        let cache = web_page();
        let o = |budget| WalkOpts {
            boring_roles: &["section", "panel"],
            ..opts(budget)
        };
        let (full, _) = walked_after(&cache, &["/doc"], &o(300), None);
        // Pages of two lines, each after the last element of the one before.
        let mut paged = Vec::new();
        let mut after = None;
        loop {
            let (page, w) = walked_after(&cache, &["/doc"], &o(2), after);
            assert!(w.after_found);
            paged.extend(page.iter().copied());
            if !w.truncated {
                break;
            }
            after = Some(page.last().unwrap().1);
        }
        assert_eq!(paged, full);
        // An element no longer in the tree is reported.
        let (page, w) = walked_after(&cache, &["/doc"], &o(2), Some("/gone"));
        assert!(page.is_empty() && !w.after_found);
    }

    #[test]
    fn interactive_walk_keeps_operable_elements_under_named_ancestors() {
        let mut c = web_page();
        // A named list item around one same-named link says nothing; a
        // named list with two items is context; a section's click is no
        // reason to list it.
        c.insert("/nav", node("landmark", "Navigation", true, &[], &["/l"]));
        c.insert("/l", node("list", "Menu", true, &[], &["/li", "/li2"]));
        c.insert("/li", node("list item", "Home", true, &[], &["/link"]));
        c.insert("/li2", node("list item", "", true, &[], &["/txt", "/chk"]));
        c.insert("/txt", node("static", "Plain text", true, &[], &[]));
        let mut chk = node("static", "Remember", true, &[], &[]);
        chk.operable = true;
        c.insert("/chk", chk);
        let o = WalkOpts {
            boring_roles: &["section", "panel"],
            interactive: true,
            ..opts(300)
        };
        assert_eq!(
            walked_after(&c, &["/doc"], &o, None).0,
            vec![
                (1, "/doc"),
                (2, "/btn"),
                (2, "/lab2"),
                (3, "/btn2"),
                (2, "/l"),
                (3, "/link"),
                (3, "/chk"),
            ]
        );
        assert!(!interactive(&c["/sec"]));
        assert!(interactive(&with_actions(
            node("section", "", true, &[], &[]),
            &["click", "expand"]
        )));
        assert!(interactive(&node("text", "", true, &["editable"], &[])));
    }

    fn printed_nodes<'a>(infos: &'a [Info<&'static str>]) -> Vec<Printed<'a, &'static str>> {
        let mut out = vec![Printed::Raw("app".into())];
        out.extend(infos.iter().enumerate().map(|(k, info)| Printed::Node {
            depth: 1,
            rid: k as u64 + 1,
            info,
        }));
        out
    }

    #[test]
    fn capped_format_cuts_at_whole_lines_and_names_the_last_element() {
        let infos: Vec<Info<&str>> = ["Alpha", "Beta", "Gamma", "Delta"]
            .iter()
            .enumerate()
            .map(|(k, n)| {
                node(
                    "label",
                    n,
                    true,
                    if k % 2 == 0 { &["focused"] } else { &[] },
                    &[],
                )
            })
            .collect();
        let items = printed_nodes(&infos);
        let all = format(&items);
        let c = format_capped(&items, 0);
        assert_eq!(
            (c.text.as_str(), c.last, c.left),
            (all.as_str(), Some(4), 0)
        );
        // "app" and the first two lines fit in 60 characters.
        let c = format_capped(&items, 60);
        assert_eq!(c.text.lines().count(), 3, "{}", c.text);
        assert!(c.text.chars().count() <= 60);
        assert_eq!((c.last, c.left), (Some(2), 2));
        // At least one element, however small the cap.
        let c = format_capped(&items, 1);
        assert_eq!((c.last, c.left), (Some(1), 3));
        // A folded run is one line: kept or cut whole.
        let plain: Vec<Info<&str>> = ["a", "b", "c"]
            .iter()
            .map(|n| node("list item", n, true, &[], &[]))
            .collect();
        let items = printed_nodes(&plain);
        let c = format_capped(&items, 5);
        assert_eq!((c.last, c.left), (Some(3), 0));
    }

    #[test]
    fn fingerprint_follows_visible_content_only() {
        let cache = sample();
        let fp = fingerprint(&cache, &["/w"]);
        assert_eq!(fp, fingerprint(&sample(), &["/w"]));
        let mut renamed = sample();
        renamed.get_mut("/b1").unwrap().name = "Save".into();
        assert_ne!(fp, fingerprint(&renamed, &["/w"]));
        let mut texted = sample();
        texted.get_mut("/b2").unwrap().text = Some("more".into());
        assert_ne!(fp, fingerprint(&texted, &["/w"]));
        // The hidden button may change freely.
        let mut hidden = sample();
        hidden.get_mut("/h").unwrap().name = "other".into();
        assert_eq!(fp, fingerprint(&hidden, &["/w"]));
    }
}
