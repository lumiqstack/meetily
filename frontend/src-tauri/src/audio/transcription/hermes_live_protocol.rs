// audio/transcription/hermes_live_protocol.rs
//
// Wire types for the hermes Gemini Live WebSocket, and nothing else. If the
// gateway contract changes, this is the only file that needs to change.
//
// Exchange:
//
//   ->  {"type":"start","model":"gemini-3.5-transcribe-live","mode":"VERBATIM",
//        "language_codes":[],"custom_vocabulary":[],"sample_rate":16000}
//   <-  {"type":"session.started","model":"...","sample_rate":16000}
//   ->  <binary PCM16 LE, mono, 16 kHz frames>
//   <-  {"type":"transcript.interim","text":"...","language_code":"en-US"}
//   <-  {"type":"transcript.final","text":"...","language_code":"en-US","speaker":"..."}
//   ->  {"type":"stop"}
//   <-  {"type":"session.finished"}
//
// Deserialization is deliberately tolerant: unknown fields are ignored (no
// `deny_unknown_fields`) and unknown event types deserialize to `Unknown`
// rather than erroring, so the gateway can add messages without breaking a
// shipped client mid-recording.

use serde::{Deserialize, Serialize};

/// Transcription mode requested of the gateway.
///
/// Diarization and word-level timestamps must not be combined with `Smart`;
/// live sessions always use `Verbatim`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum LiveMode {
    #[serde(rename = "VERBATIM")]
    Verbatim,
    #[serde(rename = "SMART")]
    Smart,
}

/// Client → gateway messages.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type")]
pub enum ClientMessage {
    #[serde(rename = "start")]
    Start {
        model: String,
        mode: LiveMode,
        /// Empty means "let the gateway detect the language".
        language_codes: Vec<String>,
        custom_vocabulary: Vec<String>,
        sample_rate: u32,
    },
    /// Ends the session; the gateway drains pending transcripts, then replies
    /// with `session.finished`.
    #[serde(rename = "stop")]
    Stop,
}

impl ClientMessage {
    /// The `start` frame for a live session.
    ///
    /// `language` is Meetily's preference: `None` or "auto" is sent as an empty
    /// list so the gateway detects the language itself.
    pub fn start(model: impl Into<String>, sample_rate: u32, language: Option<&str>) -> Self {
        let language_codes = language
            .map(str::trim)
            .filter(|l| !l.is_empty() && *l != "auto")
            .map(|l| vec![l.to_string()])
            .unwrap_or_default();

        Self::Start {
            model: model.into(),
            mode: LiveMode::Verbatim,
            language_codes,
            custom_vocabulary: Vec::new(),
            sample_rate,
        }
    }

    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }
}

/// Gateway → client messages.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type")]
pub enum ServerMessage {
    #[serde(rename = "session.started")]
    SessionStarted {
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        sample_rate: Option<u32>,
    },

    /// A replaceable hypothesis. Displayed as a live caption, never persisted.
    ///
    /// Observed against the live gateway: `text` is **cumulative**, not a
    /// delta — each interim carries the whole utterance so far ("The", "The
    /// quarterly", "The quarterly review", ...), and the last one equals the
    /// following final. Consumers must therefore *replace* the caption on each
    /// event, never append.
    #[serde(rename = "transcript.interim")]
    TranscriptInterim {
        text: String,
        #[serde(default)]
        language_code: Option<String>,
    },

    /// A settled utterance. This is the only message that becomes a stored
    /// transcript segment.
    #[serde(rename = "transcript.final")]
    TranscriptFinal {
        text: String,
        #[serde(default)]
        language_code: Option<String>,
        #[serde(default)]
        speaker: Option<String>,
    },

    #[serde(rename = "session.finished")]
    SessionFinished,

    #[serde(rename = "error")]
    Error {
        #[serde(default)]
        message: Option<String>,
    },

    /// Any event type this build does not know about.
    #[serde(other)]
    Unknown,
}

