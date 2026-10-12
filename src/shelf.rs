//! The shelf on disk: a folder of symlinks, one per held file or folder. holdthis4me never
//! copies anything. Removing an item deletes only its link, never the file it points to.
//!
//! Hardening, because a bug here could cost real files:
//! - Every change goes through a handle on the shelf folder itself, opened without
//!   following symlinks and only if it's a real directory owned by this user and not
//!   writable by anyone else. A shelf path swapped for a symlink to some other folder is
//!   refused, so "Clear" can never sweep another folder's links.
//! - Removing first moves the entry aside under a private name, then checks what it
//!   actually moved: only a symlink is deleted, anything else is put back. Nothing can
//!   be swapped in between the check and the delete.
//! - Nothing here reads the held files themselves (no stat of targets), so a held file on
//!   a dead network mount can't hang anything.

use std::collections::HashSet;
use std::ffi::{CString, OsStr};
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::SystemTime;

/// Entries named like this are mid-removal (see `remove`); never listed as items.
const STAGING: &str = ".ht4m-removing-";
/// Longest file name Linux filesystems take, in bytes.
const NAME_MAX: usize = 255;

pub struct Item {
    /// The symlink inside the shelf folder.
    pub link: PathBuf,
    /// The real file or folder. Drags hand this out, never the link, because a dropped
    /// link would make Nautilus create another link instead of copying the file.
    pub target: PathBuf,
}

/// `~/.local/share/holdthis4me/shelf`.
pub fn default_dir() -> PathBuf {
    gtk::glib::user_data_dir().join("holdthis4me").join("shelf")
}

/// Create the shelf folder if needed, private to this user (0700), and the folder it's in.
pub fn prepare(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    for d in [Some(dir), dir.parent()].into_iter().flatten() {
        let meta = fs::symlink_metadata(d)?;
        if meta.is_dir() && meta.permissions().mode() & 0o077 != 0 {
            fs::set_permissions(d, fs::Permissions::from_mode(0o700))?;
        }
    }
    let fd = open_dir(dir)?;
    // A removal interrupted half-way leaves its link under a staging name: finish it.
    // Only symlinks are deleted; anything else stays.
    for entry in fs::read_dir(dir)?.flatten() {
        if entry.file_name().as_bytes().starts_with(STAGING.as_bytes()) {
            let name = cstr(&entry.file_name())?;
            if is_symlink_at(&fd, &name)? {
                check(unsafe { libc::unlinkat(fd.as_raw_fd(), name.as_ptr(), 0) })?;
            }
        }
    }
    Ok(())
}

/// Ok if the shelf folder is one this code will add to and remove from (see `open_dir`).
pub fn usable(dir: &Path) -> io::Result<()> {
    open_dir(dir).map(drop)
}

fn cstr(s: &OsStr) -> io::Result<CString> {
    CString::new(s.as_bytes()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in name"))
}

fn check(ret: libc::c_int) -> io::Result<()> {
    if ret < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

/// The shelf folder itself, only if it's a real directory (not a symlink to one), owned by
/// us, and not writable by group or others.
fn open_dir(dir: &Path) -> io::Result<OwnedFd> {
    let path = cstr(dir.as_os_str())?;
    // SAFETY: plain syscalls on a valid C string; the fd is owned from here on.
    let fd = unsafe {
        libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
    };
    check(fd)?;
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    check(unsafe { libc::fstat(fd.as_raw_fd(), &mut st) })?;
    if st.st_uid != unsafe { libc::getuid() } || st.st_mode & 0o022 != 0 {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "shelf folder isn't private to this user"));
    }
    Ok(fd)
}

fn is_symlink_at(fd: &OwnedFd, name: &CString) -> io::Result<bool> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    check(unsafe { libc::fstatat(fd.as_raw_fd(), name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) })?;
    Ok(st.st_mode & libc::S_IFMT == libc::S_IFLNK)
}

/// Rename within the folder, never replacing anything already at `to`.
fn rename_at(fd: &OwnedFd, from: &CString, to: &CString) -> io::Result<()> {
    let raw = fd.as_raw_fd();
    check(unsafe { libc::renameat2(raw, from.as_ptr(), raw, to.as_ptr(), libc::RENAME_NOREPLACE) })
}

