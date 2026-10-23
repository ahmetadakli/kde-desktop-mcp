//! MCP tool definitions (names, descriptions, input schemas).

use serde_json::{Value as Json, json};

fn schema(params: &[(&str, &str, Option<Json>)]) -> Json {
    let mut props = serde_json::Map::new();
    let mut required = Vec::new();
    for (name, kind, default) in params {
        let mut p = json!({ "type": kind });
        match default {
            Some(d) => { p["default"] = d.clone(); }
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

pub fn tools() -> Json {
    let id = ("id", "integer", None);
    let t = |name: &str, desc: &str, input: Json| json!({
        "name": name, "description": desc, "inputSchema": input });
    let mut batch_schema = schema(&[("steps", "array", None)]);
    batch_schema["properties"]["steps"]["items"] = json!({
        "type": "object",
        "properties": { "tool": { "type": "string" }, "args": { "type": "object" } },
        "required": ["tool"],
    });
    json!([
        t("apps", "List applications with their top-level windows.\nMarks which are allowed and which window is active.", schema(&[])),
        t("tree", "Return the accessibility tree as `[id] role \"name\" {states} actions=…`.\nPass `app` (name from `apps`) for its windows, or `root_id` to expand a subtree.\nIds are used by act/set_text/get_text/focus/click.",
          schema(&[("app", "string", Some(json!(""))), ("root_id", "integer", Some(json!(0))),
                   ("max_depth", "integer", Some(json!(12))), ("max_nodes", "integer", Some(json!(300))),
                   ("include_hidden", "boolean", Some(json!(false)))])),
        t("find", "Search an allowed app for elements whose name, text or role contains `query`\n(case-insensitive). Optionally filter by exact role name as shown by tree.",
          schema(&[("query", "string", None), ("app", "string", None), ("role", "string", Some(json!(""))),
                   ("max_results", "integer", Some(json!(30)))])),
        t("act", "Perform an accessibility action on an element (default: its first action,\nusually click/press/invoke). Use the action names listed by tree/find.",
          schema(&[id.clone(), ("action", "string", Some(json!("")))])),
        t("set_text", "Replace the contents of an editable text element.",
          schema(&[id.clone(), ("text", "string", None)])),
        t("get_text", "Read the full text of an element (documents, fields, labels).",
          schema(&[id.clone(), ("limit", "integer", Some(json!(20000)))])),
        t("focus", "Move keyboard focus to an element. Tries the accessibility focus request\nfirst, then a real click on the element's centre (the element's window must\nbe active). Reports which method worked.",
          schema(std::slice::from_ref(&id))),
        t("click", "Real pointer click on an element's centre, for elements without actions.\nThe element's window must be active.",
          schema(&[id.clone(), ("button", "string", Some(json!("left"))), ("clicks", "integer", Some(json!(1)))])),
        t("key", "Press a key or chord in the active window, e.g. \"ctrl+s\", \"alt+tab\", \"enter\",\n\"f5\", \"left\". Only sent when the active window belongs to an allowed app.",
          schema(&[("combo", "string", None), ("repeat", "integer", Some(json!(1)))])),
        t("type_text", "Type text into the focused element of the active window. Works with any\nkeyboard layout and Unicode. On Linux this goes through the clipboard and\noverwrites it.",
          schema(&[("text", "string", None)])),
        t("mouse", "Fallback pointer input in screen coordinates: move, then click\n(`clicks`=0 to only move) and/or scroll (positive = down).",
          schema(&[("x", "integer", None), ("y", "integer", None), ("button", "string", Some(json!("left"))),
                   ("clicks", "integer", Some(json!(1))), ("scroll", "integer", Some(json!(0)))])),
        t("activate_window", "Bring an allowed app's window to the front by title or app-name substring.",
          schema(&[("query", "string", None)])),
        t("screenshot", "Last resort only: capture the active window to a PNG and return its path\n(read it with the Read tool). Prefer tree/find.", schema(&[])),
        t("batch", "Run several tool calls in order in one round trip, stopping at the first\nerror. `steps` is a list of {\"tool\": name, \"args\": {...}}, e.g.\n[{\"tool\": \"activate_window\", \"args\": {\"query\": \"Kate\"}},\n {\"tool\": \"key\", \"args\": {\"combo\": \"ctrl+end\"}},\n {\"tool\": \"type_text\", \"args\": {\"text\": \"hello\"}}].\nIds from tree/find made earlier can be used; ids printed inside the same\nbatch cannot.",
          batch_schema),
    ])
}

