// audio/transcription/hermes_live_session.rs
//
// Live transcription over the hermes Gemini Live WebSocket.
//
// Audio source: the *continuous* mixed stream tapped in pipeline.rs, not the
// VAD receiver. Gemini Live runs its own endpointing, so it needs unbroken
// audio; feeding it silence-stripped segments would also delay every interim
// caption until after the utterance had already ended.
//
// Reliability contract (deliberate, and narrower than it looks):
//
//   * Only frames that were never successfully written to the socket are
//     retried. The gateway acknowledges no audio offsets and issues no
//     utterance IDs, so Meetily cannot know how much of a written frame Gemini
//     actually consumed. Replaying "everything after the last final" would
//     invent duplicates.
//   * Finals are never dropped by comparing text. Identical text can be a
//     legitimate repetition ("yes. yes.").
//   * A drop after frames were sent means the live transcript may have a gap.
//     We say so, and move on from the current live position.
//   * The complete local recording is written on a separate path and is never
//     affected by anything here. Batch REST retranscription is the recovery
//     path for any gap.

use super::hermes_live_protocol::{ClientMessage, ServerMessage};
use super::pcm::pcm16_le_bytes;
use crate::audio::recording_state::AudioChunk;
use futures_util::{SinkExt, StreamExt};
use log::{error, info, warn};
use serde::Serialize;
use std::collections::VecDeque;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Runtime};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};

/// Audio is streamed to the gateway at this rate, mono.
pub const LIVE_SAMPLE_RATE: u32 = 16000;

/// Gemini Live sessions are capped at ten minutes. Rotate early, at the first
/// utterance boundary after this point, so the seam falls in a gap in speech.
const ROTATE_AFTER: Duration = Duration::from_secs(9 * 60);

/// If no utterance boundary appears, rotate anyway before the hard limit.
const ROTATE_HARD_CAP: Duration = Duration::from_secs(9 * 60 + 45);

/// How long to wait for `session.finished` after sending `stop`.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// Cap on never-written audio held across a reconnect (~30s at 16kHz mono
/// PCM16). Bounding this is what keeps a reconnect near the live position
/// instead of replaying a long backlog.
const MAX_PENDING_BYTES: usize = LIVE_SAMPLE_RATE as usize * 2 * 30;

const MAX_RECONNECT_ATTEMPTS: u32 = 6;
const BACKOFF_BASE: Duration = Duration::from_millis(250);
const BACKOFF_CAP: Duration = Duration::from_secs(8);

/// Interim caption payload. Deliberately *not* a `TranscriptUpdate`: interims
/// must never reach the persistence listener, which stores every
/// `transcript-update` it sees.
#[derive(Debug, Clone, Serialize)]
pub struct InterimCaption {
    pub text: String,
    pub language_code: Option<String>,
}

#[derive(Debug, Clone)]
pub struct LiveSessionConfig {
    /// Fully resolved `wss://.../v1/live` endpoint.
    pub endpoint: String,
    pub bearer_token: String,
    pub model: String,
    /// `None` (or "auto") lets the gateway detect the language.
    pub language: Option<String>,
}

impl LiveSessionConfig {
    /// Build from saved transcript settings.
    pub async fn from_saved_settings<R: Runtime>(app: &AppHandle<R>) -> Result<Self, String> {
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
            )?;

        let bearer_token = setting
            .gemini_transcribe_api_key
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .ok_or(
                "Gemini transcription requires the proxy bearer token. Set it in transcript settings.",
            )?
            .to_string();

        Ok(Self {
            endpoint: super::hermes_endpoints::resolve_live_endpoint(base_url)?,
            bearer_token,
            model: crate::config::GEMINI_LIVE_MODEL.to_string(),
            language: crate::get_language_preference_internal(),
        })
    }
}

/// Maps gateway transcripts onto Meetily's recording-relative timeline.
///
/// The gateway sends no offsets, so a final's span is inferred from how much
/// audio has been forwarded. **These timestamps are approximate**: a final
/// arrives after the gateway has processed the audio, by which time later
/// audio has usually already been sent, so `end` overshoots by roughly the
/// gateway's processing latency. Good enough to order segments and to seek
/// near the right place; not good enough to drive precise playback sync.
/// Making them authoritative requires per-utterance offsets from the gateway.
#[derive(Debug, Default)]
struct TimelineLedger {
    forwarded_end: f64,
    last_final_end: f64,
}

