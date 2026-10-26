//! The clipboard through the Wayland data-control protocols (ext, else wlr):
//! reading it for `clipboard_get` and for saving it around a paste, and a
//! paste source for `type_text` that reports each time an app reads it, so
//! the user's clipboard can be put back as soon as the paste went in.

use std::io::{ErrorKind, Write};
use std::os::fd::OwnedFd;
use std::time::{Duration, Instant};

use tokio::io::AsyncReadExt;
use tokio::sync::{mpsc, oneshot};
use wl_clipboard_rs::paste;

use crate::backend::R;

/// KDE's hint that the selection is a password: Klipper keeps it out of its
/// history, and `clipboard_get` does not read it.
pub const HINT_MIME: &str = "x-kde-passwordManagerHint";
/// Text types the paste source offers, the set wl-copy offers.
const TEXT_MIMES: [&str; 5] = [
    "text/plain;charset=utf-8",
    "text/plain",
    "UTF8_STRING",
    "STRING",
    "TEXT",
];
/// How long the clipboard owner may take to hand over its contents.
const READ_WAIT: Duration = Duration::from_millis(500);
/// Largest clipboard `type_text` puts back; anything bigger stays replaced.
pub const RESTORE_MAX: usize = 4 << 20;
/// After the first read of a paste, how long to wait for more: some apps ask
/// for a second type while handling one paste.
const PASTE_SETTLE: Duration = Duration::from_millis(50);

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

/// Types the clipboard offers now; empty when nothing is selected.
pub async fn mime_types() -> R<Vec<String>> {
    let got = tokio::task::spawn_blocking(|| {
        paste::get_mime_types_ordered(paste::ClipboardType::Regular, paste::Seat::Unspecified)
    })
    .await
    .map_err(|e| format!("clipboard: {e}"))?;
    match got {
        Ok(m) => Ok(m),
        Err(paste::Error::ClipboardEmpty | paste::Error::NoSeats) => Ok(Vec::new()),
        Err(e) => Err(format!("clipboard: {e}")),
    }
}

