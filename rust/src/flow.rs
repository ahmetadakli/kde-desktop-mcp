//! A batch's steps and their control flow: parsed and checked as a whole
//! before any step runs (`parse`), then evaluated as the steps run.
//!
//! A top-level step is one of
//! - `{"tool", "args", "optional", "if", "on_fail"}`: a tool call;
//! - `{"repeat": {"steps", "until", "max"}, "optional", "if", "on_fail"}`;
//! - `{"stop_if": COND}`: ends the batch, successfully, when COND holds.
//!
//! COND is `{"step": N, TEST}` on an earlier top-level step; TEST is one of
//! `"ok": bool`, `"found": bool`, `"contains": "text"`. Steps inside a
//! repeat or an on_fail are plain tool calls (tool, args, optional).
//! Steps are numbered from 1 at the top level; `"$N"` and `"step": N`
//! name top-level steps only, a repeat by its last step's latest run.

use serde_json::{Map, Value as Json};

use crate::backend::R;

/// Most runs of a repeat.
pub const MAX_RUNS: u64 = 20;
/// Runs of a repeat without `max`.
const DEFAULT_RUNS: u64 = 10;

/// A plain tool call: a step inside a repeat or an on_fail, or the call of
/// a top-level tool step.
#[derive(Debug, Clone)]
pub struct Call {
    pub tool: String,
    pub args: Json,
    pub optional: bool,
}

/// What a condition asks of a step's result.
#[derive(Debug, Clone, PartialEq)]
pub enum Test {
    /// The step succeeded (true) or failed (false).
    Ok(bool),
    /// The step succeeded and produced an element (find: had a match).
    Found(bool),
    /// The step's reply (or error) contains this text, case-insensitive.
    Contains(String),
}

impl Test {
    /// Whether it holds for a step that ran.
    pub fn holds(&self, ok: bool, element: Option<u64>, text: &str) -> bool {
        match self {
            Test::Ok(want) => ok == *want,
            Test::Found(want) => (ok && element.is_some()) == *want,
            Test::Contains(s) => text.to_lowercase().contains(&s.to_lowercase()),
        }
    }

    fn describe(&self) -> String {
        match self {
            Test::Ok(true) => "ok".into(),
            Test::Ok(false) => "failed".into(),
            Test::Found(true) => "found".into(),
            Test::Found(false) => "not found".into(),
            Test::Contains(s) => format!("contains {}", crate::render::py_repr(s)),
        }
    }
}

/// A test on an earlier top-level step (1-based).
#[derive(Debug, Clone, PartialEq)]
pub struct Cond {
    pub step: usize,
    pub test: Test,
}

impl Cond {
    /// "step 2 found", for skip and stop lines.
    pub fn describe(&self) -> String {
        format!("step {} {}", self.step, self.test.describe())
    }

