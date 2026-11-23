//! Files only this user can read: a large tree, a screenshot.
//!
//! `tree(to_file=true)` puts the whole text, ids and all, in a new file
//! only this user can read, and the reply names it. The ids in the file are
//! the ones the server issued, so they work in later calls like any other.
//! A screenshot is captured into such a file too, and `inline=false` keeps
//! it; `save_to` copies the image to a new file the caller names.
//!
//! Linux: `$XDG_RUNTIME_DIR/desktop-mcp/<kind>` when the runtime directory
//! is private (as the XDG spec requires), else `$XDG_CACHE_HOME` or
//! `~/.cache` under `desktop-mcp/<kind>`; directories 0700, files 0600.
//! Windows: `%LOCALAPPDATA%\desktop-mcp\<kind>`, which inherits the
//! profile's ACL (the user, SYSTEM and Administrators), else the temp
//! directory.
//!
//! A file is always new (an existing file or symlink at its name makes the
//! write fail, never be written through), and only the newest `keep` files
//! of a kind are kept.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// A kind of private file: its directory, name and how many are kept
/// (older ones are removed whenever a new one is made).
pub struct Kind {
    leaf: &'static str,
    prefix: &'static str,
    suffix: &'static str,
    pub keep: usize,
}

pub const TREES: Kind = Kind {
    leaf: "trees",
    prefix: "tree-",
    suffix: ".txt",
    keep: 10,
};

pub const SHOTS: Kind = Kind {
    leaf: "shots",
    prefix: "shot-",
    suffix: ".png",
    keep: 20,
};

/// Where files of `kind` go, or None without a usable base directory.
pub fn dir(kind: &Kind) -> Option<PathBuf> {
    let leaf = |base: PathBuf| base.join("desktop-mcp").join(kind.leaf);
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

#[cfg(unix)]
unsafe extern "C" {
    fn geteuid() -> u32;
}

/// The user this process runs as.
#[cfg(unix)]
fn me() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { geteuid() }
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
        .ok_or_else(|| io::Error::other("the directory has no parent"))?;
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

/// A new, empty private file of `kind` in `dir` (made private first if
/// need be), with a name no other call gets; all but the newest
/// `kind.keep` files of that kind there are removed.
pub fn create(dir: &Path, kind: &Kind) -> io::Result<(PathBuf, fs::File)> {
    ensure_private(dir)?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let path = dir.join(format!(
        "{}{nanos}-{}-{seq}{}",
        kind.prefix,
        std::process::id(),
        kind.suffix
    ));
    let file = create_new(&path)?;
    // A directory owned by someone else would have the file owned by us
    // inside it: refuse that too.
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if fs::metadata(dir)?.uid() != me() {
            let _ = fs::remove_file(&path);
            return Err(io::Error::other(format!(
                "{} belongs to another user",
                dir.display()
            )));
        }
    }
    // The new file counts: `keep` in all.
    prune(dir, kind, kind.keep);
    Ok((path, file))
}

/// Create `path`, which must not exist (not even as a dangling symlink),
/// readable only by this user.
fn create_new(path: &Path) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)
}

/// Write `text` to a new private tree file in `dir`.
pub fn write(dir: &Path, text: &str) -> io::Result<Spilled> {
    let (path, mut file) = create(dir, &TREES)?;
    if let Err(e) = file.write_all(text.as_bytes()).and_then(|()| file.flush()) {
        let _ = fs::remove_file(&path);
        return Err(e);
    }
    Ok(Spilled {
        path,
        bytes: text.len(),
        lines: text.lines().count(),
    })
}

