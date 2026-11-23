use super::audio_files::tests::{assert_keepers_intact, write_keepers};
use super::verdict::ORPHAN_QUIET_PERIOD;
use super::*;
use crate::audio::engine_coordinator::global_coordinator_test_lock;
use std::time::Duration;

async fn test_pool() -> SqlitePool {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("in-memory sqlite pool");
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("migrations must apply to a fresh database");
    pool
}

async fn insert_meeting(pool: &SqlitePool, id: &str, folder: Option<&Path>) {
    sqlx::query(
        "INSERT INTO meetings (id, title, created_at, updated_at, folder_path)
         VALUES (?, ?, '2026-07-24T10:00:00Z', '2026-07-24T10:00:00Z', ?)",
    )
    .bind(id)
    .bind(format!("Meeting {}", id))
    .bind(folder.map(|f| f.to_string_lossy().into_owned()))
    .execute(pool)
    .await
    .unwrap();
}

async fn insert_transcript(pool: &SqlitePool, meeting_id: &str) {
    sqlx::query(
        "INSERT INTO transcripts (id, meeting_id, transcript, timestamp)
         VALUES (?, ?, 'hello', '2026-07-24T10:00:00Z')",
    )
    .bind(format!("t-{}", meeting_id))
    .bind(meeting_id)
    .execute(pool)
    .await
    .unwrap();
}

async fn mark_incomplete(pool: &SqlitePool, meeting_id: &str) {
    sqlx::query("UPDATE meetings SET transcription_incomplete = 1 WHERE id = ?")
        .bind(meeting_id)
        .execute(pool)
        .await
        .unwrap();
}

async fn count(pool: &SqlitePool, sql: &str, id: &str) -> i64 {
    sqlx::query_scalar(sql).bind(id).fetch_one(pool).await.unwrap()
}

/// A recorded meeting folder under `root`: 1000 bytes of audio plus the JSON files.
fn recorded(root: &Path, name: &str) -> (PathBuf, Vec<(PathBuf, Vec<u8>)>) {
    let folder = root.join(name);
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join("audio.mp4"), vec![7u8; 1000]).unwrap();
    let keepers = write_keepers(&folder);
    (folder, keepers)
}

fn request(ids: &[&str], orphans: &[&Path]) -> PurgeRequest {
    PurgeRequest {
        meeting_ids: ids.iter().map(|s| s.to_string()).collect(),
        orphan_folders: orphans.iter().map(|p| p.to_string_lossy().into_owned()).collect(),
    }
}

fn later() -> SystemTime {
    SystemTime::now() + ORPHAN_QUIET_PERIOD + Duration::from_secs(3600)
}

/// `judge` holds a meeting's audio for a missing transcript exactly when the
/// sidebar and pipeline (`MEETING_LIST_SQL.transcript_needed`) say it still
/// needs transcribing.
#[tokio::test]
async fn judge_agrees_with_transcript_needed() {
    let pool = test_pool().await;
    let root = tempfile::tempdir().unwrap();
    let shapes: [(&str, bool, bool); 4] = [
        ("agree-none", false, false),
        ("agree-done", true, false),
        ("agree-partial", true, true),
        ("agree-zero-incomplete", false, true),
    ];
    for (id, transcript, incomplete) in shapes {
        let (folder, _) = recorded(root.path(), id);
        insert_meeting(&pool, id, Some(&folder)).await;
        if transcript {
            insert_transcript(&pool, id).await;
        }
        if incomplete {
            mark_incomplete(&pool, id).await;
        }
    }

    let listed = MeetingsRepository::get_meetings(&pool).await.unwrap();
    let index = FolderIndex::load(&pool, vec![root.path().to_path_buf()]).await.unwrap();
    for facts in MeetingsRepository::audio_facts(&pool, None).await.unwrap() {
        let needed = listed.iter().find(|m| m.id == facts.meeting_id).unwrap().transcript_needed;
        let verdict = judge(&facts, false, &Activity::idle(), &index, scan_of(&facts).unwrap());
        let held_for_transcript = matches!(
            verdict,
            Verdict::Held { reason: HoldReason::NotTranscribed | HoldReason::TranscriptIncomplete, .. }
        );
        assert_eq!(held_for_transcript, needed, "{}", facts.meeting_id);
    }
}

