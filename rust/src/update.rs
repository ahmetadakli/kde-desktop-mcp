//! Whether a newer release is out, for `doctor`: the latest GitHub release
//! of the project, read with `curl` (Linux, and Windows 10 and later, have
//! it) so the server links no HTTP client. Only `doctor` asks, never a tool
//! call; DESKTOP_MCP_NO_UPDATE_CHECK=1 turns it off.

use std::time::Duration;

pub const RELEASES: &str = "https://github.com/ahmetadakli/kde-desktop-mcp/releases";
pub const OFF_ENV: &str = "DESKTOP_MCP_NO_UPDATE_CHECK";
const WAIT: Duration = Duration::from_secs(4);

/// The latest release's version ("0.16.0", without the "v"): where
/// `…/releases/latest` redirects (`…/releases/tag/v0.16.0`). Not the REST
/// API, whose limit for unauthenticated calls (60 an hour per address) and
/// User-Agent rules make it fail now and then.
pub async fn latest() -> Result<String, String> {
    let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
    let run = tokio::process::Command::new("curl")
        .args([
            "-sS",
            "-o",
            null,
            "-w",
            "%{redirect_url}",
            "--max-time",
            "3",
        ])
        .arg(format!("{RELEASES}/latest"))
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(WAIT, run)
        .await
        .map_err(|_| "no answer from GitHub".to_string())?
        .map_err(|e| format!("curl: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "GitHub: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    tag_of(&String::from_utf8_lossy(&out.stdout)).ok_or_else(|| "no release found".into())
}

/// The version in a `…/releases/tag/v0.16.0` URL.
fn tag_of(url: &str) -> Option<String> {
    let tag = url.trim().rsplit_once("/releases/tag/")?.1;
    let v = tag.trim_start_matches('v');
    v.starts_with(|c: char| c.is_ascii_digit())
        .then(|| v.to_string())
}

/// Whether version `b` is newer than `a` ("0.15.0" < "0.16.0" < "1.0.0");
/// parts that are not numbers count as 0.
pub fn newer(a: &str, b: &str) -> bool {
    let parts = |v: &str| -> Vec<u64> {
        v.trim_start_matches('v')
            .split(['.', '-', '+'])
            .take(3)
            .map(|p| p.parse().unwrap_or(0))
            .collect()
    };
    parts(b) > parts(a)
}

/// The doctor line: whether this build is the latest release.
pub async fn line(current: &str) -> String {
    if cfg!(test) {
        return "not checked (tests)".into();
    }
    if std::env::var(OFF_ENV).is_ok_and(|v| v.trim() == "1") {
        return format!("not checked ({OFF_ENV}=1)");
    }
    match latest().await {
        Ok(v) if newer(current, &v) => format!(
            "{v} is out (this is {current}): {RELEASES}/latest; to update, see the README's \"Updating\""
        ),
        Ok(v) => format!("up to date (latest release {v})"),
        Err(e) => format!("could not check the latest release ({e})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_by_number() {
        assert!(newer("0.15.0", "0.16.0"));
        assert!(newer("0.15.0", "v0.15.1"));
        assert!(newer("0.9.9", "0.10.0"));
        assert!(newer("0.15.0", "1.0.0"));
        assert!(!newer("0.15.0", "0.15.0"));
        assert!(!newer("0.16.0", "0.15.3"));
        assert!(!newer("0.15.0", "0.15.0-rc1"));
    }

    #[test]
    fn the_tag_comes_from_the_redirect() {
        assert_eq!(
            tag_of("https://github.com/a/b/releases/tag/v0.16.0\n").as_deref(),
            Some("0.16.0")
        );
        // No release yet: GitHub redirects to the releases page.
        assert_eq!(tag_of("https://github.com/a/b/releases"), None);
        assert_eq!(tag_of(""), None);
    }
}