impl TimelineLedger {
    /// Record that audio ending at `end_secs` (recording-relative) was sent.
    fn note_forwarded(&mut self, end_secs: f64) {
        if end_secs > self.forwarded_end {
            self.forwarded_end = end_secs;
        }
    }

    /// Claim the span for a newly-arrived final.
    fn take_span(&mut self) -> (f64, f64) {
        let start = self.last_final_end;
        let end = self.forwarded_end.max(start);
        self.last_final_end = end;
        (start, end)
    }
}

/// Never-written audio frames, oldest first.
#[derive(Debug, Default)]
struct PendingFrames {
    frames: VecDeque<Vec<u8>>,
    bytes: usize,
    dropped: bool,
}

impl PendingFrames {
    fn push_back(&mut self, frame: Vec<u8>) {
        self.bytes += frame.len();
        self.frames.push_back(frame);
        self.trim();
    }

    /// A frame that failed to send goes back where it came from: it was never
    /// written, so it is still the oldest unwritten audio.
    fn push_front(&mut self, frame: Vec<u8>) {
        self.bytes += frame.len();
        self.frames.push_front(frame);
        self.trim();
    }

    fn pop_front(&mut self) -> Option<Vec<u8>> {
        let frame = self.frames.pop_front()?;
        self.bytes -= frame.len();
        Some(frame)
    }

    /// Drop the oldest audio past the cap. This is what keeps a long outage
    /// from making us replay minutes of stale audio on reconnect.
    fn trim(&mut self) {
        while self.bytes > MAX_PENDING_BYTES {
            match self.frames.pop_front() {
                Some(frame) => {
                    self.bytes -= frame.len();
                    self.dropped = true;
                }
                None => break,
            }
        }
    }

    fn take_dropped(&mut self) -> bool {
        std::mem::take(&mut self.dropped)
    }

    fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }
}

/// Why the inner session loop returned.
#[derive(Debug, PartialEq)]
enum SessionOutcome {
    /// Audio input ended: the recording stopped.
    InputClosed,
    /// Time to rotate before hitting the ten-minute cap.
    Rotate,
    /// Connection failed or the gateway reported an error.
    Disconnected(String),
}

pub fn start_live_transcription_task<R: Runtime>(
    app: AppHandle<R>,
    receiver: UnboundedReceiver<AudioChunk>,
    config: LiveSessionConfig,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        run_live_transcription(app, receiver, config).await;
    })
}