#[tokio::test]
async fn purge_removes_audio_of_transcribed_meetings_and_keeps_everything_else() {
    let pool = test_pool().await;
    let root = tempfile::tempdir().unwrap();
    let (done, done_keepers) = recorded(root.path(), "Done");
    let (pending, _) = recorded(root.path(), "Pending");
    insert_meeting(&pool, "purge-done", Some(&done)).await;
    insert_transcript(&pool, "purge-done").await;
    insert_meeting(&pool, "purge-pending", Some(&pending)).await;

    let report = purge(&pool, vec![root.path().to_path_buf()], request(&["purge-done", "purge-pending", "purge-ghost"], &[]))
        .await
        .unwrap();

    assert_eq!(report.purged_meetings, vec!["purge-done"]);
    assert_eq!(report.freed_bytes, 1000);
    assert!(report.failed.is_empty());
    let skipped: Vec<(&str, HoldReason)> = report.skipped.iter().map(|s| (s.target.as_str(), s.reason)).collect();
    assert_eq!(
        skipped,
        vec![("purge-pending", HoldReason::NotTranscribed), ("purge-ghost", HoldReason::Vanished)]
    );

    assert!(!done.join("audio.mp4").exists());
    assert_keepers_intact(&done_keepers);
    assert!(pending.join("audio.mp4").exists(), "the only copy of an untranscribed meeting survives");
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM meetings WHERE id = ?", "purge-done").await, 1);
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM transcripts WHERE meeting_id = ?", "purge-done").await, 1);
    assert!(crate::recordings::primary_audio(&done).is_err());

    let again = purge(&pool, vec![root.path().to_path_buf()], request(&["purge-done"], &[])).await.unwrap();
    assert_eq!(again.purged_meetings, vec!["purge-done"]);
    assert_eq!(again.freed_bytes, 0);
    assert_keepers_intact(&done_keepers);
}

#[tokio::test]
async fn purge_skips_a_meeting_whose_retranscription_holds_the_slot() {
    let _serial = global_coordinator_test_lock();
    let pool = test_pool().await;
    let root = tempfile::tempdir().unwrap();
    let (folder, keepers) = recorded(root.path(), "Busy");
    insert_meeting(&pool, "purge-busy", Some(&folder)).await;
    insert_transcript(&pool, "purge-busy").await;

    let held = crate::audio::retranscription::reserve_meeting_audio("purge-busy").unwrap();
    let report = purge(&pool, vec![root.path().to_path_buf()], request(&["purge-busy"], &[])).await.unwrap();
    assert_eq!(report.skipped.len(), 1);
    assert_eq!(report.skipped[0].reason, HoldReason::Transcribing);
    assert!(folder.join("audio.mp4").exists());
    drop(held);

    let report = purge(&pool, vec![root.path().to_path_buf()], request(&["purge-busy"], &[])).await.unwrap();
    assert_eq!(report.freed_bytes, 1000);
    assert_keepers_intact(&keepers);
}