    /// Whether it holds, given the records of the steps before.
    pub fn holds(&self, done: &[Record]) -> bool {
        match done.get(self.step - 1).map(|r| &r.outcome) {
            Some(Outcome::Ran { ok, element, text }) => self.test.holds(*ok, *element, text),
            // A skipped step (or a stop_if) meets no condition.
            _ => false,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Body {
    Tool(Call),
    Repeat {
        steps: Vec<Call>,
        until: Test,
        max: u64,
    },
    Stop(Cond),
}

/// A repeat's parts, borrowed from its `Body`.
pub struct Repeat<'a> {
    pub steps: &'a [Call],
    pub until: &'a Test,
    pub max: u64,
}

/// A top-level step.
#[derive(Debug, Clone)]
pub struct Step {
    pub body: Body,
    pub optional: bool,
    pub cond: Option<Cond>,
    pub on_fail: Vec<Call>,
}

impl Step {
    /// Its tool name, "repeat" or "stop_if".
    pub fn name(&self) -> &str {
        match &self.body {
            Body::Tool(c) => &c.tool,
            Body::Repeat { .. } => "repeat",
            Body::Stop(_) => "stop_if",
        }
    }

    /// Every tool call it may make.
    pub fn calls(&self) -> impl Iterator<Item = &Call> {
        let own: &[Call] = match &self.body {
            Body::Tool(c) => std::slice::from_ref(c),
            Body::Repeat { steps, .. } => steps,
            Body::Stop(_) => &[],
        };
        own.iter().chain(&self.on_fail)
    }
}

/// What a top-level step left, for conditions and `"$N"` references.
#[derive(Debug, Clone)]
pub struct Record {
    pub tool: String,
    pub outcome: Outcome,
}

#[derive(Debug, Clone)]
pub enum Outcome {
    /// Not run: its `if` did not hold (or a stop_if, which has no result).
    Skipped,
    Ran {
        ok: bool,
        /// The element it produced (`STEP_ELEMENT`); for a repeat, its last
        /// step's in the latest run.
        element: Option<u64>,
        /// Its reply or error; for a repeat, its last step's.
        text: String,
    },
}

/// The step a `"$N"` value of argument `key` names, if it is one; an
/// error when it is malformed or not before `step`.
fn ref_index(key: &str, value: &Json, step: usize) -> R<Option<usize>> {
    if key != "id" && !key.ends_with("_id") {
        return Ok(None);
    }
    let Some(text) = value.as_str().and_then(|v| v.strip_prefix('$')) else {
        return Ok(None);
    };
    let n: usize = text
        .parse()
        .map_err(|_| format!("`{key}` {value}: use \"$N\" for the element step N produced"))?;
    if n == 0 || n >= step {
        return Err(format!(
            "`{key}` \"${n}\": refer to an earlier step (1 to {})",
            step - 1
        ));
    }
    Ok(Some(n))
}

/// Replace `"$N"` in the top-level id arguments (`id`, `*_id`) of a call
/// in top-level step `step` (1-based) with the element step N produced.
pub fn resolve_refs(args: &mut Json, step: usize, done: &[Record]) -> R<()> {
    let Some(map) = args.as_object_mut() else {
        return Ok(());
    };
    for (key, value) in map.iter_mut() {
        let Some(n) = ref_index(key, value, step)? else {
            continue;
        };
        let Some(Record { tool, outcome }) = done.get(n - 1) else {
            return Err(format!("`{key}` \"${n}\": step {n} has not run"));
        };
        *value = match outcome {
            Outcome::Ran {
                ok: true,
                element: Some(rid),
                ..
            } => Json::from(*rid),
            Outcome::Ran { ok: false, .. } => {
                return Err(format!("`{key}` \"${n}\": step {n} ({tool}) failed"));
            }
            Outcome::Skipped => {
                return Err(format!("`{key}` \"${n}\": step {n} ({tool}) was skipped"));
            }
            Outcome::Ran { .. } => {
                return Err(format!(
                    "`{key}` \"${n}\": step {n} ({tool}) produced no element \
                     (find, wait_for, and tools given an element do)"
                ));
            }
        };
    }
    Ok(())
}

const STEP_KEYS: &str = "tool, args, optional, if, on_fail (or repeat, or stop_if alone)";

/// Parse and check a batch's `steps` before anything runs: tools exist,
/// conditions and `"$N"` name earlier steps and can hold, repeats are
/// bounded. Errors start with the top-level step, e.g. "[3] …".
pub fn parse(steps: &[Json], known_tool: impl Fn(&str) -> bool) -> R<Vec<Step>> {
    let mut parsed: Vec<Step> = Vec::new();
    for (i, raw) in steps.iter().enumerate() {
        let n = i + 1;
        let step = parse_step(raw, n, &parsed, &known_tool).map_err(|e| format!("[{n}] {e}"))?;
        parsed.push(step);
    }
    Ok(parsed)
}

fn as_object<'a>(v: &'a Json, what: &str) -> R<&'a Map<String, Json>> {
    v.as_object()
        .ok_or_else(|| format!("{what} must be an object, not {v}"))
}

fn bool_arg(obj: &Map<String, Json>, key: &str) -> R<bool> {
    match obj.get(key) {
        None | Some(Json::Null) => Ok(false),
        Some(Json::Bool(b)) => Ok(*b),
        Some(v) => Err(format!("`{key}` must be true or false, not {v}")),
    }
}

