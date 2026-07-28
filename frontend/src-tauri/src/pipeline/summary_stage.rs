//! Summary stage: the Rust equivalent of what `PendingWorkPanel` used to do
//! from React, minus the polling. `process_transcript_background` is awaited
//! directly, and the Obsidian export rides along inside it on success.

use crate::database::models::Transcript;
use crate::database::repositories::meeting::MeetingsRepository;
use crate::database::repositories::setting::SettingsRepository;
use crate::database::repositories::summary::SummaryProcessesRepository;
use crate::database::repositories::transcript_chunk::TranscriptChunksRepository;
use crate::pipeline::settings::PipelineSettings;
use crate::pipeline::StageError;
use crate::summary::SummaryService;
use sqlx::SqlitePool;
use std::path::Path;
use tauri::{AppHandle, Runtime};

/// Format transcript segments exactly the way the frontend's
/// `buildSummaryTranscriptPayload` (`src/lib/summary-payload.ts`) does:
/// `[MM:SS] Speaker: text` per segment, single newline between segments,
/// falling back to the stored wall-clock timestamp for rows without audio
/// offsets. Speaker tags map through the same table as `displaySpeaker`.
///
/// Note this is deliberately NOT `obsidian::format_transcript_markdown`,
/// which joins with a blank line for readability in a note.
pub fn build_summary_transcript_text(segments: &[Transcript]) -> String {
    segments
        .iter()
        .map(|segment| {
            let time = match segment.audio_start_time {
                Some(seconds) => {
                    let total_secs = seconds.max(0.0) as u64;
                    format!("[{:02}:{:02}]", total_secs / 60, total_secs % 60)
                }
                None => segment.timestamp.clone(),
            };
            let speaker = match segment.speaker.as_deref() {
                Some("mic") => Some("Me"),
                Some("system") => Some("Others"),
                Some(name) => Some(name),
                None => None,
            };
            match speaker {
                Some(speaker) => format!("{} {}: {}", time, speaker, segment.transcript),
                None => format!("{} {}", time, segment.transcript),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Provider failures worth retrying later rather than counting against the
/// attempt budget: the summariser being unreachable says nothing about the
/// meeting.
fn is_transient_provider_error(error: &str) -> bool {
    let error = error.to_lowercase();
    [
        "connection",
        "connect",
        "timed out",
        "timeout",
        "network",
        "unreachable",
        "refused",
        "dns",
        "temporarily",
        "503",
        "502",
        "504",
        "429",
        "rate limit",
        "overloaded",
    ]
    .iter()
    .any(|needle| error.contains(needle))
}

/// Read the user's explicit per-meeting summary language, if they set one.
/// Auto-detection is handled inside `process_transcript_background`, so
/// `None` here means "auto", matching the frontend's behaviour.
async fn resolve_summary_language(pool: &SqlitePool, meeting_id: &str) -> Option<String> {
    let meeting = MeetingsRepository::get_meeting_metadata(pool, meeting_id)
        .await
        .ok()
        .flatten()?;
    let folder = meeting.folder_path.filter(|p| !p.trim().is_empty())?;
    crate::summary::metadata::read_summary_language_from_metadata(Path::new(&folder))
        .ok()
        .flatten()
}

/// Generate the AI summary for a meeting and wait for it to finish.
pub async fn run_summary_stage<R: Runtime>(
    app: &AppHandle<R>,
    pool: &SqlitePool,
    meeting_id: &str,
    settings: &PipelineSettings,
) -> Result<(), StageError> {
    let (segments, _total) =
        MeetingsRepository::get_meeting_transcripts_paginated(pool, meeting_id, i64::MAX, 0)
            .await
            .map_err(|e| StageError::hard(format!("Failed to load transcripts: {}", e)))?;

    if segments.is_empty() {
        return Err(StageError::hard("No transcripts available for summary"));
    }

    let text = build_summary_transcript_text(&segments);

    let model_config = SettingsRepository::get_model_config(pool)
        .await
        .map_err(|e| StageError::hard(format!("Failed to read model config: {}", e)))?
        .ok_or_else(|| {
            StageError::hard("No summary model configured — set one in Settings first")
        })?;

    let summary_language = resolve_summary_language(pool, meeting_id).await;

    SummaryProcessesRepository::create_or_reset_process(pool, meeting_id)
        .await
        .map_err(|e| StageError::hard(format!("Failed to initialize summary process: {}", e)))?;

    TranscriptChunksRepository::save_transcript_data(
        pool,
        meeting_id,
        &text,
        &model_config.provider,
        &model_config.model,
        settings.summary_chunk_size,
        settings.summary_overlap,
    )
    .await
    .map_err(|e| StageError::hard(format!("Failed to save transcript data: {}", e)))?;

    // Awaited in-process: no status polling, and the Obsidian auto-export
    // happens inside on success.
    SummaryService::process_transcript_background(
        app.clone(),
        pool.clone(),
        meeting_id.to_string(),
        text,
        model_config.provider.clone(),
        model_config.model.clone(),
        String::new(),
        settings.summary_template_id.clone(),
        summary_language,
    )
    .await;

    // The service reports outcomes through the process row.
    let status = sqlx::query_scalar::<_, String>(
        "SELECT status FROM summary_processes WHERE meeting_id = ?",
    )
    .bind(meeting_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| StageError::transient(format!("Failed to read summary status: {}", e)))?;

    match status.as_deref().map(str::to_lowercase).as_deref() {
        Some("completed") => Ok(()),
        Some("cancelled") => Err(StageError::transient("Summary generation was cancelled")),
        _ => {
            let error = sqlx::query_scalar::<_, Option<String>>(
                "SELECT error FROM summary_processes WHERE meeting_id = ?",
            )
            .bind(meeting_id)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten()
            .flatten()
            .unwrap_or_else(|| "Summary generation failed".to_string());

            if is_transient_provider_error(&error) {
                Err(StageError::transient(error))
            } else {
                Err(StageError::hard(error))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segment(
        text: &str,
        speaker: Option<&str>,
        audio_start_time: Option<f64>,
        timestamp: &str,
    ) -> Transcript {
        Transcript {
            id: String::new(),
            meeting_id: String::new(),
            transcript: text.to_string(),
            timestamp: timestamp.to_string(),
            summary: None,
            action_items: None,
            key_points: None,
            audio_start_time,
            audio_end_time: None,
            duration: None,
            speaker: speaker.map(|s| s.to_string()),
        }
    }

    #[test]
    fn summary_text_matches_the_frontend_payload_format() {
        let segments = vec![
            segment("hello there", Some("mic"), Some(65.9), "10:00:00"),
            segment("hi back", Some("system"), Some(70.0), "10:00:05"),
            segment("named speaker", Some("Alice Smith"), Some(0.0), "10:00:10"),
            segment("no speaker, old row", None, None, "10:00:15"),
        ];

        // Single newline join, unlike the Obsidian note format.
        assert_eq!(
            build_summary_transcript_text(&segments),
            "[01:05] Me: hello there\n\
             [01:10] Others: hi back\n\
             [00:00] Alice Smith: named speaker\n\
             10:00:15 no speaker, old row"
        );
    }

    #[test]
    fn endpoint_outages_are_transient_but_bad_config_is_not() {
        assert!(is_transient_provider_error("error sending request: connection refused"));
        assert!(is_transient_provider_error("HTTP 503 Service Unavailable"));
        assert!(is_transient_provider_error("Request timed out"));
        assert!(!is_transient_provider_error("Failed to load template 'nope'"));
        assert!(!is_transient_provider_error("invalid api key"));
    }
}