impl ServerMessage {
    pub fn parse(payload: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(payload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- client → gateway -------------------------------------------------

    #[test]
    fn start_frame_matches_the_documented_shape() {
        let json = ClientMessage::start(crate::config::GEMINI_LIVE_MODEL, 16000, None)
            .to_json()
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(value["type"], "start");
        assert_eq!(value["model"], "gemini-3.5-transcribe-live");
        assert_eq!(value["mode"], "VERBATIM");
        assert_eq!(value["sample_rate"], 16000);
        assert_eq!(value["language_codes"], serde_json::json!([]));
        assert_eq!(value["custom_vocabulary"], serde_json::json!([]));
    }

    #[test]
    fn an_explicit_language_is_forwarded() {
        let json = ClientMessage::start("m", 16000, Some("en-US")).to_json().unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["language_codes"], serde_json::json!(["en-US"]));
    }

    #[test]
    fn auto_and_blank_languages_mean_gateway_detection() {
        for language in [Some("auto"), Some(""), Some("   "), None] {
            let json = ClientMessage::start("m", 16000, language).to_json().unwrap();
            let value: serde_json::Value = serde_json::from_str(&json).unwrap();
            assert_eq!(
                value["language_codes"],
                serde_json::json!([]),
                "language {:?} should mean auto-detect",
                language
            );
        }
    }

    #[test]
    fn stop_frame_is_just_a_type() {
        assert_eq!(ClientMessage::Stop.to_json().unwrap(), r#"{"type":"stop"}"#);
    }

    // ---- gateway → client -------------------------------------------------

    #[test]
    fn parses_session_started() {
        let msg = ServerMessage::parse(
            r#"{"type":"session.started","model":"gemini-3.5-transcribe-live","sample_rate":16000}"#,
        )
        .unwrap();
        assert_eq!(
            msg,
            ServerMessage::SessionStarted {
                model: Some("gemini-3.5-transcribe-live".into()),
                sample_rate: Some(16000),
            }
        );
    }

    #[test]
    fn parses_interim_and_final() {
        assert_eq!(
            ServerMessage::parse(
                r#"{"type":"transcript.interim","text":"partial hypothesis","language_code":"en-US"}"#
            )
            .unwrap(),
            ServerMessage::TranscriptInterim {
                text: "partial hypothesis".into(),
                language_code: Some("en-US".into()),
            }
        );

        assert_eq!(
            ServerMessage::parse(
                r#"{"type":"transcript.final","text":"Final utterance.","language_code":"en-US","speaker":"S1"}"#
            )
            .unwrap(),
            ServerMessage::TranscriptFinal {
                text: "Final utterance.".into(),
                language_code: Some("en-US".into()),
                speaker: Some("S1".into()),
            }
        );
    }

    #[test]
    fn optional_fields_default_to_none() {
        assert_eq!(
            ServerMessage::parse(r#"{"type":"transcript.final","text":"Hi."}"#).unwrap(),
            ServerMessage::TranscriptFinal {
                text: "Hi.".into(),
                language_code: None,
                speaker: None,
            }
        );
        assert_eq!(
            ServerMessage::parse(r#"{"type":"session.started"}"#).unwrap(),
            ServerMessage::SessionStarted {
                model: None,
                sample_rate: None
            }
        );
    }

    #[test]
    fn parses_session_finished_and_error() {
        assert_eq!(
            ServerMessage::parse(r#"{"type":"session.finished"}"#).unwrap(),
            ServerMessage::SessionFinished
        );
        assert_eq!(
            ServerMessage::parse(r#"{"type":"error","message":"quota exceeded"}"#).unwrap(),
            ServerMessage::Error {
                message: Some("quota exceeded".into())
            }
        );
    }

    #[test]
    fn unknown_fields_are_ignored() {
        // The gateway must be able to add fields without breaking this build.
        assert_eq!(
            ServerMessage::parse(
                r#"{"type":"transcript.final","text":"Hi.","confidence":0.9,"nested":{"a":1}}"#
            )
            .unwrap(),
            ServerMessage::TranscriptFinal {
                text: "Hi.".into(),
                language_code: None,
                speaker: None,
            }
        );
    }

    #[test]
    fn unknown_event_types_are_not_fatal() {
        // Must not abort a recording mid-meeting.
        assert_eq!(
            ServerMessage::parse(r#"{"type":"metrics.usage","tokens":42}"#).unwrap(),
            ServerMessage::Unknown
        );
    }

    #[test]
    fn malformed_payloads_surface_an_error() {
        assert!(ServerMessage::parse("not json").is_err());
        assert!(ServerMessage::parse(r#"{"no_type":true}"#).is_err());
    }
}