async fn run_live_transcription<R: Runtime>(
    app: AppHandle<R>,
    mut receiver: UnboundedReceiver<AudioChunk>,
    config: LiveSessionConfig,
) {
    info!(
        "✨ Gemini Live transcription starting - endpoint: {}, model: {}",
        config.endpoint, config.model
    );

    let mut ledger = TimelineLedger::default();
    let mut pending = PendingFrames::default();
    let mut attempt = 0u32;

    loop {
        let socket = match connect(&config).await {
            Ok(socket) => {
                attempt = 0;
                socket
            }
            Err(e) => {
                // A rejected upgrade is a configuration problem, not a blip:
                // retrying cannot fix a bad token or a wrong URL.
                if e.fatal {
                    error!("Gemini Live connection rejected: {}", e.message);
                    emit_error(&app, &e.message);
                    break;
                }

                attempt += 1;
                if attempt > MAX_RECONNECT_ATTEMPTS {
                    error!(
                        "Gemini Live: giving up after {} attempts: {}",
                        MAX_RECONNECT_ATTEMPTS, e.message
                    );
                    emit_error(
                        &app,
                        &format!(
                            "Live transcription disconnected: {}. The recording is still being saved and can be transcribed afterwards.",
                            e.message
                        ),
                    );
                    break;
                }

                let delay = backoff_delay(attempt);
                warn!(
                    "Gemini Live connect failed (attempt {}/{}), retrying in {:?}: {}",
                    attempt, MAX_RECONNECT_ATTEMPTS, delay, e.message
                );
                emit_warning(
                    &app,
                    &format!("Reconnecting to live transcription ({})…", e.message),
                );

                // Keep draining audio into the bounded buffer while we wait,
                // so the pipeline's unbounded channel does not grow without
                // limit during an outage.
                if !sleep_while_buffering(&mut receiver, &mut pending, &mut ledger, delay).await {
                    break;
                }
                continue;
            }
        };

        let outcome = run_session(
            &app,
            socket,
            &mut receiver,
            &config,
            &mut ledger,
            &mut pending,
        )
        .await;

        match outcome {
            SessionOutcome::InputClosed => {
                info!("Gemini Live: audio input closed, session complete");
                break;
            }
            SessionOutcome::Rotate => {
                info!("Gemini Live: rotating session before the ten-minute cap");
                continue;
            }
            SessionOutcome::Disconnected(reason) => {
                // Interims belong to a session that no longer exists.
                clear_interim(&app);
                warn!("Gemini Live disconnected: {}", reason);
                emit_warning(
                    &app,
                    "Live transcription reconnected — the live transcript may have a gap. The full recording was saved and can be re-transcribed.",
                );

                attempt += 1;
                if attempt > MAX_RECONNECT_ATTEMPTS {
                    error!("Gemini Live: giving up after {} attempts", MAX_RECONNECT_ATTEMPTS);
                    emit_error(
                        &app,
                        "Live transcription stopped after repeated disconnects. The recording is still being saved and can be transcribed afterwards.",
                    );
                    break;
                }

                let delay = backoff_delay(attempt);
                if !sleep_while_buffering(&mut receiver, &mut pending, &mut ledger, delay).await {
                    break;
                }
            }
        }
    }

    clear_interim(&app);
    info!("✅ Gemini Live transcription task finished");
}

struct ConnectError {
    message: String,
    /// True when retrying cannot help (auth rejected, bad request).
    fatal: bool,
}

type LiveSocket = tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
>;

async fn connect(config: &LiveSessionConfig) -> Result<LiveSocket, ConnectError> {
    let mut request = config
        .endpoint
        .as_str()
        .into_client_request()
        .map_err(|e| ConnectError {
            message: format!("Invalid live endpoint '{}': {}", config.endpoint, e),
            fatal: true,
        })?;

    let header =
        format!("Bearer {}", config.bearer_token)
            .parse()
            .map_err(|_| ConnectError {
                message: "The proxy bearer token contains characters that cannot be sent in an HTTP header.".to_string(),
                fatal: true,
            })?;
    request.headers_mut().insert(AUTHORIZATION, header);

    let (socket, _response) =
        tokio_tungstenite::connect_async(request)
            .await
            .map_err(|e| ConnectError {
                message: describe_ws_error(&e),
                fatal: is_fatal_ws_error(&e),
            })?;

    Ok(socket)
}

/// An upgrade rejected with 4xx will be rejected identically on every retry.
fn is_fatal_ws_error(error: &WsError) -> bool {
    match error {
        WsError::Http(response) => response.status().is_client_error(),
        _ => false,
    }
}

fn describe_ws_error(error: &WsError) -> String {
    match error {
        WsError::Http(response) => {
            let status = response.status();
            if status == 401 || status == 403 {
                format!(
                    "gateway rejected the bearer token ({}) — check it in transcript settings",
                    status
                )
            } else {
                format!("gateway returned {}", status)
            }
        }
        other => other.to_string(),
    }
}

fn backoff_delay(attempt: u32) -> Duration {
    let scaled = BACKOFF_BASE.saturating_mul(1u32 << attempt.min(5).saturating_sub(1));
    scaled.min(BACKOFF_CAP)
}