fn parse_step(
    raw: &Json,
    n: usize,
    earlier: &[Step],
    known_tool: &impl Fn(&str) -> bool,
) -> R<Step> {
    let obj = as_object(raw, "a step")?;
    let kinds: Vec<&str> = ["tool", "repeat", "stop_if"]
        .into_iter()
        .filter(|k| obj.contains_key(*k))
        .collect();
    if kinds.len() != 1 {
        return Err(format!(
            "a step has exactly one of tool, repeat or stop_if (found {})",
            if kinds.is_empty() {
                "none".to_string()
            } else {
                kinds.join(" and ")
            }
        ));
    }
    let allowed: &[&str] = match kinds[0] {
        "tool" => &["tool", "args", "optional", "if", "on_fail"],
        "repeat" => &["repeat", "optional", "if", "on_fail"],
        _ => &["stop_if"],
    };
    if let Some(k) = obj.keys().find(|k| !allowed.contains(&k.as_str())) {
        return Err(match kinds[0] {
            "stop_if" => format!("a stop_if step takes nothing else (found `{k}`)"),
            "repeat" if k == "args" || k == "until" || k == "max" || k == "steps" => {
                format!(
                    "`{k}` goes inside \"repeat\": {{\"steps\": […], \"until\": …, \"max\": …}}"
                )
            }
            _ => format!("unknown key `{k}`; a step takes {STEP_KEYS}"),
        });
    }
    if let Some(c) = obj.get("stop_if") {
        let cond = parse_cond(c, n, earlier).map_err(|e| format!("stop_if: {e}"))?;
        return Ok(Step {
            body: Body::Stop(cond),
            optional: false,
            cond: None,
            on_fail: Vec::new(),
        });
    }
    let optional = bool_arg(obj, "optional")?;
    let cond = match obj.get("if") {
        None | Some(Json::Null) => None,
        Some(c) => Some(parse_cond(c, n, earlier).map_err(|e| format!("if: {e}"))?),
    };
    let on_fail = match obj.get("on_fail") {
        None | Some(Json::Null) => Vec::new(),
        Some(v) => parse_calls(v, n, "on_fail", known_tool)?,
    };
    let body = match obj.get("repeat") {
        Some(r) => parse_repeat(r, n, known_tool)?,
        None => Body::Tool(parse_call(obj, n, false, known_tool)?),
    };
    let body = match body {
        Body::Tool(c) => Body::Tool(Call { optional, ..c }),
        b => b,
    };
    Ok(Step {
        body,
        optional,
        cond,
        on_fail,
    })
}

/// A plain tool call (inside a repeat or on_fail, or a tool step's own
/// keys, already checked). `n`: the top-level step, for `"$N"`; `nested`:
/// inside a repeat or on_fail.
fn parse_call(
    obj: &Map<String, Json>,
    n: usize,
    nested: bool,
    known_tool: &impl Fn(&str) -> bool,
) -> R<Call> {
    let tool = match obj.get("tool") {
        Some(Json::String(t)) => t.clone(),
        Some(v) => return Err(format!("`tool` must be a tool name, not {v}")),
        None => return Err("missing `tool`".into()),
    };
    if tool == "batch" {
        return Err("batch cannot contain batch".into());
    }
    if !known_tool(&tool) {
        return Err(format!("unknown tool '{tool}'"));
    }
    let args = match obj.get("args") {
        None | Some(Json::Null) => Json::Object(Map::new()),
        Some(a @ Json::Object(_)) => a.clone(),
        Some(v) => return Err(format!("{tool}: `args` must be an object, not {v}")),
    };
    for (key, value) in args.as_object().into_iter().flatten() {
        ref_index(key, value, n).map_err(|e| {
            if nested {
                format!("{tool}: {e} (inside step {n}: \"$N\" names a top-level step)")
            } else {
                format!("{tool}: {e}")
            }
        })?;
    }
    Ok(Call {
        tool,
        args,
        optional: bool_arg(obj, "optional")?,
    })
}

