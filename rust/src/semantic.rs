//! `act`'s verified actions (check, uncheck, toggle, expand, collapse,
//! select with `option`, scroll_to): the parts that only look at what was
//! read. Which verb an `action` names, which of an element's actions does
//! it, which entries a control offers and which one it shows.

use std::hash::Hash;

use crate::backend::Cache;
use crate::render;

/// A verified action of `act`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verb {
    Check,
    Uncheck,
    Toggle,
    Expand,
    Collapse,
    Select,
    ScrollTo,
}

/// An action name as compared: lower case, without spaces, `_` or `-`
/// ("Expand or contract" and "expandOrContract" alike).
pub fn norm(action: &str) -> String {
    action
        .chars()
        .filter(|c| !matches!(c, ' ' | '_' | '-'))
        .flat_map(char::to_lowercase)
        .collect()
}

impl Verb {
    /// The verb `action` names, if any.
    pub fn parse(action: &str) -> Option<Verb> {
        Some(match norm(action).as_str() {
            "check" => Verb::Check,
            "uncheck" => Verb::Uncheck,
            "toggle" => Verb::Toggle,
            "expand" => Verb::Expand,
            "collapse" => Verb::Collapse,
            "select" => Verb::Select,
            "scrollto" => Verb::ScrollTo,
            _ => return None,
        })
    }

    /// What the on-screen indicator says while it runs.
    pub fn label(self) -> &'static str {
        match self {
            Verb::Check | Verb::Uncheck | Verb::Toggle => "checking",
            Verb::Expand | Verb::Collapse => "expanding",
            Verb::Select => "selecting",
            Verb::ScrollTo => "scrolling into view",
        }
    }
}

/// Whether `role` is one of `roles`, compared as `norm` does: Windows
/// spells them without spaces ("checkbox", "listitem").
fn role_in(role: &str, roles: &[&str]) -> bool {
    let r = norm(role);
    roles.iter().any(|x| norm(x) == r)
}

/// Roles that are checked or unchecked whatever the platform says.
const CHECK_ROLES: [&str; 6] = [
    "check box",
    "check menu item",
    "radio button",
    "radio menu item",
    "toggle button",
    "switch",
];

/// Whether an element of `role` is checked or unchecked whatever the
/// platform says.
pub fn is_check_role(role: &str) -> bool {
    role_in(role, &CHECK_ROLES)
}

/// Roles unchecked only by checking another one.
pub fn is_radio(role: &str) -> bool {
    role_in(role, &["radio button", "radio menu item"])
}

/// Roles whose expanding opens a menu or popup.
pub fn opens_menu(role: &str) -> bool {
    role_in(role, &["combo box", "menu", "menu item", "menu bar"])
}

/// Entries of a list, menu or tab list: what `select` picks among. An
/// item's own children are not entries of the control. "tab item" and
/// "data item" are Windows' page tab and table cell.
const OPTION_ROLES: [&str; 11] = [
    "list item",
    "menu item",
    "check menu item",
    "radio menu item",
    "tree item",
    "table cell",
    "table row",
    "page tab",
    "radio button",
    "tab item",
    "data item",
];

/// Whether an element of `role` is an entry `select` picks among.
pub fn is_option_role(role: &str) -> bool {
    role_in(role, &OPTION_ROLES)
}

/// Roles whose generic press or click does something other than expand
/// or check (open a file, select a row): only an action named for that
/// does.
const ITEM_ROLES: [&str; 7] = [
    "list item",
    "tree item",
    "table cell",
    "table row",
    "tree table",
    "menu item",
    "data item",
];

/// Whether an element of `role` is a row or cell of a list, tree or table
/// (or a menu entry), whose generic action opens or selects it.
pub fn is_item_role(role: &str) -> bool {
    role_in(role, &ITEM_ROLES)
}

/// Rows and cells, which never open a popup to select in: their generic
/// action opens the file or selects the row.
const ROW_ROLES: [&str; 6] = [
    "list item",
    "tree item",
    "table cell",
    "table row",
    "tree table",
    "data item",
];

/// Whether `select` may open the control (`role`, with `actions` and the
/// AT-SPI HAS_POPUP state `popup`) to read its entries: a combo box, a
/// menu, a button that says it has a popup, or one with an action named
/// for opening. Never a row or cell, nor a plain button (OK, Delete).
pub fn may_open(role: &str, actions: &[String], popup: bool) -> bool {
    if role_in(role, &ROW_ROLES) {
        return false;
    }
    role_in(role, &["combo box", "menu", "push button menu"])
        || popup
        || first_of(actions, &["showmenu", "open", "expand"]).is_some()
}

