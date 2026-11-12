//! Whether an input tool's input reached the desktop, so its reply can say
//! whether a retry is safe.
//!
//! Classified where the input is written, not from the reply's words: the
//! server drives the desktop only through `Sends`, which notes every write
//! (an action, a text write, a key, a pointer event, a window change) in
//! the running call's `track` scope. A backend that refuses a write before
//! sending anything (a key name it does not know, no matching window) says
//! so with `nothing_sent`. A call that wrote nothing failed or did nothing
//! without effect: "nothing sent", safe to retry. One that wrote and then
//! failed (an action without reply, half the text typed, a batch stopped
//! after some steps) may have acted: "maybe sent", look before retrying.
//! A reply without either succeeded and sent its input.

use std::cell::Cell;
use std::future::Future;
use std::ops::Deref;
use std::time::Duration;

use crate::backend::{Activated, Backend, Launched, R, WindowChange, WindowGeo};

/// The marker of a reply or error whose call sent nothing.
pub const NOTHING_SENT: &str = "nothing sent";
/// The marker of an error after input was, or may have been, sent.
pub const MAYBE_SENT: &str = "maybe sent: look before retrying";

tokio::task_local! {
    /// Whether the call running in this `track` scope wrote anything.
    static SENT: Cell<bool>;
    /// Whether the backend write running in this `write` scope said it
    /// sent nothing (`nothing_sent`).
    static UNSENT: Cell<bool>;
}

/// Note that input was (or may have been) sent in the running call.
pub fn sent() {
    let _ = SENT.try_with(|s| s.set(true));
}

/// For a backend: the write running now fails with `e` before sending
/// anything (an unknown key or button name, no window matches). Returns `e`.
pub fn nothing_sent(e: impl Into<String>) -> String {
    let _ = UNSENT.try_with(|u| u.set(true));
    e.into()
}

/// Run `f` in a scope of its own: its result, and whether it sent input.
/// What it sent counts for the enclosing scope as well (a batch's steps
/// for the batch).
pub async fn track<T>(f: impl Future<Output = T>) -> (T, bool) {
    let (r, wrote) = SENT
        .scope(Cell::new(false), async {
            let r = f.await;
            (r, SENT.with(Cell::get))
        })
        .await;
    if wrote {
        sent();
    }
    (r, wrote)
}

/// Run `f` without counting what it writes: bringing a tool's `window` to
/// the front first, which a retry repeats harmlessly.
pub async fn untracked<T>(f: impl Future<Output = T>) -> T {
    SENT.scope(Cell::new(false), f).await
}

/// One backend write: noted as sent unless it failed saying it sent
/// nothing (`nothing_sent`) or `declined` says its result changed nothing.
async fn write_unless<T>(f: impl Future<Output = R<T>>, declined: fn(&R<T>) -> bool) -> R<T> {
    let (r, unsent) = UNSENT
        .scope(Cell::new(false), async {
            let r = f.await;
            (r, UNSENT.with(Cell::get))
        })
        .await;
    if !(r.is_err() && unsent) && !declined(&r) {
        sent();
    }
    r
}

/// `write_unless` for a write whose every result counts.
async fn write<T>(f: impl Future<Output = R<T>>) -> R<T> {
    write_unless(f, |_| false).await
}

/// The error `e` of an input tool with its marker: MAYBE_SENT when the call
/// wrote anything, else NOTHING_SENT; once only. The call's marker decides:
/// after a write, an inner "nothing sent" (a fallback refused after an
/// earlier write) becomes "nothing more sent", so a reply never carries both.
pub fn error(e: String, wrote: bool) -> String {
    let marker = if wrote { MAYBE_SENT } else { NOTHING_SENT };
    let e = if wrote { unsay_nothing(e) } else { e };
    if e.contains(marker) {
        e
    } else {
        format!("{e}; {marker}")
    }
}

/// The successful reply `text` of an input tool: says NOTHING_SENT when the
/// call wrote nothing (and does not say so already); never says it when the
/// call wrote.
pub fn reply(text: &mut String, wrote: bool) {
    if wrote {
        if text.contains(NOTHING_SENT) {
            *text = unsay_nothing(std::mem::take(text));
        }
    } else if !text.contains(NOTHING_SENT) {
        text.push_str("; ");
        text.push_str(NOTHING_SENT);
    }
}

