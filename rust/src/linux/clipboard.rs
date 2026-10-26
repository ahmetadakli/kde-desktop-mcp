//! The clipboard through the Wayland data-control protocols (ext, else wlr),
//! over one connection kept for the life of the process: reading it for
//! `clipboard_get` and for saving it around a paste, owning it for
//! `clipboard_set` and for putting the user's text back, and a paste source
//! for `type_text` that reports each time an app reads it, so the user's
//! clipboard can be put back as soon as the paste went in.
//!
//! A thread dispatches the connection's events: it keeps the current
//! selection's offer and answers paste requests for our own selections.
//! Requests go out from the calling task; a `wl_display.sync` after them
//! confirms the compositor handled them, since it handles requests in order.

use std::io::{ErrorKind, Write};
use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::AsyncReadExt;
use tokio::sync::{mpsc, oneshot};

use crate::backend::R;

/// KDE's hint that the selection is a password: Klipper keeps it out of its
/// history, and `clipboard_get` does not read it.
pub const HINT_MIME: &str = "x-kde-passwordManagerHint";
/// Text types our selections offer, the set wl-copy offers, in the order
/// a read prefers them.
const TEXT_MIMES: [&str; 5] = [
    "text/plain;charset=utf-8",
    "text/plain",
    "UTF8_STRING",
    "STRING",
    "TEXT",
];
/// How long the clipboard owner may take to hand over its contents.
const READ_WAIT: Duration = Duration::from_millis(500);
/// How long the compositor may take to answer a round trip (to take a new
/// selection); longer means the connection is treated as broken.
const SYNC_WAIT: Duration = Duration::from_millis(250);
/// Largest clipboard `type_text` puts back; anything bigger stays replaced.
pub const RESTORE_MAX: usize = 4 << 20;
/// After a paste was read, how long no new request may come before the
/// paste counts as done. Some apps ask for a second type while handling one
/// paste (Firefox fills its paste event's data, then inserts); they ask
/// right after the first read ends, while Qt and GTK read once and are done
/// within 1 ms of the request. Counted from the end of our last write, so a
/// slow reader of a long text is waited for.
const PASTE_QUIET: Duration = Duration::from_millis(20);
/// Longest wait for reads of one paste to stop.
const PASTE_SETTLE_MAX: Duration = Duration::from_secs(1);

/// What the clipboard holds, from its offered types.
#[derive(Debug, PartialEq)]
pub enum Kind {
    Empty,
    /// Plain text; `secret` when marked as a password.
    Text {
        secret: bool,
    },
    /// Copied files (a file manager's copy): the text form is only paths.
    Files,
    /// Something else (an image...); the first type offered.
    Other(String),
}

pub fn classify(mimes: &[String]) -> Kind {
    let has = |m: &str| mimes.iter().any(|x| x == m);
    if mimes.iter().all(|m| m == HINT_MIME) {
        Kind::Empty
    } else if has("text/uri-list") || has("x-special/gnome-copied-files") {
        Kind::Files
    } else if mimes
        .iter()
        .any(|m| TEXT_MIMES.contains(&m.as_str()) || m.starts_with("text/plain"))
    {
        Kind::Text {
            secret: has(HINT_MIME),
        }
    } else {
        Kind::Other(mimes[0].clone())
    }
}

/// The text type to read from an offer of `mimes`.
fn text_mime(mimes: &[String]) -> Option<&str> {
    TEXT_MIMES
        .iter()
        .find(|t| mimes.iter().any(|m| m == *t))
        .copied()
        .or_else(|| {
            mimes
                .iter()
                .find(|m| m.starts_with("text/plain"))
                .map(|m| m.as_str())
        })
}

/// One paste request to our paste source: `done` once all of the text is
/// in the reader's pipe and our end is closed.
#[derive(Debug)]
struct Read {
    id: u64,
    at: Instant,
    done: bool,
}

/// The data-control connection for one protocol.
enum Conn {
    Ext(ext::Conn),
    Wlr(wlr::Conn),
}

macro_rules! each {
    ($conn:expr, $c:ident => $body:expr) => {
        match $conn {
            Conn::Ext($c) => $body,
            Conn::Wlr($c) => $body,
        }
    };
}