/// Index of the first of `names` (in `norm` form, in order of preference)
/// that `actions` has; showContextMenu never.
fn first_of(actions: &[String], names: &[&str]) -> Option<usize> {
    let normed: Vec<String> = actions.iter().map(|a| norm(a)).collect();
    names
        .iter()
        .find_map(|want| normed.iter().position(|a| a == want))
}

/// The action that makes a checkable element of `role` `checked`
/// (`want`). A row or cell (Qt's checkable item-view cells, GTK's toggle
/// cells) only by an action named for checking, or toggle where that is
/// not Qt's (`qt`), which flips the row's selection: press, click and
/// activate select or open it.
pub fn check_action(role: &str, actions: &[String], want: bool, qt: bool) -> Option<usize> {
    if is_item_role(role) {
        let named = if want { "check" } else { "uncheck" };
        let names: &[&str] = if qt { &[named] } else { &[named, "toggle"] };
        return first_of(actions, names);
    }
    let names: &[&str] = if want {
        &[
            "check",
            "toggle",
            "press",
            "click",
            "select",
            "activate",
            "dodefault",
        ]
    } else {
        &[
            "uncheck",
            "toggle",
            "press",
            "click",
            "activate",
            "dodefault",
        ]
    };
    first_of(actions, names)
}

/// Actions named for expanding or collapsing.
const EXPAND_NAMES: [&str; 4] = ["expand", "expandorcontract", "expandorcollapse", "open"];
const COLLAPSE_NAMES: [&str; 4] = ["collapse", "expandorcontract", "expandorcollapse", "close"];
/// What a disclosure button, an accordion header or a combo box expands
/// with when it has nothing named for it.
const PRESS_NAMES: [&str; 6] = [
    "press",
    "click",
    "toggle",
    "showmenu",
    "activate",
    "dodefault",
];

/// Whether `actions` has one named for expanding or collapsing.
pub fn has_expand_action(actions: &[String]) -> bool {
    first_of(actions, &EXPAND_NAMES[..3]).is_some()
}

/// The action that expands (`want`) or collapses an element of `role`.
pub fn expand_action(role: &str, actions: &[String], want: bool) -> Option<usize> {
    let named: &[&str] = if want { &EXPAND_NAMES } else { &COLLAPSE_NAMES };
    first_of(actions, named).or_else(|| {
        (!is_item_role(role))
            .then(|| first_of(actions, &PRESS_NAMES))
            .flatten()
    })
}

/// The action that picks an entry (a list item's select, a menu item's
/// click). Not toggle: Qt's flips a row's selection without choosing it.
pub fn item_action(actions: &[String]) -> Option<usize> {
    first_of(
        actions,
        &["select", "click", "press", "activate", "trigger", "check"],
    )
}

/// The action that opens a combo box's or menu button's popup (one that
/// `may_open` allows).
pub fn open_action(actions: &[String]) -> Option<usize> {
    first_of(
        actions,
        &["showmenu", "open", "expand", "press", "click", "activate"],
    )
}

/// The entries of the control `root` (OPTION_ROLES), in tree order, from
/// what `cache` holds; an entry's own children are not looked into.
pub fn options<Id: Clone + Eq + Hash>(cache: &Cache<Id>, root: &Id) -> Vec<Id> {
    walk_options(cache, root).0
}

/// `options`, and the nodes on the way to more that `cache` lacks.
pub fn walk_options<Id: Clone + Eq + Hash>(cache: &Cache<Id>, root: &Id) -> (Vec<Id>, Vec<Id>) {
    let mut out = Vec::new();
    let mut missing = Vec::new();
    let mut stack: Vec<Id> = match cache.get(root) {
        Some(info) => info.children.iter().rev().cloned().collect(),
        None => return (out, vec![root.clone()]),
    };
    while let Some(id) = stack.pop() {
        let Some(info) = cache.get(&id) else {
            missing.push(id);
            continue;
        };
        if is_option_role(&info.role) {
            if !info.name.trim().is_empty() {
                out.push(id);
            }
            continue;
        }
        stack.extend(info.children.iter().rev().cloned());
    }
    (out, missing)
}

/// Which of `labels` a control named `name` (with `text`) shows as its
/// value: a combo box is named after it on Qt and GTK. Its index.
pub fn named_option(name: &str, text: Option<&str>, labels: &[String]) -> Option<usize> {
    let shown: Vec<String> = [Some(name), text]
        .into_iter()
        .flatten()
        .map(render::menu_norm)
        .filter(|s| !s.is_empty())
        .collect();
    if shown.is_empty() {
        return None;
    }
    labels
        .iter()
        .position(|l| shown.contains(&render::menu_norm(l)))
}

