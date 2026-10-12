//! The shelf on disk: a folder of symlinks, one per held file or folder. holdthis4me never
//! copies anything. Removing an item deletes only its link, never the file it points to.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub struct Item {
    /// The symlink inside the shelf folder.
    pub link: PathBuf,
    /// The real file or folder. Drags hand this out, never the link, because a dropped
    /// link would make Nautilus create another link instead of copying the file.
    pub target: PathBuf,
    pub exists: bool,
}

/// `~/.local/share/holdthis4me/shelf`.
pub fn default_dir() -> PathBuf {
    gtk::glib::user_data_dir().join("holdthis4me").join("shelf")
}

/// Held items, newest first. Anything in the folder that isn't a symlink is ignored.
pub fn list(dir: &Path) -> Vec<Item> {
    let Ok(entries) = fs::read_dir(dir) else { return Vec::new() };
    let mut items: Vec<(SystemTime, Item)> = entries
        .flatten()
        .filter_map(|e| {
            let link = e.path();
            let meta = fs::symlink_metadata(&link).ok()?;
            if !meta.file_type().is_symlink() {
                return None;
            }
            let target = resolve(&link)?;
            let added = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            let exists = target.exists();
            Some((added, Item { link, target, exists }))
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

/// Hold `target`. Returns the new link, or None if it's already held or is itself one of
/// the shelf's links (dragging an item back onto the shelf).
pub fn add(dir: &Path, target: &Path) -> io::Result<Option<PathBuf>> {
    if !target.is_absolute() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "path must be absolute"));
    }
    if target.parent() == Some(dir) {
        return Ok(None);
    }
    if list(dir).iter().any(|i| i.target == target) {
        return Ok(None);
    }
    fs::create_dir_all(dir)?;
    let name = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "root".into());
    for n in 1.. {
        let link = dir.join(numbered(&name, n));
        match std::os::unix::fs::symlink(target, &link) {
            Ok(()) => return Ok(Some(link)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    unreachable!()
}

/// `report.pdf`, then `report (2).pdf`, `report (3).pdf`... Folders and dotfiles have no
/// extension to keep (`.bashrc (2)`, not ` (2).bashrc`).
fn numbered(name: &str, n: u32) -> String {
    if n == 1 {
        return name.to_owned();
    }
    match name.rfind('.') {
        Some(dot) if dot > 0 => format!("{} ({n}){}", &name[..dot], &name[dot..]),
        _ => format!("{name} ({n})"),
    }
}

/// Stop holding an item: delete its link. Refuses anything that isn't a symlink directly
/// inside the shelf folder, so a bug elsewhere can never delete a real file.
pub fn remove(dir: &Path, link: &Path) -> io::Result<()> {
    if link.parent() != Some(dir) || !fs::symlink_metadata(link)?.file_type().is_symlink() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a shelf link"));
    }
    fs::remove_file(link)
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

    #[test]
    fn removing_an_item_never_touches_the_file() {
        let root = tmp("remove");
        let shelf = root.join("shelf");
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

        // Never deletes a real file, even one placed inside the shelf folder.
        let stray = shelf.join("stray.txt");
        fs::write(&stray, "x").unwrap();
        assert!(remove(&shelf, &stray).is_err());
        assert!(remove(&shelf, &file).is_err());
        assert!(stray.exists() && file.exists());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn same_names_get_numbered_and_duplicates_are_skipped() {
        let root = tmp("names");
        let shelf = root.join("shelf");
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
    fn missing_targets_are_listed_as_missing() {
        let root = tmp("missing");
        let shelf = root.join("shelf");
        let file = root.join("gone.txt");
        fs::write(&file, "x").unwrap();
        add(&shelf, &file).unwrap();
        fs::remove_file(&file).unwrap();
        let items = list(&shelf);
        assert_eq!(items.len(), 1);
        assert!(!items[0].exists);
        assert_eq!(items[0].target, file);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn numbering_keeps_extensions() {
        assert_eq!(numbered("a.tar.gz", 2), "a.tar (2).gz");
        assert_eq!(numbered(".bashrc", 2), ".bashrc (2)");
        assert_eq!(numbered("Photos", 3), "Photos (3)");
    }
}