static CONN: tokio::sync::Mutex<Option<Arc<Conn>>> = tokio::sync::Mutex::const_new(None);

/// The shared connection, made on first use and again after it broke.
async fn conn() -> R<Arc<Conn>> {
    let mut slot = CONN.lock().await;
    if let Some(c) = slot.as_ref()
        && each!(&**c, c => c.alive())
    {
        return Ok(c.clone());
    }
    let made = tokio::task::spawn_blocking(|| {
        ext::Conn::connect()
            .map(Conn::Ext)
            .or_else(|_| wlr::Conn::connect().map(Conn::Wlr))
    });
    let c = Arc::new(
        tokio::time::timeout(Duration::from_secs(2), made)
            .await
            .map_err(|_| "clipboard: the compositor did not answer".to_string())?
            .map_err(|e| format!("clipboard: {e}"))??,
    );
    *slot = Some(c.clone());
    Ok(c)
}

/// Wait for a round trip the connection started; a timeout marks the
/// connection broken so the next call makes a new one.
async fn confirm(c: &Conn, done: oneshot::Receiver<()>) -> R<()> {
    match tokio::time::timeout(SYNC_WAIT, done).await {
        Ok(Ok(())) => Ok(()),
        _ => {
            each!(c, c => c.kill());
            Err("clipboard: the compositor did not answer".into())
        }
    }
}

/// Make `text` the selection, offered as text (and with the password hint
/// when `hint`), served by the connection's thread until another selection
/// replaces it. Returns once the compositor has it, so a paste sent
/// afterwards cannot get the previous one.
async fn offer(
    text: &[u8],
    mimes: &[&str],
    hint: bool,
    reads: Option<mpsc::UnboundedSender<Read>>,
) -> R<()> {
    let c = conn().await?;
    let mut mimes: Vec<String> = mimes.iter().map(|m| m.to_string()).collect();
    if hint {
        mimes.push(HINT_MIME.into());
    }
    let done = each!(&*c, c => c.set(text.into(), mimes, reads))?;
    confirm(&c, done).await
}

/// Own the clipboard with `text`; `secret` adds KDE's password hint (kept
/// when putting back a password the user had copied).
pub async fn own(text: &[u8], secret: bool) -> R<()> {
    offer(text, &TEXT_MIMES, secret, None).await
}

/// Types the clipboard offers now; empty when nothing is selected.
pub async fn mime_types() -> R<Vec<String>> {
    let c = conn().await?;
    // After the round trip, the selection events sent before it are in.
    let done = each!(&*c, c => c.sync())?;
    confirm(&c, done).await?;
    Ok(each!(&*c, c => c.mimes()))
}

/// Up to `max` bytes of the clipboard as text, and whether there was more.
/// Gives up when the owner does not answer within READ_WAIT, so a hung app
/// cannot hold a tool call.
pub async fn read_text(max: usize) -> R<(Vec<u8>, bool)> {
    let c = conn().await?;
    let done = each!(&*c, c => c.sync())?;
    confirm(&c, done).await?;
    let fd = each!(&*c, c => c.receive_text())?;
    let pipe = tokio::net::unix::pipe::Receiver::from_owned_fd(fd)
        .map_err(|e| format!("clipboard: {e}"))?;
    let mut buf = Vec::new();
    tokio::time::timeout(READ_WAIT, pipe.take(max as u64 + 1).read_to_end(&mut buf))
        .await
        .map_err(|_| "clipboard: its owner did not answer".to_string())?
        .map_err(|e| format!("clipboard: {e}"))?;
    let more = buf.len() > max;
    buf.truncate(max);
    Ok((buf, more))
}

/// At most `limit` characters of `bytes` as text (invalid UTF-8 replaced),
/// and whether any were cut.
pub fn cap_chars(bytes: &[u8], limit: usize) -> (String, bool) {
    let text = String::from_utf8_lossy(bytes);
    match text.char_indices().nth(limit) {
        Some((i, _)) => (text[..i].to_string(), true),
        None => (text.into_owned(), false),
    }
}