#[tokio::test]
async fn purge_skips_a_meeting_with_a_journaled_job() {
    let pool = test_pool().await;
    let root = tempfile::tempdir().unwrap();
    let (folder, _) = recorded(root.path(), "Journaled");
    insert_meeting(&pool, "purge-journaled", Some(&folder)).await;
    insert_transcript(&pool, "purge-journaled").await;
    sqlx::query(
        "INSERT INTO background_jobs (id, kind, title, meeting_id, folder_path, interrupted, created_at)
         VALUES ('purge-journaled', 'retranscription', 't', 'purge-journaled', ?, 1, '2026-07-24T10:00:00Z')",
    )
    .bind(folder.to_string_lossy().into_owned())
    .execute(&pool)
    .await
    .unwrap();

    let report = purge(&pool, vec![root.path().to_path_buf()], request(&["purge-journaled"], &[])).await.unwrap();
    assert_eq!(report.skipped[0].reason, HoldReason::InterruptedJob);
    assert!(folder.join("audio.mp4").exists());
}

#[tokio::test]
async fn purge_totals_a_mixed_batch() {
    let pool = test_pool().await;
    let root = tempfile::tempdir().unwrap();
    let (a, a_keepers) = recorded(root.path(), "A");
    std::fs::write(a.join("audio.wav"), vec![1u8; 500]).unwrap();
    let (b, b_keepers) = recorded(root.path(), "B");
    let shared = root.path().join("Shared");
    std::fs::create_dir_all(&shared).unwrap();
    std::fs::write(shared.join("audio.mp4"), b"x").unwrap();
    std::fs::write(shared.join("metadata.json"), b"{}").unwrap();
    for (id, folder) in [("mix-a", &a), ("mix-b", &b), ("mix-s1", &shared), ("mix-s2", &shared)] {
        insert_meeting(&pool, id, Some(folder)).await;
        insert_transcript(&pool, id).await;
    }

    let report = purge(
        &pool,
        vec![root.path().to_path_buf()],
        request(&["mix-a", "mix-b", "mix-a", "mix-s1"], &[]),
    )
    .await
    .unwrap();
    assert_eq!(report.purged_meetings, vec!["mix-a", "mix-b"]);
    assert_eq!(report.freed_bytes, 2500);
    assert_eq!(report.skipped.len(), 1);
    assert_eq!(report.skipped[0].reason, HoldReason::SharedFolder);
    assert!(shared.join("audio.mp4").exists());
    assert_keepers_intact(&a_keepers);
    assert_keepers_intact(&b_keepers);
}

#[tokio::test]
async fn survey_lists_meetings_with_audio_and_orphans() {
    let _serial = global_coordinator_test_lock();
    let pool = test_pool().await;
    let root = tempfile::tempdir().unwrap();
    let (done, _) = recorded(root.path(), "Done");
    insert_meeting(&pool, "survey-done", Some(&done)).await;
    insert_transcript(&pool, "survey-done").await;
    let (pending, _) = recorded(root.path(), "Pending");
    insert_meeting(&pool, "survey-pending", Some(&pending)).await;
    let vtt = root.path().join("Vtt");
    std::fs::create_dir_all(&vtt).unwrap();
    write_keepers(&vtt);
    insert_meeting(&pool, "survey-vtt", Some(&vtt)).await;
    insert_meeting(&pool, "survey-recap", None).await;
    let (orphan, _) = recorded(root.path(), "Orphan");

    let survey = survey_at(&pool, vec![root.path().to_path_buf()], later()).await.unwrap();
    let mut ids: Vec<(&str, &AudioStatus)> = survey.meetings.iter().map(|m| (m.meeting_id.as_str(), &m.audio)).collect();
    ids.sort_by_key(|(id, _)| *id);
    assert_eq!(
        ids,
        vec![
            ("survey-done", &AudioStatus::Removable { bytes: 1000 }),
            ("survey-pending", &AudioStatus::Held { bytes: 1000, reason: HoldReason::NotTranscribed }),
        ]
    );
    assert_eq!(survey.orphans.len(), 1);
    assert_eq!(survey.orphans[0].name, "Orphan");
    assert_eq!(FolderKey::of(&survey.orphans[0].folder), FolderKey::of(&orphan));
    assert!(survey.orphans[0].hold.is_none());
    assert_eq!(survey.audio_bytes, 2000 + survey.orphans[0].bytes);
}

