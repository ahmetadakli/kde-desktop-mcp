//! Hyprland's request socket, spoken directly instead of spawning `hyprctl`.

use std::path::PathBuf;

use serde_json::Value as Json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use crate::backend::R;

pub struct Hypr {
    socket: PathBuf,
}

impl Hypr {
    /// The running instance's socket; None outside a Hyprland session.
    pub fn from_env() -> Option<Self> {
        let sig = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok()?;
        let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap_or_default();
        // Hyprland 0.40 moved the sockets from /tmp into XDG_RUNTIME_DIR.
        let candidates = [
            PathBuf::from(runtime).join("hypr").join(&sig).join(".socket.sock"),
            PathBuf::from("/tmp/hypr").join(&sig).join(".socket.sock"),
        ];
        let socket = candidates.iter().find(|p| p.exists()).unwrap_or(&candidates[0]).clone();
        Some(Hypr { socket })
    }

    async fn request(&self, cmd: &str) -> R<String> {
        let mut s = UnixStream::connect(&self.socket).await
            .map_err(|e| format!("hyprland socket {}: {e}", self.socket.display()))?;
        s.write_all(cmd.as_bytes()).await.map_err(|e| e.to_string())?;
        let mut out = String::new();
        s.read_to_string(&mut out).await.map_err(|e| e.to_string())?;
        Ok(out)
    }

    /// `hyprctl -j <args>`.
    pub async fn json(&self, args: &str) -> R<Json> {
        let raw = self.request(&format!("j/{args}")).await?;
        if raw.trim().is_empty() {
            return Ok(Json::Null);
        }
        serde_json::from_str(&raw).map_err(|e| format!("hyprctl {args}: {e}: {raw}"))
    }

    /// `hyprctl dispatch <args>`.
    pub async fn dispatch(&self, args: &str) -> R<()> {
        let out = self.request(&format!("dispatch {args}")).await?;
        match out.trim() {
            "ok" | "" => Ok(()),
            other => Err(format!("hyprctl dispatch {args}: {other}")),
        }
    }

    /// The focused client, or None.
    pub async fn active_window(&self) -> R<Option<Json>> {
        let aw = self.json("activewindow").await?;
        Ok(aw.get("address").and_then(Json::as_str).filter(|a| !a.is_empty()).map(|_| aw.clone()))
    }
}

/// Collapse whitespace: toolkits and compositors space titles differently
/// (Kate's AT-SPI frame name has two spaces before the dash, its title one).
pub fn norm(title: &str) -> String {
    title.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `wtype` invocations for `text`, with newlines and tabs as real keys.
pub fn wtype_commands(text: &str) -> Vec<Vec<String>> {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut cmds = Vec::new();
    let mut chunk = String::new();
    for ch in text.chars() {
        if ch == '\n' || ch == '\t' {
            if !chunk.is_empty() {
                cmds.push(vec!["--".into(), std::mem::take(&mut chunk)]);
            }
            cmds.push(vec!["-k".into(), if ch == '\n' { "Return" } else { "Tab" }.into()]);
        } else {
            chunk.push(ch);
        }
    }
    if !chunk.is_empty() {
        cmds.push(vec!["--".into(), chunk]);
    }
    cmds
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wtype_splits_newlines_and_tabs() {
        let got = wtype_commands("a b\nc\td\r\n");
        let want: Vec<Vec<&str>> = vec![
            vec!["--", "a b"], vec!["-k", "Return"], vec!["--", "c"], vec!["-k", "Tab"],
            vec!["--", "d"], vec!["-k", "Return"],
        ];
        assert_eq!(got, want.iter().map(|v| v.iter().map(|s| s.to_string()).collect::<Vec<_>>())
                   .collect::<Vec<_>>());
    }

    #[test]
    fn titles_compare_without_spacing() {
        assert_eq!(norm("notes.txt  — Kate"), norm("notes.txt — Kate"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn speaks_the_socket_protocol() {
        let dir = std::env::temp_dir().join(format!("hypr-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".socket.sock");
        let _ = std::fs::remove_file(&path);
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let mut got = Vec::new();
            for _ in 0..3 {
                let (mut s, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 256];
                let n = s.read(&mut buf).await.unwrap();
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let reply = match req.as_str() {
                    "j/activewindow" => r#"{"address":"0x1","pid":42,"title":"t"}"#,
                    "dispatch movecursor 10 20" => "ok",
                    _ => "Invalid dispatcher",
                };
                s.write_all(reply.as_bytes()).await.unwrap();
                got.push(req);
            }
            got
        });
        let h = Hypr { socket: path };
        assert_eq!(h.active_window().await.unwrap().unwrap()["pid"], 42);
        h.dispatch("movecursor 10 20").await.unwrap();
        assert!(h.dispatch("nope").await.is_err());
        assert_eq!(server.await.unwrap(),
                   vec!["j/activewindow", "dispatch movecursor 10 20", "dispatch nope"]);
    }
}
