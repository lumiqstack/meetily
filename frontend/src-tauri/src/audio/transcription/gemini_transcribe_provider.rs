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

use super::gemini_batch::{
    classify_status, classify_transport, parse_duration_secs, GeminiBatchError, GeminiBatchOptions,
    GeminiWord, UPLOAD_MIME, UPLOAD_TIMEOUT,
};
use super::hermes_endpoints::resolve_rest_endpoint;
use super::pcm::encode_wav_pcm16;
use super::provider::{TranscriptionError, TranscriptionProvider, TranscriptResult};
use async_trait::async_trait;
use log::{info, warn};
use serde::Deserialize;
use std::path::Path;
use tokio_util::sync::CancellationToken;

/// Transcription mode.
///
/// Deliberately a constant rather than a parameter: annotations combined with
/// `smart` are rejected with 422, and hard-coding `verbatim` makes that
/// combination unrepresentable instead of merely discouraged.
const MODE_VERBATIM: &str = "verbatim";

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

        // Sized for the batch path: a ~29 MB upload plus the gateway
        // transcribing an hour of audio. It is a ceiling, not a wait, so short
        // per-segment requests are unaffected.
        let client = reqwest::Client::builder()
            .timeout(UPLOAD_TIMEOUT)
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

// ============================================================================
// BATCH FILE UPLOAD
// ============================================================================

/// Gateway response. `words` is absent unless annotations were requested.
#[derive(Debug, Deserialize)]
struct BatchResponse {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    transcript: Option<String>,
    #[serde(default)]
    words: Option<Vec<WireWord>>,
}

#[derive(Debug, Deserialize)]
struct WireWord {
    text: String,
    start_offset: String,
    end_offset: String,
    #[serde(default)]
    speaker: Option<String>,
}

/// One uploaded chunk's result.
#[derive(Debug)]
pub struct BatchUploadResult {
    pub text: String,
    /// Offsets already parsed to seconds, relative to *this upload*.
    pub words: Option<Vec<GeminiWord>>,
}

impl GeminiTranscribeProvider {
    /// Upload one prepared audio file and return its transcript.
    ///
    /// The body is streamed from disk rather than buffered: a chunk is ~29 MB
    /// and reading it into memory would cost that per concurrent job for no
    /// benefit. Races `cancel` so a cancelled job stops uploading promptly
    /// instead of finishing a multi-minute transfer nobody wants.
    pub async fn upload_file(
        &self,
        path: &Path,
        options: &GeminiBatchOptions,
        cancel: &CancellationToken,
    ) -> Result<BatchUploadResult, GeminiBatchError> {
        let file = tokio::fs::File::open(path).await.map_err(|e| {
            GeminiBatchError::Transcode(format!("could not open {}: {}", path.display(), e))
        })?;
        let length = file
            .metadata()
            .await
            .map_err(|e| GeminiBatchError::Transcode(format!("could not stat upload: {}", e)))?
            .len();

        let stream = tokio_util::codec::FramedRead::new(file, tokio_util::codec::BytesCodec::new());
        // Length is declared so the gateway sees a normal Content-Length
        // multipart part rather than a chunked stream.
        let part = reqwest::multipart::Part::stream_with_length(reqwest::Body::wrap_stream(stream), length)
            .file_name("audio.mp3")
            .mime_str(UPLOAD_MIME)
            .map_err(|e| GeminiBatchError::Transport(format!("invalid mime: {}", e)))?;

        let mut form = reqwest::multipart::Form::new()
            .part("file", part)
            .text("model", self.model.clone())
            .text("mode", MODE_VERBATIM)
            .text("diarization", options.diarization.to_string())
            .text("word_timestamps", options.word_timestamps.to_string());

        if let Some(language) = options
            .language
            .as_deref()
            .map(str::trim)
            .filter(|l| !l.is_empty() && *l != "auto")
        {
            form = form.text("language", language.to_string());
        }

        let request = self
            .client
            .post(&self.endpoint)
            .bearer_auth(&self.bearer_token)
            .multipart(form);

        let response = tokio::select! {
            result = request.send() => result.map_err(|e| classify_transport(&e))?,
            _ = cancel.cancelled() => return Err(GeminiBatchError::Cancelled),
        };

        let status = response.status();
        let body = tokio::select! {
            result = response.text() => result.map_err(|e| classify_transport(&e))?,
            _ = cancel.cancelled() => return Err(GeminiBatchError::Cancelled),
        };

        if !status.is_success() {
            let error = classify_status(status.as_u16(), &body);
            warn!("Gemini batch upload failed: {}", error);
            return Err(error);
        }

        parse_batch_response(&body, options)
    }
}