#[tokio::test]
async fn orphans_never_include_folders_a_meeting_uses() {
    let _serial = global_coordinator_test_lock();
    let pool = test_pool().await;
    let root = tempfile::tempdir().unwrap();
    let (spelled, _) = recorded(root.path(), "Spelled");
    let differently = format!("{}{}", spelled.to_string_lossy(), std::path::MAIN_SEPARATOR);
    insert_meeting(&pool, "orph-spelled", Some(Path::new(&differently))).await;

    let parent = root.path().join("Parent");
    let (child, _) = recorded(&parent, "Child");
    std::fs::write(parent.join("metadata.json"), b"{}").unwrap();
    insert_meeting(&pool, "orph-child", Some(&child)).await;

    let plain = root.path().join("Holiday photos");
    std::fs::create_dir_all(&plain).unwrap();
    std::fs::write(plain.join("beach.mp4"), b"video").unwrap();

    let (orphan, _) = recorded(root.path(), "Orphan");

    let index = FolderIndex::load(&pool, vec![root.path().to_path_buf()]).await.unwrap();
    let found = orphans::find_orphans(&index, &Activity::idle(), later());
    let keys: Vec<FolderKey> = found.iter().map(|o| FolderKey::of(&o.folder)).collect();
    assert_eq!(keys, vec![FolderKey::of(&orphan)]);

    let recent = orphans::find_orphans(&index, &Activity::idle(), SystemTime::now());
    assert_eq!(recent.len(), 1);
    assert_eq!(recent[0].hold, Some(HoldReason::RecentlyActive));
}

#[tokio::test]
async fn purge_removes_an_old_orphan_and_ignores_paths_outside_the_scan() {
    let _serial = global_coordinator_test_lock();
    let pool = test_pool().await;
    let root = tempfile::tempdir().unwrap();
    let (orphan, _) = recorded(root.path(), "Orphan");
    let (fresh, _) = recorded(root.path(), "Fresh");
    let elsewhere = tempfile::tempdir().unwrap();
    let (outside, outside_keepers) = recorded(elsewhere.path(), "NotOurs");

    let report = purge_at(
        &pool,
        vec![root.path().to_path_buf()],
        request(&[], &[&orphan, &outside]),
        later(),
    )
    .await
    .unwrap();
    assert_eq!(report.purged_orphans, 1);
    assert!(!orphan.exists());
    assert!(fresh.exists(), "an orphan not named in the request stays");
    assert_eq!(report.skipped.len(), 1);
    assert_eq!(report.skipped[0].reason, HoldReason::Vanished);
    assert!(outside.join("audio.mp4").exists());
    assert_keepers_intact(&outside_keepers);

    let recent = purge(&pool, vec![root.path().to_path_buf()], request(&[], &[&fresh])).await.unwrap();
    assert_eq!(recent.skipped[0].reason, HoldReason::RecentlyActive);
    assert!(fresh.join("audio.mp4").exists());
}

