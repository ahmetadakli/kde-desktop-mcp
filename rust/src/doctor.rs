//! doctor's summary for programs: the checks as MCP `structuredContent`
//! (described by the tool's `outputSchema`), and one compact JSON line at
//! the end of the text report with what failed, what blocks the server and
//! what to do first. The line names checks without their details, which
//! the report above it already gives.

use serde_json::{Value as Json, json};

/// One line of the report.
pub struct Row {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

/// Checks without which the server cannot work at all, most basic first,
/// and what to do when one fails. The first failed one gives `next_step`:
/// with no session, the checks after it fail too. A failed check not
/// listed here (screenshot, screens, the self-client guard…) only takes
/// away part of what the server does.
const BLOCKING: [(&str, &str); 7] = [
    (
        "session",
        "start the server from inside the Wayland session, so WAYLAND_DISPLAY reaches it",
    ),
    (
        "hyprctl",
        "start the server from inside the Hyprland session, so HYPRLAND_INSTANCE_SIGNATURE reaches it",
    ),
    (
        "KWin scripting",
        "check that KWin runs in this session and answers on D-Bus (org.kde.KWin)",
    ),
    (
        "accessibility (AT-SPI)",
        "start the AT-SPI bus (at-spi2-core) in the session, then restart the apps",
    ),
    (
        "virtual keyboard (uinput)",
        "give the user write access to /dev/uinput (see the README's requirements)",
    ),
    (
        "virtual pointer (uinput)",
        "give the user write access to /dev/uinput (see the README's requirements)",
    ),
    (
        "allowlist",
        "ask the user to add the apps to drive to the allowlist file, one per line",
    ),
];

/// `next_step` when nothing blocks and every check passed.
pub const READY: &str = "ready: apps lists what can be driven";

/// The summary of `rows`: {version, platform, session, checks, blockers,
/// next_step}.
pub fn summary(version: &str, platform: &str, session: &str, rows: &[Row]) -> Json {
    let failed = |name: &str| rows.iter().any(|r| !r.ok && r.name == name);
    let blocking: Vec<&(&str, &str)> = BLOCKING.iter().filter(|(n, _)| failed(n)).collect();
    // Failed checks that block nothing, each named once.
    let mut others: Vec<&str> = Vec::new();
    for r in rows.iter().filter(|r| !r.ok) {
        if !BLOCKING.iter().any(|(n, _)| *n == r.name) && !others.contains(&r.name.as_str()) {
            others.push(&r.name);
        }
    }
    let next_step = match (blocking.first(), others.is_empty()) {
        (Some((_, step)), _) => (*step).to_string(),
        (None, true) => READY.to_string(),
        (None, false) => format!("ready; to use all of it, fix: {}", others.join(", ")),
    };
    json!({
        "version": version,
        "platform": platform,
        "session": session,
        "checks": rows
            .iter()
            .map(|r| json!({ "name": r.name, "ok": r.ok, "detail": r.detail }))
            .collect::<Vec<_>>(),
        "blockers": blocking.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
        "next_step": next_step,
    })
}

/// The last line of the text report: `summary` without the checks' details
/// (only the names of the failed ones).
pub fn line(summary: &Json) -> String {
    let failed: Vec<&Json> = summary["checks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| c["ok"] == json!(false))
        .map(|c| &c["name"])
        .collect();
    json!({
        "version": summary["version"],
        "platform": summary["platform"],
        "session": summary["session"],
        "failed": failed,
        "blockers": summary["blockers"],
        "next_step": summary["next_step"],
    })
    .to_string()
}

/// doctor's `outputSchema`.
pub fn output_schema() -> Json {
    let s = json!({ "type": "string" });
    let strings = json!({ "type": "array", "items": s });
    json!({
        "type": "object",
        "properties": {
            "version": s,
            "platform": s,
            "session": s,
            "checks": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": { "name": s, "ok": { "type": "boolean" }, "detail": s },
                    "required": ["name", "ok", "detail"],
                },
            },
            "blockers": strings,
            "next_step": s,
        },
        "required": ["version", "platform", "session", "checks", "blockers", "next_step"],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, ok: bool) -> Row {
        Row {
            name: name.into(),
            ok,
            detail: format!("{name} detail"),
        }
    }

    /// Whether `v` fits `schema` (the subset `output_schema` uses).
    fn fits(v: &Json, schema: &Json) -> bool {
        match schema["type"].as_str().unwrap() {
            "string" => v.is_string(),
            "boolean" => v.is_boolean(),
            "array" => v
                .as_array()
                .is_some_and(|a| a.iter().all(|i| fits(i, &schema["items"]))),
            "object" => {
                let Some(o) = v.as_object() else {
                    return false;
                };
                let props = schema["properties"].as_object().unwrap();
                schema["required"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|k| o.contains_key(k.as_str().unwrap()))
                    && o.iter()
                        .all(|(k, v)| props.get(k).is_some_and(|p| fits(v, p)))
            }
            t => panic!("type {t}"),
        }
    }

    #[test]
    fn a_working_setup_is_ready() {
        let rows = [
            row("update", true),
            row("allowlist", true),
            row("session", true),
        ];
        let s = summary("1.2.3", "linux", "kde-plasma", &rows);
        assert!(fits(&s, &output_schema()), "{s}");
        assert_eq!(s["checks"].as_array().unwrap().len(), 3);
        assert_eq!(
            s["checks"][1],
            json!({ "name": "allowlist", "ok": true, "detail": "allowlist detail" })
        );
        assert_eq!(s["blockers"], json!([]));
        assert_eq!(s["next_step"], READY);
        assert_eq!(
            line(&s),
            r#"{"blockers":[],"failed":[],"next_step":"ready: apps lists what can be driven","platform":"linux","session":"kde-plasma","version":"1.2.3"}"#
        );
    }

    #[test]
    fn the_most_basic_blocker_gives_the_next_step() {
        let rows = [
            row("allowlist", false),
            row("session", false),
            row("accessibility (AT-SPI)", false),
            row("screenshot", false),
        ];
        let s = summary("1.2.3", "linux", "kde-plasma", &rows);
        assert!(fits(&s, &output_schema()), "{s}");
        assert_eq!(
            s["blockers"],
            json!(["session", "accessibility (AT-SPI)", "allowlist"])
        );
        assert!(
            s["next_step"].as_str().unwrap().contains("WAYLAND_DISPLAY"),
            "{s}"
        );
        let l: Json = serde_json::from_str(&line(&s)).unwrap();
        assert_eq!(
            l["failed"],
            json!([
                "allowlist",
                "session",
                "accessibility (AT-SPI)",
                "screenshot"
            ])
        );
        assert!(!line(&s).contains("detail"));
    }

    #[test]
    fn failures_that_block_nothing_leave_it_ready() {
        let rows = [
            row("allowlist", true),
            row("allowlist line", false),
            row("allowlist line", false),
            row("screenshot", false),
        ];
        let s = summary("1.2.3", "windows", "windows", &rows);
        assert_eq!(s["blockers"], json!([]));
        assert_eq!(
            s["next_step"],
            "ready; to use all of it, fix: allowlist line, screenshot"
        );
    }

    #[test]
    fn the_output_schema_is_small_and_closed_over_the_summary() {
        let schema = output_schema();
        assert!(schema.to_string().len() < 600, "{schema}");
        assert!(!fits(&json!({ "version": "1" }), &schema));
    }
}
