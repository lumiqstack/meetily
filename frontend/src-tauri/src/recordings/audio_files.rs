use crate::audio::constants::AUDIO_EXTENSIONS;
use std::fs::FileType;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

const CHECKPOINTS_DIR: &str = ".checkpoints";

/// Retranscription's preferred names, in the order it has always tried them.
const PRIMARY_NAMES: &[&str] = &[
    "audio.mp4",
    "audio.m4a",
    "audio.wav",
    "audio.mp3",
    "audio.flac",
    "audio.ogg",
    "recording.mp4",
    "audio.mkv",
    "audio.webm",
    "audio.wma",
];

/// Every file in a meeting folder that counts as its audio. This is the only
/// place that knows which files those are.
///
/// Invariant: `primary()` always returns a member of `files`, and the purge
/// deletes all of `files`, so after a purge nothing remains that
/// retranscription could pick up.
pub(crate) struct AudioFiles {
    folder: PathBuf,
    shape: FolderShape,
    files: Vec<AudioFile>,
}

struct AudioFile {
    path: PathBuf,
    in_checkpoints: bool,
    len: u64,
    online_only: bool,
}

/// What the folder itself is, captured with the scan so `judge` stays pure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FolderShape {
    /// The path is a symlink or junction rather than a plain directory.
    pub symlink: bool,
    /// Holds one of the files the app writes into every meeting folder.
    pub marked: bool,
}

impl FolderShape {
    pub(crate) fn probe(folder: &Path) -> Self {
        let symlink = std::fs::symlink_metadata(folder)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false);
        Self {
            symlink,
            marked: crate::storage::migrate::is_meeting_folder(folder),
        }
    }
}

pub(crate) enum Scan {
    FolderMissing,
    /// Lists nothing and is a cloud placeholder (`storage::online_only_hint`).
    OnlineOnlyFolder,
    Empty,
    Found(AudioFiles),
}

pub(crate) struct Deleted {
    pub freed_bytes: u64,
    /// File names that could not be removed (Windows sharing violation, permissions).
    pub failed: Vec<String>,
}

fn is_audio_name(path: &Path) -> bool {
    path.extension()
        .map(|ext| AUDIO_EXTENSIONS.contains(&ext.to_string_lossy().to_lowercase().as_str()))
        .unwrap_or(false)
}

fn regular_files(dir: &Path) -> std::io::Result<Vec<(PathBuf, FileType)>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        // `DirEntry::file_type` does not follow symlinks, so a link is never listed.
        let file_type = entry.file_type()?;
        if file_type.is_file() {
            out.push((entry.path(), file_type));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

fn audio_file(path: PathBuf, in_checkpoints: bool) -> AudioFile {
    let len = std::fs::symlink_metadata(&path).map(|m| m.len()).unwrap_or(0);
    let online_only = crate::storage::is_online_only_file(&path);
    AudioFile { path, in_checkpoints, len, online_only }
}

impl AudioFiles {
    pub(crate) fn scan(folder: &Path) -> std::io::Result<Scan> {
        let top = match regular_files(folder) {
            Ok(files) => files,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Scan::FolderMissing),
            Err(e) => return Err(e),
        };

        let mut files: Vec<AudioFile> = top
            .into_iter()
            .filter(|(path, _)| is_audio_name(path))
            .map(|(path, _)| audio_file(path, false))
            .collect();

        let checkpoints = folder.join(CHECKPOINTS_DIR);
        let checkpoints_is_dir = std::fs::symlink_metadata(&checkpoints)
            .map(|m| m.file_type().is_dir())
            .unwrap_or(false);
        if checkpoints_is_dir {
            for (path, _) in regular_files(&checkpoints)? {
                files.push(audio_file(path, true));
            }
        }

        if files.is_empty() {
            if crate::storage::online_only_hint(folder).is_some() {
                return Ok(Scan::OnlineOnlyFolder);
            }
            return Ok(Scan::Empty);
        }

        Ok(Scan::Found(AudioFiles {
            folder: folder.to_path_buf(),
            shape: FolderShape::probe(folder),
            files,
        }))
    }

    /// The file retranscription decodes: fixed names first, then any audio
    /// extension. `.checkpoints` chunks are never primary.
    pub(crate) fn primary(&self) -> Option<&Path> {
        let top = || self.files.iter().filter(|f| !f.in_checkpoints);
        for name in PRIMARY_NAMES {
            if let Some(file) = top().find(|f| f.path.file_name().is_some_and(|n| n == *name)) {
                return Some(&file.path);
            }
        }
        top().next().map(|f| f.path.as_path())
    }

    pub(crate) fn shape(&self) -> FolderShape {
        self.shape
    }

    /// Bytes these files occupy on this computer. A cloud placeholder counts 0.
    pub(crate) fn local_bytes(&self) -> u64 {
        self.files.iter().filter(|f| !f.online_only).map(|f| f.len).sum()
    }

    pub(crate) fn all_online_only(&self) -> bool {
        self.files.iter().all(|f| f.online_only)
    }

    /// Non-empty `audio.mp4` or any `.checkpoints/*.mp4`: transcript recovery's question.
    pub(crate) fn recoverable(&self) -> bool {
        self.files.iter().any(|f| {
            if f.in_checkpoints {
                f.path.extension().and_then(|e| e.to_str()) == Some("mp4")
            } else {
                f.path.file_name().is_some_and(|n| n == "audio.mp4") && f.len > 0
            }
        })
    }

    /// Only `Removable::delete` and `folder::remove_meeting_folder` call this.
    pub(super) fn delete_all(self) -> Deleted {
        // The primary file goes last: a crash part-way leaves a folder that
        // either still enhances from the same file or has no audio at all,
        // never one whose fallback scan resolves to a different file.
        let primary = self.primary().map(Path::to_path_buf);
        let mut order: Vec<&AudioFile> = self.files.iter().filter(|f| f.in_checkpoints).collect();
        order.extend(
            self.files
                .iter()
                .filter(|f| !f.in_checkpoints && Some(&f.path) != primary.as_ref()),
        );
        order.extend(self.files.iter().filter(|f| Some(&f.path) == primary.as_ref()));

        let mut deleted = Deleted { freed_bytes: 0, failed: Vec::new() };
        for file in order {
            match std::fs::remove_file(&file.path) {
                Ok(()) => {
                    if !file.online_only {
                        deleted.freed_bytes += file.len;
                    }
                }
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                Err(e) => {
                    let name = file
                        .path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    deleted.failed.push(format!("{name} ({:?})", e.kind()));
                }
            }
        }

        // Non-recursive: anything the scan did not list keeps the directory.
        let _ = std::fs::remove_dir(self.folder.join(CHECKPOINTS_DIR));
        deleted
    }
}

