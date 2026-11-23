use super::audio_files::{AudioFiles, Scan};
use sqlx::SqlitePool;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

/// A folder path normalized for comparison: separators, a trailing separator,
/// the `\\?\` prefix, and case on Windows, where filesystems ignore it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct FolderKey(Vec<String>);

impl FolderKey {
    pub(crate) fn of(path: impl AsRef<Path>) -> Self {
        let raw = path.as_ref().to_string_lossy();
        let path = raw.strip_prefix(r"\\?\").unwrap_or(&raw);
        Self(
            Path::new(path)
                .components()
                .map(|c| {
                    let component = c.as_os_str().to_string_lossy();
                    if cfg!(windows) {
                        component.to_lowercase()
                    } else {
                        component.into_owned()
                    }
                })
                .collect(),
        )
    }

    /// True when `other` is this folder or sits anywhere inside it.
    pub(crate) fn contains(&self, other: &FolderKey) -> bool {
        !self.0.is_empty() && other.0.starts_with(&self.0)
    }

    pub(crate) fn overlaps(&self, other: &FolderKey) -> bool {
        self.contains(other) || other.contains(self)
    }
}

/// Every `meetings.folder_path`, loaded once, plus the configured recordings
/// roots. Answers "who else uses this folder".
pub(crate) struct FolderIndex {
    meetings: Vec<(String, PathBuf, FolderKey)>,
    roots: Vec<(PathBuf, FolderKey)>,
}

impl FolderIndex {
    pub(crate) async fn load(pool: &SqlitePool, roots: Vec<PathBuf>) -> sqlx::Result<Self> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT id, folder_path FROM meetings WHERE folder_path IS NOT NULL AND TRIM(folder_path) <> ''",
        )
        .fetch_all(pool)
        .await?;
        Ok(Self::new(rows, roots))
    }

    pub(crate) fn new(rows: Vec<(String, String)>, roots: Vec<PathBuf>) -> Self {
        Self {
            meetings: rows
                .into_iter()
                .map(|(id, folder)| {
                    let key = FolderKey::of(&folder);
                    (id, PathBuf::from(folder), key)
                })
                .collect(),
            roots: roots
                .into_iter()
                .map(|root| {
                    let key = FolderKey::of(&root);
                    (root, key)
                })
                .collect(),
        }
    }

    /// True when another meeting's folder equals, contains, or sits inside `folder`.
    pub(crate) fn shared_or_nested(&self, meeting_id: &str, folder: &FolderKey) -> bool {
        self.meetings
            .iter()
            .any(|(id, _, key)| id != meeting_id && key.overlaps(folder))
    }

    /// True when any meeting's folder equals, contains, or sits inside `folder`.
    pub(crate) fn touches(&self, folder: &FolderKey) -> bool {
        self.meetings.iter().any(|(_, _, key)| key.overlaps(folder))
    }

    /// True when `folder` is a recordings root or one of its ancestors.
    pub(crate) fn is_root_or_above(&self, folder: &FolderKey) -> bool {
        self.roots.iter().any(|(_, root)| folder.contains(root))
    }

    /// The configured roots plus the parent of every meeting folder: every
    /// place recordings have ever been saved. Deduplicated, existing only.
    pub(crate) fn recordings_roots(&self) -> Vec<PathBuf> {
        let candidates = self
            .roots
            .iter()
            .map(|(path, _)| path.clone())
            .chain(self.meetings.iter().filter_map(|(_, path, _)| path.parent().map(Path::to_path_buf)));
        let mut seen = Vec::<FolderKey>::new();
        let mut out = Vec::new();
        for path in candidates {
            let key = FolderKey::of(&path);
            if key.0.is_empty() || seen.contains(&key) || !path.is_dir() {
                continue;
            }
            seen.push(key);
            out.push(path);
        }
        out
    }
}

/// Files the app or the OS writes into a meeting folder besides its audio.
/// Removed by name; anything else keeps the folder.
const APP_FILES: &[&str] = &[
    "metadata.json",
    "transcripts.json",
    "transcript.vtt",
    ".metadata.json.tmp",
    ".transcripts.json.tmp",
    ".DS_Store",
    "Thumbs.db",
];

