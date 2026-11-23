//! A large tree written to a private file instead of the reply.
//!
//! `tree(to_file=true)` puts the whole text, ids and all, in a new file
//! only this user can read, and the reply names it. The ids in the file are
//! the ones the server issued, so they work in later calls like any other.
//!
//! Linux: `$XDG_RUNTIME_DIR/desktop-mcp/trees` when the runtime directory is
//! private (as the XDG spec requires), else `$XDG_CACHE_HOME` or `~/.cache`
//! under `desktop-mcp/trees`; directories 0700, files 0600. Windows:
//! `%LOCALAPPDATA%\desktop-mcp\trees`, which inherits the profile's ACL
//! (the user, SYSTEM and Administrators), else the temp directory.
//!
//! A file is always new (an existing file or symlink at its name makes the
//! write fail, never be written through), and only the newest `KEEP` trees
//! are kept.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Tree files kept in the directory; older ones are removed on each write.
pub const KEEP: usize = 10;
const PREFIX: &str = "tree-";
const SUFFIX: &str = ".txt";

/// Where tree files go, or None without a usable base directory.
pub fn dir() -> Option<PathBuf> {
    let leaf = |base: PathBuf| base.join("desktop-mcp").join("trees");
    let env = |k: &str| {
        std::env::var_os(k)
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
    };
    if cfg!(windows) {
        return Some(leaf(env("LOCALAPPDATA").unwrap_or_else(std::env::temp_dir)));
    }
    if let Some(runtime) = env("XDG_RUNTIME_DIR").filter(|p| private(p).is_ok()) {
        return Some(leaf(runtime));
    }
    env("XDG_CACHE_HOME")
        .or_else(|| env("HOME").map(|h| h.join(".cache")))
        .map(leaf)
}

/// A real directory (not a symlink) no other user can enter.
fn private(path: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_dir() {
        return Err(io::Error::other(format!(
            "{} is not a directory",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o077 != 0 {
            return Err(io::Error::other(format!(
                "{} is open to other users",
                path.display()
            )));
        }
    }
    Ok(())
}

/// Create `dir` and its parent as private directories, or check existing
/// ones.
fn ensure_private(dir: &Path) -> io::Result<()> {
    let parent = dir
        .parent()
        .ok_or_else(|| io::Error::other("tree directory has no parent"))?;
    for path in [parent, dir] {
        match fs::symlink_metadata(path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                if let Some(base) = path.parent() {
                    fs::create_dir_all(base)?;
                }
                #[cfg_attr(not(unix), allow(unused_mut))]
                let mut b = fs::DirBuilder::new();
                #[cfg(unix)]
                std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
                match b.create(path) {
                    // Made by someone else meanwhile: checked below.
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                    r => r?,
                }
            }
            Err(e) => return Err(e),
            Ok(_) => {}
        }
        private(path)?;
    }
    Ok(())
}

/// What `write` put on disk.
pub struct Spilled {
    pub path: PathBuf,
    pub bytes: usize,
    pub lines: usize,
}

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Write `text` to a new private file in `dir` and remove all but the
/// newest `KEEP` tree files there.
pub fn write(dir: &Path, text: &str) -> io::Result<Spilled> {
    ensure_private(dir)?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let path = dir.join(format!(
        "{PREFIX}{nanos}-{}-{seq}{SUFFIX}",
        std::process::id()
    ));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(&path)?;
    let written = (|| {
        // A directory owned by someone else would have the file owned by
        // us inside it: refuse that too.
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if file.metadata()?.uid() != fs::metadata(dir)?.uid() {
                return Err(io::Error::other(format!(
                    "{} belongs to another user",
                    dir.display()
                )));
            }
        }
        file.write_all(text.as_bytes())?;
        file.flush()
    })();
    if let Err(e) = written {
        let _ = fs::remove_file(&path);
        return Err(e);
    }
    prune(dir, KEEP);
    Ok(Spilled {
        path,
        bytes: text.len(),
        lines: text.lines().count(),
    })
}

/// Remove all but the newest `keep` tree files in `dir`. Names start with
/// the creation time, so name order is age order.
fn prune(dir: &Path, keep: usize) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.starts_with(PREFIX) && n.ends_with(SUFFIX))
        .collect();
    // Same-length decimal times sort as numbers; pad for safety.
    names.sort_by_key(|n| {
        let t = n[PREFIX.len()..].split('-').next().unwrap_or("");
        (format!("{t:0>40}"), n.clone())
    });
    let old = names.len().saturating_sub(keep);
    for n in &names[..old] {
        let _ = fs::remove_file(dir.join(n));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("desktop-mcp-spill-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn write_makes_new_private_files_and_keeps_the_newest() {
        let base = scratch("keep");
        let dir = base.join("trees");
        let first = write(&dir, "a\nb\n").unwrap();
        assert_eq!((first.bytes, first.lines), (4, 2));
        assert_eq!(fs::read_to_string(&first.path).unwrap(), "a\nb\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&first.path), 0o600);
            assert_eq!(mode(&dir), 0o700);
            assert_eq!(mode(&base), 0o700);
        }
        let mut paths = vec![first.path];
        for i in 0..KEEP {
            let s = write(&dir, &format!("{i}")).unwrap();
            assert!(!paths.contains(&s.path));
            paths.push(s.path);
        }
        // The oldest went; the newest KEEP stay.
        assert!(!paths[0].exists());
        assert!(paths[1..].iter().all(|p| p.exists()));
        assert_eq!(fs::read_dir(&dir).unwrap().count(), KEEP);
        fs::remove_dir_all(&base).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_open_to_others_or_a_symlink_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let base = scratch("open");
        let dir = base.join("trees");
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&base, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(write(&dir, "x").is_err());
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
        let link = base.join("link");
        std::os::unix::fs::symlink(&dir, &link).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(write(&link, "x").is_err());
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn prune_leaves_other_files_alone() {
        let dir = scratch("other");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("notes.txt"), "keep me").unwrap();
        for t in [5, 30, 100] {
            fs::write(dir.join(format!("{PREFIX}{t}-1-0{SUFFIX}")), "").unwrap();
        }
        prune(&dir, 1);
        let mut left: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left, ["notes.txt", "tree-100-1-0.txt"]);
        fs::remove_dir_all(&dir).unwrap();
    }
}
