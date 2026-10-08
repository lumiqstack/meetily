// audio/transcription/mod.rs
//
// Transcription module: Provider abstraction, engine management, and worker pool.

pub mod provider;
pub mod whisper_provider;
pub mod parakeet_provider;
pub mod openai_compatible_provider;
pub mod engine;
pub mod worker;

// Shared PCM conversion for remote providers.
pub mod pcm;

// Gemini transcription through the hermes proxy: batch over REST, live over
// a WebSocket. Both transports derive from one configured base URL.
pub mod gemini_batch;
pub mod gemini_transcribe_provider;
pub mod hermes_endpoints;
pub mod hermes_live_protocol;
pub mod hermes_live_session;

// Re-export commonly used types
pub use provider::{TranscriptionError, TranscriptionProvider, TranscriptResult};
pub use whisper_provider::WhisperProvider;
pub use parakeet_provider::ParakeetProvider;
pub use openai_compatible_provider::OpenAICompatibleProvider;
pub use gemini_transcribe_provider::GeminiTranscribeProvider;
pub use engine::{
    TranscriptionEngine,
    validate_transcription_model_ready,
    get_or_init_transcription_engine,
    get_or_init_whisper
};
pub use worker::{
    start_transcription_task,
    reset_speech_detected_flag,
    TranscriptUpdate
};

/// The language code a remote server should receive for the stored
/// preference. "auto" and "auto-translate" are Meetily modes rather than ISO
/// codes, so like a blank value they mean "let the server detect".
pub fn remote_language_code(preference: Option<&str>) -> Option<String> {
    preference
        .map(str::trim)
        .filter(|l| !l.is_empty() && !matches!(*l, "auto" | "auto-translate"))
        .map(str::to_string)
}

#[cfg(test)]
mod remote_language_tests {
    use super::remote_language_code;

    #[test]
    fn a_language_code_is_forwarded() {
        assert_eq!(remote_language_code(Some("es")), Some("es".to_string()));
        assert_eq!(remote_language_code(Some(" en-US ")), Some("en-US".to_string()));
    }

    #[test]
    fn modes_and_blanks_mean_server_detection() {
        assert_eq!(remote_language_code(Some("auto")), None);
        assert_eq!(remote_language_code(Some("auto-translate")), None);
        assert_eq!(remote_language_code(Some("")), None);
        assert_eq!(remote_language_code(Some("   ")), None);
        assert_eq!(remote_language_code(None), None);
    }
}