/// Everything `remove_meeting_folder` would delete, in bytes on this computer.
pub(super) fn removable_bytes(folder: &Path) -> u64 {
    let audio = match AudioFiles::scan(folder) {
        Ok(Scan::Found(files)) => files.local_bytes(),
        _ => 0,
    };
    let app_files: u64 = APP_FILES
        .iter()
        .filter_map(|name| std::fs::symlink_metadata(folder.join(name)).ok())
        .filter(|meta| meta.is_file())
        .map(|meta| meta.len())
        .sum();
    audio + app_files
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum FolderRemoval {
    Removed { freed_bytes: u64 },
    /// Our files are gone, something we do not recognize is still there, the folder stays.
    KeptForeignFiles { freed_bytes: u64 },
    Failed { freed_bytes: u64, failed: Vec<String> },
}

/// Removes a whole meeting folder without ever calling `remove_dir_all` on it.
pub(super) fn remove_meeting_folder(folder: &Path) -> FolderRemoval {
    let (mut freed_bytes, mut failed) = match AudioFiles::scan(folder) {
        Ok(Scan::FolderMissing) => return FolderRemoval::Removed { freed_bytes: 0 },
        Ok(Scan::Found(files)) => {
            let deleted = files.delete_all();
            (deleted.freed_bytes, deleted.failed)
        }
        Ok(Scan::Empty | Scan::OnlineOnlyFolder) => (0, Vec::new()),
        Err(e) => return FolderRemoval::Failed { freed_bytes: 0, failed: vec![format!("{:?}", e.kind())] },
    };

    for name in APP_FILES {
        let path = folder.join(name);
        let Ok(meta) = std::fs::symlink_metadata(&path) else { continue };
        if meta.is_dir() {
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => freed_bytes += meta.len(),
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => failed.push(format!("{name} ({:?})", e.kind())),
        }
    }
    let _ = std::fs::remove_dir(folder.join(".checkpoints"));

    match std::fs::remove_dir(folder) {
        Ok(()) => FolderRemoval::Removed { freed_bytes },
        Err(e) if e.kind() == ErrorKind::NotFound => FolderRemoval::Removed { freed_bytes },
        Err(_) if !failed.is_empty() => FolderRemoval::Failed { freed_bytes, failed },
        Err(e) => {
            let not_empty = std::fs::read_dir(folder).map(|mut d| d.next().is_some()).unwrap_or(false);
            if not_empty {
                FolderRemoval::KeptForeignFiles { freed_bytes }
            } else {
                FolderRemoval::Failed { freed_bytes, failed: vec![format!("{:?}", e.kind())] }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_ignore_separator_and_trailing_slash_differences() {
        assert_eq!(FolderKey::of("/rec/a/"), FolderKey::of("/rec/a"));
        assert_eq!(FolderKey::of("/rec//a"), FolderKey::of("/rec/a"));
        assert_ne!(FolderKey::of("/rec/a"), FolderKey::of("/rec/ab"));
        #[cfg(windows)]
        {
            assert_eq!(FolderKey::of(r"C:\Rec\A\"), FolderKey::of("c:/rec/a"));
            assert_eq!(FolderKey::of(r"\\?\C:\rec\a"), FolderKey::of(r"C:\rec\a"));
        }
    }

    #[test]
    fn contains_is_a_component_prefix_test() {
        let parent = FolderKey::of("/rec");
        assert!(parent.contains(&FolderKey::of("/rec/a")));
        assert!(parent.contains(&FolderKey::of("/rec")));
        assert!(!parent.contains(&FolderKey::of("/recordings/a")));
        assert!(!FolderKey::of("/rec/a").contains(&parent));
    }

    #[test]
    fn index_sees_shared_nested_and_root_folders() {
        let index = FolderIndex::new(
            vec![
                ("a".into(), "/rec/a".into()),
                ("b".into(), "/rec/a/".into()),
                ("c".into(), "/rec/c".into()),
                ("d".into(), "/rec/c/inner".into()),
                ("e".into(), "/rec/e".into()),
            ],
            vec![PathBuf::from("/rec")],
        );
        assert!(index.shared_or_nested("a", &FolderKey::of("/rec/a")));
        assert!(index.shared_or_nested("c", &FolderKey::of("/rec/c")));
        assert!(index.shared_or_nested("d", &FolderKey::of("/rec/c/inner")));
        assert!(!index.shared_or_nested("e", &FolderKey::of("/rec/e")));
        assert!(index.touches(&FolderKey::of("/rec/e")));
        assert!(!index.touches(&FolderKey::of("/rec/f")));
        assert!(index.is_root_or_above(&FolderKey::of("/rec")));
        assert!(index.is_root_or_above(&FolderKey::of("/")));
        assert!(!index.is_root_or_above(&FolderKey::of("/rec/e")));
    }

    #[test]
    fn a_clean_meeting_folder_is_removed() {
        let root = tempfile::tempdir().unwrap();
        let folder = root.path().join("Meeting");
        std::fs::create_dir_all(folder.join(".checkpoints")).unwrap();
        std::fs::write(folder.join("audio.mp4"), b"0123456789").unwrap();
        std::fs::write(folder.join(".checkpoints/audio_chunk_000.mp4"), b"01234").unwrap();
        std::fs::write(folder.join("metadata.json"), b"{}").unwrap();
        std::fs::write(folder.join("transcripts.json"), b"{}").unwrap();

        assert_eq!(remove_meeting_folder(&folder), FolderRemoval::Removed { freed_bytes: 19 });
        assert!(!folder.exists());
        assert!(root.path().exists());
        assert_eq!(remove_meeting_folder(&folder), FolderRemoval::Removed { freed_bytes: 0 });
    }

    #[test]
    fn a_folder_with_a_foreign_file_keeps_that_file() {
        let root = tempfile::tempdir().unwrap();
        let folder = root.path().join("Meeting");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("audio.mp4"), b"0123456789").unwrap();
        std::fs::write(folder.join("metadata.json"), b"{}").unwrap();
        std::fs::write(folder.join("slides.pdf"), b"keep me").unwrap();

        assert_eq!(
            remove_meeting_folder(&folder),
            FolderRemoval::KeptForeignFiles { freed_bytes: 12 }
        );
        assert_eq!(std::fs::read(folder.join("slides.pdf")).unwrap(), b"keep me");
        assert!(!folder.join("audio.mp4").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_inside_the_folder_is_never_followed() {
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(outside.path().join("data")).unwrap();
        std::fs::write(outside.path().join("data/audio.mp4"), b"precious").unwrap();

        let root = tempfile::tempdir().unwrap();
        let folder = root.path().join("Meeting");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("metadata.json"), b"{}").unwrap();
        std::os::unix::fs::symlink(outside.path().join("data"), folder.join(".checkpoints")).unwrap();

        let outcome = remove_meeting_folder(&folder);
        assert_eq!(outcome, FolderRemoval::KeptForeignFiles { freed_bytes: 2 });
        assert_eq!(std::fs::read(outside.path().join("data/audio.mp4")).unwrap(), b"precious");
    }
}