/// Up to `max` bytes of the clipboard as text, and whether there was more.
/// Gives up when the owner does not answer within READ_WAIT, so a hung app
/// cannot hold a tool call.
pub async fn read_text(max: usize) -> R<(Vec<u8>, bool)> {
    let got = tokio::task::spawn_blocking(|| {
        paste::get_contents(
            paste::ClipboardType::Regular,
            paste::Seat::Unspecified,
            paste::MimeType::Text,
        )
    })
    .await
    .map_err(|e| format!("clipboard: {e}"))?;
    let (pipe, _) = got.map_err(|e| format!("clipboard: {e}"))?;
    let fd: OwnedFd = pipe.into();
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

/// Text owned as the clipboard selection by a thread of this process until
/// another selection replaces it; each read by an app is reported.
pub struct PasteSource {
    reads: mpsc::UnboundedReceiver<Instant>,
}

impl PasteSource {
    /// Returns once the compositor has made `text` the selection. Offered
    /// with the password hint, so Klipper's history does not fill up with
    /// typed text (and Klipper reads only the hint, not the text).
    pub async fn start(text: &str) -> R<Self> {
        let bytes = text.as_bytes().to_vec();
        let (ready_tx, ready) = oneshot::channel();
        let (reads_tx, reads) = mpsc::unbounded_channel();
        std::thread::spawn(move || {
            let mut ready = Some(ready_tx);
            let mut res = ext::serve(&bytes, &mut ready, &reads_tx);
            if res.is_err() && ready.is_some() {
                res = wlr::serve(&bytes, &mut ready, &reads_tx);
            }
            if let (Err(e), Some(tx)) = (res, ready.take()) {
                let _ = tx.send(Err(e));
            }
        });
        ready
            .await
            .map_err(|_| "clipboard: paste source failed".to_string())??;
        Ok(PasteSource { reads })
    }

    /// Wait for an app to read the paste at or after `since` (reads before
    /// it come from clipboard managers), then until reads stop for
    /// PASTE_SETTLE. False when none comes within `limit` or another
    /// selection replaced this one first.
    pub async fn wait_read(&mut self, since: Instant, limit: Duration) -> bool {
        let deadline = tokio::time::Instant::from_std(since + limit);
        loop {
            match tokio::time::timeout_at(deadline, self.reads.recv()).await {
                Ok(Some(t)) if t >= since => break,
                Ok(Some(_)) => {}
                _ => return false,
            }
        }
        for _ in 0..20 {
            if !matches!(
                tokio::time::timeout(PASTE_SETTLE, self.reads.recv()).await,
                Ok(Some(_))
            ) {
                break;
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

/// The paste source for one data-control protocol; ext and wlr have the
/// same requests and events under different names.
macro_rules! data_control {
    ($name:ident, $proto:path, $manager:ident, $device:ident, $offer:ident, $source:ident,
     $m_mod:ident, $d_mod:ident, $o_mod:ident, $s_mod:ident) => {
        mod $name {
            use std::time::Instant;

            use smithay_client_toolkit::reexports::client::{
                Connection, Dispatch, QueueHandle, event_created_child,
                globals::{GlobalListContents, registry_queue_init},
                protocol::{wl_registry::WlRegistry, wl_seat::WlSeat},
            };
            use tokio::sync::{mpsc, oneshot};
            use $proto::{
                $d_mod::{self, $device},
                $m_mod::$manager,
                $o_mod::$offer,
                $s_mod::{self, $source},
            };

            use super::{HINT_MIME, TEXT_MIMES, send_data};
            use crate::backend::R;

            struct State {
                text: Vec<u8>,
                reads: mpsc::UnboundedSender<Instant>,
                done: bool,
            }

            impl Dispatch<WlRegistry, GlobalListContents> for State {
                fn event(
                    _: &mut Self,
                    _: &WlRegistry,
                    _: <WlRegistry as smithay_client_toolkit::reexports::client::Proxy>::Event,
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
                    _: <WlSeat as smithay_client_toolkit::reexports::client::Proxy>::Event,
                    _: &(),
                    _: &Connection,
                    _: &QueueHandle<Self>,
                ) {
                }
            }

            impl Dispatch<$manager, ()> for State {
                fn event(
                    _: &mut Self,
                    _: &$manager,
                    _: <$manager as smithay_client_toolkit::reexports::client::Proxy>::Event,
                    _: &(),
                    _: &Connection,
                    _: &QueueHandle<Self>,
                ) {
                }
            }

            impl Dispatch<$device, ()> for State {
                fn event(
                    _: &mut Self,
                    _: &$device,
                    event: $d_mod::Event,
                    _: &(),
                    _: &Connection,
                    _: &QueueHandle<Self>,
                ) {
                    // Offers of other clients' selections are not needed.
                    if let $d_mod::Event::DataOffer { id } = event {
                        id.destroy();
                    }
                }

                event_created_child!(State, $device, [
                    $d_mod::EVT_DATA_OFFER_OPCODE => ($offer, ()),
                ]);
            }

            impl Dispatch<$offer, ()> for State {
                fn event(
                    _: &mut Self,
                    _: &$offer,
                    _: <$offer as smithay_client_toolkit::reexports::client::Proxy>::Event,
                    _: &(),
                    _: &Connection,
                    _: &QueueHandle<Self>,
                ) {
                }
            }

            impl Dispatch<$source, ()> for State {
                fn event(
                    state: &mut Self,
                    source: &$source,
                    event: $s_mod::Event,
                    _: &(),
                    _: &Connection,
                    _: &QueueHandle<Self>,
                ) {
                    match event {
                        $s_mod::Event::Send { mime_type, fd } => {
                            if mime_type == HINT_MIME {
                                send_data(fd, b"secret");
                            } else {
                                send_data(fd, &state.text);
                                let _ = state.reads.send(Instant::now());
                            }
                        }
                        $s_mod::Event::Cancelled => {
                            source.destroy();
                            state.done = true;
                        }
                        _ => {}
                    }
                }
            }

            /// Own the selection with `text`, signal `ready` once the
            /// compositor has it, and serve reads until it is replaced.
            /// Errors only come before `ready`.
            pub(super) fn serve(
                text: &[u8],
                ready: &mut Option<oneshot::Sender<R<()>>>,
                reads: &mpsc::UnboundedSender<Instant>,
            ) -> R<()> {
                let err = |e: &dyn std::fmt::Display| format!("clipboard: {e}");
                let conn = Connection::connect_to_env().map_err(|e| err(&e))?;
                let (globals, mut queue) = registry_queue_init::<State>(&conn).map_err(|e| err(&e))?;
                let qh = queue.handle();
                let manager: $manager = globals.bind(&qh, 1..=1, ()).map_err(|e| err(&e))?;
                let seat: WlSeat = globals.bind(&qh, 1..=1, ()).map_err(|e| err(&e))?;
                let device = manager.get_data_device(&seat, &qh, ());
                let source = manager.create_data_source(&qh, ());
                for mime in TEXT_MIMES.iter().chain([&HINT_MIME]) {
                    source.offer(mime.to_string());
                }
                device.set_selection(Some(&source));
                let mut state = State {
                    text: text.to_vec(),
                    reads: reads.clone(),
                    done: false,
                };
                // Requests are handled in order: after the round trip the
                // selection is ours, so a paste cannot get the previous one.
                queue.roundtrip(&mut state).map_err(|e| err(&e))?;
                if let Some(tx) = ready.take() {
                    let _ = tx.send(Ok(()));
                }
                while !state.done && queue.blocking_dispatch(&mut state).is_ok() {}
                device.destroy();
                Ok(())
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
    fn cap_chars_counts_characters_not_bytes() {
        assert_eq!(cap_chars("çağ€x".as_bytes(), 4), ("çağ€".into(), true));
        assert_eq!(cap_chars("çağ".as_bytes(), 3), ("çağ".into(), false));
        assert_eq!(cap_chars(b"ab\xffc", 10), ("ab\u{fffd}c".into(), false));
        assert_eq!(cap_chars(b"", 0), (String::new(), false));
    }

    /// Against a throwaway compositor only, never the user's session:
    /// `dbus-run-session -- kwin_wayland --virtual --socket wl-mcp-test
    /// --exit-with-session <script running this test>` with
    /// DESKTOP_MCP_NESTED_TEST=wl-mcp-test set.
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
            crate::linux::own_clipboard("before ç".as_bytes(), false).unwrap();
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

            let restore = bytes.clone();
            tokio::task::spawn_blocking(move || crate::linux::own_clipboard(&restore, secret))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(read_text(100).await.unwrap(), (bytes, false));
            // Replaced: the serving thread ends and closes the channel.
            let closed = tokio::time::timeout(Duration::from_secs(1), async {
                while source.reads.recv().await.is_some() {}
            })
            .await;
            assert!(closed.is_ok(), "paste source still serving");

            // Nobody pastes: not read, so type_text would not restore.
            let mut source = PasteSource::start("unread").await.unwrap();
            assert!(
                !source
                    .wait_read(Instant::now(), Duration::from_millis(200))
                    .await
            );
            assert_eq!(read_text(3).await.unwrap(), (b"unr".to_vec(), true));

            wl_clipboard_rs::copy::Options::new()
                .copy(
                    wl_clipboard_rs::copy::Source::Bytes(b"file:///tmp/x".to_vec().into()),
                    wl_clipboard_rs::copy::MimeType::Specific("text/uri-list".into()),
                )
                .unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert!(matches!(save().await, Saved::Nothing("it held files")));
        });
    }
}
