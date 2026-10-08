//! Transcription stage: re-runs transcription for an imported or recovered
//! meeting that has audio but no transcripts.
//!
//! Local (on-device Whisper/Parakeet) work is idle-gated and yields to live
//! recording; remote transcription touches neither the engines nor the CPU
//! and runs whenever there is work.

use crate::audio::retranscription::start_retranscription;
use crate::audio::transcription::gemini_batch::GeminiBatchError;
use crate::pipeline::{classify_message, FailureKind, StageError, PIPELINE_RETRANSCRIPTION};
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
        // The same preference live transcription uses, so an automatic run
        // honours the language the user chose.
        crate::get_language_preference_internal(),
        config.model.clone(),
        config.provider.clone(),
        // The automatic pipeline does not ask for diarization; the user opts
        // into it per job from the retranscribe dialog.
        false,
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
            match classify_transcription_failure(&e) {
                FailureKind::Transient => Err(StageError::transient(message)),
                FailureKind::Timeout => Err(StageError::timeout(message)),
                FailureKind::Hard => Err(StageError::hard(message)),
            }
        }
    }
}

/// Phrases a transcription stage treats as retry-later: cancellation (how the
/// pipeline yields the engine to a live recording) and engine contention
/// ("someone else is using it right now").
const TRANSCRIBE_RETRY_LATER: &[&str] = &[
    "cancel",
    "on-device transcription engine",
    "already in progress",
];

/// Retry class for a failed transcription attempt. Gemini batch failures carry
/// their own type and are classified structurally; other engines only surface
/// text, which goes through the same classifier as summaries.
pub(crate) fn classify_transcription_failure(error: &anyhow::Error) -> FailureKind {
    if let Some(batch) = error.downcast_ref::<GeminiBatchError>() {
        return match batch {
            GeminiBatchError::Timeout => FailureKind::Timeout,
            _ if batch.is_transient() || batch.is_cancellation() => FailureKind::Transient,
            _ => FailureKind::Hard,
        };
    }
    classify_message(&error.to_string(), TRANSCRIBE_RETRY_LATER)
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

    fn gemini(error: GeminiBatchError) -> anyhow::Error {
        anyhow::Error::new(error)
    }

    #[test]
    fn gemini_timeout_is_a_timeout() {
        assert_eq!(
            classify_transcription_failure(&gemini(GeminiBatchError::Timeout)),
            FailureKind::Timeout
        );
    }

    #[test]
    fn remote_request_timeout_is_a_timeout() {
        let error = anyhow::anyhow!(
            "Remote transcription failed on segment 3: Transcription engine failed: Remote transcription request timed out"
        );
        assert_eq!(classify_transcription_failure(&error), FailureKind::Timeout);
    }

    #[test]
    fn gateway_network_and_quota_failures_stay_transient() {
        assert_eq!(
            classify_transcription_failure(&gemini(GeminiBatchError::Transport(
                "Could not connect to the transcription gateway".to_string()
            ))),
            FailureKind::Transient
        );
        assert_eq!(
            classify_transcription_failure(&gemini(GeminiBatchError::ServerError(503))),
            FailureKind::Transient
        );
        assert_eq!(
            classify_transcription_failure(&gemini(GeminiBatchError::QuotaExceeded)),
            FailureKind::Transient
        );
        for message in [
            "Could not connect to the remote transcription server",
            "Remote transcription server returned 429 Too Many Requests",
            "Remote transcription server returned 502 Bad Gateway",
            "Remote transcription server returned 503 Service Unavailable",
            "Remote transcription server returned 504 Gateway Timeout",
        ] {
            assert_eq!(
                classify_transcription_failure(&anyhow::anyhow!(message)),
                FailureKind::Transient,
                "{message}"
            );
        }
    }

    #[test]
    fn cancellation_stays_transient() {
        assert_eq!(
            classify_transcription_failure(&gemini(GeminiBatchError::Cancelled)),
            FailureKind::Transient
        );
        assert_eq!(
            classify_transcription_failure(&anyhow::anyhow!("Retranscription cancelled")),
            FailureKind::Transient
        );
    }

    #[test]
    fn rejections_and_bad_configuration_stay_hard() {
        assert_eq!(
            classify_transcription_failure(&gemini(GeminiBatchError::Auth)),
            FailureKind::Hard
        );
        assert_eq!(
            classify_transcription_failure(&gemini(GeminiBatchError::InvalidRequest(
                "HTTP 400".to_string()
            ))),
            FailureKind::Hard
        );
        assert_eq!(
            classify_transcription_failure(&anyhow::anyhow!(
                "Remote transcription server returned 401 Unauthorized"
            )),
            FailureKind::Hard
        );
    }
}
