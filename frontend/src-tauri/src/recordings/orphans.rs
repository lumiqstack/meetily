use super::folder::{removable_bytes, FolderIndex, FolderKey};
use super::verdict::{judge_orphan, Activity, HoldReason};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub(crate) struct Orphan {
    pub folder: PathBuf,
    pub bytes: u64,
    pub newest_change: SystemTime,
    pub hold: Option<HoldReason>,
}

/// Newest mtime of the folder and its direct entries, not following links.
fn newest_change(folder: &Path) -> SystemTime {
    let mtime = |path: &Path| std::fs::symlink_metadata(path).and_then(|m| m.modified()).ok();
    let mut newest = mtime(folder).unwrap_or(SystemTime::UNIX_EPOCH);
    if let Ok(entries) = std::fs::read_dir(folder) {
        for entry in entries.flatten() {
            if let Some(t) = mtime(&entry.path()) {
                newest = newest.max(t);
            }
        }
    }
    newest
}

/// Folders that look like meetings and that no meeting row accounts for.
/// Only direct children of the recordings roots are considered.
pub(crate) fn find_orphans(index: &FolderIndex, activity: &Activity, now: SystemTime) -> Vec<Orphan> {
    let mut seen = Vec::<FolderKey>::new();
    let mut orphans = Vec::new();
    for root in index.recordings_roots() {
        let Ok(entries) = std::fs::read_dir(&root) else { continue };
        for entry in entries.flatten() {
            // Does not follow links, so a symlink or junction is never a candidate.
            let Ok(file_type) = entry.file_type() else { continue };
            if !file_type.is_dir() {
                continue;
            }
            let folder = entry.path();
            let key = FolderKey::of(&folder);
            if seen.contains(&key)
                || index.is_root_or_above(&key)
                || index.touches(&key)
                || !crate::storage::migrate::is_meeting_folder(&folder)
            {
                continue;
            }
            seen.push(key.clone());
            let changed = newest_change(&folder);
            orphans.push(Orphan {
                bytes: removable_bytes(&folder),
                hold: judge_orphan(&key, changed, now, activity).err(),
                newest_change: changed,
                folder,
            });
        }
    }
    orphans.sort_by(|a, b| a.folder.cmp(&b.folder));
    orphans
}

/// Where recordings are or were saved by configuration: `save_folder`, the
/// default and historical recordings folders, and the data root (as a guard,
/// so a meeting row pointing at it is never treated as a meeting folder).
pub(crate) fn configured_roots(save_folder: Option<&Path>) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = save_folder.map(Path::to_path_buf).into_iter().collect();
    roots.push(crate::storage::default_recordings_dir());
    roots.push(crate::storage::historical_recordings_dir());
    roots.push(crate::storage::root());
    roots
}