/// `text` without NOTHING_SENT, for a call that did write: "nothing more
/// sent" (the part that said it sent nothing, after what came before).
fn unsay_nothing(text: String) -> String {
    text.replace(NOTHING_SENT, "nothing more sent")
}

/// Batch step numbers as "[1..3] [5]".
pub fn steps(ns: &[usize]) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < ns.len() {
        let mut j = i;
        while j + 1 < ns.len() && ns[j + 1] == ns[j] + 1 {
            j += 1;
        }
        out.push(match j > i {
            true => format!("[{}..{}]", ns[i], ns[j]),
            false => format!("[{}]", ns[i]),
        });
        i = j + 1;
    }
    out.join(" ")
}

/// What a batch that stopped at step `at` of `total` delivered: the steps
/// that sent input (`sent`), the ones that failed after writing (`maybe`),
/// and the ones a retry still has to run (`not sent`: the steps after `at`,
/// and `at` itself when it wrote nothing). `at` counts as maybe sent when
/// it wrote anything, since it failed. `input`: the batch has input steps
/// (else only the steps not run are named).
pub fn stopped(sent: &[usize], maybe: &[usize], at: usize, total: usize, input: bool) -> String {
    let mut parts = vec!["stopped".to_string()];
    if !input {
        if at < total {
            let rest: Vec<usize> = (at + 1..=total).collect();
            parts.push(format!("not run: {}", steps(&rest)));
        }
        return parts.join("; ");
    }
    if sent.is_empty() && maybe.is_empty() {
        // Nothing reached the desktop: the whole batch may run again.
        parts.push(NOTHING_SENT.into());
        return parts.join("; ");
    }
    let wrote_at = sent.contains(&at) || maybe.contains(&at);
    let sent: Vec<usize> = sent.iter().copied().filter(|&k| k != at).collect();
    let mut maybe: Vec<usize> = maybe.iter().copied().filter(|&k| k != at).collect();
    if wrote_at {
        maybe.push(at);
        maybe.sort_unstable();
    }
    if !sent.is_empty() {
        parts.push(format!("sent: {}", steps(&sent)));
    }
    if !maybe.is_empty() {
        parts.push(format!("maybe sent: {}", steps(&maybe)));
    }
    let first = if wrote_at { at + 1 } else { at };
    if first <= total {
        let rest: Vec<usize> = (first..=total).collect();
        parts.push(format!("not sent: {}", steps(&rest)));
    }
    parts.join("; ")
}

/// The backend, as the server drives it: every method that writes to the
/// desktop notes it (`write`); reads go straight through (`Deref`).
pub struct Sends<B>(pub B);

impl<B> Deref for Sends<B> {
    type Target = B;
    fn deref(&self) -> &B {
        &self.0
    }
}