#[tokio::test]
async fn delete_meeting_removes_the_rows_and_the_folder() {
    let pool = test_pool().await;
    let root = tempfile::tempdir().unwrap();
    let (folder, _) = recorded(root.path(), "Gone");
    insert_meeting(&pool, "del-gone", Some(&folder)).await;
    insert_transcript(&pool, "del-gone").await;
    sqlx::query(
        "INSERT INTO background_jobs (id, kind, title, meeting_id, folder_path, interrupted, created_at)
         VALUES ('del-gone', 'retranscription', 't', 'del-gone', NULL, 1, '2026-07-24T10:00:00Z')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let report = delete_meeting(&pool, vec![root.path().to_path_buf()], "del-gone").await.unwrap();
    assert!(matches!(report, DeleteMeetingReport::Removed { freed_bytes } if freed_bytes > 1000));
    assert!(!folder.exists());
    assert!(root.path().exists());
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM meetings WHERE id = ?", "del-gone").await, 0);
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM background_jobs WHERE meeting_id = ?", "del-gone").await, 0);

    assert!(delete_meeting(&pool, vec![root.path().to_path_buf()], "del-gone").await.is_err());
}

#[tokio::test]
async fn delete_meeting_keeps_shared_and_busy_folders() {
    let _serial = global_coordinator_test_lock();
    let pool = test_pool().await;
    let root = tempfile::tempdir().unwrap();
    let (shared, shared_keepers) = recorded(root.path(), "Shared");
    insert_meeting(&pool, "del-s1", Some(&shared)).await;
    insert_meeting(&pool, "del-s2", Some(&shared)).await;
    let report = delete_meeting(&pool, vec![root.path().to_path_buf()], "del-s1").await.unwrap();
    assert_eq!(report, DeleteMeetingReport::KeptShared);
    assert!(shared.join("audio.mp4").exists());
    assert_keepers_intact(&shared_keepers);
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM meetings WHERE id = ?", "del-s1").await, 0);

    let (busy, busy_keepers) = recorded(root.path(), "Busy");
    insert_meeting(&pool, "del-busy", Some(&busy)).await;
    let held = crate::audio::retranscription::reserve_meeting_audio("del-busy").unwrap();
    let report = delete_meeting(&pool, vec![root.path().to_path_buf()], "del-busy").await.unwrap();
    drop(held);
    assert_eq!(report, DeleteMeetingReport::KeptBusy);
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM meetings WHERE id = ?", "del-busy").await, 0);
    assert!(busy.join("audio.mp4").exists());
    assert_keepers_intact(&busy_keepers);

    let index = FolderIndex::load(&pool, vec![root.path().to_path_buf()]).await.unwrap();
    let listed = orphans::find_orphans(&index, &Activity::idle(), later());
    assert!(listed.iter().any(|o| FolderKey::of(&o.folder) == FolderKey::of(&busy)), "the kept folder is listed as an orphan");
}

#[tokio::test]
async fn delete_meeting_keeps_a_folder_that_is_not_a_meeting_folder() {
    let pool = test_pool().await;
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("audio.mp4"), b"x").unwrap();
    std::fs::write(root.path().join("metadata.json"), b"{}").unwrap();
    insert_meeting(&pool, "del-root", Some(root.path())).await;
    let report = delete_meeting(&pool, vec![root.path().to_path_buf()], "del-root").await.unwrap();
    assert_eq!(report, DeleteMeetingReport::KeptUnrecognized);
    assert!(root.path().join("audio.mp4").exists());

    insert_meeting(&pool, "del-none", None).await;
    let report = delete_meeting(&pool, vec![root.path().to_path_buf()], "del-none").await.unwrap();
    assert_eq!(report, DeleteMeetingReport::None);
}

#[test]
fn wire_shapes_match_the_frontend_types() {
    let status = serde_json::to_value(AudioStatus::Held { bytes: 5, reason: HoldReason::Recording }).unwrap();
    assert_eq!(status["state"], "held");
    assert_eq!(status["bytes"], 5);
    assert_eq!(status["reason"]["code"], "recording");
    let absent = serde_json::to_value(AudioStatus::Absent { reason: AbsentReason::FolderMissing }).unwrap();
    assert_eq!(absent["state"], "absent");
    assert_eq!(absent["reason"]["code"], "folder_missing");
    let report = serde_json::to_value(DeleteMeetingReport::KeptForeignFiles { freed_bytes: 3, path: "p".into() }).unwrap();
    assert_eq!(report["folder"], "kept_foreign_files");
    assert_eq!(report["freed_bytes"], 3);
    let request: PurgeRequest =
        serde_json::from_value(serde_json::json!({ "meetingIds": ["a"], "orphanFolders": [] })).unwrap();
    assert_eq!(request.meeting_ids, vec!["a"]);
}
