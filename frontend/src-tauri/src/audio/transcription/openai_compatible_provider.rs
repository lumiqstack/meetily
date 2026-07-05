// audio/transcription/openai_compatible_provider.rs
//
// Remote transcription provider for any OpenAI-compatible server exposing
// POST {base}/v1/audio/transcriptions (OpenAI, oMLX, LiteLLM, vLLM, custom
// gateways, etc.). Audio is WAV-encoded in memory and sent as multipart form
// data, mirroring the official OpenAI Whisper API contract.

use super::provider::{TranscriptionError, TranscriptionProvider, TranscriptResult};
use async_trait::async_trait;
use log::{info, warn};
use serde::Deserialize;
use std::time::Duration;

/// Request timeout for a single chunk. Chunks are at most ~25s of audio, but a
/// remote server may queue requests behind other inference work.
const REQUEST_TIMEOUT_SECS: u64 = 120;

pub struct OpenAICompatibleProvider {
    client: reqwest::Client,
    /// Fully resolved transcriptions endpoint, e.g. "http://127.0.0.1:8000/v1/audio/transcriptions"
    endpoint: String,
    model: String,
    api_key: Option<String>,
}

#[derive(Deserialize)]
struct TranscriptionResponse {
    text: String,
}

/// Normalize a user-supplied base URL into the full transcriptions endpoint.
/// Accepts "http://host:port", "http://host:port/v1", or the full endpoint.
pub fn resolve_transcriptions_endpoint(base_url: &str) -> String {
    let trimmed = base_url.trim().trim_end_matches('/');
    if trimmed.ends_with("/audio/transcriptions") {
        trimmed.to_string()
    } else if trimmed.ends_with("/v1") {
        format!("{}/audio/transcriptions", trimmed)
    } else {
        format!("{}/v1/audio/transcriptions", trimmed)
    }
}

/// Encode 16kHz mono f32 samples as a 16-bit PCM WAV file in memory.
fn encode_wav_pcm16(samples: &[f32], sample_rate: u32) -> Vec<u8> {
    let num_samples = samples.len() as u32;
    let data_size = num_samples * 2; // 16-bit mono
    let byte_rate = sample_rate * 2;

    let mut wav = Vec::with_capacity(44 + data_size as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_size).to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&byte_rate.to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes()); // block align
    wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_size.to_le_bytes());

    for &sample in samples {
        let clamped = (sample.clamp(-1.0, 1.0) * 32767.0) as i16;
        wav.extend_from_slice(&clamped.to_le_bytes());
    }

    wav
}

impl OpenAICompatibleProvider {
    pub fn new(base_url: &str, model: String, api_key: Option<String>) -> Result<Self, String> {
        let endpoint = resolve_transcriptions_endpoint(base_url);

        reqwest::Url::parse(&endpoint)
            .map_err(|e| format!("Invalid transcription endpoint URL '{}': {}", endpoint, e))?;

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .build()
            .map_err(|e| format!("Failed to build HTTP client: {}", e))?;

        info!(
            "🌐 OpenAI-compatible transcription provider ready - endpoint: {}, model: {}",
            endpoint, model
        );

        Ok(Self {
            client,
            endpoint,
            model,
            api_key: api_key.filter(|k| !k.is_empty()),
        })
    }

    /// Build a provider from the saved transcript settings in the database.
    /// Used by batch flows (retranscription, import) that receive only a
    /// provider/model pair from the frontend.
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
            .openai_compatible_base_url
            .as_deref()
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .ok_or("Remote transcription endpoint is not configured. Please set the base URL in transcript settings.")?
            .to_string();

        let model = model_override
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| setting.model.clone());

        Self::new(&base_url, model, setting.openai_compatible_api_key)
    }
}

#[async_trait]
impl TranscriptionProvider for OpenAICompatibleProvider {
    async fn transcribe(
        &self,
        audio: Vec<f32>,
        language: Option<String>,
    ) -> std::result::Result<TranscriptResult, TranscriptionError> {
        const MIN_SAMPLES: usize = 1600; // 100ms at 16kHz
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
            .text("model", self.model.clone())
            .text("response_format", "json");

        if let Some(lang) = language.filter(|l| !l.is_empty() && l != "auto") {
            form = form.text("language", lang);
        }

        let mut request = self.client.post(&self.endpoint).multipart(form);
        if let Some(key) = &self.api_key {
            request = request.bearer_auth(key);
        }

        let response = request.send().await.map_err(|e| {
            TranscriptionError::EngineFailed(format!(
                "Request to {} failed: {}",
                self.endpoint, e
            ))
        })?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            let snippet: String = body.chars().take(300).collect();
            warn!(
                "Remote transcription failed - status: {}, body: {}",
                status, snippet
            );
            return Err(TranscriptionError::EngineFailed(format!(
                "Server returned {}: {}",
                status, snippet
            )));
        }

        let parsed: TranscriptionResponse = response.json().await.map_err(|e| {
            TranscriptionError::EngineFailed(format!("Invalid response JSON: {}", e))
        })?;

        Ok(TranscriptResult {
            text: parsed.text.trim().to_string(),
            confidence: None, // OpenAI-compatible endpoints don't return confidence
            is_partial: false,
        })
    }

    async fn is_model_loaded(&self) -> bool {
        // Remote server manages its own model lifecycle
        true
    }

    async fn get_current_model(&self) -> Option<String> {
        Some(self.model.clone())
    }

    fn provider_name(&self) -> &'static str {
        "OpenAI-compatible"
    }
}
