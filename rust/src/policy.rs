//! The allowlist file: which apps may be used, how far (tiers), which tool
//! calls need the user's confirmation, and where password fields may be
//! filled in.
//!
//! ```text
//! firefox              # full: everything (the default)
//! konsole:view         # read tools only
//! dolphin:click        # plus act, click, focus, scroll, drag, mouse
//! confirm: close_window, key:konsole
//! passwords: keepassxc
//! ```
//!
//! Lines without a `:` are plain app names, as in files from before tiers,
//! which therefore keep working unchanged.

use std::collections::BTreeMap;
use std::fmt;

use serde_json::Value as Json;

/// How far an allowed app may be used. Ordered: each tier allows what the
/// ones before it do.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    /// Read tools only: tree, find, get_text, wait_for, screenshot…
    View,
    /// Plus pointer input and actions: act, click, focus, scroll, drag,
    /// mouse (and window management); no typing, keys or text writes.
    Click,
    /// Everything.
    Full,
}

impl Tier {
    fn parse(s: &str) -> Option<Tier> {
        match s {
            "view" => Some(Tier::View),
            "click" => Some(Tier::Click),
            "full" => Some(Tier::Full),
            _ => None,
        }
    }

    /// What the tier allows, for refusals.
    pub fn allows(self) -> &'static str {
        match self {
            Tier::View => "reading only (tree, find, get_text, wait_for, screenshot)",
            Tier::Click => {
                "reading, act, click, focus, scroll, drag, mouse and window management, \
                 but no typing, keys, set_text or clipboard_set"
            }
            Tier::Full => "everything",
        }
    }
}

impl fmt::Display for Tier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Tier::View => "view",
            Tier::Click => "click",
            Tier::Full => "full",
        })
    }
}

/// The tier a tool call needs in the app it reads or sends input to.
/// Tools not listed (new ones included) need `full`, so a tool added
/// later is never let through by mistake.
pub fn needed_tier(tool: &str, args: &Json) -> Tier {
    match tool {
        "apps" | "windows" | "doctor" | "tree" | "find" | "get_text" | "wait_for" | "wait_idle"
        | "screenshot" | "pause" | "clipboard_get" | "cursor_position" | "activate_window"
        | "batch" => Tier::View,
        // Starting an app with arguments can run anything it can.
        "launch" => match args.get("args").and_then(Json::as_array) {
            Some(list) if !list.is_empty() => Tier::Full,
            _ => Tier::Click,
        },
        "act" | "menu" | "click" | "focus" | "scroll" | "drag" | "mouse" | "move_window"
        | "close_window" => Tier::Click,
        // hold presses keys as well as buttons.
        _ => Tier::Full,
    }
}

/// One `confirm` entry: a tool, for every app or only for `app`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfirmRule {
    pub tool: String,
    pub app: Option<String>,
}

impl fmt::Display for ConfirmRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.app {
            Some(app) => write!(f, "{}:{app}", self.tool),
            None => f.write_str(&self.tool),
        }
    }
}

/// The parsed allowlist file.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Policy {
    /// Allowed apps (lower case) and their tiers.
    pub apps: BTreeMap<String, Tier>,
    pub confirm: Vec<ConfirmRule>,
    /// Apps whose password fields set_text and type_text may fill in;
    /// `*` for all.
    pub passwords: Vec<String>,
    /// Lines that could not be read as meant, for doctor.
    pub warnings: Vec<String>,
}

/// Set to 1 to let set_text and type_text fill in password fields in any
/// allowed app (full tier still needed).
pub const PASSWORDS_ENV: &str = "DESKTOP_MCP_ALLOW_PASSWORDS";

/// Directive lines: `name: item, item…`.
const DIRECTIVES: [&str; 2] = ["confirm", "passwords"];

