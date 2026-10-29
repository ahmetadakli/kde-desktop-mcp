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

/// Tools that only read, marked `readOnlyHint` so clients may run them in parallel.
const READ_ONLY: [&str; 12] = [
    "apps",
    "windows",
    "doctor",
    "tree",
    "find",
    "get_text",
    "wait_for",
    "wait_idle",
    "screenshot",
    "pause",
    "clipboard_get",
    "cursor_position",
];

/// Input tools that take an optional `window` (title or app-name
/// substring): they activate the matching window first unless it is
/// already in front.
pub const WINDOW_TOOLS: [&str; 10] = [
    "act",
    "set_text",
    "focus",
    "click",
    "drag",
    "scroll",
    "key",
    "type_text",
    "mouse",
    "screenshot",
];

/// Input tools that report an `effect` line and take `observe` and
/// `settle_ms`: what to look at once the input settled.
pub const OBSERVE_TOOLS: [&str; 8] = [
    "act",
    "set_text",
    "click",
    "drag",
    "scroll",
    "key",
    "type_text",
    "mouse",
];

/// Values of `observe`.
pub const OBSERVE_KINDS: [&str; 5] = ["none", "effect", "focus", "tree", "screenshot"];

/// Sent as `instructions` in the initialize result. Clients that load tool
/// schemas lazily show only this and the tool names at session start, so it
/// carries the workflow and the targeting rules that every tool shares. It
/// costs context in every session: keep it short.
pub const INSTRUCTIONS: &str = "\
Controls GUI apps on the user's desktop (KDE Plasma; also Hyprland, Windows) through the \
accessibility tree, with real keyboard and mouse input where needed.