/// The clipboard as it was before a paste: what to put back, or why not.
pub enum Saved {
    Text { bytes: Vec<u8>, secret: bool },
    Empty,
    Nothing(&'static str),
}

pub async fn save() -> Saved {
    let mimes = match mime_types().await {
        Ok(m) => m,
        Err(_) => return Saved::Nothing("it could not be read"),
    };
    match classify(&mimes) {
        Kind::Empty => Saved::Empty,
        Kind::Files => Saved::Nothing("it held files"),
        Kind::Other(_) => Saved::Nothing("it held no text"),
        Kind::Text { secret } => match read_text(RESTORE_MAX).await {
            Ok((_, true)) => Saved::Nothing("it held more than 4 MiB"),
            Ok((bytes, false)) => Saved::Text { bytes, secret },
            Err(_) => Saved::Nothing("it could not be read"),
        },
    }
}

/// Text owned as the clipboard selection until another selection replaces
/// it; each read by an app is reported.
pub struct PasteSource {
    reads: mpsc::UnboundedReceiver<Read>,
}

impl PasteSource {
    /// Returns once the compositor has made `text` the selection. Offered
    /// with the password hint, so Klipper's history does not fill up with
    /// typed text (and Klipper reads only the hint, not the text).
    pub async fn start(text: &str) -> R<Self> {
        let (tx, reads) = mpsc::unbounded_channel();
        offer(text.as_bytes(), &TEXT_MIMES, true, Some(tx)).await?;
        Ok(PasteSource { reads })
    }