/// The steps of a repeat or on_fail: plain tool calls.
fn parse_calls(v: &Json, n: usize, what: &str, known_tool: &impl Fn(&str) -> bool) -> R<Vec<Call>> {
    let list = match v {
        Json::Array(l) if !l.is_empty() => l,
        _ => {
            let name = if what == "repeat" {
                "repeat `steps`"
            } else {
                what
            };
            return Err(format!("{name} must be a non-empty list of steps"));
        }
    };
    let mut calls = Vec::new();
    for (k, raw) in list.iter().enumerate() {
        let fail = |e: String| format!("{what} step {}: {e}", k + 1);
        let obj = as_object(raw, "a step").map_err(fail)?;
        for key in obj.keys() {
            let e = match key.as_str() {
                "tool" | "args" | "optional" => continue,
                "repeat" => "repeat cannot be nested; put it at the top level".to_string(),
                "if" | "on_fail" | "stop_if" => {
                    format!("`{key}` works only on top-level steps, not inside repeat or on_fail")
                }
                _ => format!("unknown key `{key}`; a step here takes tool, args, optional"),
            };
            return Err(fail(e));
        }
        calls.push(parse_call(obj, n, true, known_tool).map_err(fail)?);
    }
    Ok(calls)
}

fn parse_repeat(v: &Json, n: usize, known_tool: &impl Fn(&str) -> bool) -> R<Body> {
    let obj = as_object(v, "repeat")
        .map_err(|e| format!("{e}: {{\"steps\": […], \"until\": {{…}}, \"max\": 10}}"))?;
    if let Some(k) = obj
        .keys()
        .find(|k| !["steps", "until", "max"].contains(&k.as_str()))
    {
        return Err(format!(
            "repeat: unknown key `{k}`; it takes steps, until, max"
        ));
    }
    let steps = parse_calls(
        obj.get("steps").unwrap_or(&Json::Null),
        n,
        "repeat",
        known_tool,
    )?;
    let until = match obj.get("until") {
        Some(Json::Object(u)) => {
            if u.contains_key("step") {
                return Err("repeat: `until` tests the repeat's last step; drop `step`".into());
            }
            parse_test(u).map_err(|e| format!("repeat: until: {e}"))?
        }
        Some(v) => {
            return Err(format!(
                "repeat: `until` must be an object like {{\"found\": true}}, not {v}"
            ));
        }
        None => return Err("repeat needs `until`, e.g. {\"found\": true}".into()),
    };
    if until == Test::Ok(false) && !steps.last().is_some_and(|c| c.optional) {
        return Err(
            "repeat: until {\"ok\": false} never holds: a failing step ends the repeat \
             unless it is optional"
                .into(),
        );
    }
    let max = match obj.get("max") {
        None | Some(Json::Null) => DEFAULT_RUNS,
        Some(m) => match m.as_u64() {
            Some(m @ 1..=MAX_RUNS) => m,
            _ => return Err(format!("repeat: `max` must be 1 to {MAX_RUNS}, not {m}")),
        },
    };
    Ok(Body::Repeat { steps, until, max })
}

/// `{"ok": bool}`, `{"found": bool}` or `{"contains": "text"}` (plus
/// `step`, read by the caller).
fn parse_test(obj: &Map<String, Json>) -> R<Test> {
    if let Some(k) = obj
        .keys()
        .find(|k| !["step", "ok", "found", "contains"].contains(&k.as_str()))
    {
        return Err(format!(
            "unknown key `{k}`; a condition takes one of ok, found, contains"
        ));
    }
    let tests: Vec<(&str, &Json)> = ["ok", "found", "contains"]
        .into_iter()
        .filter_map(|k| obj.get(k).map(|v| (k, v)))
        .collect();
    let [(key, value)] = tests[..] else {
        return Err(
            "give exactly one of \"ok\": bool, \"found\": bool, \"contains\": \"text\"".into(),
        );
    };
    match (key, value) {
        ("ok", Json::Bool(b)) => Ok(Test::Ok(*b)),
        ("found", Json::Bool(b)) => Ok(Test::Found(*b)),
        ("contains", Json::String(s)) if !s.is_empty() => Ok(Test::Contains(s.clone())),
        ("contains", v) => Err(format!("`contains` must be a non-empty string, not {v}")),
        (k, v) => Err(format!("`{k}` must be true or false, not {v}")),
    }
}

