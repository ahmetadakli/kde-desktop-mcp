//! MCP tool definitions (names, descriptions, input schemas).

use serde_json::{Value as Json, json};

/// Input schema from (name, type, default): None is required, Some(null)
/// optional without a default.
fn schema(params: &[(&str, &str, Option<Json>)]) -> Json {
    let mut props = serde_json::Map::new();
    let mut required = Vec::new();
    for (name, kind, default) in params {
        let mut p = json!({ "type": kind });
        match default {
            Some(Json::Null) => {}
            Some(d) => {
                p["default"] = d.clone();
            }
            None => required.push(json!(name)),
        }
        props.insert(name.to_string(), p);
    }
    let mut s = json!({ "type": "object", "properties": props });
    if !required.is_empty() {
        s["required"] = Json::Array(required);
    }
    s
}

/// How act/click/focus/get_text/set_text pick their element.
const TARGET: &str = "\nTarget: `id` from tree/find, or `app` + `query` (+ `role`) to find and act in\none call: the first visible element whose name equals `query`\n(case-insensitive) wins, else the first whose name, text or role contains it.\nThe reply then starts with the element that was picked.";

pub fn tools() -> Json {
    let target = |extra: &[(&'static str, &'static str, Option<Json>)]| {
        let mut p: Vec<(&str, &str, Option<Json>)> = vec![
            ("id", "integer", Some(Json::Null)),
            ("app", "string", Some(Json::Null)),
            ("query", "string", Some(Json::Null)),
            ("role", "string", Some(Json::Null)),
        ];
        p.extend(extra.iter().cloned());
        schema(&p)
    };
    let d = |text: &str| format!("{text}{TARGET}");
    let t = |name: &str, desc: &str, input: Json| {
        json!({
        "name": name, "description": desc, "inputSchema": input })
    };
    let mut batch_schema = schema(&[("steps", "array", None)]);
    batch_schema["properties"]["steps"]["items"] = json!({
        "type": "object",
        "properties": { "tool": { "type": "string" }, "args": { "type": "object" } },
        "required": ["tool"],
    });
    json!([
        t(
            "apps",
            "List applications with their top-level windows.\nMarks which are allowed and which window is active.",
            schema(&[])
        ),
        t(
            "tree",
            "Return the accessibility tree as `[id] role \"name\" text=… {states} actions=…`,\none space of indent per level. Runs of sibling leaves with the same role and\nactions share a line: `list item ×3: [8] \"a\" [9] \"b\" [10] \"c\"`.\nPass `app` (name from `apps`) for its windows, or `root_id` to expand a subtree.\nTo locate something, `find` is cheaper; to act on a named element, pass\n`app` + `query` to act/click/focus directly.",
            schema(&[
                ("app", "string", Some(json!(""))),
                ("root_id", "integer", Some(json!(0))),
                ("max_depth", "integer", Some(json!(12))),
                ("max_nodes", "integer", Some(json!(300))),
                ("include_hidden", "boolean", Some(json!(false)))
            ])
        ),
        t(
            "find",
            "Search an allowed app for elements whose name, text or role contains `query`\n(case-insensitive). Optionally filter by exact role name as shown by tree.\nUsually cheaper than tree.",
            schema(&[
                ("query", "string", None),
                ("app", "string", None),
                ("role", "string", Some(json!(""))),
                ("max_results", "integer", Some(json!(30)))
            ])
        ),
        t(
            "act",
            &d(
                "Perform an accessibility action on an element (default: its first action,\nusually click/press/invoke). Use the action names listed by tree/find.\nMentions it when the active window changed (e.g. a dialog opened)."
            ),
            target(&[("action", "string", Some(json!("")))])
        ),
        t(
            "set_text",
            &d("Replace the contents of an editable text element."),
            target(&[("text", "string", None)])
        ),
        t(
            "get_text",
            &d("Read the full text of an element (documents, fields, labels)."),
            target(&[("limit", "integer", Some(json!(20000)))])
        ),
        t(
            "focus",
            &d(
                "Move keyboard focus to an element. Tries the accessibility focus request\nfirst, then a real click on the element's centre (the element's window must\nbe active). Reports which method worked."
            ),
            target(&[])
        ),
        t(
            "click",
            &d(
                "Real pointer click on an element's centre, for elements without actions.\nThe element's window must be active. To see what the click opened, use\nwait_for."
            ),
            target(&[
                ("button", "string", Some(json!("left"))),
                ("clicks", "integer", Some(json!(1)))
            ])
        ),
        t(
            "wait_for",
            "Wait until an element matching `query` (+ `role`) is visible in `app`, or with\n`gone`=true until none is; checks every 50 ms up to `timeout_ms`. Use instead\nof polling with tree/find after an action that takes time (a dialog, a load).",
            schema(&[
                ("app", "string", None),
                ("query", "string", None),
                ("role", "string", Some(json!(""))),
                ("gone", "boolean", Some(json!(false))),
                ("timeout_ms", "integer", Some(json!(5000)))
            ])
        ),
        t(
            "key",
            "Press a key or chord in the active window, e.g. \"ctrl+s\", \"alt+tab\", \"enter\",\n\"f5\", \"left\". Only sent when the active window belongs to an allowed app.\nTo wait for what a key opens or closes (a dialog), use wait_for.",
            schema(&[
                ("combo", "string", None),
                ("repeat", "integer", Some(json!(1)))
            ])
        ),
        t(
            "type_text",
            "Type text into the focused element of the active window. Works with any\nkeyboard layout and Unicode. On Linux this goes through the clipboard and\noverwrites it.",
            schema(&[("text", "string", None)])
        ),
        t(
            "mouse",
            "Fallback pointer input in screen coordinates: move, then click\n(`clicks`=0 to only move) and/or scroll (positive = down).",
            schema(&[
                ("x", "integer", None),
                ("y", "integer", None),
                ("button", "string", Some(json!("left"))),
                ("clicks", "integer", Some(json!(1))),
                ("scroll", "integer", Some(json!(0)))
            ])
        ),
        t(
            "activate_window",
            "Bring an allowed app's window to the front by title or app-name substring.",
            schema(&[("query", "string", None)])
        ),
        t(
            "screenshot",
            "Last resort only: capture the active window to a PNG and return its path\n(read it with the Read tool). Prefer tree/find.",
            schema(&[])
        ),
        t(
            "batch",
            "Run several tool calls in order in one round trip, stopping at the first\nerror. Prefer it whenever the next steps don't depend on reading a result. `steps` is a list of {\"tool\": name, \"args\": {...}}, e.g.\n[{\"tool\": \"activate_window\", \"args\": {\"query\": \"Kate\"}},\n {\"tool\": \"key\", \"args\": {\"combo\": \"ctrl+end\"}},\n {\"tool\": \"type_text\", \"args\": {\"text\": \"hello\"}}].\nIds from tree/find made earlier can be used; ids printed inside the same\nbatch cannot, but `app` + `query` targets can name elements instead.",
            batch_schema
        ),
    ])
}