/// Items of a directive: comma- or space-separated, lower case.
fn items(rest: &str) -> impl Iterator<Item = String> + '_ {
    rest.split([',', ' ', '\t'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

impl Policy {
    /// Parse the allowlist file's text. `#` starts a comment; names are
    /// case-insensitive. An app listed twice gets the lower tier, and an
    /// unknown tier counts as `view` (with a warning), so a typo never
    /// grants more than meant.
    pub fn parse(text: &str) -> Policy {
        let mut p = Policy::default();
        for raw in text.lines() {
            let line = raw.split('#').next().unwrap_or("").trim().to_lowercase();
            if line.is_empty() {
                continue;
            }
            if let Some((head, rest)) = line.split_once(':')
                && DIRECTIVES.contains(&head.trim())
                && Tier::parse(rest.trim()).is_none()
            {
                match head.trim() {
                    "confirm" => {
                        for item in items(rest) {
                            let rule = match item.split_once(':') {
                                Some((tool, app)) if !tool.is_empty() && !app.is_empty() => {
                                    ConfirmRule {
                                        tool: tool.into(),
                                        app: Some(app.into()),
                                    }
                                }
                                Some(_) => {
                                    p.warnings
                                        .push(format!("confirm item '{item}' is not tool[:app]"));
                                    continue;
                                }
                                None => ConfirmRule {
                                    tool: item,
                                    app: None,
                                },
                            };
                            if !p.confirm.contains(&rule) {
                                p.confirm.push(rule);
                            }
                        }
                    }
                    _ => {
                        for app in items(rest) {
                            let app = if app == "all" { "*".into() } else { app };
                            if !p.passwords.contains(&app) {
                                p.passwords.push(app);
                            }
                        }
                    }
                }
                continue;
            }
            let (app, tier) = match line.rsplit_once(':') {
                None => (line.as_str(), Tier::Full),
                Some((app, tier)) => {
                    let app = app.trim();
                    let tier = tier.trim();
                    match Tier::parse(tier) {
                        Some(t) => (app, t),
                        None => {
                            p.warnings.push(format!(
                                "unknown tier '{tier}' for '{app}' (view, click or full); \
                                 treated as view"
                            ));
                            (app, Tier::View)
                        }
                    }
                }
            };
            if app.is_empty() {
                p.warnings.push(format!("line '{line}' names no app"));
                continue;
            }
            p.apps
                .entry(app.to_string())
                .and_modify(|t| *t = (*t).min(tier))
                .or_insert(tier);
        }
        p
    }

    /// Allowed app names, sorted.
    pub fn names(&self) -> Vec<String> {
        self.apps.keys().cloned().collect()
    }

    /// The tier of `app` (any case, trimmed); None when it is not allowed.
    pub fn tier(&self, app: &str) -> Option<Tier> {
        self.apps.get(&app.trim().to_lowercase()).copied()
    }

    /// Allowed apps with at least tier `need`.
    pub fn names_with(&self, need: Tier) -> Vec<String> {
        self.apps
            .iter()
            .filter(|(_, t)| **t >= need)
            .map(|(a, _)| a.clone())
            .collect()
    }

    /// Whether a call of `tool` on `app` (None: not known) needs the user's
    /// confirmation: a rule names the tool for every app, or for this one.
    pub fn needs_confirm(&self, tool: &str, app: Option<&str>) -> bool {
        let app = app.map(|a| a.trim().to_lowercase());
        self.confirm
            .iter()
            .any(|r| r.tool == tool && (r.app.is_none() || r.app == app))
    }

    /// Whether set_text and type_text may fill in password fields of `app`:
    /// `passwords:` names it (or `*`), or PASSWORDS_ENV is 1. `env` is the
    /// variable's value, passed in for tests.
    pub fn passwords_allowed(&self, app: &str, env: Option<&str>) -> bool {
        let app = app.trim().to_lowercase();
        env.map(str::trim) == Some("1") || self.passwords.iter().any(|a| a == "*" || *a == app)
    }

    /// `firefox, konsole:view`: the apps as doctor lists them, the tier
    /// shown unless it is full.
    pub fn apps_line(&self) -> String {
        self.apps
            .iter()
            .map(|(a, t)| match t {
                Tier::Full => a.clone(),
                t => format!("{a}:{t}"),
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn old_files_read_as_full_access() {
        let p = Policy::parse("# apps\nKate\n  firefox  # browser\n\nkate\n");
        assert_eq!(p.names(), vec!["firefox", "kate"]);
        assert_eq!(p.tier("FIREFOX"), Some(Tier::Full));
        assert_eq!(p.tier("dolphin"), None);
        assert!(p.confirm.is_empty() && p.passwords.is_empty() && p.warnings.is_empty());
    }

    #[test]
    fn tiers_directives_and_mistakes() {
        let p = Policy::parse(
            "firefox\nkonsole:view\nDolphin : Click\nkate:full\nokular:veiw\n\
             firefox:click\n:view\n\
             confirm: close_window, key:konsole\nconfirm: set_text:kate close_window\n\
             passwords: keepassxc, all\nconfirm: :x\n",
        );
        assert_eq!(p.tier("firefox"), Some(Tier::Click), "lowest tier wins");
        assert_eq!(p.tier("konsole"), Some(Tier::View));
        assert_eq!(p.tier("dolphin"), Some(Tier::Click));
        assert_eq!(p.tier("kate"), Some(Tier::Full));
        assert_eq!(p.tier("okular"), Some(Tier::View), "typo counts as view");
        assert_eq!(
            p.names_with(Tier::Click),
            vec!["dolphin", "firefox", "kate"]
        );
        assert_eq!(
            p.confirm.iter().map(|r| r.to_string()).collect::<Vec<_>>(),
            vec!["close_window", "key:konsole", "set_text:kate"]
        );
        assert_eq!(p.passwords, vec!["keepassxc", "*"]);
        assert_eq!(p.warnings.len(), 3, "{:?}", p.warnings);
        assert!(p.warnings[0].contains("veiw"), "{:?}", p.warnings);
        assert_eq!(
            p.apps_line(),
            "dolphin:click, firefox:click, kate, konsole:view, okular:view"
        );
        // An app called like a directive still works with a tier.
        let p = Policy::parse("confirm:view\n");
        assert_eq!(p.tier("confirm"), Some(Tier::View));
        assert!(p.confirm.is_empty());
    }

    #[test]
    fn confirm_rules_match_tool_and_app() {
        let p = Policy::parse("confirm: close_window, key:konsole\n");
        assert!(p.needs_confirm("close_window", None));
        assert!(p.needs_confirm("close_window", Some("kate")));
        assert!(p.needs_confirm("key", Some("Konsole")));
        assert!(!p.needs_confirm("key", Some("kate")));
        assert!(!p.needs_confirm("key", None));
        assert!(!p.needs_confirm("type_text", Some("konsole")));
    }

    #[test]
    fn password_opt_in() {
        let p = Policy::parse("firefox\nkeepassxc\npasswords: keepassxc\n");
        assert!(p.passwords_allowed("KeePassXC", None));
        assert!(!p.passwords_allowed("firefox", None));
        assert!(!p.passwords_allowed("firefox", Some("0")));
        assert!(p.passwords_allowed("firefox", Some("1")));
        assert!(Policy::parse("passwords: *").passwords_allowed("x", None));
    }

    #[test]
    fn tools_need_the_tier_of_their_class() {
        let none = json!({});
        for t in [
            "apps",
            "windows",
            "tree",
            "find",
            "get_text",
            "wait_for",
            "screenshot",
            "clipboard_get",
            "activate_window",
        ] {
            assert_eq!(needed_tier(t, &none), Tier::View, "{t}");
        }
        for t in ["act", "click", "focus", "scroll", "drag", "mouse", "launch"] {
            assert_eq!(needed_tier(t, &none), Tier::Click, "{t}");
        }
        for t in [
            "key",
            "type_text",
            "set_text",
            "clipboard_set",
            "file_dialog",
            "some_new_tool",
        ] {
            assert_eq!(needed_tier(t, &none), Tier::Full, "{t}");
        }
        assert_eq!(
            needed_tier("launch", &json!({"args": ["-e", "sh"]})),
            Tier::Full
        );
    }
}