/// Wait out a backoff without dropping the audio arriving meanwhile.
///
/// Returns false if the recording ended while waiting.
async fn sleep_while_buffering(
    receiver: &mut UnboundedReceiver<AudioChunk>,
    pending: &mut PendingFrames,
    ledger: &mut TimelineLedger,
    delay: Duration,
) -> bool {
    let deadline = tokio::time::Instant::now() + delay;
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => return true,
            chunk = receiver.recv() => match chunk {
                Some(chunk) => {
                    let (frame, end) = encode_chunk(chunk);
                    ledger.note_forwarded(end);
                    pending.push_back(frame);
                }
                None => return false,
            },
        }
    }
}

/// Convert a pipeline chunk into a wire frame plus its recording-relative end.
fn encode_chunk(chunk: AudioChunk) -> (Vec<u8>, f64) {
    let duration = chunk.data.len() as f64 / chunk.sample_rate as f64;
    let end = chunk.timestamp + duration;

    let samples = if chunk.sample_rate == LIVE_SAMPLE_RATE {
        chunk.data
    } else {
        crate::audio::audio_processing::resample_audio(
            &chunk.data,
            chunk.sample_rate,
            LIVE_SAMPLE_RATE,
        )
    };

    (pcm16_le_bytes(&samples), end)
}

async fn run_session<R: Runtime>(
    app: &AppHandle<R>,
    mut socket: LiveSocket,
    receiver: &mut UnboundedReceiver<AudioChunk>,
    config: &LiveSessionConfig,
    ledger: &mut TimelineLedger,
    pending: &mut PendingFrames,
) -> SessionOutcome {
    let start_frame = match ClientMessage::start(
        config.model.clone(),
        LIVE_SAMPLE_RATE,
        config.language.as_deref(),
    )
    .to_json()
    {
        Ok(json) => json,
        Err(e) => return SessionOutcome::Disconnected(format!("could not encode start frame: {}", e)),
    };

    if let Err(e) = socket.send(Message::Text(start_frame)).await {
        return SessionOutcome::Disconnected(format!("could not send start frame: {}", e));
    }

    let session_start = Instant::now();
    let mut wrote_audio = false;
    // Rotation waits for an utterance boundary; the hard cap forces it.
    let mut rotate_requested = false;

    loop {
        // Flush buffered audio before accepting more.
        if !pending.is_empty() {
            if pending.take_dropped() {
                warn!("Gemini Live: dropped buffered audio past the {}s cap", 30);
                emit_warning(
                    app,
                    "Live transcription fell behind and skipped some audio — the full recording was still saved.",
                );
            }

            if let Some(frame) = pending.pop_front() {
                match socket.send(Message::Binary(frame.clone())).await {
                    Ok(()) => wrote_audio = true,
                    Err(e) => {
                        // Never written, so it is still owed.
                        pending.push_front(frame);
                        return disconnect_reason(e, wrote_audio);
                    }
                }
            }
            continue;
        }

        if rotate_requested {
            return finish_session(app, socket, ledger, SessionOutcome::Rotate).await;
        }

        let hard_cap = tokio::time::sleep(
            ROTATE_HARD_CAP.saturating_sub(session_start.elapsed()),
        );

        tokio::select! {
            biased;

            message = socket.next() => {
                match message {
                    Some(Ok(Message::Text(payload))) => {
                        match ServerMessage::parse(&payload) {
                            Ok(ServerMessage::TranscriptInterim { text, language_code }) => {
                                emit_interim(app, InterimCaption { text, language_code });
                            }
                            Ok(ServerMessage::TranscriptFinal { text, speaker, .. }) => {
                                emit_final(app, ledger, &text, speaker);
                                // Rotate at an utterance boundary, where a seam
                                // costs nothing.
                                if session_start.elapsed() >= ROTATE_AFTER {
                                    return finish_session(app, socket, ledger, SessionOutcome::Rotate).await;
                                }
                            }
                            Ok(ServerMessage::SessionStarted { .. }) => {
                                info!("Gemini Live session started");
                            }
                            Ok(ServerMessage::SessionFinished) => {
                                return SessionOutcome::Disconnected(
                                    "gateway ended the session".to_string(),
                                );
                            }
                            Ok(ServerMessage::Error { message }) => {
                                let detail = message.unwrap_or_else(|| "unspecified".to_string());
                                return SessionOutcome::Disconnected(
                                    format!("gateway error: {}", detail),
                                );
                            }
                            // Forward compatibility: never fatal.
                            Ok(ServerMessage::Unknown) => {}
                            Err(e) => warn!("Gemini Live: unparseable event ({}): {}", e, payload),
                        }
                    }
                    Some(Ok(Message::Close(frame))) => {
                        return SessionOutcome::Disconnected(match frame {
                            Some(f) => format!("gateway closed the connection ({})", f.code),
                            None => "gateway closed the connection".to_string(),
                        });
                    }
                    // Ping/Pong are answered by the library; binary is unexpected.
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return disconnect_reason(e, wrote_audio),
                    None => {
                        return SessionOutcome::Disconnected("connection closed".to_string());
                    }
                }
            }

            chunk = receiver.recv() => {
                match chunk {
                    Some(chunk) => {
                        let (frame, end) = encode_chunk(chunk);
                        ledger.note_forwarded(end);
                        match socket.send(Message::Binary(frame.clone())).await {
                            Ok(()) => wrote_audio = true,
                            Err(e) => {
                                pending.push_front(frame);
                                return disconnect_reason(e, wrote_audio);
                            }
                        }
                    }
                    None => {
                        return finish_session(app, socket, ledger, SessionOutcome::InputClosed).await;
                    }
                }
            }

            _ = hard_cap => {
                // No utterance boundary appeared in time; rotate anyway and
                // accept losing only the in-flight interim.
                warn!("Gemini Live: forcing rotation at the hard cap");
                rotate_requested = true;
            }
        }
    }
}