/// Held items, newest first. Anything in the folder that isn't a symlink is ignored, and
/// so is everything if the folder isn't the real, private shelf folder.
pub fn list(dir: &Path) -> Vec<Item> {
    if open_dir(dir).is_err() {
        return Vec::new();
    }
    let Ok(entries) = fs::read_dir(dir) else { return Vec::new() };
    let mut items: Vec<(SystemTime, Item)> = entries
        .flatten()
        .filter(|e| !e.file_name().as_bytes().starts_with(STAGING.as_bytes()))
        .filter_map(|e| {
            let link = e.path();
            let meta = fs::symlink_metadata(&link).ok()?;
            if !meta.file_type().is_symlink() {
                return None;
            }
            let target = resolve(&link)?;
            let added = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            Some((added, Item { link, target }))
        })
        .collect();
    items.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.link.cmp(&b.1.link)));
    items.into_iter().map(|(_, i)| i).collect()
}

/// Where a shelf link points, made absolute (a link edited by hand may be relative).
fn resolve(link: &Path) -> Option<PathBuf> {
    let t = fs::read_link(link).ok()?;
    Some(if t.is_absolute() { t } else { link.parent()?.join(t) })
}

/// What to actually hold for `target`, or None if it's the shelf itself or one of its
/// links. A path that goes through the shelf (a file inside a held folder, reached via
/// its link) is resolved to the real location, so no link ever points back into the shelf.
fn real_target(dir: &Path, target: &Path) -> io::Result<Option<PathBuf>> {
    if !target.is_absolute() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "path must be absolute"));
    }
    if target == dir || target.parent() == Some(dir) {
        return Ok(None);
    }
    let target = if target.starts_with(dir) { fs::canonicalize(target)? } else { target.to_owned() };
    let real_dir = fs::canonicalize(dir).unwrap_or_else(|_| dir.to_owned());
    if target.starts_with(dir) || target.starts_with(&real_dir) {
        return Ok(None);
    }
    Ok(Some(target))
}

/// Hold each of `targets`. Per target: the new link, None if it's already held (or is the
/// shelf itself), or the error. The shelf is read once, however many are added.
pub fn add_all(dir: &Path, targets: &[PathBuf]) -> Vec<io::Result<Option<PathBuf>>> {
    let fd = match open_dir(dir) {
        Ok(fd) => fd,
        Err(e) => return targets.iter().map(|_| Err(io::Error::new(e.kind(), e.to_string()))).collect(),
    };
    let mut held: HashSet<PathBuf> = list(dir).into_iter().map(|i| i.target).collect();
    targets
        .iter()
        .map(|t| {
            let Some(target) = real_target(dir, t)? else { return Ok(None) };
            if !held.insert(target.clone()) {
                return Ok(None);
            }
            let link = add_link(&fd, dir, &target)?;
            Ok(Some(link))
        })
        .collect()
}

/// Hold one target (see `add_all`).
#[cfg(test)]
pub fn add(dir: &Path, target: &Path) -> io::Result<Option<PathBuf>> {
    add_all(dir, &[target.to_owned()]).pop().expect("one result")
}