impl<B: Backend> Sends<B> {
    pub async fn do_action(&self, id: &B::Id, index: usize) -> R<bool> {
        write(self.0.do_action(id, index)).await
    }
    pub async fn select_item(&self, item: &B::Id) -> R<bool> {
        write(self.0.select_item(item)).await
    }
    /// Ok(false), the app declining, scrolled nothing.
    pub async fn scroll_into_view(&self, id: &B::Id) -> R<bool> {
        write_unless(self.0.scroll_into_view(id), |r| matches!(r, Ok(false))).await
    }
    pub async fn set_text(&self, id: &B::Id, text: &str) -> R<bool> {
        write(self.0.set_text(id, text)).await
    }
    /// Ok(None), the app declining, moved no focus.
    pub async fn try_focus(&self, id: &B::Id) -> R<Option<String>> {
        write_unless(self.0.try_focus(id), |r| matches!(r, Ok(None))).await
    }
    pub async fn press_combo(&self, combo: &str) -> R<()> {
        write(self.0.press_combo(combo)).await
    }
    pub async fn type_text(&self, text: &str) -> R<String> {
        write(self.0.type_text(text)).await
    }
    pub async fn pointer(&self, x: i64, y: i64, button: &str, clicks: i64, scroll: i64) -> R<()> {
        write(self.0.pointer(x, y, button, clicks, scroll)).await
    }
    pub async fn move_pointer(&self, x: i64, y: i64) -> R<()> {
        write(self.0.move_pointer(x, y)).await
    }
    pub async fn button(&self, button: &str, down: bool) -> R<()> {
        write(self.0.button(button, down)).await
    }
    pub async fn key(&self, key: &str, down: bool) -> R<()> {
        write(self.0.key(key, down)).await
    }
    pub async fn activate_window(&self, query: &str, allowed: &[String]) -> R<Option<Activated>> {
        write(self.0.activate_window(query, allowed)).await
    }
    pub async fn set_window(
        &self,
        query: &str,
        allowed: &[String],
        change: &WindowChange,
    ) -> R<WindowGeo> {
        write(self.0.set_window(query, allowed, change)).await
    }
    pub async fn close_window(&self, query: &str, allowed: &[String]) -> R<WindowGeo> {
        write(self.0.close_window(query, allowed)).await
    }
    pub async fn clipboard_set(&self, text: &str) -> R<()> {
        write(self.0.clipboard_set(text)).await
    }
    pub async fn launch(
        &self,
        app: &str,
        args: &[String],
        allowed: &[String],
        wait: Duration,
    ) -> R<Launched> {
        write(self.0.launch(app, args, allowed, wait)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markers_once() {
        assert_eq!(
            error("no visible element".into(), false),
            "no visible element; nothing sent"
        );
        assert_eq!(
            error("it is disabled; nothing sent".into(), false),
            "it is disabled; nothing sent"
        );
        assert_eq!(
            error("DoAction got no reply".into(), true),
            "DoAction got no reply; maybe sent: look before retrying"
        );
        // After a write, the call's marker decides: an inner "nothing
        // sent" (a fallback refused after an earlier write) is not left in.
        assert_eq!(
            error(
                "sent Selection, but ...; the key fallback: needs the full tier; nothing sent"
                    .into(),
                true
            ),
            "sent Selection, but ...; the key fallback: needs the full tier; nothing more sent; \
             maybe sent: look before retrying"
        );
        let mut t = "already \"A\"; nothing sent".to_string();
        reply(&mut t, true);
        assert_eq!(t, "already \"A\"; nothing more sent");
        let mut t = "Press -> ok".to_string();
        reply(&mut t, true);
        assert_eq!(t, "Press -> ok");
        reply(&mut t, false);
        assert_eq!(t, "Press -> ok; nothing sent");
        reply(&mut t, false);
        assert_eq!(t, "Press -> ok; nothing sent");
    }

    #[test]
    fn step_ranges() {
        assert_eq!(steps(&[]), "");
        assert_eq!(steps(&[2]), "[2]");
        assert_eq!(steps(&[1, 2, 3, 5, 7, 8]), "[1..3] [5] [7..8]");
        assert_eq!(
            stopped(&[1, 2], &[3], 3, 5, true),
            "stopped; sent: [1..2]; maybe sent: [3]; not sent: [4..5]"
        );
        // The failing step sent nothing: it is still to send.
        assert_eq!(
            stopped(&[1], &[], 3, 4, true),
            "stopped; sent: [1]; not sent: [3..4]"
        );
        assert_eq!(
            stopped(&[1], &[], 2, 2, true),
            "stopped; sent: [1]; not sent: [2]"
        );
        // It failed after writing (its on_fail sent): maybe sent.
        assert_eq!(
            stopped(&[1, 2], &[], 2, 3, true),
            "stopped; sent: [1]; maybe sent: [2]; not sent: [3]"
        );
        assert_eq!(stopped(&[], &[], 1, 2, true), "stopped; nothing sent");
        assert_eq!(stopped(&[], &[], 2, 2, true), "stopped; nothing sent");
        assert_eq!(stopped(&[], &[], 1, 3, false), "stopped; not run: [2..3]");
    }

    #[tokio::test]
    async fn tracks_writes_and_refusals() {
        // A write noted inside counts for the scope and the one around it.
        let (((), wrote_inner), wrote_outer) = track(track(async {
            write(async { Ok::<_, String>(()) }).await.unwrap();
        }))
        .await;
        assert!(wrote_inner && wrote_outer);
        // A backend refusal before sending: not sent.
        let (r, wrote) = track(write(async { Err::<(), _>(nothing_sent("no such key")) })).await;
        assert_eq!(r.unwrap_err(), "no such key");
        assert!(!wrote);
        // A failure after the write started (a timeout): sent, maybe.
        let (_, wrote) = track(write(async { Err::<(), _>("timed out".to_string()) })).await;
        assert!(wrote);
        // Untracked writes (the `window` activation) do not count.
        let (_, wrote) = track(untracked(write(async { Ok::<_, String>(()) }))).await;
        assert!(!wrote);
    }
}