fn disconnect_reason(error: WsError, wrote_audio: bool) -> SessionOutcome {
    let reason = if wrote_audio {
        format!("{} (after audio was sent)", error)
    } else {
        error.to_string()
    };
    SessionOutcome::Disconnected(reason)
}

/// Send `stop`, emit whatever the gateway drains, and wait for
/// `session.finished`. Trailing finals are the tail of the meeting — losing
/// them would silently truncate the transcript.
async fn finish_session<R: Runtime>(
    app: &AppHandle<R>,
    mut socket: LiveSocket,
    ledger: &mut TimelineLedger,
    outcome: SessionOutcome,
) -> SessionOutcome {
    if let Err(e) = socket.send(Message::Text(
        ClientMessage::Stop.to_json().unwrap_or_else(|_| r#"{"type":"stop"}"#.to_string()),
    ))
    .await
    {
        warn!("Gemini Live: could not send stop frame: {}", e);
        return outcome;
    }

    let drain = async {
        while let Some(message) = socket.next().await {
            match message {
                Ok(Message::Text(payload)) => match ServerMessage::parse(&payload) {
                    Ok(ServerMessage::TranscriptFinal { text, speaker, .. }) => {
                        emit_final(app, ledger, &text, speaker);
                    }
                    // Interims are worthless once the session is ending.
                    Ok(ServerMessage::TranscriptInterim { .. }) => {}
                    Ok(ServerMessage::SessionFinished) => return,
                    Ok(ServerMessage::Error { message }) => {
                        warn!(
                            "Gemini Live: error while draining: {}",
                            message.unwrap_or_else(|| "unspecified".to_string())
                        );
                        return;
                    }
                    _ => {}
                },
                Ok(Message::Close(_)) | Err(_) => return,
                Ok(_) => {}
            }
        }
    };

    if tokio::time::timeout(DRAIN_TIMEOUT, drain).await.is_err() {
        warn!("Gemini Live: timed out waiting for session.finished");
    }

    let _ = socket.close(None).await;
    outcome
}

// ---------------------------------------------------------------------------
// Event emission
// ---------------------------------------------------------------------------

fn emit_final<R: Runtime>(
    app: &AppHandle<R>,
    ledger: &mut TimelineLedger,
    text: &str,
    speaker: Option<String>,
) {
    let text = text.trim();
    if text.is_empty() {
        return;
    }

    let (start, end) = ledger.take_span();
    let update = super::worker::TranscriptUpdate {
        text: text.to_string(),
        timestamp: super::worker::format_current_timestamp(),
        // Live audio is the mix of mic and system; there is no trustworthy
        // per-utterance attribution unless the gateway supplies one.
        source: "Audio".to_string(),
        sequence_id: super::worker::next_sequence_id(),
        chunk_start_time: start,
        is_partial: false,
        confidence: 1.0,
        audio_start_time: start,
        audio_end_time: end,
        duration: end - start,
        speaker,
    };

    super::worker::emit_speech_detected_once(app);

    if let Err(e) = app.emit("transcript-update", &update) {
        error!("Gemini Live: failed to emit transcript update: {}", e);
    }
}

fn emit_interim<R: Runtime>(app: &AppHandle<R>, caption: InterimCaption) {
    if let Err(e) = app.emit("transcript-interim", &caption) {
        error!("Gemini Live: failed to emit interim caption: {}", e);
    }
}

fn clear_interim<R: Runtime>(app: &AppHandle<R>) {
    emit_interim(
        app,
        InterimCaption {
            text: String::new(),
            language_code: None,
        },
    );
}

fn emit_warning<R: Runtime>(app: &AppHandle<R>, message: &str) {
    let _ = app.emit("transcription-warning", message);
}

fn emit_error<R: Runtime>(app: &AppHandle<R>, message: &str) {
    let _ = app.emit(
        "transcription-error",
        serde_json::json!({
            "error": message,
            "userMessage": message,
            "actionable": true,
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- timeline ledger --------------------------------------------------

    #[test]
    fn finals_partition_the_forwarded_timeline() {
        let mut ledger = TimelineLedger::default();

        ledger.note_forwarded(3.0);
        assert_eq!(ledger.take_span(), (0.0, 3.0));

        ledger.note_forwarded(7.5);
        assert_eq!(ledger.take_span(), (3.0, 7.5));
    }

    #[test]
    fn spans_never_run_backwards() {
        // Two finals arriving before any further audio must not produce a
        // negative-duration segment.
        let mut ledger = TimelineLedger::default();
        ledger.note_forwarded(5.0);
        assert_eq!(ledger.take_span(), (0.0, 5.0));

        let (start, end) = ledger.take_span();
        assert_eq!((start, end), (5.0, 5.0));
        assert!(end >= start);
    }

    #[test]
    fn out_of_order_forwarding_does_not_rewind_the_clock() {
        let mut ledger = TimelineLedger::default();
        ledger.note_forwarded(10.0);
        ledger.note_forwarded(4.0);
        assert_eq!(ledger.take_span(), (0.0, 10.0));
    }

    /// Documents the known approximation: a final is attributed up to the
    /// most recently forwarded audio, so its end overshoots the true end of
    /// speech by the gateway's processing latency.
    #[test]
    fn final_span_overshoots_by_at_most_the_forwarded_lead() {
        let mut ledger = TimelineLedger::default();
        let true_utterance_end = 4.0;
        let forwarded_lead = 0.6; // one pipeline window

        ledger.note_forwarded(true_utterance_end + forwarded_lead);
        let (_, end) = ledger.take_span();

        assert!(end >= true_utterance_end);
        assert!((end - true_utterance_end) <= forwarded_lead + f64::EPSILON);
    }

    // ---- pending frames ---------------------------------------------------

    #[test]
    fn unsent_frames_are_retried_in_order() {
        let mut pending = PendingFrames::default();
        pending.push_back(vec![1, 1]);
        pending.push_back(vec![2, 2]);

        assert_eq!(pending.pop_front(), Some(vec![1, 1]));
        assert_eq!(pending.pop_front(), Some(vec![2, 2]));
        assert!(pending.is_empty());
    }

    #[test]
    fn a_failed_send_returns_the_frame_to_the_front() {
        let mut pending = PendingFrames::default();
        pending.push_back(vec![9, 9]);

        let frame = pending.pop_front().unwrap();
        pending.push_front(frame); // send failed

        assert_eq!(pending.pop_front(), Some(vec![9, 9]));
    }

    #[test]
    fn buffer_is_bounded_and_drops_oldest_audio() {
        let mut pending = PendingFrames::default();
        let frame = vec![0u8; MAX_PENDING_BYTES / 4];
        for _ in 0..10 {
            pending.push_back(frame.clone());
        }

        assert!(pending.bytes <= MAX_PENDING_BYTES);
        assert!(pending.take_dropped(), "overflow must be reported");
        assert!(!pending.take_dropped(), "the flag is consumed once");
    }

    #[test]
    fn accounting_stays_consistent_across_push_and_pop() {
        let mut pending = PendingFrames::default();
        pending.push_back(vec![0u8; 100]);
        pending.push_front(vec![0u8; 50]);
        assert_eq!(pending.bytes, 150);

        pending.pop_front();
        assert_eq!(pending.bytes, 100);
        pending.pop_front();
        assert_eq!(pending.bytes, 0);
        assert!(pending.is_empty());
    }

    // ---- backoff ----------------------------------------------------------

    #[test]
    fn backoff_grows_then_saturates() {
        assert_eq!(backoff_delay(1), Duration::from_millis(250));
        assert_eq!(backoff_delay(2), Duration::from_millis(500));
        assert_eq!(backoff_delay(3), Duration::from_secs(1));
        assert!(backoff_delay(10) <= BACKOFF_CAP);
    }

    // ---- chunk encoding ---------------------------------------------------

    #[test]
    fn chunks_already_at_16k_are_not_resampled() {
        let chunk = AudioChunk {
            data: vec![0.0; 1600],
            sample_rate: LIVE_SAMPLE_RATE,
            timestamp: 2.0,
            chunk_id: 1,
            device_type: crate::audio::recording_state::DeviceType::Microphone,
            dominant_source: None,
        };

        let (frame, end) = encode_chunk(chunk);
        assert_eq!(frame.len(), 1600 * 2);
        assert!((end - 2.1).abs() < 1e-9);
    }

    #[test]
    fn chunks_are_resampled_down_to_16k() {
        let chunk = AudioChunk {
            data: vec![0.0; 4800], // 100ms at 48kHz
            sample_rate: 48000,
            timestamp: 0.0,
            chunk_id: 1,
            device_type: crate::audio::recording_state::DeviceType::Microphone,
            dominant_source: None,
        };

        let (frame, end) = encode_chunk(chunk);
        // 100ms at 16kHz = 1600 samples = 3200 bytes, allowing for resampler
        // edge effects.
        assert!(
            (frame.len() as i64 - 3200).abs() < 400,
            "unexpected frame size {}",
            frame.len()
        );
        assert!((end - 0.1).abs() < 1e-9);
    }

    // ---- error classification ---------------------------------------------

    #[test]
    fn client_errors_on_upgrade_are_fatal() {
        let response = tokio_tungstenite::tungstenite::http::Response::builder()
            .status(401)
            .body(None)
            .unwrap();
        let error = WsError::Http(response);

        assert!(is_fatal_ws_error(&error));
        assert!(describe_ws_error(&error).contains("bearer token"));
    }

    #[test]
    fn server_errors_on_upgrade_are_retried() {
        let response = tokio_tungstenite::tungstenite::http::Response::builder()
            .status(503)
            .body(None)
            .unwrap();
        assert!(!is_fatal_ws_error(&WsError::Http(response)));
    }

    #[test]
    fn transport_errors_are_retried() {
        let error = WsError::Io(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "reset",
        ));
        assert!(!is_fatal_ws_error(&error));
    }

    #[test]
    fn a_drop_after_audio_was_sent_is_reported_as_such() {
        // This is the signal that drives the "may contain a gap" warning.
        let error = || WsError::Io(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "gone"));

        match disconnect_reason(error(), true) {
            SessionOutcome::Disconnected(reason) => {
                assert!(reason.contains("after audio was sent"), "{}", reason)
            }
            other => panic!("unexpected outcome: {:?}", other),
        }

        match disconnect_reason(error(), false) {
            SessionOutcome::Disconnected(reason) => {
                assert!(!reason.contains("after audio was sent"), "{}", reason)
            }
            other => panic!("unexpected outcome: {:?}", other),
        }
    }
}