/// Parse a successful gateway body into text plus optional words.
fn parse_batch_response(
    body: &str,
    options: &GeminiBatchOptions,
) -> Result<BatchUploadResult, GeminiBatchError> {
    let parsed: BatchResponse = serde_json::from_str(body).map_err(|e| {
        GeminiBatchError::MalformedResponse(format!("{} (body: {})", e, snippet(body)))
    })?;

    let text = parsed
        .text
        .or(parsed.transcript)
        .ok_or_else(|| {
            GeminiBatchError::MalformedResponse(format!(
                "no 'text' or 'transcript' field (body: {})",
                snippet(body)
            ))
        })?
        .trim()
        .to_string();

    let words = match parsed.words {
        Some(wire) => {
            let mut parsed_words = Vec::with_capacity(wire.len());
            for word in wire {
                parsed_words.push(GeminiWord {
                    text: word.text,
                    start_secs: parse_duration_secs(&word.start_offset)?,
                    end_secs: parse_duration_secs(&word.end_offset)?,
                    speaker: word.speaker,
                });
            }
            Some(parsed_words)
        }
        None => None,
    };

    // Silently degrading an authoritative pass to one undifferentiated blob is
    // worse than failing it: the caller asked for segmentation and would get
    // an hour-long row with no way to tell something went wrong.
    if options.word_timestamps && words.as_ref().map_or(true, |w| w.is_empty()) {
        return Err(GeminiBatchError::MissingAnnotations);
    }

    Ok(BatchUploadResult { text, words })
}