/// The entry `control` shows now: the one its name or text names
/// (`named_option`), else the one selected (or checked); None when
/// several are (a multi-selection list).
pub fn current<Id: Clone + Eq + Hash>(cache: &Cache<Id>, control: &Id, items: &[Id]) -> Option<Id> {
    let info = cache.get(control)?;
    let labels: Vec<String> = items.iter().map(|id| cache[id].name.clone()).collect();
    if let Some(i) = named_option(&info.name, info.text.as_deref(), &labels) {
        return Some(items[i].clone());
    }
    let mut on = items.iter().filter(|id| {
        let st = &cache[*id].states;
        st.contains(&"selected") || st.contains(&"checked")
    });
    match (on.next(), on.next()) {
        (Some(one), None) => Some(one.clone()),
        _ => None,
    }
}

/// Entries named in an error at most.
const OPTIONS_LISTED: usize = 25;

/// The entry of `items` that `want` names (as `render::pick_menu_item`:
/// equal label first, else the only one containing it). Err lists the
/// choices, at most OPTIONS_LISTED.
pub fn pick_option<Id: Clone + Eq + Hash>(
    cache: &Cache<Id>,
    items: &[Id],
    want: &str,
) -> Result<Id, String> {
    render::pick_menu_item(cache, items, want).map_err(|_| {
        let w = render::menu_norm(want);
        let partial: Vec<&Id> = items
            .iter()
            .filter(|id| !w.is_empty() && render::menu_norm(&cache[*id].name).contains(&w))
            .collect();
        let (what, list): (String, Vec<&Id>) = if partial.len() > 1 {
            (
                format!("{} matches several entries", render::json_str(want)),
                partial,
            )
        } else {
            (
                format!("no entry {} here", render::json_str(want)),
                items.iter().collect(),
            )
        };
        let mut names: Vec<String> = list
            .iter()
            .take(OPTIONS_LISTED)
            .map(|id| render::json_str(cache[*id].name.trim()))
            .collect();
        if list.len() > OPTIONS_LISTED {
            names.push(format!("… {} more", list.len() - OPTIONS_LISTED));
        }
        format!("{what}; there are: {}", names.join(", "))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Info;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn node(role: &str, name: &str, kids: &[u32], states: &[&'static str]) -> Info<u32> {
        Info {
            role: role.into(),
            name: name.into(),
            children: kids.to_vec(),
            states: states.to_vec(),
            showing: true,
            ..Info::default()
        }
    }

    #[test]
    fn verbs_parse_in_any_spelling() {
        assert_eq!(Verb::parse("Check"), Some(Verb::Check));
        assert_eq!(Verb::parse("scroll_to"), Some(Verb::ScrollTo));
        assert_eq!(Verb::parse("scroll to"), Some(Verb::ScrollTo));
        assert_eq!(Verb::parse("Expand"), Some(Verb::Expand));
        assert_eq!(Verb::parse("press"), None);
        assert_eq!(Verb::parse("expand or contract"), None);
    }

    #[test]
    fn check_actions_by_toolkit() {
        let cb = "check box";
        // Qt check box.
        assert_eq!(
            check_action(cb, &names(&["Toggle", "Press"]), true, true),
            Some(0)
        );
        // Firefox names the action after what it will do.
        assert_eq!(check_action(cb, &names(&["check"]), true, false), Some(0));
        assert_eq!(
            check_action(cb, &names(&["uncheck"]), false, false),
            Some(0)
        );
        assert_eq!(check_action(cb, &names(&["uncheck"]), true, false), None);
        // GTK.
        assert_eq!(check_action(cb, &names(&["click"]), false, false), Some(0));
        assert_eq!(
            check_action(cb, &names(&["showContextMenu"]), true, false),
            None
        );
        // Windows: UI Automation's Toggle.
        assert_eq!(
            check_action("checkbox", &names(&["toggle"]), true, false),
            Some(0)
        );
    }

    #[test]
    fn rows_are_checked_only_by_a_check_action() {
        // A checkable Qt item-view cell: Toggle selects the row.
        assert_eq!(
            check_action("tree item", &names(&["Toggle"]), true, true),
            None
        );
        // GTK's toggle cell: toggle checks it; activate opens the row.
        let gtk = names(&["activate", "toggle"]);
        assert_eq!(check_action("table cell", &gtk, true, false), Some(1));
        assert_eq!(
            check_action("list item", &names(&["activate", "click"]), true, false),
            None
        );
        // Firefox.
        assert_eq!(
            check_action("list item", &names(&["check"]), true, false),
            Some(0)
        );
    }

    #[test]
    fn windows_role_spellings() {
        assert!(is_check_role("checkbox") && is_check_role("check box"));
        assert!(is_radio("radiobutton"));
        assert!(opens_menu("combobox"));
        for r in ["listitem", "treeitem", "menuitem", "tabitem", "dataitem"] {
            assert!(is_option_role(r), "{r}");
        }
        assert!(is_item_role("listitem") && is_item_role("dataitem"));
        assert!(!is_item_role("button"));
        assert_eq!(expand_action("treeitem", &names(&["invoke"]), true), None);
    }

    #[test]
    fn only_popup_controls_are_opened_to_select() {
        let press = names(&["Press"]);
        assert!(!may_open("push button", &press, false));
        assert!(may_open("push button", &press, true));
        assert!(may_open(
            "push button",
            &names(&["Press", "Show Menu"]),
            false
        ));
        assert!(may_open("combo box", &press, false));
        assert!(may_open("combobox", &names(&["expand", "collapse"]), false));
        // A row is never opened, whatever it says.
        assert!(!may_open("list item", &names(&["activate"]), true));
        assert!(!may_open("tree item", &names(&["expand"]), false));
        assert!(!may_open("menu item", &names(&["click"]), false));
        assert!(may_open("menu item", &names(&["click"]), true));
    }

    #[test]
    fn expand_actions_never_press_an_item() {
        let gtk = names(&["activate", "edit", "expand or contract"]);
        assert_eq!(expand_action("table cell", &gtk, true), Some(2));
        assert_eq!(expand_action("table cell", &gtk, false), Some(2));
        let firefox = names(&["collapse"]);
        assert_eq!(expand_action("tree item", &firefox, false), Some(0));
        assert_eq!(expand_action("tree item", &firefox, true), None);
        // A Qt tree item's Toggle selects it: not an expand.
        assert_eq!(expand_action("tree item", &names(&["Toggle"]), true), None);
        // A disclosure button presses.
        assert_eq!(
            expand_action("push button", &names(&["Press"]), true),
            Some(0)
        );
        assert!(has_expand_action(&gtk));
        assert!(!has_expand_action(&names(&["open"])));
    }

    #[test]
    fn item_and_open_actions() {
        assert_eq!(item_action(&names(&["Toggle"])), None);
        assert_eq!(item_action(&names(&["jump", "select"])), Some(1));
        assert_eq!(open_action(&names(&["Press", "Show Menu"])), Some(1));
    }

    /// A combo box (1) named after its value, holding a list (2) of three
    /// entries (3-5), entry 5 with a child of its own.
    fn combo(name: &str, selected: Option<u32>) -> Cache<u32> {
        let mut c = Cache::new();
        c.insert(1, node("combo box", name, &[2], &[]));
        c.insert(2, node("list", "", &[3, 4, 5], &[]));
        for (id, n) in [(3, "UTF-8"), (4, "UTF-16"), (5, "ISO-8859-1")] {
            let st: &[&'static str] = if selected == Some(id) {
                &["selected"]
            } else {
                &[]
            };
            let kids: &[u32] = if id == 5 { &[6] } else { &[] };
            c.insert(id, node("list item", n, kids, st));
        }
        c.insert(6, node("list item", "nested", &[], &[]));
        c
    }

    #[test]
    fn options_and_the_one_shown() {
        let c = combo("UTF-16", Some(3));
        let items = options(&c, &1);
        assert_eq!(items, vec![3, 4, 5]);
        // The name wins over a stale selection.
        assert_eq!(current(&c, &1, &items), Some(4));
        // A control named after its label: the selected entry.
        let c = combo("Encoding", Some(5));
        assert_eq!(current(&c, &1, &items), Some(5));
        let c = combo("Encoding", None);
        assert_eq!(current(&c, &1, &items), None);
        // Several selected (a multi-selection list): none is the one shown.
        let mut c = combo("Encoding", Some(5));
        c.get_mut(&3).unwrap().states.push("selected");
        assert_eq!(current(&c, &1, &items), None);
    }

    #[test]
    fn options_are_picked_by_label() {
        let c = combo("x", None);
        let items = options(&c, &1);
        assert_eq!(pick_option(&c, &items, "utf-8"), Ok(3));
        assert_eq!(pick_option(&c, &items, "8859"), Ok(5));
        let e = pick_option(&c, &items, "UTF").unwrap_err();
        assert_eq!(
            e,
            "\"UTF\" matches several entries; there are: \"UTF-8\", \"UTF-16\""
        );
        let e = pick_option(&c, &items, "latin").unwrap_err();
        assert!(
            e.starts_with("no entry \"latin\" here; there are: \"UTF-8\""),
            "{e}"
        );
    }
}
