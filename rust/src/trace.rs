//! Timing spans for finding where a call spends its time: with
//! DESKTOP_MCP_TRACE=1 each span prints "trace <ms> <name>" to stderr when
//! it ends, nested spans indented. Off, a span costs one atomic load.

use std::cell::Cell;
use std::future::Future;
use std::sync::OnceLock;
use std::time::Instant;

pub fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("DESKTOP_MCP_TRACE").is_ok_and(|v| v.trim() == "1"))
}

thread_local! {
    static DEPTH: Cell<usize> = const { Cell::new(0) };
}

/// Await `fut`, timing it as `name` when tracing is on.
pub async fn time<T>(name: &str, fut: impl Future<Output = T>) -> T {
    if !on() {
        return fut.await;
    }
    let depth = DEPTH.with(|d| {
        let v = d.get();
        d.set(v + 1);
        v
    });
    let start = Instant::now();
    let out = fut.await;
    DEPTH.with(|d| d.set(depth));
    eprintln!(
        "trace {:8.3} {}{name}",
        start.elapsed().as_secs_f64() * 1000.0,
        "  ".repeat(depth)
    );
    out
}