/// Replaces `retranscription::find_audio_file`, keeping its error text and
/// the OneDrive hint.
pub(crate) fn primary_audio(folder: &Path) -> anyhow::Result<PathBuf> {
    if let Ok(Scan::Found(files)) = AudioFiles::scan(folder) {
        if let Some(primary) = files.primary() {
            return Ok(primary.to_path_buf());
        }
    }
    if let Some(hint) = crate::storage::online_only_hint(folder) {
        return Err(anyhow::anyhow!("No audio file found in: {}. {}", folder.display(), hint));
    }
    Err(anyhow::anyhow!("No audio file found in: {}", folder.display()))
}

pub(crate) fn has_recoverable_audio(folder: &Path) -> bool {
    matches!(AudioFiles::scan(folder), Ok(Scan::Found(files)) if files.recoverable())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn write(path: &Path, bytes: &[u8]) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, bytes).unwrap();
    }

    /// The files the purge must never touch, with fixed contents to compare after.
    pub(crate) fn write_keepers(folder: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        let keepers = vec![
            (folder.join("metadata.json"), br#"{"summary_language":"en"}"#.to_vec()),
            (folder.join("transcripts.json"), br#"{"segments":[]}"#.to_vec()),
            (folder.join("transcript.vtt"), b"WEBVTT\n\n00:00.000 --> 00:01.000\nhi\n".to_vec()),
        ];
        for (path, bytes) in &keepers {
            write(path, bytes);
        }
        keepers
    }

    pub(crate) fn assert_keepers_intact(keepers: &[(PathBuf, Vec<u8>)]) {
        for (path, bytes) in keepers {
            assert_eq!(&std::fs::read(path).unwrap(), bytes, "{} changed", path.display());
        }
    }

    fn found(folder: &Path) -> AudioFiles {
        match AudioFiles::scan(folder).unwrap() {
            Scan::Found(files) => files,
            _ => panic!("expected audio in {}", folder.display()),
        }
    }

    #[test]
    fn primary_prefers_fixed_names_in_order() {
        let dir = tempfile::tempdir().unwrap();
        assert!(primary_audio(dir.path()).is_err());

        write(&dir.path().join("audio.m4a"), b"fake");
        write(&dir.path().join("audio.mp4"), b"fake");
        assert_eq!(primary_audio(dir.path()).unwrap().file_name().unwrap(), "audio.mp4");
    }

    #[test]
    fn primary_finds_non_mp4_imports() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("audio.wav"), b"fake");
        assert_eq!(primary_audio(dir.path()).unwrap().file_name().unwrap(), "audio.wav");
    }

    #[test]
    fn primary_falls_back_to_any_audio_extension() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("my_recording.flac"), b"fake");
        write(&dir.path().join("notes.txt"), b"text");
        assert_eq!(
            primary_audio(dir.path()).unwrap().file_name().unwrap(),
            "my_recording.flac"
        );
    }

    #[test]
    fn primary_accepts_a_zero_byte_file_as_before() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("audio.mp4"), b"");
        assert!(primary_audio(dir.path()).is_ok());
        assert!(!has_recoverable_audio(dir.path()));
    }

    #[test]
    fn missing_or_empty_folder_reports_no_audio() {
        let dir = tempfile::tempdir().unwrap();
        let err = primary_audio(dir.path()).unwrap_err().to_string();
        assert!(err.contains("No audio file found"), "{err}");
        assert!(primary_audio(Path::new("/nonexistent/path/12345")).is_err());
        assert!(matches!(
            AudioFiles::scan(Path::new("/nonexistent/path/12345")).unwrap(),
            Scan::FolderMissing
        ));
    }

    #[test]
    fn checkpoint_chunks_are_audio_but_never_primary() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join(".checkpoints/audio_chunk_000.mp4"), b"x");
        let files = found(dir.path());
        assert!(files.primary().is_none());
        assert!(files.recoverable());
        assert!(primary_audio(dir.path()).is_err());
    }

    #[test]
    fn recoverable_matches_the_old_recovery_probe() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("audio.mp4"), b"streamed");
        assert!(has_recoverable_audio(dir.path()));

        let other = tempfile::tempdir().unwrap();
        write(&other.path().join("audio.wav"), b"imported");
        assert!(!has_recoverable_audio(other.path()));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_audio_file_is_neither_listed_nor_followed() {
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("precious.mp3");
        write(&target, b"not ours");

        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(&target, dir.path().join("audio.mp3")).unwrap();
        assert!(matches!(AudioFiles::scan(dir.path()).unwrap(), Scan::Empty));
        assert!(primary_audio(dir.path()).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"not ours");
    }

    #[test]
    fn delete_all_removes_every_audio_file_and_keeps_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let keepers = write_keepers(dir.path());
        for ext in AUDIO_EXTENSIONS {
            write(&dir.path().join(format!("audio.{ext}")), b"0123456789");
        }
        write(&dir.path().join("Upper.MP3"), b"0123456789");
        write(&dir.path().join(".checkpoints/audio_chunk_000.mp4"), b"0123456789");
        write(&dir.path().join(".checkpoints/audio_chunk_001.mp4"), b"0123456789");
        write(&dir.path().join("notes.txt"), b"mine");

        let files = found(dir.path());
        let expected = (AUDIO_EXTENSIONS.len() as u64 + 3) * 10;
        assert_eq!(files.local_bytes(), expected);

        let deleted = files.delete_all();
        assert_eq!(deleted.freed_bytes, expected);
        assert!(deleted.failed.is_empty());

        assert!(matches!(AudioFiles::scan(dir.path()).unwrap(), Scan::Empty));
        assert!(!dir.path().join(".checkpoints").exists());
        assert!(primary_audio(dir.path()).is_err());
        assert_keepers_intact(&keepers);
        assert_eq!(std::fs::read(dir.path().join("notes.txt")).unwrap(), b"mine");
    }

    #[test]
    fn delete_all_twice_converges() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("audio.mp4"), b"abc");
        let first = found(dir.path());
        let second = found(dir.path());
        assert_eq!(first.delete_all().freed_bytes, 3);
        let again = second.delete_all();
        assert_eq!(again.freed_bytes, 0);
        assert!(again.failed.is_empty());
    }

    /// One definition of "the audio of a meeting": across every folder shape,
    /// retranscription finds a file exactly when the scan lists a primary one,
    /// and after `delete_all` it never does.
    #[test]
    fn primary_audio_agrees_with_the_scan_for_every_folder_shape() {
        let root = tempfile::tempdir().unwrap();
        let mut shapes: Vec<(String, Vec<&str>)> = vec![
            ("live".into(), vec!["audio.mp4", "metadata.json", "transcripts.json"]),
            ("vtt-only".into(), vec!["transcript.vtt", "metadata.json", "transcripts.json"]),
            ("legacy".into(), vec![".checkpoints/audio_chunk_000.mp4", "metadata.json"]),
            ("legacy-and-live".into(), vec![".checkpoints/audio_chunk_000.mp4", "audio.mp4"]),
            ("json-only".into(), vec!["metadata.json", "transcripts.json"]),
            ("empty".into(), vec![]),
            ("odd-name".into(), vec!["meeting.WEBM", "notes.txt"]),
        ];
        let import_names: Vec<String> = AUDIO_EXTENSIONS.iter().map(|e| format!("audio.{e}")).collect();
        for name in &import_names {
            shapes.push((format!("import-{name}"), vec![name.as_str(), "metadata.json"]));
        }

        for (label, files) in &shapes {
            let folder = root.path().join(label);
            std::fs::create_dir_all(&folder).unwrap();
            for file in files {
                write(&folder.join(file), b"data");
            }
            let scan_has_primary =
                matches!(AudioFiles::scan(&folder).unwrap(), Scan::Found(ref f) if f.primary().is_some());
            assert_eq!(primary_audio(&folder).is_ok(), scan_has_primary, "{label}");

            if let Scan::Found(found) = AudioFiles::scan(&folder).unwrap() {
                found.delete_all();
            }
            assert!(primary_audio(&folder).is_err(), "{label} still resolves after delete_all");
        }

        let missing = root.path().join("missing");
        assert!(primary_audio(&missing).is_err());
        assert!(matches!(AudioFiles::scan(&missing).unwrap(), Scan::FolderMissing));
    }
}