/// Check a path a caller asked an image to be saved at: absolute, a `.png`
/// file that does not exist yet, in an existing directory of this user's
/// inside their home folder that no one else can write to. Returns the
/// path to write, its directory resolved.
pub fn check_save_path(path: &Path, home: Option<&Path>) -> Result<PathBuf, String> {
    let raw = path.as_os_str().to_string_lossy();
    if !path.is_absolute() {
        return Err(format!("save_to must be an absolute path, got {raw}"));
    }
    let name = match path.file_name() {
        // file_name() drops a trailing "/" or "/.", which a write would not.
        Some(n) if raw.ends_with(&*n.to_string_lossy()) => n,
        _ => return Err(format!("save_to must end with a file name, got {raw}")),
    };
    if !Path::new(name)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("png"))
    {
        return Err(format!("save_to must name a .png file, got {raw}"));
    }
    let parent = path.parent().unwrap_or(Path::new("/"));
    let dir = fs::canonicalize(parent)
        .map_err(|e| format!("save_to: directory {} : {e}", parent.display()))?;
    let home = home
        .and_then(|h| fs::canonicalize(h).ok())
        .ok_or("save_to: no home folder to save in")?;
    if !dir.starts_with(&home) {
        return Err(format!(
            "save_to must be inside the home folder {}, got {raw}",
            home.display()
        ));
    }
    let meta = fs::metadata(&dir).map_err(|e| format!("save_to: {}: {e}", dir.display()))?;
    if !meta.is_dir() {
        return Err(format!("save_to: {} is not a directory", dir.display()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.uid() != me() || meta.mode() & 0o022 != 0 {
            return Err(format!(
                "save_to: {} must be the user's own directory, not writable by others",
                dir.display()
            ));
        }
    }
    let path = dir.join(name);
    if fs::symlink_metadata(&path).is_ok() {
        return Err(format!(
            "save_to: {} already exists; a new file is made, none replaced",
            path.display()
        ));
    }
    Ok(path)
}

/// Write `bytes` to `path` (checked by `check_save_path`) as a new file
/// readable only by this user. Something made at `path` meanwhile, a
/// symlink included, makes this fail instead of being written through.
pub fn save(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = create_new(path)?;
    if let Err(e) = file.write_all(bytes).and_then(|()| file.flush()) {
        drop(file);
        let _ = fs::remove_file(path);
        return Err(e);
    }
    Ok(())
}

/// Remove all but the newest `keep` files of `kind` in `dir`. Names start
/// with the creation time, so name order is age order.
fn prune(dir: &Path, kind: &Kind, keep: usize) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.starts_with(kind.prefix) && n.ends_with(kind.suffix))
        .collect();
    // Same-length decimal times sort as numbers; pad for safety.
    names.sort_by_key(|n| {
        let t = n[kind.prefix.len()..].split('-').next().unwrap_or("");
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
        for i in 0..TREES.keep {
            let s = write(&dir, &format!("{i}")).unwrap();
            assert!(!paths.contains(&s.path));
            paths.push(s.path);
        }
        // The oldest went; the newest `keep` stay.
        assert!(!paths[0].exists());
        assert!(paths[1..].iter().all(|p| p.exists()));
        assert_eq!(fs::read_dir(&dir).unwrap().count(), TREES.keep);
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
            fs::write(dir.join(format!("tree-{t}-1-0.txt")), "").unwrap();
        }
        fs::write(dir.join("shot-1-1-0.png"), "").unwrap();
        prune(&dir, &TREES, 1);
        let mut left: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left, ["notes.txt", "shot-1-1-0.png", "tree-100-1-0.txt"]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn shot_files_are_new_private_and_pruned_apart_from_trees() {
        let base = scratch("shots");
        let dir = base.join("shots");
        let tree = write(&dir, "t").unwrap();
        let mut paths = Vec::new();
        for _ in 0..SHOTS.keep + 3 {
            let (path, file) = create(&dir, &SHOTS).unwrap();
            drop(file);
            assert!(path.extension().is_some_and(|e| e == "png"));
            assert!(!paths.contains(&path));
            paths.push(path);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(paths.last().unwrap()), 0o600);
        }
        assert!(paths[..3].iter().all(|p| !p.exists()));
        assert!(paths[3..].iter().all(|p| p.exists()));
        // Shots never prune trees.
        assert!(tree.path.exists());
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn save_to_takes_only_a_new_png_in_the_users_own_directory() {
        let home = scratch("save");
        let pics = home.join("pics");
        fs::create_dir_all(&pics).unwrap();
        let check = |p: &Path| check_save_path(p, Some(&home));
        let err = |p: &Path, want: &str| {
            let e = check(p).unwrap_err();
            assert!(e.contains(want), "{e}");
        };
        let ok = pics.join("a.PNG");
        assert_eq!(
            check(&ok).unwrap(),
            fs::canonicalize(&pics).unwrap().join("a.PNG")
        );
        err(Path::new("pics/a.png"), "absolute");
        err(&pics.join("a.jpg"), ".png");
        err(&pics.join("missing").join("a.png"), "missing");
        let outside = scratch("save-outside");
        fs::create_dir_all(&outside).unwrap();
        err(&outside.join("a.png"), "inside the home folder");
        // Through a symlink the real directory counts.
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, pics.join("out")).unwrap();
            err(&pics.join("out").join("a.png"), "inside the home folder");
        }
        let path = check(&ok).unwrap();
        save(&path, b"png").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"png");
        err(&ok, "already exists");
        // Written meanwhile: save fails and leaves it alone.
        assert!(save(&path, b"other").is_err());
        assert_eq!(fs::read(&path).unwrap(), b"png");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            // A dangling symlink is not written through.
            let target = outside.join("target.png");
            let link = pics.join("link.png");
            std::os::unix::fs::symlink(&target, &link).unwrap();
            err(&link, "already exists");
            assert!(save(&link, b"x").is_err());
            assert!(!target.exists());
            // A directory others can write to is refused.
            fs::set_permissions(&pics, fs::Permissions::from_mode(0o777)).unwrap();
            err(&pics.join("b.png"), "not writable by others");
            fs::set_permissions(&pics, fs::Permissions::from_mode(0o755)).unwrap();
        }
        fs::remove_dir_all(&home).unwrap();
        fs::remove_dir_all(&outside).unwrap();
    }
}