/// Pull the transcript out of the gateway response.
///
/// Verified against the live gateway: it answers
/// `{"text": "...", "model": "gemini-3.5-transcribe"}`. `transcript` is
/// accepted as a fallback spelling, and anything else fails loudly with the
/// body — a schema drift must be obvious, not silently transcribe every
/// segment as empty.
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

    /// Exactly the body the live gateway returned during smoke testing.
    #[test]
    fn parses_the_real_gateway_response() {
        assert_eq!(
            extract_transcript(
                r#"{"text":"The quarterly review meeting will start at 9:15 on Tuesday.","model":"gemini-3.5-transcribe"}"#
            )
            .unwrap(),
            "The quarterly review meeting will start at 9:15 on Tuesday."
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

    // ---- gateway contract (mock server) -----------------------------------

    mod contract {
        use super::super::*;
        use tempfile::NamedTempFile;
        use tokio_util::sync::CancellationToken;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        const OK_BODY: &str = r#"{"text":"hello there","model":"gemini-3.5-transcribe",
            "words":[{"text":"hello","start_offset":"0.100s","end_offset":"0.420s","speaker":"spk:0"},
                     {"text":"there","start_offset":"0.430s","end_offset":"0.800s","speaker":"spk:0"}]}"#;

        fn audio_file() -> NamedTempFile {
            use std::io::Write;
            let mut file = NamedTempFile::new().unwrap();
            file.write_all(b"fake mp3 payload").unwrap();
            file.flush().unwrap();
            file
        }

        async fn server_returning(status: u16, body: &str) -> MockServer {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1/transcriptions"))
                .respond_with(ResponseTemplate::new(status).set_body_string(body))
                .mount(&server)
                .await;
            server
        }

        fn provider_for(server: &MockServer) -> GeminiTranscribeProvider {
            GeminiTranscribeProvider::new(
                &server.uri(),
                crate::config::GEMINI_BATCH_MODEL.to_string(),
                Some("test-token".into()),
            )
            .unwrap()
        }

        async fn upload(
            server: &MockServer,
            options: &GeminiBatchOptions,
        ) -> Result<BatchUploadResult, GeminiBatchError> {
            let file = audio_file();
            provider_for(server)
                .upload_file(file.path(), options, &CancellationToken::new())
                .await
        }

        #[tokio::test]
        async fn sends_every_documented_multipart_field() {
            let server = server_returning(200, OK_BODY).await;
            let options = GeminiBatchOptions {
                diarization: true,
                word_timestamps: true,
                language: Some("es-MX".into()),
            };
            upload(&server, &options).await.unwrap();

            let requests = server.received_requests().await.unwrap();
            assert_eq!(requests.len(), 1);
            let body = String::from_utf8_lossy(&requests[0].body);

            for expected in [
                "name=\"model\"",
                "gemini-3.5-transcribe",
                "name=\"mode\"",
                // Always verbatim: annotations + smart is a 422, and the mode
                // is a constant so that combination cannot be constructed.
                "verbatim",
                "name=\"diarization\"",
                "name=\"word_timestamps\"",
                "name=\"language\"",
                "es-MX",
                "name=\"file\"",
                "audio/mpeg",
            ] {
                assert!(body.contains(expected), "missing {:?} in request body", expected);
            }

            let auth = requests[0]
                .headers
                .get("authorization")
                .expect("bearer token must be sent");
            assert_eq!(auth, "Bearer test-token");
        }

        #[tokio::test]
        async fn annotation_flags_track_the_options() {
            for (diarization, word_timestamps) in
                [(false, false), (true, false), (false, true), (true, true)]
            {
                let server = server_returning(200, OK_BODY).await;
                let options = GeminiBatchOptions {
                    diarization,
                    word_timestamps,
                    language: None,
                };
                let _ = upload(&server, &options).await;

                let requests = server.received_requests().await.unwrap();
                let body = String::from_utf8_lossy(&requests[0].body);
                assert!(
                    body.contains(&format!(
                        "name=\"diarization\"\r\n\r\n{}",
                        diarization
                    )),
                    "diarization={} not propagated",
                    diarization
                );
                assert!(
                    body.contains(&format!(
                        "name=\"word_timestamps\"\r\n\r\n{}",
                        word_timestamps
                    )),
                    "word_timestamps={} not propagated",
                    word_timestamps
                );
            }
        }

        #[tokio::test]
        async fn omits_language_when_unset_or_auto() {
            for language in [None, Some("auto".to_string()), Some("   ".to_string())] {
                let server = server_returning(200, OK_BODY).await;
                let options = GeminiBatchOptions {
                    diarization: false,
                    word_timestamps: true,
                    language,
                };
                upload(&server, &options).await.unwrap();

                let requests = server.received_requests().await.unwrap();
                let body = String::from_utf8_lossy(&requests[0].body);
                assert!(!body.contains("name=\"language\""));
            }
        }

        #[tokio::test]
        async fn parses_words_with_offsets_and_opaque_speakers() {
            let server = server_returning(200, OK_BODY).await;
            let result = upload(&server, &GeminiBatchOptions::authoritative(true, None))
                .await
                .unwrap();

            assert_eq!(result.text, "hello there");
            let words = result.words.expect("words must be parsed");
            assert_eq!(words.len(), 2);
            assert_eq!(words[0].start_secs, 0.1);
            assert_eq!(words[0].end_secs, 0.42);
            // Opaque: stored as-is, never parsed for structure.
            assert_eq!(words[0].speaker.as_deref(), Some("spk:0"));
        }

        #[tokio::test]
        async fn unannotated_response_without_words_is_fine() {
            let server = server_returning(200, r#"{"text":"just text"}"#).await;
            let result = upload(&server, &GeminiBatchOptions::unannotated(None))
                .await
                .unwrap();
            assert_eq!(result.text, "just text");
            assert!(result.words.is_none());
        }

        #[tokio::test]
        async fn requesting_word_timestamps_and_getting_none_is_an_error() {
            // Degrading an authoritative pass to one blob would look like
            // success while destroying the segmentation the caller asked for.
            let server = server_returning(200, r#"{"text":"just text"}"#).await;
            let error = upload(&server, &GeminiBatchOptions::authoritative(false, None))
                .await
                .unwrap_err();
            assert!(matches!(error, GeminiBatchError::MissingAnnotations));
            assert!(!error.is_transient(), "must not be retried forever");
        }

        #[tokio::test]
        async fn http_statuses_map_to_typed_errors() {
            let cases: Vec<(u16, &str, fn(&GeminiBatchError) -> bool)> = vec![
                (429, r#"{"detail":"Google transcription rate limit exceeded; retry later"}"#,
                 |e| matches!(e, GeminiBatchError::QuotaExceeded)),
                (401, "{}", |e| matches!(e, GeminiBatchError::Auth)),
                (403, "{}", |e| matches!(e, GeminiBatchError::Auth)),
                (422, "{}", |e| matches!(e, GeminiBatchError::InvalidRequest(_))),
                (500, "{}", |e| matches!(e, GeminiBatchError::ServerError(_))),
                (503, "{}", |e| matches!(e, GeminiBatchError::ServerError(_))),
            ];

            for (status, body, is_expected) in cases {
                let server = server_returning(status, body).await;
                let error = upload(&server, &GeminiBatchOptions::unannotated(None))
                    .await
                    .unwrap_err();
                assert!(is_expected(&error), "status {} produced {:?}", status, error);
            }
        }

        #[tokio::test]
        async fn quota_and_server_errors_are_retryable_but_auth_is_not() {
            let server = server_returning(429, "{}").await;
            let quota = upload(&server, &GeminiBatchOptions::unannotated(None))
                .await
                .unwrap_err();
            assert!(quota.is_transient());

            let server = server_returning(401, "{}").await;
            let auth = upload(&server, &GeminiBatchOptions::unannotated(None))
                .await
                .unwrap_err();
            assert!(!auth.is_transient(), "a bad token will never fix itself");
        }

        #[tokio::test]
        async fn a_success_body_that_is_not_json_is_a_hard_error() {
            let server = server_returning(200, "<html>gateway</html>").await;
            let error = upload(&server, &GeminiBatchOptions::unannotated(None))
                .await
                .unwrap_err();
            assert!(matches!(error, GeminiBatchError::MalformedResponse(_)));
            assert!(!error.is_transient());
        }

        #[tokio::test]
        async fn a_cancelled_token_stops_before_sending_anything() {
            let server = server_returning(200, OK_BODY).await;
            let file = audio_file();
            let cancel = CancellationToken::new();
            cancel.cancel();

            let error = provider_for(&server)
                .upload_file(file.path(), &GeminiBatchOptions::unannotated(None), &cancel)
                .await
                .unwrap_err();

            assert!(matches!(error, GeminiBatchError::Cancelled));
            assert!(
                server.received_requests().await.unwrap().is_empty(),
                "a cancelled job must not spend a request"
            );
        }

        #[tokio::test]
        async fn a_missing_upload_file_fails_before_any_request() {
            let server = server_returning(200, OK_BODY).await;
            let error = provider_for(&server)
                .upload_file(
                    std::path::Path::new("does-not-exist.mp3"),
                    &GeminiBatchOptions::unannotated(None),
                    &CancellationToken::new(),
                )
                .await
                .unwrap_err();
            assert!(matches!(error, GeminiBatchError::Transcode(_)));
            assert!(server.received_requests().await.unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn audio_shorter_than_the_minimum_is_rejected_before_any_request() {
        let provider =
            GeminiTranscribeProvider::new(BASE, "m".into(), Some("tok".into())).unwrap();
        let err = provider.transcribe(vec![0.0; 10], None).await.unwrap_err();
        assert!(matches!(err, TranscriptionError::AudioTooShort { .. }));
    }
}
