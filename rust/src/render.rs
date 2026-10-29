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
            out.push(format!(
                "{indent}{role} ×{}{actions}: {}",
                j - i,
                members.join(" ")
            ));
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
/// their children lifted to the same depth (see `Shape` for what else is
/// left out). On Err, every node the walk could not reach yet, so one fetch
/// brings them all.
pub fn walk<Id: Clone + Eq + Hash>(
    cache: &Cache<Id>,
    children: &[Id],
    depth: usize,
    opts: &WalkOpts,
    lines: &mut Vec<Line<Id>>,
) -> Result<(), Vec<Id>> {
    let mut missing = Vec::new();
    walk_into(cache, children, depth, opts, lines, &mut missing);
    done((), missing)
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
/// get thrown away) to collect all of them.
fn walk_into<Id: Clone + Eq + Hash>(
    cache: &Cache<Id>,
    children: &[Id],
    depth: usize,
    opts: &WalkOpts,
    lines: &mut Vec<Line<Id>>,
    missing: &mut Vec<Id>,
) {
    if lines.len() >= opts.budget || depth > opts.max_depth {
        return;
    }
    for (c, info) in get(cache, children, missing) {
        if lines.len() >= opts.budget {
            // Once, not once per open level.
            if !matches!(lines.last(), Some(Line::Raw(s)) if s == TRUNCATED) {
                lines.push(Line::Raw(TRUNCATED.into()));
            }
            return;
        }
        if opts.only_showing && !info.showing {
            continue;
        }
        if shape(cache, info, opts) == Shape::Lift {
            walk_into(cache, &info.children, depth, opts, lines, missing);
            continue;
        }
        let at = lines.len();
        lines.push(Line::Node {
            depth,
            id: c.clone(),
        });
        // Whether the children were looked at, not cut by budget or depth.
        let walked = at + 1 < opts.budget && depth < opts.max_depth;
        walk_into(cache, &info.children, depth + 1, opts, lines, missing);
        if lines.len() == at + 1
            && (info.children.is_empty() || walked)
            && EMPTY_LEAF_ROLES.contains(&info.role.as_str())
            && bare(info)
            && shown_actions(info).is_empty()
        {
            lines.pop();
        }
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

// ---------------------------------------------------------------- menus

/// Roles of a menu entry. A named "menu" is a submenu entry (GTK, Firefox);
/// an unnamed one only holds entries.
pub const MENU_ITEM_ROLES: [&str; 5] = [
    "menu item",
    "menu",
    "check menu item",
    "radio menu item",
    "tearoff menu item",
];

/// Roles that only hold menu entries: the popup under a Qt or Firefox
/// entry, and unnamed menus or fillers.
const MENU_HOLDER_ROLES: [&str; 4] = ["popup menu", "menu", "filler", "panel"];

/// Where `menu_bars` does not look (nor fetch below): menus, documents and
/// big views, none of which holds an app's menu bar.
pub const MENU_BAR_STOP: [&str; 12] = [
    "menu bar",
    "menu",
    "popup menu",
    "document web",
    "document frame",
    "document",
    "list",
    "table",
    "tree table",
    "tree",
    "terminal",
    "text",
];

/// The menu bars below `roots` (hidden ones too: Dolphin and Kate keep
/// theirs in the tree when the hamburger menu replaces it), in document
/// order, the showing ones first. Children of MENU_BAR_STOP roles are not
/// visited.
pub fn menu_bars<Id: Clone + Eq + Hash>(
    cache: &Cache<Id>,
    roots: &[Id],
) -> Result<Vec<Id>, Vec<Id>> {
    let mut missing = Vec::new();
    let mut stack: Vec<&Id> = get(cache, roots, &mut missing)
        .into_iter()
        .map(|(c, _)| c)
        .rev()
        .collect();
    let mut found = Vec::new();
    let mut seen = 0;
    while let Some(id) = stack.pop() {
        seen += 1;
        if seen > FIND_MAX_SEEN {
            break;
        }
        let info = &cache[id];
        if info.role == "menu bar" {
            found.push(id.clone());
        }
        if MENU_BAR_STOP.contains(&info.role.as_str()) {
            continue;
        }
        stack.extend(
            get(cache, &info.children, &mut missing)
                .into_iter()
                .map(|(c, _)| c)
                .rev(),
        );
    }
    found.sort_by_key(|id| !cache[id].showing);
    done(found, missing)
}

/// The entries of a menu bar or submenu entry `node`: its children of the
/// MENU_ITEM_ROLES with a name, looking through holders (a Qt entry's popup
/// menu); separators and the like are left out.
pub fn menu_items<Id: Clone + Eq + Hash>(cache: &Cache<Id>, node: &Id) -> Result<Vec<Id>, Vec<Id>> {
    let mut missing = Vec::new();
    let mut items = Vec::new();
    let Some(info) = cache.get(node) else {
        return Err(vec![node.clone()]);
    };
    let mut stack: Vec<&Id> = get(cache, &info.children, &mut missing)
        .into_iter()
        .map(|(c, _)| c)
        .rev()
        .collect();
    while let Some(id) = stack.pop() {
        let info = &cache[id];
        let role = info.role.as_str();
        if MENU_ITEM_ROLES.contains(&role) && !info.name.trim().is_empty() {
            items.push(id.clone());
        } else if MENU_HOLDER_ROLES.contains(&role) {
            stack.extend(
                get(cache, &info.children, &mut missing)
                    .into_iter()
                    .map(|(c, _)| c)
                    .rev(),
            );
        }
    }
    done(items, missing)
}

/// Whether a menu entry opens a submenu rather than doing something: a
/// named "menu" (GTK, Firefox), an entry with a popup below it or Qt's
/// "Show Menu" action.
pub fn is_submenu<Id: Clone + Eq + Hash>(cache: &Cache<Id>, info: &Info<Id>) -> bool {
    info.role == "menu"
        || info
            .actions
            .iter()
            .any(|a| a.eq_ignore_ascii_case("show menu"))
        || info
            .children
            .iter()
            .filter_map(|c| cache.get(c))
            .any(|c| c.role == "popup menu" || c.role == "menu")
}

/// A menu label as compared: without accelerator markers (`&`, `_`; doubled
/// ones stand for the character), a shortcut after a tab, or trailing
/// ellipses; case and Turkish dotted/dotless i folded, spaces collapsed.
pub fn menu_norm(label: &str) -> String {
    let label = label.split('\t').next().unwrap_or("");
    let mut out = String::new();
    let mut chars = label.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '&' || c == '_' {
            if chars.peek() == Some(&c) {
                out.push(c);
                chars.next();
            }
            continue;
        }
        out.push(c);
    }
    let mut s = out.trim().to_string();
    loop {
        let t = s
            .trim_end_matches('…')
            .trim_end_matches("...")
            .trim_end()
            .to_string();
        if t == s {
            break;
        }
        s = t;
    }
    let folded: String = s
        .chars()
        .flat_map(char::to_lowercase)
        .filter(|c| *c != '\u{307}')
        .map(|c| if c == 'ı' { 'i' } else { c })
        .collect();
    folded.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The entry of `items` that `want` names: the first whose label equals it
/// (see `menu_norm`), else the only one containing it. Err lists the labels
/// to choose from.
pub fn pick_menu_item<Id: Clone + Eq + Hash>(
    cache: &Cache<Id>,
    items: &[Id],
    want: &str,
) -> Result<Id, String> {
    let w = menu_norm(want);
    let label = |id: &Id| menu_norm(&cache[id].name);
    if let Some(id) = items.iter().find(|id| label(id) == w) {
        return Ok(id.clone());
    }
    let partial: Vec<&Id> = items
        .iter()
        .filter(|id| !w.is_empty() && label(id).contains(&w))
        .collect();
    if let [one] = partial.as_slice() {
        return Ok((*one).clone());
    }
    let names = |ids: &mut dyn Iterator<Item = &Id>| {
        ids.map(|id| json_str(cache[id].name.trim()))
            .collect::<Vec<_>>()
            .join(", ")
    };
    if partial.is_empty() {
        Err(format!(
            "no entry {} here; there are: {}",
            json_str(want),
            names(&mut items.iter())
        ))
    } else {
        Err(format!(
            "{} matches several entries: {}",
            json_str(want),
            names(&mut partial.into_iter())
        ))
    }
}

/// A menu `path`: a list of labels, or one string split at `>`; empty for
/// none.
pub fn menu_path(path: Option<&Json>) -> Result<Vec<String>, String> {
    let bad =
        || "path must be a list of menu labels or one string like \"File > Save As\"".to_string();
    match path {
        None | Some(Json::Null) => Ok(Vec::new()),
        Some(Json::String(s)) => Ok(s
            .split('>')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect()),
        Some(Json::Array(list)) => list
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::trim)
                    .filter(|p| !p.is_empty())
                    .map(str::to_string)
                    .ok_or_else(bad)
            })
            .collect(),
        Some(_) => Err(bad()),
    }
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

    fn menu_cache() -> Cache<&'static str> {
        let mut c = Cache::new();
        c.insert(
            "/w",
            node(
                "frame",
                "notes.txt — Kate",
                true,
                &["active"],
                &["/doc", "/tb", "/mb"],
            ),
        );
        c.insert("/doc", node("text", "notes", true, &[], &["/ignored"]));
        c.insert("/tb", node("tool bar", "", true, &[], &["/hb"]));
        c.insert("/hb", node("push button", "Menüyü Aç", true, &[], &[]));
        // Hidden, as with the hamburger menu: still found.
        c.insert(
            "/mb",
            node("menu bar", "", false, &[], &["/file", "/sep", "/edit"]),
        );
        let mut file = node("menu item", "&Dosya", false, &[], &["/pop"]);
        file.actions = vec!["Show Menu".into()];
        c.insert("/file", file);
        c.insert("/sep", node("separator", "", false, &["disabled"], &[]));
        c.insert("/edit", node("menu", "Düzen", false, &[], &[]));
        c.insert(
            "/pop",
            node(
                "popup menu",
                "",
                false,
                &[],
                &["/new", "/s1", "/saveas", "/savecopy", "/quit"],
            ),
        );
        c.insert("/new", node("menu item", "Yeni", false, &[], &[]));
        c.insert("/s1", node("separator", "", false, &[], &[]));
        c.insert(
            "/saveas",
            node("menu item", "Farklı Kaydet…", false, &[], &[]),
        );
        c.insert(
            "/savecopy",
            node(
                "menu item",
                "Kopyayı Farklı Kaydet…\tCtrl+Shift+S",
                false,
                &[],
                &[],
            ),
        );
        c.insert("/quit", node("menu item", "Çık", false, &["disabled"], &[]));
        c
    }

    #[test]
    fn menu_labels_compare_without_markers_case_or_ellipses() {
        assert_eq!(menu_norm("&Dosya"), "dosya");
        assert_eq!(menu_norm("Save _As..."), "save as");
        assert_eq!(menu_norm("Farklı Kaydet…"), menu_norm("farkli kaydet"));
        assert_eq!(menu_norm("İPTAL"), menu_norm("iptal"));
        assert_eq!(menu_norm("Rock && Roll"), "rock & roll");
        assert_eq!(menu_norm("Print\tCtrl+P"), "print");
        assert_eq!(menu_norm("  Open   Recent … "), "open recent");
    }

    #[test]
    fn menu_bars_include_hidden_ones_and_skip_documents() {
        let c = menu_cache();
        assert_eq!(menu_bars(&c, &["/w"]).unwrap(), vec!["/mb"]);
        let mut shown = c.clone();
        shown.insert("/mb2", node("menu bar", "", true, &[], &[]));
        shown.get_mut("/tb").unwrap().children.push("/mb2");
        assert_eq!(menu_bars(&shown, &["/w"]).unwrap(), vec!["/mb2", "/mb"]);
        // "/ignored" below the text is never asked for.
        let mut gap = c.clone();
        gap.remove("/hb");
        assert_eq!(menu_bars(&gap, &["/w"]), Err(vec!["/hb"]));
    }

    #[test]
    fn menu_items_look_through_popups_and_skip_separators() {
        let c = menu_cache();
        assert_eq!(menu_items(&c, &"/mb").unwrap(), vec!["/file", "/edit"]);
        assert_eq!(
            menu_items(&c, &"/file").unwrap(),
            vec!["/new", "/saveas", "/savecopy", "/quit"]
        );
        assert!(menu_items(&c, &"/edit").unwrap().is_empty());
        assert!(is_submenu(&c, &c["/file"]) && is_submenu(&c, &c["/edit"]));
        assert!(!is_submenu(&c, &c["/saveas"]));
        let mut gap = c.clone();
        gap.remove("/pop");
        assert_eq!(menu_items(&gap, &"/file"), Err(vec!["/pop"]));
    }

    #[test]
    fn menu_entries_are_picked_exactly_then_by_a_unique_part() {
        let c = menu_cache();
        let items = menu_items(&c, &"/file").unwrap();
        assert_eq!(pick_menu_item(&c, &items, "farklı kaydet"), Ok("/saveas"));
        assert_eq!(
            pick_menu_item(&c, &items, "FARKLI KAYDET..."),
            Ok("/saveas")
        );
        assert_eq!(pick_menu_item(&c, &items, "kopya"), Ok("/savecopy"));
        let err = pick_menu_item(&c, &items, "kaydet").unwrap_err();
        assert!(
            err.contains("several") && err.contains("\"Farklı Kaydet…\""),
            "{err}"
        );
        let err = pick_menu_item(&c, &items, "Save As").unwrap_err();
        assert!(
            err.contains("there are: \"Yeni\", \"Farklı Kaydet…\""),
            "{err}"
        );
        let top = menu_items(&c, &"/mb").unwrap();
        assert_eq!(pick_menu_item(&c, &top, "dosya"), Ok("/file"));
    }

    #[test]
    fn menu_paths_are_lists_or_split_at_arrows() {
        let p = |v: Json| menu_path(Some(&v));
        assert_eq!(
            p(Json::from("Dosya > Farklı Kaydet…")).unwrap(),
            vec!["Dosya", "Farklı Kaydet…"]
        );
        assert_eq!(
            p(serde_json::json!(["A > B", " C "])).unwrap(),
            vec!["A > B", "C"]
        );
        assert!(p(Json::from("")).unwrap().is_empty());
        assert!(menu_path(None).unwrap().is_empty());
        assert!(p(serde_json::json!(["A", 3])).is_err());
        assert!(p(serde_json::json!(["A", " "])).is_err());
        assert!(p(serde_json::json!(5)).is_err());
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
        walk(&cache, &["/w"], 1, &opts(300), &mut lines).unwrap();
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
        let missing = walk(&cache, &["/w"], 1, &opts(300), &mut Vec::new()).unwrap_err();
        assert_eq!(missing, vec!["/b2"]);

        let cache = sample();
        let mut lines = Vec::new();
        walk(&cache, &["/w"], 1, &opts(2), &mut lines).unwrap();
        assert_eq!(lines.last(), Some(&Line::Raw(TRUNCATED.into())));
        assert_eq!(
            lines.iter().filter(|l| matches!(l, Line::Raw(_))).count(),
            1
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
        walk(&cache, &["/w"], 1, &opts(300), &mut lines).unwrap();
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
        let missing = walk(&cache, &["/w"], 1, &opts(300), &mut lines).unwrap_err();
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
        walk(cache, roots, 1, o, &mut lines).unwrap();
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
}
