use super::audio_files::{AudioFiles, Deleted, Scan};
use super::folder::{FolderIndex, FolderKey};
use crate::database::models::DateTimeUtc;
use serde::ser::SerializeStruct;
use serde::{Serialize, Serializer};
use std::collections::HashSet;
use std::time::{Duration, SystemTime};

/// What the database says about one meeting. One row of `MeetingsRepository::audio_facts`.
#[derive(Debug, Clone, sqlx::FromRow)]
pub(crate) struct Facts {
    pub meeting_id: String,
    pub title: String,
    pub created_at: DateTimeUtc,
    pub folder_path: Option<String>,
    pub has_transcript: bool,
    pub transcription_incomplete: bool,
}

impl Facts {
    pub(crate) fn folder(&self) -> Option<&str> {
        self.folder_path.as_deref().filter(|p| !p.trim().is_empty())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LiveRecording {
    None,
    At(FolderKey),
    /// A recording is running but its folder is not known, as during the stop
    /// tail after the manager was taken. Everything counts as in use.
    Unlocated,
}

impl LiveRecording {
    fn holds(&self, folder: &FolderKey) -> bool {
        match self {
            LiveRecording::None => false,
            LiveRecording::At(live) => live.overlaps(folder),
            LiveRecording::Unlocated => true,
        }
    }
}

/// What is running right now. Captured once per survey, once per purged meeting.
#[derive(Debug, Clone)]
pub(crate) struct Activity {
    pub recording: LiveRecording,
    pub import_running: bool,
    /// `background_jobs` rows: every one is a running or interrupted job that will want its folder.
    pub journaled_meetings: HashSet<String>,
    pub journaled_folders: Vec<FolderKey>,
}

impl Activity {
    pub(crate) fn idle() -> Self {
        Self {
            recording: LiveRecording::None,
            import_running: false,
            journaled_meetings: HashSet::new(),
            journaled_folders: Vec::new(),
        }
    }

    fn journaled_folder(&self, folder: &FolderKey) -> bool {
        self.journaled_folders.iter().any(|j| j.overlaps(folder))
    }

    pub(crate) fn recording_holds(&self, folder: &FolderKey) -> bool {
        self.recording.holds(folder)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbsentReason {
    NoFolder,
    FolderMissing,
    NoAudioFiles,
    OnlineOnly,
}

impl AbsentReason {
    pub(crate) fn code(self) -> &'static str {
        match self {
            AbsentReason::NoFolder => "no_folder",
            AbsentReason::FolderMissing => "folder_missing",
            AbsentReason::NoAudioFiles => "no_audio_files",
            AbsentReason::OnlineOnly => "online_only",
        }
    }

    pub(crate) fn text(self) -> &'static str {
        match self {
            AbsentReason::NoFolder => "This meeting has no recording folder, so it cannot be enhanced.",
            AbsentReason::FolderMissing => {
                "The recording folder is no longer on this computer, so this meeting cannot be enhanced."
            }
            AbsentReason::NoAudioFiles => "This meeting has no audio on this computer, so it cannot be enhanced.",
            AbsentReason::OnlineOnly => {
                "The recording folder is online-only in OneDrive. Make it available on this computer to enhance this meeting."
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoldReason {
    NotTranscribed,
    TranscriptIncomplete,
    Recording,
    Transcribing,
    InterruptedJob,
    ImportRunning,
    RecentlyActive,
    SharedFolder,
    UnrecognizedFolder,
    OnlineOnly,
    Vanished,
}

impl HoldReason {
    pub(crate) fn code(self) -> &'static str {
        match self {
            HoldReason::NotTranscribed => "not_transcribed",
            HoldReason::TranscriptIncomplete => "transcript_incomplete",
            HoldReason::Recording => "recording",
            HoldReason::Transcribing => "transcribing",
            HoldReason::InterruptedJob => "interrupted_job",
            HoldReason::ImportRunning => "import_running",
            HoldReason::RecentlyActive => "recently_active",
            HoldReason::SharedFolder => "shared_folder",
            HoldReason::UnrecognizedFolder => "unrecognized_folder",
            HoldReason::OnlineOnly => "online_only",
            HoldReason::Vanished => "vanished",
        }
    }

    pub(crate) fn text(self) -> &'static str {
        match self {
            HoldReason::NotTranscribed => {
                "Not transcribed yet. The audio is the only copy. Transcribe it, or delete the meeting."
            }
            HoldReason::TranscriptIncomplete => "The transcript is incomplete and will be rebuilt from this audio.",
            HoldReason::Recording => "Being recorded right now.",
            HoldReason::Transcribing => "Being transcribed right now.",
            HoldReason::InterruptedJob => "An interrupted transcription is waiting. Retry or dismiss it first.",
            HoldReason::ImportRunning => "An import is running. Try again when it finishes.",
            HoldReason::RecentlyActive => "Changed in the last 8 days. It may be a recording you can still recover.",
            HoldReason::SharedFolder => "Its folder is shared with another meeting.",
            HoldReason::UnrecognizedFolder => "Its folder does not look like a Meetily recording folder.",
            HoldReason::OnlineOnly => "Online-only in OneDrive. It uses no space on this computer.",
            HoldReason::Vanished => "No longer there.",
        }
    }
}

fn serialize_reason<S: Serializer>(serializer: S, code: &str, text: &str) -> Result<S::Ok, S::Error> {
    let mut s = serializer.serialize_struct("Reason", 2)?;
    s.serialize_field("code", code)?;
    s.serialize_field("text", text)?;
    s.end()
}

impl Serialize for AbsentReason {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serialize_reason(serializer, self.code(), self.text())
    }
}

impl Serialize for HoldReason {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serialize_reason(serializer, self.code(), self.text())
    }
}

/// Proof that `judge` found this audio removable. No other constructor exists.
pub(crate) struct Removable(AudioFiles);

impl Removable {
    pub(crate) fn bytes(&self) -> u64 {
        self.0.local_bytes()
    }

    pub(super) fn delete(self) -> Deleted {
        self.0.delete_all()
    }
}

pub(crate) enum Verdict {
    Absent(AbsentReason),
    Held { bytes: u64, reason: HoldReason },
    Removable(Removable),
}

/// The eligibility rule. First match wins, most specific danger first.
/// `scan` must be the scan of `facts.folder_path`.
pub(crate) fn judge(
    facts: &Facts,
    transcribing: bool,
    activity: &Activity,
    folders: &FolderIndex,
    scan: Scan,
) -> Verdict {
    let Some(folder) = facts.folder() else {
        return Verdict::Absent(AbsentReason::NoFolder);
    };
    let files = match scan {
        Scan::FolderMissing => return Verdict::Absent(AbsentReason::FolderMissing),
        Scan::OnlineOnlyFolder => return Verdict::Absent(AbsentReason::OnlineOnly),
        Scan::Empty => return Verdict::Absent(AbsentReason::NoAudioFiles),
        Scan::Found(files) => files,
    };

    let key = FolderKey::of(folder);
    let shape = files.shape();
    let held = |reason| Verdict::Held { bytes: files.local_bytes(), reason };

    if activity.recording.holds(&key) {
        return held(HoldReason::Recording);
    }
    if transcribing {
        return held(HoldReason::Transcribing);
    }
    if activity.journaled_meetings.contains(&facts.meeting_id) || activity.journaled_folder(&key) {
        return held(HoldReason::InterruptedJob);
    }
    if !facts.has_transcript {
        return held(HoldReason::NotTranscribed);
    }
    if facts.transcription_incomplete {
        return held(HoldReason::TranscriptIncomplete);
    }
    if folders.shared_or_nested(&facts.meeting_id, &key) {
        return held(HoldReason::SharedFolder);
    }
    if shape.symlink || !shape.marked || folders.is_root_or_above(&key) {
        return held(HoldReason::UnrecognizedFolder);
    }
    if files.all_online_only() {
        return held(HoldReason::OnlineOnly);
    }
    Verdict::Removable(Removable(files))
}

/// Transcript recovery offers an interrupted recording for 7 days
/// (`useTranscriptRecovery.ts`, `indexedDBService.deleteOldMeetings(7)`). An
/// orphan stays untouchable one day longer than that.
pub(crate) const ORPHAN_QUIET_PERIOD: Duration = Duration::from_secs(8 * 24 * 60 * 60);

/// The same rule for a folder no meeting row points at. `newest_change` is the
/// newest mtime of the folder and its direct entries.
pub(crate) fn judge_orphan(
    folder: &FolderKey,
    newest_change: SystemTime,
    now: SystemTime,
    activity: &Activity,
) -> Result<(), HoldReason> {
    if activity.recording.holds(folder) {
        return Err(HoldReason::Recording);
    }
    // An import creates its folder before it journals it, so any running
    // import holds every orphan.
    if activity.import_running {
        return Err(HoldReason::ImportRunning);
    }
    if activity.journaled_folder(folder) {
        return Err(HoldReason::InterruptedJob);
    }
    match now.duration_since(newest_change) {
        Ok(age) if age >= ORPHAN_QUIET_PERIOD => Ok(()),
        _ => Err(HoldReason::RecentlyActive),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    struct Fixture {
        _root: tempfile::TempDir,
        folder: PathBuf,
    }

    fn meeting_folder(files: &[&str]) -> Fixture {
        let root = tempfile::tempdir().unwrap();
        let folder = root.path().join("Meeting");
        std::fs::create_dir_all(&folder).unwrap();
        for file in files {
            std::fs::write(folder.join(file), b"0123456789").unwrap();
        }
        Fixture { _root: root, folder }
    }

    fn facts(folder: &Path) -> Facts {
        Facts {
            meeting_id: "m".into(),
            title: "Title".into(),
            created_at: DateTimeUtc(chrono::Utc::now()),
            folder_path: Some(folder.to_string_lossy().into_owned()),
            has_transcript: true,
            transcription_incomplete: false,
        }
    }

    fn index_with(folder: &Path, extra: &[(&str, &str)]) -> FolderIndex {
        let mut rows = vec![("m".to_string(), folder.to_string_lossy().into_owned())];
        rows.extend(extra.iter().map(|(id, f)| (id.to_string(), f.to_string())));
        FolderIndex::new(rows, vec![folder.parent().unwrap().to_path_buf()])
    }

    fn outcome(verdict: Verdict) -> Result<Option<HoldReason>, AbsentReason> {
        match verdict {
            Verdict::Absent(reason) => Err(reason),
            Verdict::Held { reason, .. } => Ok(Some(reason)),
            Verdict::Removable(_) => Ok(None),
        }
    }

    fn run(
        fixture: &Fixture,
        tweak: impl FnOnce(&mut Facts, &mut Activity, &mut bool),
        extra_meetings: &[(&str, &str)],
    ) -> Result<Option<HoldReason>, AbsentReason> {
        let mut f = facts(&fixture.folder);
        let mut activity = Activity::idle();
        let mut transcribing = false;
        tweak(&mut f, &mut activity, &mut transcribing);
        let index = index_with(&fixture.folder, extra_meetings);
        let scan = match f.folder() {
            Some(folder) => AudioFiles::scan(Path::new(folder)).unwrap(),
            None => Scan::Empty,
        };
        outcome(judge(&f, transcribing, &activity, &index, scan))
    }

    #[test]
    fn every_reason_has_a_row() {
        let live = meeting_folder(&["audio.mp4", "metadata.json", "transcripts.json"]);
        let key = FolderKey::of(&live.folder);

        assert_eq!(run(&live, |_, _, _| {}, &[]), Ok(None));
        assert_eq!(run(&live, |f, _, _| f.folder_path = None, &[]), Err(AbsentReason::NoFolder));
        assert_eq!(run(&live, |f, _, _| f.folder_path = Some("  ".into()), &[]), Err(AbsentReason::NoFolder));
        assert_eq!(
            run(&live, |f, _, _| f.folder_path = Some(live.folder.join("gone").to_string_lossy().into()), &[]),
            Err(AbsentReason::FolderMissing)
        );

        let json_only = meeting_folder(&["metadata.json", "transcripts.json"]);
        assert_eq!(run(&json_only, |_, _, _| {}, &[]), Err(AbsentReason::NoAudioFiles));

        let k = key.clone();
        assert_eq!(
            run(&live, move |_, a, _| a.recording = LiveRecording::At(k), &[]),
            Ok(Some(HoldReason::Recording))
        );
        assert_eq!(
            run(&live, |_, a, _| a.recording = LiveRecording::Unlocated, &[]),
            Ok(Some(HoldReason::Recording))
        );
        assert_eq!(run(&live, |_, _, t| *t = true, &[]), Ok(Some(HoldReason::Transcribing)));
        assert_eq!(
            run(&live, |_, a, _| { a.journaled_meetings.insert("m".into()); }, &[]),
            Ok(Some(HoldReason::InterruptedJob))
        );
        let k = key.clone();
        assert_eq!(
            run(&live, move |_, a, _| a.journaled_folders.push(k), &[]),
            Ok(Some(HoldReason::InterruptedJob))
        );
        assert_eq!(run(&live, |f, _, _| f.has_transcript = false, &[]), Ok(Some(HoldReason::NotTranscribed)));
        assert_eq!(
            run(&live, |f, _, _| f.transcription_incomplete = true, &[]),
            Ok(Some(HoldReason::TranscriptIncomplete))
        );
        let shared = live.folder.to_string_lossy().into_owned() + "/";
        assert_eq!(run(&live, |_, _, _| {}, &[("other", &shared)]), Ok(Some(HoldReason::SharedFolder)));
        let nested = live.folder.join("inner").to_string_lossy().into_owned();
        assert_eq!(run(&live, |_, _, _| {}, &[("other", &nested)]), Ok(Some(HoldReason::SharedFolder)));

        let unmarked = meeting_folder(&["audio.wav"]);
        assert_eq!(run(&unmarked, |_, _, _| {}, &[]), Ok(Some(HoldReason::UnrecognizedFolder)));
        let root = live.folder.parent().unwrap().to_string_lossy().into_owned();
        assert_eq!(
            run(&live, move |f, _, _| f.folder_path = Some(root), &[]),
            Err(AbsentReason::NoAudioFiles),
            "a root holding no audio of its own is simply absent"
        );
    }

    #[test]
    fn a_recordings_root_with_audio_is_never_removable() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("audio.mp4"), b"x").unwrap();
        std::fs::write(root.path().join("metadata.json"), b"{}").unwrap();
        let mut f = facts(root.path());
        f.meeting_id = "m".into();
        let index = FolderIndex::new(
            vec![("m".into(), root.path().to_string_lossy().into_owned())],
            vec![root.path().to_path_buf()],
        );
        let scan = AudioFiles::scan(root.path()).unwrap();
        assert_eq!(
            outcome(judge(&f, false, &Activity::idle(), &index, scan)),
            Ok(Some(HoldReason::UnrecognizedFolder))
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_meeting_folder_is_unrecognized() {
        let real = meeting_folder(&["audio.mp4", "metadata.json"]);
        let links = tempfile::tempdir().unwrap();
        let link = links.path().join("Meeting");
        std::os::unix::fs::symlink(&real.folder, &link).unwrap();
        let f = facts(&link);
        let index = FolderIndex::new(vec![("m".into(), link.to_string_lossy().into_owned())], vec![]);
        let scan = AudioFiles::scan(&link).unwrap();
        assert_eq!(
            outcome(judge(&f, false, &Activity::idle(), &index, scan)),
            Ok(Some(HoldReason::UnrecognizedFolder))
        );
    }

    #[test]
    fn precedence_puts_live_use_before_missing_transcripts() {
        let live = meeting_folder(&["audio.mp4", "metadata.json"]);
        let key = FolderKey::of(&live.folder);
        assert_eq!(
            run(&live, move |f, a, t| {
                f.has_transcript = false;
                f.transcription_incomplete = true;
                *t = true;
                a.recording = LiveRecording::At(key);
            }, &[("other", "/elsewhere")]),
            Ok(Some(HoldReason::Recording))
        );
        assert_eq!(
            run(&live, |f, _, t| { f.has_transcript = false; *t = true; }, &[]),
            Ok(Some(HoldReason::Transcribing))
        );
        assert_eq!(
            run(&live, |f, _, _| { f.has_transcript = false; f.transcription_incomplete = true; }, &[]),
            Ok(Some(HoldReason::NotTranscribed))
        );
        let shared = live.folder.to_string_lossy().into_owned();
        assert_eq!(
            run(&live, |f, _, _| f.transcription_incomplete = true, &[("other", &shared)]),
            Ok(Some(HoldReason::TranscriptIncomplete))
        );
    }

    #[test]
    fn held_reports_the_bytes_it_protects() {
        let live = meeting_folder(&["audio.mp4", "audio.wav", "metadata.json"]);
        let mut f = facts(&live.folder);
        f.has_transcript = false;
        let index = index_with(&live.folder, &[]);
        match judge(&f, false, &Activity::idle(), &index, AudioFiles::scan(&live.folder).unwrap()) {
            Verdict::Held { bytes, reason } => {
                assert_eq!(bytes, 20);
                assert_eq!(reason, HoldReason::NotTranscribed);
            }
            _ => panic!("expected held"),
        }
    }

    #[test]
    fn orphans_wait_out_the_quiet_period_and_any_activity() {
        let now = SystemTime::now();
        let old = now - ORPHAN_QUIET_PERIOD - Duration::from_secs(1);
        let fresh = now - ORPHAN_QUIET_PERIOD + Duration::from_secs(60);
        let folder = FolderKey::of("/rec/orphan");

        assert_eq!(judge_orphan(&folder, old, now, &Activity::idle()), Ok(()));
        assert_eq!(judge_orphan(&folder, fresh, now, &Activity::idle()), Err(HoldReason::RecentlyActive));
        assert_eq!(
            judge_orphan(&folder, now + Duration::from_secs(3600), now, &Activity::idle()),
            Err(HoldReason::RecentlyActive)
        );

        let mut importing = Activity::idle();
        importing.import_running = true;
        assert_eq!(judge_orphan(&folder, old, now, &importing), Err(HoldReason::ImportRunning));

        let mut recording = Activity::idle();
        recording.recording = LiveRecording::At(folder.clone());
        assert_eq!(judge_orphan(&folder, old, now, &recording), Err(HoldReason::Recording));

        let mut journaled = Activity::idle();
        journaled.journaled_folders.push(FolderKey::of("/rec/orphan/"));
        assert_eq!(judge_orphan(&folder, old, now, &journaled), Err(HoldReason::InterruptedJob));
    }

    #[test]
    fn reasons_serialize_as_code_and_text() {
        let json = serde_json::to_value(HoldReason::NotTranscribed).unwrap();
        assert_eq!(json["code"], "not_transcribed");
        assert!(json["text"].as_str().unwrap().starts_with("Not transcribed yet"));
        let json = serde_json::to_value(AbsentReason::NoAudioFiles).unwrap();
        assert_eq!(json["code"], "no_audio_files");
    }
}
