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
