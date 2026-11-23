//! The Claude Code plugin package (`.claude-plugin/marketplace.json` and
//! `packaging/claude-plugin/`): its JSON files parse, its versions follow
//! Cargo.toml, and the Linux launcher runs only a verified release build.

use serde_json::Value;
use std::path::{Path, PathBuf};

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

fn json(path: &Path) -> Value {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

#[test]
fn marketplace_and_plugins_match_cargo_version() {
    let repo = repo();
    let market = json(&repo.join(".claude-plugin/marketplace.json"));
    assert!(market["name"].is_string() && market["owner"]["name"].is_string());
    let plugins = market["plugins"].as_array().unwrap();
    let names: Vec<_> = plugins
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["desktop-mcp", "desktop-mcp-windows"]);
    for entry in plugins {
        let name = entry["name"].as_str().unwrap();
        assert_eq!(entry["version"], VERSION, "marketplace entry {name}");
        let source = entry["source"].as_str().unwrap();
        assert!(
            source.starts_with("./") && !source.contains(".."),
            "{source}"
        );
        let dir = repo.join(source);
        let manifest = json(&dir.join(".claude-plugin/plugin.json"));
        assert_eq!(manifest["name"], name, "{source}");
        assert_eq!(manifest["version"], VERSION, "{source}");
        assert_eq!(manifest["description"], entry["description"], "{source}");
        // The server keeps the name `desktop`, as in a manual install.
        let servers = manifest["mcpServers"].as_object().unwrap();
        assert_eq!(servers.keys().collect::<Vec<_>>(), ["desktop"], "{source}");
        let server = &servers["desktop"];
        let args: Vec<_> = server["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a.as_str().unwrap())
            .collect();
        let script = args.last().unwrap();
        let rel = script.strip_prefix("${CLAUDE_PLUGIN_ROOT}/").expect(script);
        let text = std::fs::read_to_string(dir.join(rel)).unwrap();
        let pinned = match server["command"].as_str().unwrap() {
            "sh" => format!("\nversion={VERSION}\n"),
            "powershell" => format!("\n$version = '{VERSION}'\n"),
            other => panic!("{source}: command {other}"),
        };
        assert!(text.contains(&pinned), "{rel} does not pin {VERSION}");
    }
}

#[cfg(unix)]
mod launcher {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::process::{Command, Output};

    const TOOLS: &[&str] = &[
        "sh",
        "uname",
        "curl",
        "awk",
        "sha256sum",
        "cut",
        "mkdir",
        "mktemp",
        "rm",
        "tar",
        "gzip",
        "chmod",
        "mv",
        "touch",
        "find",
    ];

    struct Setup {
        tmp: PathBuf,
        bin: PathBuf,
        data: PathBuf,
        releases: PathBuf,
    }

    fn which(tool: &str) -> PathBuf {
        let path = std::env::var_os("PATH").unwrap();
        std::env::split_paths(&path)
            .map(|d| d.join(tool))
            .find(|p| p.is_file())
            .unwrap_or_else(|| panic!("{tool} not found on PATH"))
    }

    fn script(path: &Path, body: &str) {
        std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// A PATH holding only the tools the launcher uses (so no installed
    /// desktop-mcp), and a release whose binary is a script that echoes.
    fn setup(test: &str) -> Setup {
        let tmp =
            std::env::temp_dir().join(format!("desktop-mcp-plugin-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let bin = tmp.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        for tool in TOOLS {
            std::os::unix::fs::symlink(which(tool), bin.join(tool)).unwrap();
        }
        let name = format!("desktop-mcp-v{VERSION}-x86_64-linux");
        let stage = tmp.join("stage");
        std::fs::create_dir_all(stage.join(&name)).unwrap();
        script(
            &stage.join(&name).join("desktop-mcp"),
            "echo \"release $*\"",
        );
        let releases = tmp.join("releases");
        let assets = releases.join(format!("download/v{VERSION}"));
        std::fs::create_dir_all(&assets).unwrap();
        let archive = assets.join(format!("{name}.tar.gz"));
        let status = Command::new("tar")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(&stage)
            .arg(&name)
            .status()
            .unwrap();
        assert!(status.success());
        let sums = Command::new("sha256sum")
            .arg(format!("{name}.tar.gz"))
            .current_dir(&assets)
            .output()
            .unwrap();
        assert!(sums.status.success());
        std::fs::write(assets.join("SHA256SUMS"), sums.stdout).unwrap();
        Setup {
            data: tmp.join("data"),
            tmp,
            bin,
            releases,
        }
    }

    fn run(s: &Setup) -> Output {
        let launcher = repo().join("packaging/claude-plugin/desktop-mcp/scripts/desktop-mcp.sh");
        Command::new(which("sh"))
            .arg(launcher)
            .arg("--version")
            .env_clear()
            .env("PATH", &s.bin)
            .env("HOME", &s.tmp)
            .env("CLAUDE_PLUGIN_DATA", &s.data)
            .env(
                "DESKTOP_MCP_RELEASES_URL",
                format!("file://{}", s.releases.display()),
            )
            .output()
            .unwrap()
    }

    fn cached(s: &Setup) -> PathBuf {
        s.data.join(format!("v{VERSION}/desktop-mcp"))
    }

    #[test]
    fn downloads_verifies_caches_and_runs_the_release() {
        let s = setup("ok");
        let out = run(&s);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&out.stdout), "release --version\n");
        assert!(cached(&s).is_file());
        // The cached copy runs without the release.
        std::fs::remove_dir_all(&s.releases).unwrap();
        let out = run(&s);
        assert_eq!(String::from_utf8_lossy(&out.stdout), "release --version\n");
        assert_eq!(
            String::from_utf8_lossy(&out.stderr),
            "",
            "no download the second time"
        );
        std::fs::remove_dir_all(&s.tmp).unwrap();
    }

    #[test]
    fn a_checksum_mismatch_runs_nothing() {
        let s = setup("mismatch");
        let sums = s.releases.join(format!("download/v{VERSION}/SHA256SUMS"));
        let text = std::fs::read_to_string(&sums).unwrap();
        let flipped = if text.starts_with('0') { "1" } else { "0" };
        std::fs::write(&sums, format!("{flipped}{}", &text[1..])).unwrap();
        let out = run(&s);
        assert!(!out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout), "");
        assert!(String::from_utf8_lossy(&out.stderr).contains("checksum mismatch"));
        assert!(!cached(&s).exists());
        // No staging directory is left behind.
        assert_eq!(std::fs::read_dir(&s.data).unwrap().count(), 0);
        std::fs::remove_dir_all(&s.tmp).unwrap();
    }

    #[test]
    fn a_missing_checksum_entry_runs_nothing() {
        let s = setup("noentry");
        let sums = s.releases.join(format!("download/v{VERSION}/SHA256SUMS"));
        std::fs::write(&sums, format!("{}  other.tar.gz\n", "0".repeat(64))).unwrap();
        let out = run(&s);
        assert!(!out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout), "");
        assert!(!cached(&s).exists());
        std::fs::remove_dir_all(&s.tmp).unwrap();
    }

    #[test]
    fn a_desktop_mcp_on_path_wins() {
        let s = setup("path");
        script(&s.bin.join("desktop-mcp"), "echo \"installed $*\"");
        std::fs::remove_dir_all(&s.releases).unwrap();
        let out = run(&s);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "installed --version\n"
        );
        assert!(!s.data.exists(), "nothing downloaded");
        std::fs::remove_dir_all(&s.tmp).unwrap();
    }
}
