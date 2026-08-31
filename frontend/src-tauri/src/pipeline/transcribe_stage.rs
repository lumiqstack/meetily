//! Transcription stage: re-runs transcription for an imported or recovered
//! meeting that has audio but no transcripts.
//!
//! Local (on-device Whisper/Parakeet) work is idle-gated and yields to live
//! recording; remote transcription touches neither the engines nor the CPU
//! and runs whenever there is work.

use crate::audio::retranscription::start_retranscription;
use crate::pipeline::{StageError, PIPELINE_RETRANSCRIPTION};
use sqlx::SqlitePool;
use tauri::{AppHandle, Runtime};

/// Transcription provider/model as configured by the user.
pub struct TranscriptConfig {
    pub provider: Option<String>,
    pub model: Option<String>,
}

impl TranscriptConfig {
    /// Remote transcription does not use the shared local engines, so it is
    /// exempt from idle gating and recording preemption.
    pub fn is_remote(&self) -> bool {
        self.provider
            .as_deref()
            .is_some_and(crate::config::is_remote_transcription_provider)
    }
}

pub async fn load_transcript_config(pool: &SqlitePool) -> TranscriptConfig {
    match sqlx::query_as::<_, (String, String)>(
        "SELECT provider, model FROM transcript_settings WHERE id = '1'",
    )
    .fetch_optional(pool)
    .await
    {
        Ok(Some((provider, model))) => TranscriptConfig {
            provider: Some(provider),
            model: Some(model),
        },
        Ok(None) => TranscriptConfig {
            provider: None,
            model: None,
        },
        Err(e) => {
            log::warn!("Pipeline: failed to read transcript settings: {}", e);
            TranscriptConfig {
                provider: None,
                model: None,
            }
        }
    }
}

/// Transcribe a meeting's recording and wait for it to finish.
///
/// The meeting is registered as pipeline-owned for the duration, so a live
/// recording starting mid-job can cancel it (see
/// `pipeline::yield_engine_for_recording`) instead of failing to start.
pub async fn run_transcribe_stage<R: Runtime>(
    app: &AppHandle<R>,
    meeting_id: &str,
    folder_path: &str,
    config: &TranscriptConfig,
) -> Result<(), StageError> {
    PIPELINE_RETRANSCRIPTION.claim(meeting_id);

    let result = start_retranscription(
        app.clone(),
        meeting_id.to_string(),
        folder_path.to_string(),
        None,
        config.model.clone(),
        config.provider.clone(),
    )
    .await;

    PIPELINE_RETRANSCRIPTION.release(meeting_id);

    match result {
        Ok(outcome) => {
            log::info!(
                "[pipeline] transcribed meeting {} ({} segments, {:.1}s audio)",
                meeting_id,
                outcome.segments_count,
                outcome.duration_seconds
            );
            Ok(())
        }
        Err(e) => {
            let message = e.to_string();
            // Cancellation is how the pipeline yields the engine to a live
            // recording, and engine contention means "someone else is using
            // it right now" — both should simply be retried later.
            let transient = message.to_lowercase().contains("cancel")
                || message.contains("on-device transcription engine")
                || message.contains("already in progress");
            if transient {
                Err(StageError::transient(message))
            } else {
                Err(StageError::hard(message))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_for(provider: Option<&str>) -> TranscriptConfig {
        TranscriptConfig {
            provider: provider.map(str::to_string),
            model: None,
        }
    }

    #[test]
    fn every_remote_provider_counts_as_remote() {
        assert!(config_for(Some("openaiCompatible")).is_remote());
        assert!(config_for(Some("geminiTranscribe")).is_remote());
    }

    #[test]
    fn local_engines_and_unset_do_not_count_as_remote() {
        assert!(!config_for(Some("localWhisper")).is_remote());
        assert!(!config_for(Some("parakeet")).is_remote());
        assert!(!config_for(None).is_remote());
    }
}
