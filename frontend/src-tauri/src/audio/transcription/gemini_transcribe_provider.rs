// audio/transcription/gemini_transcribe_provider.rs
//
// Batch transcription through the hermes proxy, used for imports,
// retranscription, recovery, and any post-meeting pass. Audio is WAV-encoded
// in memory and posted as multipart form data to {base}/v1/transcriptions.
//
// The bearer token authenticates Meetily to *the proxy*. The proxy holds the
// Gemini credential and performs the Files API upload and model call upstream,
// so no Google key is ever stored on this machine.
//
// Live recording does not use this provider; see hermes_live_session.rs.

use super::hermes_endpoints::resolve_rest_endpoint;
use super::pcm::encode_wav_pcm16;
use super::provider::{TranscriptionError, TranscriptionProvider, TranscriptResult};
use async_trait::async_trait;
use log::{info, warn};
use std::time::Duration;

/// Request timeout for a single segment. Segments are at most ~25s of audio
/// (see import.rs MAX_SEGMENT_SAMPLES), but the gateway uploads to the Files
/// API before transcribing, and may queue behind other work.
const REQUEST_TIMEOUT_SECS: u64 = 180;

/// Below this the audio is too short to be worth a round trip.
const MIN_SAMPLES: usize = 1600; // 100ms at 16kHz

pub struct GeminiTranscribeProvider {
    client: reqwest::Client,
    endpoint: String,
    model: String,
    bearer_token: String,
}

impl GeminiTranscribeProvider {
    pub fn new(base_url: &str, model: String, bearer_token: Option<String>) -> Result<Self, String> {
        let endpoint = resolve_rest_endpoint(base_url)?;

        let bearer_token = bearer_token
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .ok_or_else(|| {
                "Gemini transcription requires the proxy bearer token. Set it in transcript settings."
                    .to_string()
            })?;

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .build()
            .map_err(|e| format!("Failed to build HTTP client: {}", e))?;

        let model = if model.trim().is_empty() {
            crate::config::GEMINI_BATCH_MODEL.to_string()
        } else {
            model
        };

        info!(
            "✨ Gemini transcription provider ready - endpoint: {}, model: {}",
            endpoint, model
        );

        Ok(Self {
            client,
            endpoint,
            model,
            bearer_token,
        })
    }

    /// Build a provider from saved transcript settings. Used by the batch
    /// flows (import, retranscription), which receive only a provider/model
    /// pair from the frontend.
    pub async fn from_saved_settings<R: tauri::Runtime>(
        app: &tauri::AppHandle<R>,
        model_override: Option<String>,
    ) -> Result<Self, String> {
        use tauri::Manager;

        let state = app
            .try_state::<crate::state::AppState>()
            .ok_or("App state not available")?;
        let pool = state.db_manager.pool();

        let setting =
            crate::database::repositories::setting::SettingsRepository::get_transcript_config(pool)
                .await
                .map_err(|e| format!("Failed to load transcript settings: {}", e))?
                .ok_or("No transcript settings saved")?;

        let base_url = setting
            .gemini_transcribe_base_url
            .as_deref()
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .ok_or(
                "Gemini transcription endpoint is not configured. Set the proxy base URL in transcript settings.",
            )?
            .to_string();

        let model = model_override
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| crate::config::GEMINI_BATCH_MODEL.to_string());

        Self::new(&base_url, model, setting.gemini_transcribe_api_key)
    }
}

/// Pull the transcript out of the gateway response.
///
/// The gateway contract is not pinned down yet, so accept the common spellings
/// and fail loudly with the body otherwise — a schema mismatch must be
/// obvious, not silently transcribe every segment as empty.
fn extract_transcript(body: &str) -> Result<String, TranscriptionError> {
    let parsed: serde_json::Value = serde_json::from_str(body).map_err(|e| {
        TranscriptionError::EngineFailed(format!(
            "Invalid response JSON: {} (body: {})",
            e,
            snippet(body)
        ))
    })?;

    for key in ["text", "transcript"] {
        if let Some(text) = parsed.get(key).and_then(|v| v.as_str()) {
            return Ok(text.trim().to_string());
        }
    }

    Err(TranscriptionError::EngineFailed(format!(
        "Response contained no 'text' or 'transcript' field (body: {})",
        snippet(body)
    )))
}

fn snippet(body: &str) -> String {
    body.chars().take(300).collect()
}

