// audio/transcription/openai_compatible_provider.rs
//
// Remote transcription provider for any OpenAI-compatible server exposing
// POST {base}/v1/audio/transcriptions (OpenAI, oMLX, LiteLLM, vLLM, custom
// gateways, etc.). Audio is WAV-encoded in memory and sent as multipart form
// data, mirroring the official OpenAI Whisper API contract.

use super::pcm::encode_wav_pcm16;
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
    /// Vocabulary sent as the transcription `prompt`, only when the user opted
    /// in (see [`remote_vocabulary_prompt`]).
    prompt: Option<String>,
}

/// The `prompt` to send to the remote server: the user's vocabulary, only
/// when they explicitly opted in and it is non-empty. Every construction path
/// (live recording, import, retranscription) goes through this.
pub fn remote_vocabulary_prompt(opted_in: bool, vocabulary: &str) -> Option<String> {
    let vocabulary = vocabulary.trim();
    (opted_in && !vocabulary.is_empty()).then(|| vocabulary.to_string())
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
            prompt: None,
        })
    }

    /// Attach the vocabulary prompt (from [`remote_vocabulary_prompt`]).
    pub fn with_prompt(mut self, prompt: Option<String>) -> Self {
        if prompt.is_some() {
            info!("Remote transcription will include the user's vocabulary prompt (opted in)");
        }
        self.prompt = prompt;
        self
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

        Ok(Self::new(&base_url, model, setting.openai_compatible_api_key)?.with_prompt(
            remote_vocabulary_prompt(setting.remote_vocabulary_enabled, &setting.whisper_vocabulary_hint),
        ))
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
        if let Some(prompt) = &self.prompt {
            form = form.text("prompt", prompt.clone());
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
            // A server that does not accept `prompt` must not be treated as if
            // the vocabulary were in use: say so, and how to turn it off.
            if self.prompt.is_some()
                && (status == reqwest::StatusCode::BAD_REQUEST
                    || status == reqwest::StatusCode::UNPROCESSABLE_ENTITY)
            {
                return Err(TranscriptionError::EngineFailed(format!(
                    "The remote transcription server rejected the request ({}) while the vocabulary prompt was enabled; \
                     it may not support the `prompt` field. Turn off \"Send vocabulary to the remote server\" in \
                     transcription settings and retry. Server said: {}",
                    status, snippet
                )));
            }
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

#[cfg(test)]
mod prompt_tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// One-request-at-a-time HTTP fixture that records each request body and
    /// answers with `status` and `body`.
    async fn server(status: u16, body: &'static str) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = seen.clone();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut raw = Vec::new();
                let mut buf = [0u8; 8192];
                // Read headers, then exactly Content-Length body bytes.
                let (header_end, length) = loop {
                    let n = socket.read(&mut buf).await.unwrap();
                    raw.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&raw).to_string();
                    if let Some(end) = text.find("\r\n\r\n") {
                        let length = text[..end]
                            .lines()
                            .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap()))
                            .unwrap_or(0);
                        break (end + 4, length);
                    }
                };
                while raw.len() < header_end + length {
                    let n = socket.read(&mut buf).await.unwrap();
                    if n == 0 { break; }
                    raw.extend_from_slice(&buf[..n]);
                }
                record.lock().unwrap().push(String::from_utf8_lossy(&raw[header_end..]).to_string());
                let response = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });
        (format!("http://{addr}"), seen)
    }

    fn audio() -> Vec<f32> {
        vec![0.0; 3200]
    }

    #[test]
    fn prompt_is_only_built_for_an_explicit_non_empty_opt_in() {
        assert_eq!(remote_vocabulary_prompt(false, "Acme, Zephyr"), None);
        assert_eq!(remote_vocabulary_prompt(true, "   "), None);
        assert_eq!(remote_vocabulary_prompt(true, " Acme, Zephyr "), Some("Acme, Zephyr".to_string()));
    }

    #[tokio::test]
    async fn default_requests_carry_no_vocabulary() {
        let (base, seen) = server(200, r#"{"text":"hello"}"#).await;
        let provider = OpenAICompatibleProvider::new(&base, "m".into(), None)
            .unwrap()
            .with_prompt(remote_vocabulary_prompt(false, "Acme, Zephyr"));
        assert_eq!(provider.transcribe(audio(), None).await.unwrap().text, "hello");
        let body = seen.lock().unwrap()[0].clone();
        assert!(!body.contains("name=\"prompt\""));
        assert!(!body.contains("Zephyr"));
    }

    #[tokio::test]
    async fn opted_in_requests_send_the_vocabulary_as_prompt() {
        let (base, seen) = server(200, r#"{"text":"hello"}"#).await;
        let provider = OpenAICompatibleProvider::new(&base, "m".into(), None)
            .unwrap()
            .with_prompt(remote_vocabulary_prompt(true, "Acme, Zephyr"));
        provider.transcribe(audio(), None).await.unwrap();
        let body = seen.lock().unwrap()[0].clone();
        assert!(body.contains("name=\"prompt\"\r\n\r\nAcme, Zephyr"), "{body}");
    }

    #[tokio::test]
    async fn a_server_rejecting_the_prompt_gets_an_actionable_error() {
        let (base, _seen) = server(400, r#"{"error":"unknown field prompt"}"#).await;
        let provider = OpenAICompatibleProvider::new(&base, "m".into(), None)
            .unwrap()
            .with_prompt(Some("Acme".to_string()));
        let error = provider.transcribe(audio(), None).await.unwrap_err().to_string();
        assert!(error.contains("Send vocabulary to the remote server"), "{error}");
    }
}