Workflow:
- apps lists running apps and marks the allowed ones. find (app + query) locates elements; \
act, click, focus, get_text, set_text and scroll take the target themselves, so find-and-act \
is one call. tree shows structure (document=true: a browser's page only); find is cheaper.
- Target: `id` from tree/find, or `app` + `query` (+ `role`): the first visible element whose \
name equals `query` (case-insensitive) wins, else the first whose name or text contains it. \
Window and tab titles count only when nothing else matches or `role` names that role. The \
reply starts with the element picked.
- batch runs steps in one round trip when later steps don't depend on reading a result. \
Mark waits \"optional\": true and end with an observation step (find, get_text). A later \
step's `id` (or root_id/from_id/to_id) may be \"$N\": the element step N found or acted on \
(find: its first hit).
- Input tools (key, type_text, click, mouse, scroll, focus, drag, set_text, act, screenshot) \
take `window` (title or app-name substring): pass it instead of a separate activate_window \
step; it activates that window only when it isn't in front. Without it, input fails while \
the user's terminal is in front.
- click, mouse and scroll take `modifiers` (e.g. [\"ctrl\"]); hold keeps a key or button \
down across calls.
- If the user takes over the keyboard and mouse, input fails with \"paused\" (or \
\"interrupted\" mid-batch) without acting: pause ~2 s and retry. \"stopped by the user\": \
stop and ask them; once they agree, a standalone activate_window call resumes input.
- pause for a fixed wait; wait_for for something that appears or goes away (gone=true), a \
state (state=enabled…) or a window (window=…), never as a sleep; wait_idle until an app \
stops changing. tree(interactive=true) for large pages.
- `observe`=effect|focus|tree|screenshot on input tools reports what changed or adds that \
view after the input settled: no separate call.
- screenshot is a last resort; the image comes back inline (don't Read the file). To click \
something seen only in it, pass its pixel position to mouse as image_x/image_y; \
`region`=[x, y, w, h] zooms in.

Allowlist: only apps listed in the user's allowlist file can be read, activated or sent input, \
and input only goes to an allowed active window. An app may be limited to a tier: view \
(read only) or click (no typing or keys). When a refusal says to, ask the user and retry with \
confirm: true only if they agree. Password fields are never read, and only filled in if the \
user allowed it. Errors name the file; changing it is the user's decision. When tools fail as if the setup \
were broken (AT-SPI, virtual input, KWin, screenshots), run doctor.

Clients that load tool schemas on demand: load the common ones in one search: batch, find, \
act, activate_window, key, type_text, wait_for, screenshot, tree, apps.";

/// The `confirm` argument of tools that change something.
const CONFIRM: &str =
    "Only when a refusal asked for the user's confirmation and the user agreed to this exact call.";

/// Shared by the tools that take a target; the matching rules are in
/// INSTRUCTIONS.
const TARGET: &str = "\nTarget: `id` from tree/find, or `app` + `query` (+ `role`); rules in the server instructions.";

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
        "properties": {
            "tool": { "type": "string" },
            "args": { "type": "object" },
            "optional": { "type": "boolean", "default": false },
        },
        "required": ["tool"],
    });
    let mut list = json!([
        t(
            "apps",
            "List running applications: allowed ones with their titled top-level windows\n(the active one marked), blocked ones by name on one line.",
            schema(&[])
        ),
        t(
            "tree",
            "Return the accessibility tree as `[id] role \"name\" text=… {states} actions=…`,\none space of indent per level. Actions the role implies (a link's jump, a\nbutton's press, a tab's switch, a section's click) are omitted;\n`actions=none` marks a button or link without any. Nameless wrappers are left\nout (their children lifted). Runs of sibling leaves with the same role and\nactions share a line: `list item ×3: [8] \"a\" [9] \"b\" [10] \"c\"`.\nPass `app` (name from `apps`) for its windows, or `root_id` to expand a subtree.\nWith `document`=true only the page of `app`'s active window is listed: in a\nbrowser, the current tab's web page without the tabs and toolbars.\nTo locate something, `find` is cheaper; to act on a named element, pass\n`app` + `query` to act/click/focus directly.\n`interactive`=true lists only what can be operated (actions, editable,\nselectable, checkable, expandable), each under its nearest named ancestor.\nA reply over `max_chars` (0 = no cap) or `max_nodes` ends with\n`… N more nodes; continue with tree(…, after=ID)`: repeat that call for the\nnext part. `screenshot`=true also returns the image of the app's window\nwhen it is the active one.",
            schema(&[
                ("app", "string", Some(json!(""))),
                ("root_id", "integer", Some(json!(0))),
                ("max_depth", "integer", Some(json!(12))),
                ("max_nodes", "integer", Some(json!(300))),
                ("max_chars", "integer", Some(json!(20000))),
                ("include_hidden", "boolean", Some(json!(false))),
                ("document", "boolean", Some(json!(false))),
                ("interactive", "boolean", Some(json!(false))),
                ("after", "integer", Some(Json::Null)),
                ("screenshot", "boolean", Some(json!(false))),
            ])
        ),
        t(
            "find",
            "Search an allowed app for elements whose name or text contains `query`\n(case-insensitive). Optionally filter by exact role name as shown by tree; with\n`role`, the role name is searched too. Window and tab titles (frame, window,\npage tab) are listed after all other matches. Lines read as in tree (actions\nthe role implies omitted). Usually cheaper than tree.",
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
                "Perform an accessibility action on an element (default: its first action,\nusually click/press/invoke). Use the action names listed by tree/find (a role's usual action,\ne.g. a button's press or a link's jump, is not listed but is the default).\nThe effect line says when the active window changed (e.g. a dialog opened)."
            ),
            target(&[("action", "string", Some(json!("")))])
        ),
        t(
            "set_text",
            &d(
                "Replace the contents of an editable text element, then read it back:\nthe reply says \"verified\", or fails with what the field holds instead\n(a length limit, reformatting). Refused on a password field unless the\nuser allowed it in the allowlist (then it is not read back)."
            ),
            target(&[("text", "string", None)])
        ),
        t(
            "get_text",
            &d(
                "Read the full text of an element (documents, fields, labels). A web page or\nother container gives the text of everything in it, in reading order.\nPassword fields are never read (tree and find show no text for them either)."
            ),
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
                "Real pointer click on an element's centre, for elements without actions.\nThe element's window must be active. `button`: left, right or middle;\n`clicks`: 2 = double, 3 = triple (select a line). `modifiers` are held\nduring the click, e.g. [\"ctrl\"] to add to a selection, [\"shift\"] for a\nrange. To see what the click opened, use wait_for."
            ),
            target(&[
                ("button", "string", Some(json!("left"))),
                ("clicks", "integer", Some(json!(1))),
                ("modifiers", "array", Some(Json::Null)),
            ])
        ),
        t(
            "scroll",
            &d(
                "Scroll the wheel over an element (a list, a page, a panel): `amount`\nnotches, positive = down. `modifiers` are held while scrolling, e.g.\n[\"ctrl\"] to zoom. The element's window must be active."
            ),
            target(&[
                ("amount", "integer", Some(json!(3))),
                ("modifiers", "array", Some(Json::Null)),
            ])
        ),
        t(
            "drag",
            "Press a mouse button, move in steps, release: drag and drop, sliders,\nselections, splitters. Each end is `from_id`/`to_id` (an element's centre,\nids from tree/find) or `from_x`/`from_y` and `to_x`/`to_y`: screen\ncoordinates, or pixels of the last screenshot with `image`=true. The\nactive window must belong to an allowed app.",
            schema(&[
                ("from_id", "integer", Some(Json::Null)),
                ("from_x", "integer", Some(Json::Null)),
                ("from_y", "integer", Some(Json::Null)),
                ("to_id", "integer", Some(Json::Null)),
                ("to_x", "integer", Some(Json::Null)),
                ("to_y", "integer", Some(Json::Null)),
                ("image", "boolean", Some(json!(false))),
                ("button", "string", Some(json!("left"))),
            ])
        ),
        t(
            "wait_for",
            "Wait until an element matching `query` (+ `role`) is visible in `app`, or with\n`gone`=true until none is; checks every 50 ms up to `timeout_ms` (at most\n600000 = 10 minutes; a larger value is cut and the reply says so). Use instead\nof polling with tree/find after an action that takes time (a dialog, a load, a\nlong build or download). Matches like act's `query`, except that window and\ntab titles (frame, window, page tab), which name a page before it has loaded,\nmatch only when `role` names that role. `state` (enabled, focused, checked,\nselected): the element must also have it (with `gone`: until it no longer\ndoes). `window` instead of `query`: wait for a window of an allowed app\n(`app`, if given) whose title or app name contains it to open, or with\n`gone` to close. To just wait a fixed time, use pause, not a query that\nnever matches.",
            schema(&[
                ("app", "string", Some(Json::Null)),
                ("query", "string", Some(Json::Null)),
                ("role", "string", Some(json!(""))),
                ("state", "string", Some(Json::Null)),
                ("window", "string", Some(Json::Null)),
                ("gone", "boolean", Some(json!(false))),
                ("timeout_ms", "integer", Some(json!(5000)))
            ])
        ),
        t(
            "wait_idle",
            "Wait until `app` stops changing: no change in its accessibility tree\n(elements added or removed, names, text) for `quiet_ms`, e.g. after a page\nload or an animation; at most `timeout_ms` (fails when it never settles).\nCheaper than polling with tree or screenshots.",
            schema(&[
                ("app", "string", None),
                ("quiet_ms", "integer", Some(json!(300))),
                ("timeout_ms", "integer", Some(json!(10000)))
            ])
        ),
        t(
            "pause",
            "Sleep for `ms` milliseconds (at most 600000 = 10 minutes), e.g. to let an\nanimation or video play. When waiting for something visible to appear or go\naway, use wait_for instead: it returns as soon as that happens.",
            schema(&[("ms", "integer", None)])
        ),
        t(
            "key",
            "Press a key or chord in the active window, e.g. \"ctrl+s\", \"alt+tab\", \"enter\",\n\"f5\", \"left\". Only sent when the active window belongs to an allowed app.\nThe reply names the element that has focus afterwards. To wait for what a\nkey opens or closes (a dialog), use wait_for.",
            schema(&[
                ("combo", "string", None),
                ("repeat", "integer", Some(json!(1)))
            ])
        ),
        t(
            "type_text",
            "Type text into the focused element of the active window. Works with any\nkeyboard layout and Unicode. On Linux this may paste through the clipboard;\nthe text it held is put back afterwards (images and files are not; the reply\nsays so). The reply names the focused element and warns when it is not an\neditable field. Refused while a password field has focus, unless the user\nallowed it in the allowlist.",
            schema(&[("text", "string", None)])
        ),
        t(
            "clipboard_get",
            "Read the clipboard as text, at most `limit` characters (the reply says\nwhen it was cut). Only works while an allowed app's window is active, since\nthe clipboard may hold anything the user copied. Says so instead when it\nholds no text (an image, files) or a password marked by a password manager.\nLinux only.",
            schema(&[("limit", "integer", Some(json!(20000)))])
        ),
        t(
            "clipboard_set",
            "Put `text` on the clipboard, e.g. to paste it later with key ctrl+v or to\nhand it to the user. Only while an allowed app's window is active. Linux\nonly.",
            schema(&[("text", "string", None)])
        ),
        t(
            "mouse",
            "Fallback pointer input: move, then click (`clicks`=0 to only move, 2 =\ndouble, 3 = triple; `button` left, right or middle) and/or scroll (positive\n= down), with `modifiers` (ctrl, shift, alt, meta) held meanwhile. Give\neither screen coordinates `x`/`y`, or `image_x`/`image_y`: a pixel\nposition in the last screenshot, converted to the screen for you (window\nposition and display scaling included). With a button held (hold), a move\ndrags.",
            schema(&[
                ("x", "integer", Some(Json::Null)),
                ("y", "integer", Some(Json::Null)),
                ("image_x", "integer", Some(Json::Null)),
                ("image_y", "integer", Some(Json::Null)),
                ("button", "string", Some(json!("left"))),
                ("clicks", "integer", Some(json!(1))),
                ("scroll", "integer", Some(json!(0))),
                ("modifiers", "array", Some(Json::Null)),
            ])
        ),
        t(
            "hold",
            "Press a key, chord or mouse button and leave it down across calls\n(`down`=true), or release it (`down`=false): `key` e.g. \"shift\" or\n\"ctrl+alt\", or `button` left/right/middle (at the cursor; move it with\nmouse clicks=0 first, and later moves drag). Held keys apply to all input\nthat follows. Every input reply ends with what is still held (\"held:\nshift\"). down=false alone releases everything. A failed or cancelled call\nand the server's exit release everything too.",
            schema(&[
                ("key", "string", Some(Json::Null)),
                ("button", "string", Some(Json::Null)),
                ("down", "boolean", Some(json!(true))),
            ])
        ),
        t(
            "cursor_position",
            "Where the mouse cursor is, in the screen coordinates mouse takes (KDE:\nfrom KWin; Hyprland: cursorpos; Windows: GetCursorPos).",
            schema(&[])
        ),
        t(
            "activate_window",
            "Bring an allowed app's window to the front by title or app-name substring\n(`query`), or with `terminal` the window whose terminal runs a command or\nis in a directory containing it (as listed by windows; KDE). A terminal\nwith several tabs comes to the front on whichever tab was shown.\nAfter the user pressed the stop shortcut, this call (not as a batch step)\nresumes input: only do it once the user agreed.",
            schema(&[
                ("query", "string", Some(Json::Null)),
                ("terminal", "string", Some(Json::Null)),
            ])
        ),
        t(
            "windows",
            "List windows of allowed apps with their position and size in screen\ncoordinates, active/maximized/minimized state and output, after the\nscreens (name, geometry, scale). Terminal windows also list each tab's\npseudo-terminal with its foreground command and directory. `query` filters\nby title or app name. KDE Plasma and Hyprland.",
            schema(&[("query", "string", Some(json!("")))])
        ),
        t(
            "move_window",
            "Move and/or resize the first window of an allowed app whose title or\napp name contains `query` (`x`/`y`/`width`/`height` in screen coordinates;\nomitted ones stay), or set `state` to maximize, minimize or restore.\nReplies with the new geometry. KDE Plasma and Hyprland (there a tiled\nwindow is floated to move or resize it; minimize parks it on the special\nworkspace special:minimized).",
            schema(&[
                ("query", "string", None),
                ("x", "integer", Some(Json::Null)),
                ("y", "integer", Some(Json::Null)),
                ("width", "integer", Some(Json::Null)),
                ("height", "integer", Some(Json::Null)),
                ("state", "string", Some(Json::Null)),
            ])
        ),
        t(
            "launch",
            "Start an allowed app (`app` as named by apps, e.g. \"kate\") with optional\n`args` (a list, no shell), resolved through its .desktop file or PATH, and\nwait up to `wait_ms` (at most 60000) until its window appears. Replies with\nthe window as windows prints it; finds windows a running instance opens\n(single-instance apps such as firefox or kate) too. 0 = do not wait.\nWindow tracking works on KDE Plasma and Hyprland; Linux only.",
            schema(&[
                ("app", "string", None),
                ("args", "array", Some(Json::Null)),
                ("wait_ms", "integer", Some(json!(15000))),
            ])
        ),
        t(
            "file_dialog",
            "Fill in the file Open/Save dialog in front: write `path` (absolute or\n~/…) into its file name field, press its default button and wait up to\n`timeout_ms` for it to close. Works on an allowed app's own KDE dialog and\non the desktop portal's dialog (a browser's upload or download) when it\nbelongs to an allowed app's window; nothing else in the portal is reachable.\nFor `action`=\"open\" the path must exist; for \"save\" its folder must.\nOpen the dialog first (e.g. the page's upload button), then call this.\nKDE Plasma only.",
            schema(&[
                ("path", "string", None),
                ("action", "string", Some(json!("open"))),
                ("timeout_ms", "integer", Some(json!(5000))),
            ])
        ),
        t(
            "doctor",
            "Check the setup in one report: version, allowlist (each app's tier, the\nconfirm list, password fields), which running apps are allowed or blocked, the session, AT-SPI, the virtual keyboard and\npointer, KWin scripting, screens and the screenshot path. Run it first\nwhen tools fail in ways that look like a broken installation.",
            schema(&[])
        ),
        t(
            "screenshot",
            "Last resort only: capture the active window. The image comes back in the\nreply (no need to Read the file), shrunk so its longer side is at most\n`max_size` px; `inline`=false returns only the file path. Also says which\nscreen area it shows. Prefer tree/find; they also list web page popups and\nmenus. To click something seen only in the image, pass its pixel position\nto mouse as image_x/image_y. To read small text, zoom: `region`=[x, y, w, h]\nin pixels of the last screenshot captures again and returns just that part\nat up to full resolution; image_x/image_y then refer to the zoomed image.",
            schema(&[
                ("max_size", "integer", Some(json!(1024))),
                ("inline", "boolean", Some(json!(true))),
                ("region", "array", Some(Json::Null)),
            ])
        ),
        t(
            "batch",
            "Run tool calls in order in one round trip; prefer it whenever later steps\ndon't depend on reading a result. `steps` is a list of\n{\"tool\": name, \"args\": {...}, \"optional\": bool}, e.g.\n[{\"tool\": \"key\", \"args\": {\"combo\": \"ctrl+o\", \"window\": \"Kate\"}},\n {\"tool\": \"wait_for\", \"args\": {\"app\": \"kate\", \"query\": \"Open File\"}, \"optional\": true},\n {\"tool\": \"find\", \"args\": {\"app\": \"kate\", \"query\": \"File name\"}}].\nStops at the first failing step unless it is `optional`: then it is reported\nas failed (optional) and the rest still runs. Mark waits optional and end\nwith an observation step (find, get_text, screenshot). Give the first\ninput step `window` when input goes to an app. Ids from earlier tree/find work.\nAn `id`/`*_id` arg of \"$N\" is the element step N produced: find's first\nhit, wait_for's match, the target of act/click/focus/get_text/set_text/\nscroll, else the focus key/type_text report; e.g. {\"tool\": \"act\",\n\"args\": {\"id\": \"$3\"}}. Input steps of one batch never interleave with\nanother call's.",
            batch_schema
        ),
    ]);
    for tool in list.as_array_mut().unwrap() {
        if tool["name"] == "launch" {
            tool["inputSchema"]["properties"]["args"]["items"] = json!({ "type": "string" });
        }
        if let Some(m) = tool["inputSchema"]["properties"].get_mut("modifiers") {
            m["items"] = json!({ "type": "string", "enum": ["ctrl", "shift", "alt", "meta"] });
        }
        if WINDOW_TOOLS.contains(&tool["name"].as_str().unwrap()) {
            tool["inputSchema"]["properties"]["window"] = json!({
                "type": "string",
                "description": "Title or app-name substring: activate that window first unless it is already in front."
            });
        }
        if tool["name"] == "screenshot" {
            tool["inputSchema"]["properties"]["region"]["items"] = json!({ "type": "number" });
            tool["inputSchema"]["properties"]["region"]["description"] = json!(
                "[x, y, width, height] in pixels of the last screenshot: zoom into that part."
            );
        }
        if OBSERVE_TOOLS.contains(&tool["name"].as_str().unwrap()) {
            let props = &mut tool["inputSchema"]["properties"];
            props["observe"] = json!({
                "type": "string",
                "enum": OBSERVE_KINDS,
                "default": "none",
                "description": "After the input settled, also report: effect (one line: changed / no visible change / window changed), focus (plus the focused element), tree (plus a compact tree of the active window, or of `app`'s), or screenshot (plus an inline image). none (default) adds nothing and costs nothing; act reports the effect line by default."
            });
            props["settle_ms"] = json!({
                "type": "integer",
                "default": 150,
                "description": "Longest wait for the app to react before the effect line and observation; ends early once it changed."
            });
        }
        if READ_ONLY.contains(&tool["name"].as_str().unwrap()) {
            tool["annotations"] = json!({ "readOnlyHint": true });
        } else if tool["name"] != "batch" {
            tool["inputSchema"]["properties"]["confirm"] = json!({
                "type": "boolean",
                "description": CONFIRM
            });
        }
    }
    list
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_reading_tools_are_marked_read_only() {
        let list = tools();
        for tool in list.as_array().unwrap() {
            let name = tool["name"].as_str().unwrap();
            let ro = tool["annotations"]["readOnlyHint"] == json!(true);
            assert_eq!(ro, READ_ONLY.contains(&name), "{name}");
            let c = tool["inputSchema"]["properties"]["confirm"]["type"] == json!("boolean");
            assert_eq!(c, !ro && name != "batch", "{name} confirm");
            // wait_for's `window` is what it waits for, not one to activate.
            let w = tool["inputSchema"]["properties"]["window"]["description"].is_string();
            assert_eq!(w, WINDOW_TOOLS.contains(&name), "{name} window");
            let o = tool["inputSchema"]["properties"]["observe"]["type"] == json!("string");
            assert_eq!(o, OBSERVE_TOOLS.contains(&name), "{name} observe");
        }
        let names: Vec<_> = list
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].clone())
            .collect();
        for name in READ_ONLY {
            assert!(names.contains(&json!(name)), "{name} missing");
        }
    }
}