fn add_link(fd: &OwnedFd, dir: &Path, target: &Path) -> io::Result<PathBuf> {
    let name = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "root".into());
    let target_c = cstr(target.as_os_str())?;
    for n in 1..10_000 {
        let candidate = numbered(&name, n);
        let name_c = cstr(OsStr::new(&candidate))?;
        match check(unsafe { libc::symlinkat(target_c.as_ptr(), fd.as_raw_fd(), name_c.as_ptr()) }) {
            Ok(()) => return Ok(dir.join(candidate)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(io::ErrorKind::AlreadyExists, "too many items with this name"))
}

/// `report.pdf`, then `report (2).pdf`, `report (3).pdf`... Folders and dotfiles have no
/// extension to keep (`.bashrc (2)`, not ` (2).bashrc`). Never longer than NAME_MAX bytes:
/// a long name is shortened before the number. Never a staging name.
fn numbered(name: &str, n: u32) -> String {
    let (stem, ext) = match name.rfind('.') {
        Some(dot) if dot > 0 && name.len() - dot <= 32 => (&name[..dot], &name[dot..]),
        _ => (name, ""),
    };
    let tag = if n == 1 { String::new() } else { format!(" ({n})") };
    let room = NAME_MAX.saturating_sub(tag.len() + ext.len());
    let mut cut = stem.len().min(room);
    while !stem.is_char_boundary(cut) {
        cut -= 1;
    }
    let out = format!("{}{tag}{ext}", &stem[..cut]);
    if out.starts_with(STAGING) { format!("_{out}") } else { out }
}

/// Stop holding an item: delete its link, never anything else. `link` must be directly
/// inside the shelf folder. The entry is first moved aside under a private name and only
/// deleted if what was moved is a symlink; anything else goes back where it was.
pub fn remove(dir: &Path, link: &Path) -> io::Result<()> {
    let not_link = || io::Error::new(io::ErrorKind::InvalidInput, "not a shelf link");
    if link.parent() != Some(dir) {
        return Err(not_link());
    }
    let name = cstr(link.file_name().ok_or_else(not_link)?)?;
    let fd = open_dir(dir)?;
    if !is_symlink_at(&fd, &name)? {
        return Err(not_link());
    }
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let staged = format!("{STAGING}{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed));
    let staged = cstr(OsStr::new(&staged))?;
    rename_at(&fd, &name, &staged)?;
    if is_symlink_at(&fd, &staged)? {
        return check(unsafe { libc::unlinkat(fd.as_raw_fd(), staged.as_ptr(), 0) });
    }
    // Something else was swapped in after the first check: put it back untouched.
    rename_at(&fd, &staged, &name)?;
    Err(not_link())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("holdthis4me-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn shelf_in(root: &Path) -> PathBuf {
        let shelf = root.join("shelf");
        prepare(&shelf).unwrap();
        shelf
    }

    #[test]
    fn removing_an_item_never_touches_the_file() {
        let root = tmp("remove");
        let shelf = shelf_in(&root);
        let file = root.join("keep.txt");
        fs::write(&file, "hello").unwrap();
        let dirpath = root.join("folder");
        fs::create_dir(&dirpath).unwrap();
        fs::write(dirpath.join("inside.txt"), "x").unwrap();

        let a = add(&shelf, &file).unwrap().unwrap();
        let b = add(&shelf, &dirpath).unwrap().unwrap();
        remove(&shelf, &a).unwrap();
        remove(&shelf, &b).unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "hello");
        assert!(dirpath.join("inside.txt").exists());
        assert!(list(&shelf).is_empty());
        assert_eq!(fs::read_dir(&shelf).unwrap().count(), 0, "no staging leftovers");

        // Never deletes a real file or folder, even one placed inside the shelf folder.
        let stray = shelf.join("stray.txt");
        fs::write(&stray, "x").unwrap();
        let stray_dir = shelf.join("stray-dir");
        fs::create_dir(&stray_dir).unwrap();
        assert!(remove(&shelf, &stray).is_err());
        assert!(remove(&shelf, &stray_dir).is_err());
        assert!(remove(&shelf, &file).is_err());
        assert!(remove(&shelf, &shelf.join("../keep.txt")).is_err());
        assert!(stray.exists() && stray_dir.exists() && file.exists());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_shelf_swapped_for_a_symlink_is_refused() {
        let root = tmp("swap");
        let elsewhere = root.join("Documents");
        fs::create_dir(&elsewhere).unwrap();
        fs::write(root.join("real.txt"), "x").unwrap();
        let users_link = elsewhere.join("my-link");
        std::os::unix::fs::symlink(root.join("real.txt"), &users_link).unwrap();
        // The shelf path is now a symlink to the user's own folder.
        let shelf = root.join("shelf");
        std::os::unix::fs::symlink(&elsewhere, &shelf).unwrap();

        assert!(list(&shelf).is_empty(), "lists nothing from the other folder");
        assert!(remove(&shelf, &shelf.join("my-link")).is_err());
        assert!(add(&shelf, &root.join("real.txt")).is_err());
        assert!(fs::symlink_metadata(&users_link).unwrap().file_type().is_symlink());
        assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 1);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn interrupted_removals_are_finished_but_only_for_links() {
        let root = tmp("staging");
        let shelf = shelf_in(&root);
        fs::write(root.join("real.txt"), "x").unwrap();
        std::os::unix::fs::symlink(root.join("real.txt"), shelf.join(".ht4m-removing-1-0")).unwrap();
        fs::write(shelf.join(".ht4m-removing-1-1"), "not a link").unwrap();
        assert!(list(&shelf).is_empty(), "staging entries are never items");
        prepare(&shelf).unwrap();
        assert!(fs::symlink_metadata(shelf.join(".ht4m-removing-1-0")).is_err());
        assert!(shelf.join(".ht4m-removing-1-1").exists());
        assert!(root.join("real.txt").exists());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_shelf_others_can_write_to_is_refused() {
        let root = tmp("perms");
        let shelf = shelf_in(&root);
        fs::set_permissions(&shelf, fs::Permissions::from_mode(0o777)).unwrap();
        fs::write(root.join("a.txt"), "x").unwrap();
        assert!(add(&shelf, &root.join("a.txt")).is_err());
        prepare(&shelf).unwrap(); // makes it private again
        assert_eq!(fs::metadata(&shelf).unwrap().permissions().mode() & 0o777, 0o700);
        assert!(add(&shelf, &root.join("a.txt")).unwrap().is_some());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn same_names_get_numbered_and_duplicates_are_skipped() {
        let root = tmp("names");
        let shelf = shelf_in(&root);
        for d in ["a", "b", "c"] {
            fs::create_dir(root.join(d)).unwrap();
            fs::write(root.join(d).join("report.pdf"), d).unwrap();
        }
        let l1 = add(&shelf, &root.join("a/report.pdf")).unwrap().unwrap();
        let l2 = add(&shelf, &root.join("b/report.pdf")).unwrap().unwrap();
        let l3 = add(&shelf, &root.join("c/report.pdf")).unwrap().unwrap();
        assert_eq!(l1.file_name().unwrap(), "report.pdf");
        assert_eq!(l2.file_name().unwrap(), "report (2).pdf");
        assert_eq!(l3.file_name().unwrap(), "report (3).pdf");
        assert!(add(&shelf, &root.join("b/report.pdf")).unwrap().is_none());
        assert!(add(&shelf, &l1).unwrap().is_none());
        assert!(add(&shelf, Path::new("relative.txt")).is_err());
        let held: Vec<_> = list(&shelf).into_iter().map(|i| i.target).collect();
        assert_eq!(held.len(), 3);
        assert!(held.contains(&root.join("c/report.pdf")));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn nothing_points_back_into_the_shelf() {
        let root = tmp("loops");
        let shelf = shelf_in(&root);
        let folder = root.join("Project");
        fs::create_dir(&folder).unwrap();
        fs::write(folder.join("inside.txt"), "x").unwrap();
        add(&shelf, &folder).unwrap().unwrap();

        // The shelf itself: refused.
        assert!(add(&shelf, &shelf).unwrap().is_none());
        // A file reached through the held folder's link: held at its real location.
        let via_link = shelf.join("Project").join("inside.txt");
        let link = add(&shelf, &via_link).unwrap().unwrap();
        assert_eq!(fs::read_link(&link).unwrap(), fs::canonicalize(folder.join("inside.txt")).unwrap());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn bulk_adds_and_missing_targets() {
        let root = tmp("bulk");
        let shelf = shelf_in(&root);
        let files: Vec<PathBuf> = (0..300).map(|i| root.join(format!("f{i}.txt"))).collect();
        for f in &files {
            fs::write(f, "x").unwrap();
        }
        let mut twice = files.clone();
        twice.extend(files.iter().take(10).cloned());
        let results = add_all(&shelf, &twice);
        assert_eq!(results.iter().filter(|r| matches!(r, Ok(Some(_)))).count(), 300);
        assert_eq!(results.iter().filter(|r| matches!(r, Ok(None))).count(), 10);

        // A held file that's gone is still listed (the UI shows it as missing).
        fs::remove_file(&files[0]).unwrap();
        let items = list(&shelf);
        assert_eq!(items.len(), 300);
        assert!(items.iter().any(|i| i.target == files[0] && !i.target.exists()));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn numbering_keeps_extensions_and_length_limits() {
        assert_eq!(numbered("a.tar.gz", 2), "a.tar (2).gz");
        assert_eq!(numbered(".bashrc", 2), ".bashrc (2)");
        assert_eq!(numbered("Photos", 3), "Photos (3)");
        let long = format!("{}.pdf", "é".repeat(126)); // 252 + 4 bytes: over the limit once numbered
        for n in [1, 2, 9999] {
            let out = numbered(&long, n);
            assert!(out.len() <= NAME_MAX, "{n}: {} bytes", out.len());
            assert!(out.ends_with(".pdf"));
        }
        assert!(!numbered(".ht4m-removing-1-2", 1).starts_with(STAGING));
    }
}