#[async_trait]
impl TranscriptionProvider for GeminiTranscribeProvider {
    async fn transcribe(
        &self,
        audio: Vec<f32>,
        language: Option<String>,
    ) -> std::result::Result<TranscriptResult, TranscriptionError> {
        if audio.len() < MIN_SAMPLES {
            return Err(TranscriptionError::AudioTooShort {
                samples: audio.len(),
                minimum: MIN_SAMPLES,
            });
        }

        let wav_bytes = encode_wav_pcm16(&audio, 16000);

        let file_part = reqwest::multipart::Part::bytes(wav_bytes)
            .file_name("audio.wav")
            .mime_str("audio/wav")
            .map_err(|e| TranscriptionError::EngineFailed(format!("Invalid mime type: {}", e)))?;

        let mut form = reqwest::multipart::Form::new()
            .part("file", file_part)
            .text("model", self.model.clone());

        if let Some(lang) = language.filter(|l| !l.is_empty() && l != "auto") {
            form = form.text("language", lang);
        }

        let response = self
            .client
            .post(&self.endpoint)
            .bearer_auth(&self.bearer_token)
            .multipart(form)
            .send()
            .await
            .map_err(|e| {
                TranscriptionError::EngineFailed(format!(
                    "Request to {} failed: {}",
                    self.endpoint, e
                ))
            })?;

        let status = response.status();
        let body = response.text().await.unwrap_or_default();

        if !status.is_success() {
            warn!(
                "Gemini transcription failed - status: {}, body: {}",
                status,
                snippet(&body)
            );
            // 401/403 almost always means the proxy bearer token is wrong;
            // say so rather than surfacing a bare status code.
            let hint = if status == reqwest::StatusCode::UNAUTHORIZED
                || status == reqwest::StatusCode::FORBIDDEN
            {
                " — check the proxy bearer token in transcript settings"
            } else {
                ""
            };
            return Err(TranscriptionError::EngineFailed(format!(
                "Server returned {}{}: {}",
                status,
                hint,
                snippet(&body)
            )));
        }

        Ok(TranscriptResult {
            text: extract_transcript(&body)?,
            confidence: None, // The gateway does not return per-segment confidence.
            is_partial: false,
        })
    }

    async fn is_model_loaded(&self) -> bool {
        // The gateway manages the model; nothing to load locally.
        true
    }

    async fn get_current_model(&self) -> Option<String> {
        Some(self.model.clone())
    }

    fn provider_name(&self) -> &'static str {
        "Gemini (hermes)"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "https://host.ts.net/google-transcribe";

    #[test]
    fn reads_the_text_field() {
        assert_eq!(
            extract_transcript(r#"{"text":"  hello there  "}"#).unwrap(),
            "hello there"
        );
    }

    #[test]
    fn falls_back_to_the_transcript_field() {
        assert_eq!(
            extract_transcript(r#"{"transcript":"hello"}"#).unwrap(),
            "hello"
        );
    }

    #[test]
    fn extra_fields_are_ignored() {
        assert_eq!(
            extract_transcript(r#"{"text":"hi","model":"gemini-3.5-transcribe","usage":{}}"#)
                .unwrap(),
            "hi"
        );
    }

    #[test]
    fn an_unrecognized_shape_fails_loudly_with_the_body() {
        // Silently returning "" here would transcribe whole meetings as empty.
        let err = extract_transcript(r#"{"result":{"output":"hello"}}"#).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("no 'text' or 'transcript'"), "{}", message);
        assert!(message.contains("output"), "{}", message);
    }

    #[test]
    fn non_json_fails_with_the_body() {
        let err = extract_transcript("<html>502 Bad Gateway</html>").unwrap_err();
        assert!(err.to_string().contains("502"), "{}", err);
    }

    #[test]
    fn empty_transcript_is_allowed() {
        // Silence legitimately transcribes to nothing.
        assert_eq!(extract_transcript(r#"{"text":""}"#).unwrap(), "");
    }

    #[test]
    fn bearer_token_is_required() {
        // `.err()` rather than `.unwrap_err()`: the Ok type holds a
        // reqwest::Client and is not Debug.
        let err = GeminiTranscribeProvider::new(BASE, "m".into(), None)
            .err()
            .expect("a missing bearer token must be rejected");
        assert!(err.contains("bearer token"), "{}", err);
        assert!(
            GeminiTranscribeProvider::new(BASE, "m".into(), Some("   ".into())).is_err(),
            "whitespace-only token must be rejected"
        );
    }

    #[test]
    fn blank_model_falls_back_to_the_batch_default() {
        let provider =
            GeminiTranscribeProvider::new(BASE, String::new(), Some("tok".into())).unwrap();
        assert_eq!(provider.model, crate::config::GEMINI_BATCH_MODEL);
    }

    #[test]
    fn endpoint_is_derived_once_at_construction() {
        let provider =
            GeminiTranscribeProvider::new(BASE, "m".into(), Some("tok".into())).unwrap();
        assert_eq!(
            provider.endpoint,
            "https://host.ts.net/google-transcribe/v1/transcriptions"
        );
    }

    #[tokio::test]
    async fn audio_shorter_than_the_minimum_is_rejected_before_any_request() {
        let provider =
            GeminiTranscribeProvider::new(BASE, "m".into(), Some("tok".into())).unwrap();
        let err = provider.transcribe(vec![0.0; 10], None).await.unwrap_err();
        assert!(matches!(err, TranscriptionError::AudioTooShort { .. }));
    }
}