/// `{"step": N, TEST}` in top-level step `n`.
fn parse_cond(v: &Json, n: usize, earlier: &[Step]) -> R<Cond> {
    let obj = as_object(v, "a condition")
        .map_err(|e| format!("{e}; e.g. {{\"step\": 1, \"found\": true}}"))?;
    let step = match obj.get("step") {
        None => return Err("missing `step`: the number of an earlier step".into()),
        Some(s) => match s.as_u64() {
            Some(k) if k >= 1 && (k as usize) < n => k as usize,
            _ if n == 1 => return Err("the first step has no earlier step to test".into()),
            _ => {
                return Err(format!(
                    "`step` {s} is not an earlier step (1 to {})",
                    n - 1
                ));
            }
        },
    };
    let test = parse_test(obj)?;
    let target = &earlier[step - 1];
    if let Body::Stop(_) = target.body {
        return Err(format!("step {step} is a stop_if step: it has no result"));
    }
    if test == Test::Ok(false) && !target.optional && target.on_fail.is_empty() {
        return Err(format!(
            "{{\"ok\": false}} on step {step} never holds: the batch stops when it fails; \
             mark it \"optional\": true or give it on_fail"
        ));
    }
    Ok(Cond { step, test })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn known(t: &str) -> bool {
        ["find", "act", "key", "scroll", "wait_for", "click", "pause"].contains(&t)
    }

    fn err(steps: Json) -> String {
        parse(steps.as_array().unwrap(), known).unwrap_err()
    }

    #[test]
    fn parses_every_construct() {
        let steps = json!([
            {"tool": "find", "args": {"app": "a", "query": "x"}},
            {"repeat": {"steps": [
                {"tool": "scroll", "args": {"id": "$1"}},
                {"tool": "find", "args": {"app": "a", "query": "y"}, "optional": true},
             ], "until": {"found": true}, "max": 5}, "optional": true},
            {"tool": "act", "args": {"id": "$2"}, "if": {"step": 2, "found": true},
             "on_fail": [{"tool": "key", "args": {"combo": "Return"}}]},
            {"stop_if": {"step": 3, "contains": "Saved"}},
            {"tool": "key", "args": {"combo": "a"}, "if": {"step": 3, "ok": false}},
        ]);
        let parsed = parse(steps.as_array().unwrap(), known).unwrap();
        assert_eq!(parsed.len(), 5);
        let Body::Repeat { steps, until, max } = &parsed[1].body else {
            panic!("{:?}", parsed[1]);
        };
        assert_eq!((steps.len(), until, *max), (2, &Test::Found(true), 5));
        assert!(steps[1].optional && parsed[1].optional);
        assert_eq!(
            parsed[2].cond,
            Some(Cond {
                step: 2,
                test: Test::Found(true)
            })
        );
        assert_eq!(parsed[2].on_fail[0].tool, "key");
        assert_eq!(parsed[3].name(), "stop_if");
        assert_eq!(parsed[2].calls().count(), 2);
        // max defaults to 10.
        let p = parse(
            json!([{"repeat": {"steps": [{"tool": "pause"}], "until": {"ok": true}}}])
                .as_array()
                .unwrap(),
            known,
        )
        .unwrap();
        assert!(matches!(p[0].body, Body::Repeat { max: 10, .. }));
    }

    #[test]
    fn the_documented_examples_parse() {
        for example in crate::tools::BATCH_EXAMPLES {
            let steps: Json = serde_json::from_str(example).unwrap();
            parse(steps.as_array().unwrap(), crate::tools::is_tool).unwrap();
        }
    }

    #[test]
    fn rejects_bad_batches_naming_the_step() {
        let cases = [
            (json!([{"tool": "nope"}]), "[1] unknown tool 'nope'"),
            (json!([{"tool": "batch"}]), "[1] batch cannot contain batch"),
            (
                json!([{}]),
                "[1] a step has exactly one of tool, repeat or stop_if (found none)",
            ),
            (
                json!([{"tool": "key", "repeat": {}}]),
                "[1] a step has exactly one of tool, repeat or stop_if (found tool and repeat)",
            ),
            (
                json!([{"tool": "key", "iff": {}}]),
                "[1] unknown key `iff`; a step takes tool, args, optional, if, on_fail (or repeat, or stop_if alone)",
            ),
            (
                json!([{"tool": "key", "args": "a"}]),
                "[1] key: `args` must be an object, not \"a\"",
            ),
            (
                json!([{"tool": "act", "args": {"id": "$1"}}]),
                "[1] act: `id` \"$1\": refer to an earlier step (1 to 0)",
            ),
            (
                json!([{"tool": "pause"}, {"tool": "act", "args": {"id": "$x"}}]),
                "[2] act: `id` \"$x\": use \"$N\" for the element step N produced",
            ),
            (
                json!([{"tool": "key", "if": {"step": 1, "ok": true}}]),
                "[1] if: the first step has no earlier step to test",
            ),
            (
                json!([{"tool": "pause"}, {"tool": "key", "if": {"step": 2, "ok": true}}]),
                "[2] if: `step` 2 is not an earlier step (1 to 1)",
            ),
            (
                json!([{"tool": "pause"}, {"tool": "key", "if": {"ok": true}}]),
                "[2] if: missing `step`: the number of an earlier step",
            ),
            (
                json!([{"tool": "pause"}, {"tool": "key", "if": {"step": 1, "ok": true, "found": true}}]),
                "[2] if: give exactly one of \"ok\": bool, \"found\": bool, \"contains\": \"text\"",
            ),
            (
                json!([{"tool": "pause"}, {"tool": "key", "if": {"step": 1, "contains": ""}}]),
                "[2] if: `contains` must be a non-empty string, not \"\"",
            ),
            (
                json!([{"tool": "pause"}, {"tool": "key", "if": {"step": 1, "found": "yes"}}]),
                "[2] if: `found` must be true or false, not \"yes\"",
            ),
            (
                json!([{"tool": "pause"}, {"tool": "key", "if": {"step": 1, "ok": false}}]),
                "[2] if: {\"ok\": false} on step 1 never holds: the batch stops when it fails; \
                 mark it \"optional\": true or give it on_fail",
            ),
            (
                json!([{"stop_if": {"step": 1, "ok": true}}]),
                "[1] stop_if: the first step has no earlier step to test",
            ),
            (
                json!([{"tool": "pause"}, {"stop_if": {"step": 1, "ok": true}, "tool": "key"}]),
                "[2] a step has exactly one of tool, repeat or stop_if (found tool and stop_if)",
            ),
            (
                json!([{"tool": "pause"}, {"stop_if": {"step": 1, "ok": true}, "optional": true}]),
                "[2] a stop_if step takes nothing else (found `optional`)",
            ),
            (
                json!([{"tool": "pause"}, {"stop_if": {"step": 1, "ok": true}},
                       {"tool": "key", "if": {"step": 2, "ok": true}}]),
                "[3] if: step 2 is a stop_if step: it has no result",
            ),
            (
                json!([{"repeat": {"steps": [{"repeat": {}}], "until": {"ok": true}}}]),
                "[1] repeat step 1: repeat cannot be nested; put it at the top level",
            ),
            (
                json!([{"repeat": {"steps": [{"tool": "key", "if": {"step": 1, "ok": true}}], "until": {"ok": true}}}]),
                "[1] repeat step 1: `if` works only on top-level steps, not inside repeat or on_fail",
            ),
            (
                json!([{"repeat": {"steps": [], "until": {"ok": true}}}]),
                "[1] repeat `steps` must be a non-empty list of steps",
            ),
            (
                json!([{"repeat": {"steps": [{"tool": "key"}]}}]),
                "[1] repeat needs `until`, e.g. {\"found\": true}",
            ),
            (
                json!([{"repeat": {"steps": [{"tool": "key"}], "until": {"step": 1, "found": true}}}]),
                "[1] repeat: `until` tests the repeat's last step; drop `step`",
            ),
            (
                json!([{"repeat": {"steps": [{"tool": "key"}], "until": {"ok": true}, "max": 21}}]),
                "[1] repeat: `max` must be 1 to 20, not 21",
            ),
            (
                json!([{"repeat": {"steps": [{"tool": "key"}], "until": {"ok": false}}}]),
                "[1] repeat: until {\"ok\": false} never holds: a failing step ends the repeat \
                 unless it is optional",
            ),
            (
                json!([{"repeat": {"steps": [{"tool": "key"}], "until": {"ok": true}, "every": 1}}]),
                "[1] repeat: unknown key `every`; it takes steps, until, max",
            ),
            (
                json!([{"repeat": {"steps": [{"tool": "key"}]}, "until": {"ok": true}}]),
                "[1] `until` goes inside \"repeat\": {\"steps\": […], \"until\": …, \"max\": …}",
            ),
            (
                json!([{"tool": "pause"}, {"repeat": {"steps": [{"tool": "act", "args": {"id": "$2"}}], "until": {"ok": true}}}]),
                "[2] repeat step 1: act: `id` \"$2\": refer to an earlier step (1 to 1) \
                 (inside step 2: \"$N\" names a top-level step)",
            ),
            (
                json!([{"tool": "key", "on_fail": [{"tool": "key", "on_fail": []}]}]),
                "[1] on_fail step 1: `on_fail` works only on top-level steps, not inside repeat or on_fail",
            ),
            (
                json!([{"tool": "key", "on_fail": {"tool": "key"}}]),
                "[1] on_fail must be a non-empty list of steps",
            ),
            (
                json!([{"tool": "key", "optional": "yes"}]),
                "[1] `optional` must be true or false, not \"yes\"",
            ),
        ];
        for (steps, want) in cases {
            assert_eq!(err(steps.clone()), want, "{steps}");
        }
    }

    #[test]
    fn conditions_test_what_a_step_left() {
        let ran = |ok, element, text: &str| Record {
            tool: "find".into(),
            outcome: Outcome::Ran {
                ok,
                element,
                text: text.into(),
            },
        };
        let done = [
            ran(true, Some(4), "[4] push button \"Save\""),
            ran(true, None, "no matches"),
            ran(false, None, "no element matches 'x'"),
            Record {
                tool: "key".into(),
                outcome: Outcome::Skipped,
            },
        ];
        let holds = |step, test| Cond { step, test }.holds(&done);
        assert!(holds(1, Test::Found(true)));
        assert!(holds(1, Test::Ok(true)));
        assert!(holds(1, Test::Contains("SAVE".into())));
        assert!(!holds(2, Test::Found(true)));
        assert!(holds(2, Test::Found(false)));
        assert!(holds(2, Test::Ok(true)));
        assert!(holds(3, Test::Ok(false)));
        assert!(holds(3, Test::Found(false)));
        assert!(holds(3, Test::Contains("no element".into())));
        // A skipped step meets no condition, either way.
        for test in [Test::Ok(true), Test::Ok(false), Test::Found(false)] {
            assert!(!holds(4, test));
        }
        assert_eq!(
            Cond {
                step: 2,
                test: Test::Contains("Saved".into())
            }
            .describe(),
            "step 2 contains 'Saved'"
        );
        // References.
        let mut args = json!({"id": "$1", "from_id": "$1", "text": "$2"});
        resolve_refs(&mut args, 5, &done).unwrap();
        assert_eq!(args, json!({"id": 4, "from_id": 4, "text": "$2"}));
        for (n, want) in [
            (
                2,
                "`id` \"$2\": step 2 (find) produced no element (find, wait_for, and tools given an element do)",
            ),
            (3, "`id` \"$3\": step 3 (find) failed"),
            (4, "`id` \"$4\": step 4 (key) was skipped"),
        ] {
            let mut args = json!({"id": format!("${n}")});
            assert_eq!(resolve_refs(&mut args, 5, &done).unwrap_err(), want);
        }
    }
}