    /// Wait for an app to read the paste at or after `since` (reads before
    /// it come from clipboard managers), then until every read that came
    /// since has been written and no new one came for PASTE_QUIET. False
    /// when none comes within `limit` or another selection replaced this
    /// one first.
    pub async fn wait_read(&mut self, since: Instant, limit: Duration) -> bool {
        let deadline = tokio::time::Instant::from_std(since + limit);
        let mut open = Vec::new();
        loop {
            match tokio::time::timeout_at(deadline, self.reads.recv()).await {
                Ok(Some(r)) if !r.done && r.at >= since => {
                    open.push(r.id);
                    break;
                }
                Ok(Some(_)) => {}
                _ => return false,
            }
        }
        let settle_end = tokio::time::Instant::now() + PASTE_SETTLE_MAX;
        loop {
            let wait = if open.is_empty() {
                (tokio::time::Instant::now() + PASTE_QUIET).min(settle_end)
            } else {
                settle_end
            };
            match tokio::time::timeout_at(wait, self.reads.recv()).await {
                Ok(Some(r)) if r.done => open.retain(|&id| id != r.id),
                Ok(Some(r)) => open.push(r.id),
                // Quiet, replaced, or too long: done either way.
                _ => break,
            }
        }
        true
    }
}

/// Write all of `data` to a paste request's pipe; the reader may have made
/// it non-blocking, and may close it early.
fn send_data(fd: OwnedFd, data: &[u8]) {
    let mut file = std::fs::File::from(fd);
    let mut rest = data;
    let start = Instant::now();
    while !rest.is_empty() && start.elapsed() < Duration::from_secs(5) {
        match file.write(rest) {
            Ok(0) => break,
            Ok(n) => rest = &rest[n..],
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(1))
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
}

/// The connection for one data-control protocol; ext and wlr have the same
/// requests and events under different names.
macro_rules! data_control {
    ($name:ident, $proto:path, $manager:ident, $device:ident, $offer:ident, $source:ident,
     $m_mod:ident, $d_mod:ident, $o_mod:ident, $s_mod:ident) => {
        mod $name {
            use std::os::fd::OwnedFd;
            use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
            use std::sync::{Arc, Mutex};
            use std::time::Instant;

            use smithay_client_toolkit::reexports::client::{
                Connection, Dispatch, Proxy, QueueHandle, event_created_child,
                globals::{GlobalListContents, registry_queue_init},
                protocol::{wl_callback::{self, WlCallback}, wl_registry::WlRegistry,
                           wl_seat::WlSeat},
            };
            use tokio::sync::{mpsc, oneshot};
            use $proto::{
                $d_mod::{self, $device},
                $m_mod::$manager,
                $o_mod::{self, $offer},
                $s_mod::{self, $source},
            };

            use super::{HINT_MIME, Read, send_data, text_mime};
            use crate::backend::R;

            fn err(e: impl std::fmt::Display) -> String {
                format!("clipboard: {e}")
            }

            /// What the dispatching thread shares with callers.
            #[derive(Default)]
            struct Shared {
                /// The current selection's offer; None when empty.
                selection: Mutex<Option<$offer>>,
                dead: AtomicBool,
            }

            struct State {
                shared: Arc<Shared>,
            }

            /// One of our selections.
            struct SourceData {
                text: Arc<[u8]>,
                reads: Mutex<Option<mpsc::UnboundedSender<Read>>>,
                next: AtomicU64,
            }

            type Offered = Mutex<Vec<String>>;
            type Waiter = Mutex<Option<oneshot::Sender<()>>>;

            pub(super) struct Conn {
                conn: Connection,
                qh: QueueHandle<State>,
                manager: $manager,
                device: $device,
                shared: Arc<Shared>,
            }

            impl Conn {
                /// Connect, bind the manager and the seat's device, and
                /// start dispatching once the current selection is known.
                pub(super) fn connect() -> R<Self> {
                    let conn = Connection::connect_to_env().map_err(err)?;
                    let (globals, mut queue) = registry_queue_init::<State>(&conn).map_err(err)?;
                    let qh = queue.handle();
                    let manager: $manager = globals.bind(&qh, 1..=1, ()).map_err(err)?;
                    let seat: WlSeat = globals.bind(&qh, 1..=1, ()).map_err(err)?;
                    let device = manager.get_data_device(&seat, &qh, ());
                    let shared = Arc::new(Shared::default());
                    let mut state = State {
                        shared: shared.clone(),
                    };
                    queue.roundtrip(&mut state).map_err(err)?;
                    std::thread::Builder::new()
                        .name("clipboard".into())
                        .spawn(move || {
                            // Kept going after a timeout marked it dead: our
                            // selection on it may still be pasted.
                            while queue.blocking_dispatch(&mut state).is_ok() {}
                            state.shared.dead.store(true, Ordering::Relaxed);
                        })
                        .map_err(err)?;
                    Ok(Conn {
                        conn,
                        qh,
                        manager,
                        device,
                        shared,
                    })
                }

                pub(super) fn alive(&self) -> bool {
                    !self.shared.dead.load(Ordering::Relaxed)
                }

                pub(super) fn kill(&self) {
                    self.shared.dead.store(true, Ordering::Relaxed);
                }

                /// A round trip: the receiver fires once the compositor has
                /// handled every request sent before it.
                pub(super) fn sync(&self) -> R<oneshot::Receiver<()>> {
                    let (tx, rx) = oneshot::channel();
                    self.conn
                        .display()
                        .sync(&self.qh, Waiter::new(Some(tx)));
                    self.conn.flush().map_err(err)?;
                    Ok(rx)
                }

                /// Make `text` the selection, offered as `mimes`; requests
                /// are reported to `reads`. The receiver fires once it is
                /// the selection.
                pub(super) fn set(
                    &self,
                    text: Arc<[u8]>,
                    mimes: Vec<String>,
                    reads: Option<mpsc::UnboundedSender<Read>>,
                ) -> R<oneshot::Receiver<()>> {
                    let data = SourceData {
                        text,
                        reads: Mutex::new(reads),
                        next: AtomicU64::new(0),
                    };
                    let source = self.manager.create_data_source(&self.qh, data);
                    for mime in mimes {
                        source.offer(mime);
                    }
                    self.device.set_selection(Some(&source));
                    self.sync()
                }

                pub(super) fn mimes(&self) -> Vec<String> {
                    let sel = self.shared.selection.lock().unwrap();
                    sel.as_ref()
                        .and_then(|o| o.data::<Offered>())
                        .map(|m| m.lock().unwrap().clone())
                        .unwrap_or_default()
                }

                /// The read end of a pipe the selection's owner is asked
                /// to write its text into.
                pub(super) fn receive_text(&self) -> R<OwnedFd> {
                    let sel = self.shared.selection.lock().unwrap();
                    let offer = sel.as_ref().ok_or_else(|| err("it is empty"))?;
                    let mimes = offer
                        .data::<Offered>()
                        .map(|m| m.lock().unwrap().clone())
                        .unwrap_or_default();
                    let mime = text_mime(&mimes).ok_or_else(|| err("it holds no text"))?;
                    let (reader, writer) = std::io::pipe().map_err(err)?;
                    offer.receive(mime.to_string(), std::os::fd::AsFd::as_fd(&writer));
                    self.conn.flush().map_err(err)?;
                    Ok(reader.into())
                }
            }

            impl Dispatch<WlRegistry, GlobalListContents> for State {
                fn event(
                    _: &mut Self,
                    _: &WlRegistry,
                    _: <WlRegistry as Proxy>::Event,
                    _: &GlobalListContents,
                    _: &Connection,
                    _: &QueueHandle<Self>,
                ) {
                }
            }

            impl Dispatch<WlSeat, ()> for State {
                fn event(
                    _: &mut Self,
                    _: &WlSeat,
                    _: <WlSeat as Proxy>::Event,
                    _: &(),
                    _: &Connection,
                    _: &QueueHandle<Self>,
                ) {
                }
            }

            impl Dispatch<WlCallback, Waiter> for State {
                fn event(
                    _: &mut Self,
                    _: &WlCallback,
                    event: wl_callback::Event,
                    waiter: &Waiter,
                    _: &Connection,
                    _: &QueueHandle<Self>,
                ) {
                    if let wl_callback::Event::Done { .. } = event
                        && let Some(tx) = waiter.lock().unwrap().take()
                    {
                        let _ = tx.send(());
                    }
                }
            }

            impl Dispatch<$manager, ()> for State {
                fn event(
                    _: &mut Self,
                    _: &$manager,
                    _: <$manager as Proxy>::Event,
                    _: &(),
                    _: &Connection,
                    _: &QueueHandle<Self>,
                ) {
                }
            }

            impl Dispatch<$device, ()> for State {
                fn event(
                    state: &mut Self,
                    _: &$device,
                    event: $d_mod::Event,
                    _: &(),
                    _: &Connection,
                    _: &QueueHandle<Self>,
                ) {
                    match event {
                        $d_mod::Event::Selection { id } => {
                            let old = std::mem::replace(
                                &mut *state.shared.selection.lock().unwrap(),
                                id,
                            );
                            if let Some(old) = old {
                                old.destroy();
                            }
                        }
                        // The primary selection (middle click) is not used.
                        #[allow(unreachable_patterns)]
                        $d_mod::Event::PrimarySelection { id: Some(id) } => id.destroy(),
                        $d_mod::Event::Finished => {
                            state.shared.dead.store(true, Ordering::Relaxed);
                        }
                        _ => {}
                    }
                }

                event_created_child!(State, $device, [
                    $d_mod::EVT_DATA_OFFER_OPCODE => ($offer, Offered::default()),
                ]);
            }

            impl Dispatch<$offer, Offered> for State {
                fn event(
                    _: &mut Self,
                    _: &$offer,
                    event: $o_mod::Event,
                    mimes: &Offered,
                    _: &Connection,
                    _: &QueueHandle<Self>,
                ) {
                    if let $o_mod::Event::Offer { mime_type } = event {
                        mimes.lock().unwrap().push(mime_type);
                    }
                }
            }

            impl Dispatch<$source, SourceData> for State {
                fn event(
                    _: &mut Self,
                    source: &$source,
                    event: $s_mod::Event,
                    data: &SourceData,
                    _: &Connection,
                    _: &QueueHandle<Self>,
                ) {
                    match event {
                        $s_mod::Event::Send { mime_type, fd } => {
                            if mime_type == HINT_MIME {
                                send_data(fd, b"secret");
                                return;
                            }
                            // Written on a thread of its own: a reader that
                            // does not read must not hold this connection.
                            let reads = data.reads.lock().unwrap().clone();
                            let id = data.next.fetch_add(1, Ordering::Relaxed);
                            if let Some(r) = &reads {
                                let _ = r.send(Read { id, at: Instant::now(), done: false });
                            }
                            let text = data.text.clone();
                            let _ = std::thread::Builder::new()
                                .name("clipboard-send".into())
                                .spawn(move || {
                                    send_data(fd, &text);
                                    if let Some(r) = reads {
                                        let _ = r.send(Read { id, at: Instant::now(), done: true });
                                    }
                                });
                        }
                        $s_mod::Event::Cancelled => {
                            source.destroy();
                            // Closes the channel: the paste source was replaced.
                            data.reads.lock().unwrap().take();
                        }
                        _ => {}
                    }
                }
            }
        }
    };
}

data_control!(
    ext,
    smithay_client_toolkit::reexports::protocols::ext::data_control::v1::client,
    ExtDataControlManagerV1,
    ExtDataControlDeviceV1,
    ExtDataControlOfferV1,
    ExtDataControlSourceV1,
    ext_data_control_manager_v1,
    ext_data_control_device_v1,
    ext_data_control_offer_v1,
    ext_data_control_source_v1
);
data_control!(
    wlr,
    smithay_client_toolkit::reexports::protocols_wlr::data_control::v1::client,
    ZwlrDataControlManagerV1,
    ZwlrDataControlDeviceV1,
    ZwlrDataControlOfferV1,
    ZwlrDataControlSourceV1,
    zwlr_data_control_manager_v1,
    zwlr_data_control_device_v1,
    zwlr_data_control_offer_v1,
    zwlr_data_control_source_v1
);

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(m: &[&str]) -> Kind {
        classify(&m.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn clipboard_kinds() {
        assert_eq!(kinds(&[]), Kind::Empty);
        assert_eq!(kinds(&[HINT_MIME]), Kind::Empty);
        assert_eq!(
            kinds(&["text/html", "text/plain;charset=utf-8"]),
            Kind::Text { secret: false }
        );
        assert_eq!(kinds(&["UTF8_STRING"]), Kind::Text { secret: false });
        assert_eq!(
            kinds(&["text/plain", HINT_MIME]),
            Kind::Text { secret: true }
        );
        assert_eq!(kinds(&["text/uri-list", "text/plain"]), Kind::Files);
        assert_eq!(
            kinds(&["image/png", "image/jpeg"]),
            Kind::Other("image/png".into())
        );
        // Rich text alone is not plain text.
        assert_eq!(kinds(&["text/html"]), Kind::Other("text/html".into()));
    }

    #[test]
    fn text_mime_prefers_utf8() {
        let m = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let pick = |v: &[&str]| text_mime(&m(v)).map(str::to_string);
        assert_eq!(
            pick(&["TEXT", "text/plain", "text/plain;charset=utf-8"]),
            Some("text/plain;charset=utf-8".into())
        );
        assert_eq!(
            pick(&["text/html", "text/plain;charset=iso-8859-1"]),
            Some("text/plain;charset=iso-8859-1".into())
        );
        assert_eq!(pick(&["image/png"]), None);
    }

    #[test]
    fn cap_chars_counts_characters_not_bytes() {
        assert_eq!(cap_chars("çağ€x".as_bytes(), 4), ("çağ€".into(), true));
        assert_eq!(cap_chars("çağ".as_bytes(), 3), ("çağ".into(), false));
        assert_eq!(cap_chars(b"ab\xffc", 10), ("ab\u{fffd}c".into(), false));
        assert_eq!(cap_chars(b"", 0), (String::new(), false));
    }

    /// Feed `reads` to a paste source that was never offered.
    fn fake(reads: Vec<(u64, u64, bool)>) -> (PasteSource, Instant) {
        let (tx, rx) = mpsc::unbounded_channel();
        let since = Instant::now();
        tokio::spawn(async move {
            for (id, after_ms, done) in reads {
                tokio::time::sleep(Duration::from_millis(after_ms)).await;
                let at = Instant::now();
                tx.send(Read { id, at, done }).unwrap();
            }
            // Keep the channel open, as a source still selected would.
            tokio::time::sleep(Duration::from_secs(5)).await;
        });
        (PasteSource { reads: rx }, since)
    }

    #[tokio::test]
    async fn wait_read_settles_after_the_last_write() {
        // One read, written right away: done PASTE_QUIET after the write.
        let (mut s, since) = fake(vec![(0, 2, false), (0, 0, true)]);
        let t = tokio::time::Instant::now();
        assert!(s.wait_read(since, Duration::from_secs(1)).await);
        let took = t.elapsed();
        assert!(took >= Duration::from_millis(22) && took < Duration::from_millis(200));

        // A second type asked for 10 ms after the first: waited for too.
        let (mut s, since) = fake(vec![
            (0, 1, false),
            (0, 0, true),
            (1, 10, false),
            (1, 0, true),
        ]);
        let t = tokio::time::Instant::now();
        assert!(s.wait_read(since, Duration::from_secs(1)).await);
        assert!(t.elapsed() >= Duration::from_millis(31));

        // A write still going on holds the settle open past PASTE_QUIET.
        let (mut s, since) = fake(vec![(0, 1, false), (0, 300, true)]);
        let t = tokio::time::Instant::now();
        assert!(s.wait_read(since, Duration::from_secs(1)).await);
        assert!(t.elapsed() >= Duration::from_millis(321));

        // Nobody reads: false at the limit.
        let (mut s, since) = fake(vec![]);
        assert!(!s.wait_read(since, Duration::from_millis(200)).await);
    }

    /// Against a throwaway compositor only, never the user's session:
    /// `dbus-run-session -- kwin_wayland --virtual --no-lockscreen --socket
    /// wl-mcp-test --exit-with-session <script running this test>` with
    /// DESKTOP_MCP_NESTED_TEST=wl-mcp-test set (the lock screen would read
    /// the clipboard).
    #[test]
    #[ignore]
    fn paste_source_reports_reads_and_restoring_replaces_it() {
        let socket = std::env::var("DESKTOP_MCP_NESTED_TEST").unwrap_or_default();
        if socket.is_empty() || std::env::var("WAYLAND_DISPLAY").ok() != Some(socket) {
            eprintln!("skipped: not inside the nested compositor");
            return;
        }
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            own("before ç".as_bytes(), false).await.unwrap();
            let Saved::Text { bytes, secret } = save().await else {
                panic!("text not saved");
            };
            assert_eq!((bytes.as_slice(), secret), ("before ç".as_bytes(), false));

            let mut source = PasteSource::start("typed €").await.unwrap();
            let mimes = mime_types().await.unwrap();
            assert_eq!(classify(&mimes), Kind::Text { secret: true }, "{mimes:?}");
            let since = Instant::now();
            let out = tokio::process::Command::new("wl-paste")
                .arg("-n")
                .output()
                .await
                .unwrap();
            assert_eq!(String::from_utf8_lossy(&out.stdout), "typed €");
            assert!(source.wait_read(since, Duration::from_secs(1)).await);

            own(&bytes, secret).await.unwrap();
            assert_eq!(read_text(100).await.unwrap(), (bytes.clone(), false));
            // Another client sees it at once, after the single round trip.
            let out = tokio::process::Command::new("wl-paste")
                .arg("-n")
                .output()
                .await
                .unwrap();
            assert_eq!(out.stdout, bytes);
            // Replaced: the source is cancelled and closes the channel.
            let closed = tokio::time::timeout(Duration::from_secs(1), async {
                while source.reads.recv().await.is_some() {}
            })
            .await;
            assert!(closed.is_ok(), "paste source still serving");

            // A password put back keeps its hint.
            own(b"pw", true).await.unwrap();
            assert!(matches!(save().await, Saved::Text { secret: true, .. }));

            // Nobody pastes: not read, so type_text would not restore.
            let mut source = PasteSource::start("unread").await.unwrap();
            assert!(
                !source
                    .wait_read(Instant::now(), Duration::from_millis(200))
                    .await
            );
            assert_eq!(read_text(3).await.unwrap(), (b"unr".to_vec(), true));

            // Another client's selection is seen, and the text it offers.
            let status = tokio::process::Command::new("wl-copy")
                .arg("from wl-copy")
                .status()
                .await
                .unwrap();
            assert!(status.success());
            assert_eq!(
                read_text(100).await.unwrap(),
                (b"from wl-copy".to_vec(), false)
            );

            offer(b"file:///tmp/x", &["text/uri-list"], false, None)
                .await
                .unwrap();
            assert!(matches!(save().await, Saved::Nothing("it held files")));

            // Nothing selected (the owner went away).
            let status = tokio::process::Command::new("wl-copy")
                .arg("--clear")
                .status()
                .await
                .unwrap();
            assert!(status.success());
            assert!(matches!(save().await, Saved::Empty));
        });
    }
}
