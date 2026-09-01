// audio/transcription/gemini_batch.rs
//
// Batch (post-meeting) Gemini transcription: upload the recording as ONE
// request wherever it fits, instead of one request per VAD segment.
//
// Why this exists at all: the local-engine pipeline splits audio into <=25s
// VAD segments and issues one HTTP request each. Google counts every request
// against a Tier 1 limit of 100 requests/day, so 100 x 25s is roughly 41
// minutes of audio per day — a single long meeting. Uploading whole files
// costs 1 request/hour unannotated, or 2 with annotations.
//
// This module owns the pure decisions (how many chunks, where segment
// boundaries fall, which speaker label is which). The I/O — transcoding,
// uploading, cancellation — lives in the executor half at the bottom.

use std::collections::HashMap;
use std::time::Duration;

// ============================================================================
// LIMITS AND TUNING
// ============================================================================

/// Hard gateway limit for an unannotated upload.
pub const UNANNOTATED_LIMIT_MS: u64 = 60 * 60 * 1000;

/// Hard gateway limit once diarization or word timestamps are requested.
pub const ANNOTATED_LIMIT_MS: u64 = 30 * 60 * 1000;

/// Give up re-planning after this many probe failures. Each attempt is local
/// ffmpeg work and costs no quota, but an unbounded loop would hang the job.
const MAX_PLAN_ATTEMPTS: u32 = 4;

/// Gap between words that ends a segment.
const PAUSE_SPLIT_SECS: f64 = 0.7;

/// A segment is not broken on punctuation before it reaches this length,
/// so "Yes. No. Maybe." stays one readable row rather than three.
const MIN_SEGMENT_SECS: f64 = 2.0;

/// Hard cap, so an unpunctuated monologue still yields readable rows.
const MAX_SEGMENT_SECS: f64 = 30.0;

// ============================================================================
// OPTIONS
// ============================================================================

/// Annotation choices for one batch job.
///
/// Persisted with the job (see `job_persistence`): a job resumed after a crash
/// must come back with the settings the user actually chose, not defaults.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GeminiBatchOptions {
    pub diarization: bool,
    pub word_timestamps: bool,
    /// BCP-47 (`es-MX`); a bare subtag such as `es` is also valid.
    pub language: Option<String>,
}

impl GeminiBatchOptions {
    /// The authoritative post-meeting pass. Word timestamps are an invariant
    /// here, not a toggle — they are the only thing that yields per-segment
    /// timings and click-to-seek. Diarization is independent.
    pub fn authoritative(diarization: bool, language: Option<String>) -> Self {
        Self {
            diarization,
            word_timestamps: true,
            language,
        }
    }

    /// Imports and recovery: cheapest possible, one segment per upload.
    pub fn unannotated(language: Option<String>) -> Self {
        Self {
            diarization: false,
            word_timestamps: false,
            language,
        }
    }

    pub fn is_annotated(&self) -> bool {
        self.diarization || self.word_timestamps
    }

    /// Per-request duration ceiling implied by these options.
    pub fn limit_ms(&self) -> u64 {
        if self.is_annotated() {
            ANNOTATED_LIMIT_MS
        } else {
            UNANNOTATED_LIMIT_MS
        }
    }
}

// ============================================================================
// ERRORS
// ============================================================================

/// Typed failures, so `pipeline::transcribe_stage` can classify by downcast
/// rather than matching on message substrings.
#[derive(Debug, thiserror::Error)]
pub enum GeminiBatchError {
    #[error("Transcription was cancelled")]
    Cancelled,

    #[error("Gemini transcription quota is exhausted. The recording was saved — this can be retried later.")]
    QuotaExceeded,

    #[error("The transcription gateway timed out")]
    Timeout,

    #[error("Could not reach the transcription gateway: {0}")]
    Transport(String),

    #[error("The transcription gateway returned a server error ({0})")]
    ServerError(u16),

    #[error("The transcription gateway rejected the bearer token")]
    Auth,

    #[error("The transcription gateway rejected the request: {0}")]
    InvalidRequest(String),

    #[error("Word timestamps were requested but the gateway returned none")]
    MissingAnnotations,

    #[error("Could not prepare audio for upload: {0}")]
    Transcode(String),

    #[error("Could not read the gateway response: {0}")]
    MalformedResponse(String),
}

impl GeminiBatchError {
    /// Whether retrying later could plausibly succeed.
    ///
    /// Quota is the obvious case, but a dropped connection, a gateway 502 or a
    /// timeout are equally retryable — classifying only 429 would strand jobs
    /// on transport blips.
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            Self::QuotaExceeded | Self::Timeout | Self::Transport(_) | Self::ServerError(_)
        )
    }

    pub fn is_cancellation(&self) -> bool {
        matches!(self, Self::Cancelled)
    }
}

// ============================================================================
// RESULT TYPES
// ============================================================================

/// One stored transcript row. Unlike the `(text, start_ms, end_ms)` tuple the
/// local pipeline passes around, this can carry diarization.
#[derive(Debug, Clone, PartialEq)]
pub struct GeminiBatchSegment {
    pub text: String,
    pub start_ms: f64,
    pub end_ms: f64,
    pub speaker: Option<String>,
}

/// A word as returned by the gateway, with offsets already parsed to seconds
/// **relative to its own upload**.
#[derive(Debug, Clone, PartialEq)]
pub struct GeminiWord {
    pub text: String,
    pub start_secs: f64,
    pub end_secs: f64,
    /// Opaque Google label. Never parsed for structure.
    pub speaker: Option<String>,
}

// ============================================================================
// DURATION STRINGS
// ============================================================================

/// Parse a Google duration string (`"0.100s"`) into seconds.
///
/// Every timestamp in an annotated transcript flows through here, so a silent
/// fallback to 0 would quietly collapse a whole meeting onto one instant.
/// Malformed input is an error instead.
pub fn parse_duration_secs(raw: &str) -> Result<f64, GeminiBatchError> {
    let trimmed = raw.trim();
    let digits = trimmed.strip_suffix('s').ok_or_else(|| {
        GeminiBatchError::MalformedResponse(format!("duration {:?} has no trailing 's'", raw))
    })?;

    if digits.is_empty() {
        return Err(GeminiBatchError::MalformedResponse(format!(
            "duration {:?} has no value",
            raw
        )));
    }

    let secs: f64 = digits.parse().map_err(|_| {
        GeminiBatchError::MalformedResponse(format!("duration {:?} is not a number", raw))
    })?;

    if !secs.is_finite() || secs < 0.0 {
        return Err(GeminiBatchError::MalformedResponse(format!(
            "duration {:?} is out of range",
            raw
        )));
    }

    Ok(secs)
}

// ============================================================================
// PLANNER
// ============================================================================

/// One chunk of the source recording.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkSpec {
    pub index: usize,
    /// Recording-relative start, milliseconds.
    pub start_ms: u64,
    pub duration_ms: u64,
}

impl ChunkSpec {
    pub fn end_ms(&self) -> u64 {
        self.start_ms + self.duration_ms
    }
}

/// Fewest chunks that can hold `duration_ms` at `limit_ms` each.
///
/// Planned against the **hard** limit, never a limit minus a safety margin: a
/// 5s margin would turn exactly 60 minutes into two requests and double the
/// quota cost of every full-hour meeting. Overshoot is caught by probing the
/// transcoded chunks instead — see `replan_after_overshoot`.
pub fn initial_chunk_count(duration_ms: u64, limit_ms: u64) -> u64 {
    if duration_ms == 0 || limit_ms == 0 {
        return 1;
    }
    duration_ms.div_ceil(limit_ms).max(1)
}

/// Split `duration_ms` into `count` even chunks.
///
/// Even splitting is why 75 minutes becomes 2x37.5 rather than 60+15: the same
/// request count, but no runt chunk cutting a sentence in half at 60:00.
pub fn plan_with_count(duration_ms: u64, count: u64) -> Vec<ChunkSpec> {
    let count = count.max(1);
    let nominal = duration_ms.div_ceil(count).max(1);

    let mut chunks = Vec::new();
    let mut start = 0u64;
    let mut index = 0usize;

    while start < duration_ms {
        let remaining = duration_ms - start;
        let this = nominal.min(remaining);
        chunks.push(ChunkSpec {
            index,
            start_ms: start,
            duration_ms: this,
        });
        start += this;
        index += 1;
    }

    // A zero-length source still needs one (empty) chunk to report against.
    if chunks.is_empty() {
        chunks.push(ChunkSpec {
            index: 0,
            start_ms: 0,
            duration_ms: 0,
        });
    }

    chunks
}

/// Plan for a source of `duration_ms` under these options.
pub fn plan(duration_ms: u64, options: &GeminiBatchOptions) -> Vec<ChunkSpec> {
    plan_with_count(duration_ms, initial_chunk_count(duration_ms, options.limit_ms()))
}

/// Next plan to try when a transcoded chunk measured longer than the limit.
///
/// Container rounding can push a chunk a few milliseconds past the boundary.
/// Rather than pad every plan defensively, we measure and only then add a
/// chunk — re-planning costs local ffmpeg time, never quota.
pub fn replan_after_overshoot(
    duration_ms: u64,
    previous_count: u64,
    attempt: u32,
) -> Option<Vec<ChunkSpec>> {
    if attempt >= MAX_PLAN_ATTEMPTS {
        return None;
    }
    Some(plan_with_count(duration_ms, previous_count + 1))
}

// ============================================================================
// WORDS -> SEGMENTS
// ============================================================================

fn ends_sentence(text: &str) -> bool {
    text.trim_end_matches(|c: char| c == '"' || c == '\'' || c == ')')
        .ends_with(['.', '?', '!', '。', '？', '！'])
}

/// Namespace a raw label to its chunk.
///
/// Google's labels are opaque and may be chunk-local, so `spk:0` from two
/// independent uploads is not evidence of the same person. Prefixing the chunk
/// index makes accidental merging impossible; the label is resolved to a
/// display name later and never persisted in this form.
fn namespaced_speaker(chunk_index: usize, raw: &str) -> String {
    format!("{}\u{1f}{}", chunk_index, raw)
}

/// Group one chunk's words into transcript segments.
///
/// `base_ms` is the chunk's recording-relative start, added to every offset so
/// the result is positioned in the whole recording rather than in the upload.
pub fn group_words_into_segments(
    words: &[GeminiWord],
    base_ms: f64,
    chunk_index: usize,
) -> Vec<GeminiBatchSegment> {
    let mut segments: Vec<GeminiBatchSegment> = Vec::new();
    let mut current: Option<GeminiBatchSegment> = None;
    let mut previous_end: Option<f64> = None;

    for word in words {
        let speaker = word
            .speaker
            .as_ref()
            .map(|raw| namespaced_speaker(chunk_index, raw));

        let start_ms = base_ms + word.start_secs * 1000.0;
        let end_ms = base_ms + word.end_secs * 1000.0;

        let should_break = match &current {
            None => false,
            Some(seg) => {
                let speaker_changed = seg.speaker != speaker;
                let pause = previous_end
                    .map(|prev| word.start_secs - prev >= PAUSE_SPLIT_SECS)
                    .unwrap_or(false);
                let length_secs = (seg.end_ms - seg.start_ms) / 1000.0;
                let sentence_break =
                    length_secs >= MIN_SEGMENT_SECS && ends_sentence(&seg.text);
                let too_long = length_secs >= MAX_SEGMENT_SECS;

                speaker_changed || pause || sentence_break || too_long
            }
        };

        if should_break {
            if let Some(seg) = current.take() {
                segments.push(seg);
            }
        }

        match current.as_mut() {
            Some(seg) => {
                seg.text.push(' ');
                seg.text.push_str(&word.text);
                seg.end_ms = end_ms;
            }
            None => {
                current = Some(GeminiBatchSegment {
                    text: word.text.clone(),
                    start_ms,
                    end_ms,
                    speaker,
                });
            }
        }

        previous_end = Some(word.end_secs);
    }

    if let Some(seg) = current.take() {
        segments.push(seg);
    }

    segments
}

/// The single segment produced by an unannotated upload.
pub fn whole_chunk_segment(text: String, chunk: &ChunkSpec) -> GeminiBatchSegment {
    GeminiBatchSegment {
        text,
        start_ms: chunk.start_ms as f64,
        end_ms: chunk.end_ms() as f64,
        speaker: None,
    }
}

// ============================================================================
// SPEAKER RESOLUTION
// ============================================================================

/// Replace namespaced raw labels with canonical `Speaker N`.
///
/// Done once here, over the complete ordered result, because the frontend's
/// `displaySpeaker` is stateless and per-row: it sees neither the full list nor
/// the paginated, copy and summary paths, so it cannot number by first
/// appearance. The canonical value is what gets persisted.
///
/// Numbering can change at a chunk seam, because the underlying labels are
/// chunk-scoped. That is honest — claiming continuity would invent an identity
/// Google never asserted.
pub fn resolve_speaker_labels(segments: &mut [GeminiBatchSegment]) {
    let mut assigned: HashMap<String, String> = HashMap::new();

    for segment in segments.iter_mut() {
        let Some(raw) = segment.speaker.take() else {
            continue;
        };
        let next = assigned.len() + 1;
        let display = assigned
            .entry(raw)
            .or_insert_with(|| format!("Speaker {}", next))
            .clone();
        segment.speaker = Some(display);
    }
}

// ============================================================================
// HTTP STATUS -> ERROR
// ============================================================================

/// Map a gateway response status onto the typed error model.
pub fn classify_status(status: u16, body: &str) -> GeminiBatchError {
    let snippet: String = body.chars().take(300).collect();
    match status {
        429 => GeminiBatchError::QuotaExceeded,
        401 | 403 => GeminiBatchError::Auth,
        400 | 422 => GeminiBatchError::InvalidRequest(snippet),
        500..=599 => GeminiBatchError::ServerError(status),
        other => GeminiBatchError::InvalidRequest(format!("unexpected status {}: {}", other, snippet)),
    }
}

/// Map a reqwest failure onto the typed error model.
pub fn classify_transport(error: &reqwest::Error) -> GeminiBatchError {
    if error.is_timeout() {
        GeminiBatchError::Timeout
    } else {
        GeminiBatchError::Transport(error.to_string())
    }
}

/// Upload timeout. A ~29 MB chunk plus Gemini transcribing an hour of audio
/// does not fit in the 180s used for the old per-segment requests.
pub const UPLOAD_TIMEOUT: Duration = Duration::from_secs(900);

// ============================================================================
// TRANSCODE
// ============================================================================

/// Upload encoding. 16 kHz mono MP3 @ 64 kbps puts an hour at ~29 MB, which
/// keeps the 100 MiB cap out of reach so duration is the only binding limit,
/// and normalizes every container (mkv/webm/wma/mp4) into a format the gateway
/// accepts. 16 kHz mono is what Gemini uses internally, so the ASR cost is nil.
const UPLOAD_SAMPLE_RATE: &str = "16000";
const UPLOAD_BITRATE: &str = "64k";
pub const UPLOAD_MIME: &str = "audio/mpeg";

/// A temp file that deletes itself.
///
/// The transcode/upload path has three exits — success, error and
/// cancellation — and a plain `remove_file` at the end of the happy path
/// leaks a ~29 MB file on the other two.
#[derive(Debug)]
pub struct TempAudioFile {
    path: std::path::PathBuf,
}

impl TempAudioFile {
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for TempAudioFile {
    fn drop(&mut self) {
        if self.path.exists() {
            if let Err(e) = std::fs::remove_file(&self.path) {
                log::warn!(
                    "Could not remove temp upload file {}: {}",
                    self.path.display(),
                    e
                );
            }
        }
    }
}

/// Transcode one chunk of `source` into a temp MP3 ready for upload.
///
/// Honours `cancel` while ffmpeg is running: a 29 MB transcode would otherwise
/// keep burning CPU after the user cancelled. The child is killed *and
/// awaited*, so no orphan ffmpeg survives the job.
pub async fn transcode_chunk(
    source: &std::path::Path,
    chunk: &ChunkSpec,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<TempAudioFile, GeminiBatchError> {
    let ffmpeg = crate::audio::ffmpeg::find_ffmpeg_path()
        .ok_or_else(|| GeminiBatchError::Transcode("ffmpeg not found".to_string()))?;

    let output_path = std::env::temp_dir().join(format!(
        "meetily-gemini-{}-{}.mp3",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    // Owned before spawning, so even a spawn failure cleans up.
    let temp = TempAudioFile {
        path: output_path.clone(),
    };

    let start_secs = chunk.start_ms as f64 / 1000.0;
    let duration_secs = chunk.duration_ms as f64 / 1000.0;

    let mut command = tokio::process::Command::new(ffmpeg);
    command
        .args(["-hide_banner", "-nostdin", "-loglevel", "error", "-y"])
        // -ss before -i seeks by keyframe (fast); -accurate_seek then decodes
        // to the exact point, so chunk bases stay aligned with the offsets we
        // add to word timestamps.
        .args(["-accurate_seek", "-ss", &format!("{:.3}", start_secs)])
        .args(["-t", &format!("{:.3}", duration_secs)])
        .arg("-i")
        .arg(source)
        .args(["-vn", "-map", "0:a:0"])
        .args(["-ac", "1", "-ar", UPLOAD_SAMPLE_RATE])
        .args(["-c:a", "libmp3lame", "-b:a", UPLOAD_BITRATE, "-f", "mp3"])
        .arg(&output_path)
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());

    #[cfg(target_os = "windows")]
    {
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = command
        .spawn()
        .map_err(|e| GeminiBatchError::Transcode(format!("could not start ffmpeg: {}", e)))?;

    // Taken before the wait so `wait()` (which borrows) can be raced against
    // cancellation; `wait_with_output()` would consume the child and leave
    // nothing to kill. `-loglevel error` keeps this small enough not to fill
    // the pipe while we are not reading it.
    let mut stderr_pipe = child.stderr.take();

    let status = tokio::select! {
        result = child.wait() => {
            result.map_err(|e| GeminiBatchError::Transcode(format!("ffmpeg failed: {}", e)))?
        }
        _ = cancel.cancelled() => {
            // kill_on_drop would not reap; kill and await so the process is
            // genuinely gone before we return.
            let _ = child.kill().await;
            return Err(GeminiBatchError::Cancelled);
        }
    };

    if !status.success() {
        let mut stderr_text = String::new();
        if let Some(pipe) = stderr_pipe.as_mut() {
            use tokio::io::AsyncReadExt;
            let _ = pipe.read_to_string(&mut stderr_text).await;
        }
        return Err(GeminiBatchError::Transcode(format!(
            "ffmpeg exited with {}: {}",
            status,
            stderr_text.trim()
        )));
    }

    if !output_path.exists() {
        return Err(GeminiBatchError::Transcode(
            "ffmpeg reported success but produced no file".to_string(),
        ));
    }

    Ok(temp)
}

// ============================================================================
// EXECUTOR
// ============================================================================

/// Progress callback: `(chunk_index, chunk_count, stage)`.
/// `Send` is required because the batch runs inside a spawned Tauri task and
/// the callback is held across upload awaits.
pub type ProgressFn<'a> = &'a mut (dyn FnMut(usize, usize, &str) + Send);

/// Transcode every planned chunk and verify none overshot the limit.
///
/// Returns `None` when a chunk overshot, so the caller can re-plan. Nothing is
/// uploaded from here — that ordering is the whole safety property: a chunk
/// that measures 30:00.1 is discovered before it costs a request.
async fn prepare_chunks(
    source: &std::path::Path,
    chunks: &[ChunkSpec],
    limit_ms: u64,
    cancel: &tokio_util::sync::CancellationToken,
    on_progress: ProgressFn<'_>,
) -> Result<Option<Vec<TempAudioFile>>, GeminiBatchError> {
    let mut prepared = Vec::with_capacity(chunks.len());

    for chunk in chunks {
        if cancel.is_cancelled() {
            return Err(GeminiBatchError::Cancelled);
        }
        on_progress(chunk.index, chunks.len(), "preparing");

        let temp = transcode_chunk(source, chunk, cancel).await?;

        let measured = crate::audio::ffmpeg::probe_duration_ms(temp.path())
            .map_err(GeminiBatchError::Transcode)?;

        if measured > limit_ms {
            log::warn!(
                "Gemini batch: chunk {} measured {}ms over the {}ms limit; re-planning",
                chunk.index,
                measured,
                limit_ms
            );
            // Dropping `prepared` removes the temp files via their guards.
            return Ok(None);
        }

        prepared.push(temp);
    }

    Ok(Some(prepared))
}

/// Run a full batch job: plan, prepare, upload, assemble.
pub async fn run_batch(
    provider: &super::gemini_transcribe_provider::GeminiTranscribeProvider,
    source: &std::path::Path,
    duration_ms: u64,
    options: &GeminiBatchOptions,
    cancel: &tokio_util::sync::CancellationToken,
    on_progress: ProgressFn<'_>,
) -> Result<Vec<GeminiBatchSegment>, GeminiBatchError> {
    if duration_ms == 0 {
        return Err(GeminiBatchError::Transcode(
            "the recording has no duration".to_string(),
        ));
    }

    let limit_ms = options.limit_ms();
    let mut count = initial_chunk_count(duration_ms, limit_ms);
    let mut attempt = 0u32;

    // Adaptive preflight: plan against the hard limit, measure, and only add a
    // chunk if measurement says we must. Re-planning costs local ffmpeg time,
    // never quota — which is why there is no fixed safety margin subtracted
    // from the limit (that would double the cost of every full-hour meeting).
    let (chunks, prepared) = loop {
        let chunks = plan_with_count(duration_ms, count);
        log::info!(
            "Gemini batch: planning {} chunk(s) for {}ms (limit {}ms, annotated: {})",
            chunks.len(),
            duration_ms,
            limit_ms,
            options.is_annotated()
        );

        match prepare_chunks(source, &chunks, limit_ms, cancel, on_progress).await? {
            Some(prepared) => break (chunks, prepared),
            None => {
                attempt += 1;
                match replan_after_overshoot(duration_ms, count, attempt) {
                    Some(next) => count = next.len() as u64,
                    None => {
                        return Err(GeminiBatchError::Transcode(format!(
                            "could not split the recording into chunks under {}ms after {} attempts",
                            limit_ms, attempt
                        )))
                    }
                }
            }
        }
    };

    let mut segments = Vec::new();

    for (chunk, temp) in chunks.iter().zip(prepared.iter()) {
        if cancel.is_cancelled() {
            return Err(GeminiBatchError::Cancelled);
        }
        on_progress(chunk.index, chunks.len(), "transcribing");

        let result = provider.upload_file(temp.path(), options, cancel).await?;

        match result.words {
            Some(words) if !words.is_empty() => {
                segments.extend(group_words_into_segments(
                    &words,
                    chunk.start_ms as f64,
                    chunk.index,
                ));
            }
            // No annotations requested: one segment spanning this upload,
            // rather than inventing interior timings.
            _ => {
                if !result.text.trim().is_empty() {
                    segments.push(whole_chunk_segment(result.text, chunk));
                }
            }
        }
    }

    // Once, over the complete ordered result — the frontend helper is
    // stateless and per-row, so it cannot number by first appearance.
    resolve_speaker_labels(&mut segments);

    Ok(segments)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: u64 = 60 * 1000;

    fn word(text: &str, start: f64, end: f64, speaker: Option<&str>) -> GeminiWord {
        GeminiWord {
            text: text.to_string(),
            start_secs: start,
            end_secs: end,
            speaker: speaker.map(str::to_string),
        }
    }

    // ---- planner ----------------------------------------------------------

    #[test]
    fn an_hour_unannotated_is_a_single_request() {
        // The whole point of the change: this must not be 2.
        let options = GeminiBatchOptions::unannotated(None);
        assert_eq!(plan(60 * MIN, &options).len(), 1);
    }

    #[test]
    fn exactly_at_the_annotated_limit_is_a_single_request() {
        let options = GeminiBatchOptions::authoritative(false, None);
        assert_eq!(plan(30 * MIN, &options).len(), 1);
        assert_eq!(plan(60 * MIN, &options).len(), 2);
    }

    #[test]
    fn just_over_the_limit_splits_evenly() {
        let chunks = plan(61 * MIN, &GeminiBatchOptions::unannotated(None));
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].duration_ms, chunks[1].duration_ms);
    }

    #[test]
    fn seventy_five_minutes_is_two_even_chunks_not_sixty_plus_fifteen() {
        let chunks = plan(75 * MIN, &GeminiBatchOptions::unannotated(None));
        assert_eq!(chunks.len(), 2);
        for chunk in &chunks {
            assert_eq!(chunk.duration_ms, 37 * MIN + 30_000);
        }
    }

    #[test]
    fn chunks_tile_the_source_exactly() {
        for minutes in [1u64, 29, 30, 59, 60, 61, 120, 187] {
            for options in [
                GeminiBatchOptions::unannotated(None),
                GeminiBatchOptions::authoritative(true, None),
            ] {
                let duration = minutes * MIN;
                let chunks = plan(duration, &options);

                assert_eq!(chunks[0].start_ms, 0);
                assert_eq!(chunks.last().unwrap().end_ms(), duration);
                for pair in chunks.windows(2) {
                    assert_eq!(pair[0].end_ms(), pair[1].start_ms, "gap or overlap");
                }
                for chunk in &chunks {
                    assert!(chunk.duration_ms <= options.limit_ms());
                }
                assert_eq!(chunks.len() as u64, initial_chunk_count(duration, options.limit_ms()));
            }
        }
    }

    #[test]
    fn replanning_adds_one_chunk_and_eventually_gives_up() {
        let plan_b = replan_after_overshoot(60 * MIN, 1, 1).unwrap();
        assert_eq!(plan_b.len(), 2);
        assert!(replan_after_overshoot(60 * MIN, 1, MAX_PLAN_ATTEMPTS).is_none());
    }

    // ---- duration strings -------------------------------------------------

    #[test]
    fn parses_google_duration_strings() {
        assert_eq!(parse_duration_secs("0s").unwrap(), 0.0);
        assert_eq!(parse_duration_secs("0.100s").unwrap(), 0.1);
        assert_eq!(parse_duration_secs("12.340s").unwrap(), 12.34);
        assert!((parse_duration_secs("1234.567890123s").unwrap() - 1234.567890123).abs() < 1e-9);
        assert_eq!(parse_duration_secs("  3.5s  ").unwrap(), 3.5);
    }

    #[test]
    fn malformed_durations_are_errors_not_zero() {
        // Falling back to 0 would collapse a meeting onto one instant.
        for bad in ["", "s", "1.5", "abc s", "abcs", "-1s", "NaNs", "infs"] {
            assert!(parse_duration_secs(bad).is_err(), "{:?} should be rejected", bad);
        }
    }

    // ---- grouping ---------------------------------------------------------

    #[test]
    fn a_pause_ends_a_segment() {
        let words = vec![
            word("hello", 0.0, 0.4, None),
            word("there", 0.4, 0.8, None),
            // 0.9s gap > PAUSE_SPLIT_SECS
            word("later", 1.7, 2.0, None),
        ];
        let segments = group_words_into_segments(&words, 0.0, 0);
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].text, "hello there");
        assert_eq!(segments[1].text, "later");
    }

    #[test]
    fn a_speaker_change_ends_a_segment_even_without_a_pause() {
        let words = vec![
            word("yes", 0.0, 0.2, Some("spk:0")),
            word("no", 0.2, 0.4, Some("spk:1")),
        ];
        let segments = group_words_into_segments(&words, 0.0, 0);
        assert_eq!(segments.len(), 2);
    }

    #[test]
    fn punctuation_does_not_split_below_the_minimum_length() {
        // "Yes. No." inside 2s stays one row rather than becoming two stubs.
        let words = vec![
            word("Yes.", 0.0, 0.3, None),
            word("No.", 0.35, 0.6, None),
        ];
        assert_eq!(group_words_into_segments(&words, 0.0, 0).len(), 1);
    }

    #[test]
    fn punctuation_splits_once_past_the_minimum_length() {
        let words = vec![
            word("This", 0.0, 1.0, None),
            word("ends.", 1.0, 2.5, None),
            word("Next", 2.6, 3.0, None),
        ];
        let segments = group_words_into_segments(&words, 0.0, 0);
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[1].text, "Next");
    }

    #[test]
    fn an_unpunctuated_monologue_still_breaks_at_the_cap() {
        // No pauses, no punctuation, no speaker changes — only MAX_SEGMENT
        // prevents one unreadable row.
        let words: Vec<GeminiWord> = (0..200)
            .map(|i| {
                let t = i as f64 * 0.3;
                word("and", t, t + 0.3, None)
            })
            .collect();
        let segments = group_words_into_segments(&words, 0.0, 0);
        assert!(segments.len() > 1);
        for seg in &segments {
            assert!((seg.end_ms - seg.start_ms) / 1000.0 <= MAX_SEGMENT_SECS + 1.0);
        }
    }

    #[test]
    fn offsets_are_shifted_by_the_chunk_base() {
        let words = vec![word("later", 1.0, 2.0, None)];
        let base_ms = (30 * MIN) as f64;
        let segments = group_words_into_segments(&words, base_ms, 1);
        assert_eq!(segments[0].start_ms, base_ms + 1000.0);
        assert_eq!(segments[0].end_ms, base_ms + 2000.0);
    }

    #[test]
    fn segments_are_ordered_and_non_overlapping_but_gaps_survive() {
        // Real silence is a valid gap; filling it would invent timings.
        let words = vec![
            word("before", 0.0, 0.5, None),
            word("after", 10.0, 10.5, None),
        ];
        let segments = group_words_into_segments(&words, 0.0, 0);
        assert_eq!(segments.len(), 2);
        assert!(segments[0].end_ms < segments[1].start_ms);
        assert!(segments[1].start_ms - segments[0].end_ms > 1000.0, "gap must be preserved");
    }

    #[test]
    fn no_words_yields_no_segments() {
        assert!(group_words_into_segments(&[], 0.0, 0).is_empty());
    }

    // ---- unannotated ------------------------------------------------------

    #[test]
    fn unannotated_upload_becomes_exactly_one_segment_spanning_the_chunk() {
        let chunk = ChunkSpec {
            index: 0,
            start_ms: 0,
            duration_ms: 60 * MIN,
        };
        let segment = whole_chunk_segment("everything that was said".into(), &chunk);
        assert_eq!(segment.start_ms, 0.0);
        assert_eq!(segment.end_ms, (60 * MIN) as f64);
        assert_eq!(segment.speaker, None);
    }

    // ---- speaker resolution ----------------------------------------------

    #[test]
    fn speakers_are_numbered_by_first_appearance() {
        let mut segments = vec![
            GeminiBatchSegment { text: "a".into(), start_ms: 0.0, end_ms: 1.0, speaker: Some(namespaced_speaker(0, "spk:7")) },
            GeminiBatchSegment { text: "b".into(), start_ms: 1.0, end_ms: 2.0, speaker: Some(namespaced_speaker(0, "spk:3")) },
            GeminiBatchSegment { text: "c".into(), start_ms: 2.0, end_ms: 3.0, speaker: Some(namespaced_speaker(0, "spk:7")) },
        ];
        resolve_speaker_labels(&mut segments);

        // Numbering follows encounter order, not Google's numbering.
        assert_eq!(segments[0].speaker.as_deref(), Some("Speaker 1"));
        assert_eq!(segments[1].speaker.as_deref(), Some("Speaker 2"));
        assert_eq!(segments[2].speaker.as_deref(), Some("Speaker 1"));
    }

    #[test]
    fn identical_labels_from_different_chunks_are_not_conflated() {
        // spk:0 in chunk 0 and spk:0 in chunk 1 are not evidence of the same
        // person; treating them as one would fabricate an identity claim.
        let mut segments = vec![
            GeminiBatchSegment { text: "a".into(), start_ms: 0.0, end_ms: 1.0, speaker: Some(namespaced_speaker(0, "spk:0")) },
            GeminiBatchSegment { text: "b".into(), start_ms: 1.0, end_ms: 2.0, speaker: Some(namespaced_speaker(1, "spk:0")) },
        ];
        resolve_speaker_labels(&mut segments);
        assert_ne!(segments[0].speaker, segments[1].speaker);
    }

    #[test]
    fn segments_without_a_speaker_stay_unlabelled() {
        let mut segments = vec![GeminiBatchSegment {
            text: "a".into(),
            start_ms: 0.0,
            end_ms: 1.0,
            speaker: None,
        }];
        resolve_speaker_labels(&mut segments);
        assert_eq!(segments[0].speaker, None);
    }

    // ---- options ----------------------------------------------------------

    #[test]
    fn authoritative_pass_always_requests_word_timestamps() {
        let options = GeminiBatchOptions::authoritative(false, None);
        assert!(options.word_timestamps, "word timestamps are an invariant here");
        assert_eq!(options.limit_ms(), ANNOTATED_LIMIT_MS);
    }

    #[test]
    fn diarization_alone_still_counts_as_annotated() {
        let options = GeminiBatchOptions {
            diarization: true,
            word_timestamps: false,
            language: None,
        };
        assert!(options.is_annotated());
        assert_eq!(options.limit_ms(), ANNOTATED_LIMIT_MS);
    }

    #[test]
    fn unannotated_gets_the_full_hour() {
        assert_eq!(
            GeminiBatchOptions::unannotated(None).limit_ms(),
            UNANNOTATED_LIMIT_MS
        );
    }

    // ---- error classification --------------------------------------------

    #[test]
    fn quota_and_infrastructure_failures_are_transient() {
        assert!(GeminiBatchError::QuotaExceeded.is_transient());
        assert!(GeminiBatchError::Timeout.is_transient());
        assert!(GeminiBatchError::Transport("reset".into()).is_transient());
        assert!(GeminiBatchError::ServerError(502).is_transient());
    }

    #[test]
    fn configuration_and_contract_failures_are_hard() {
        assert!(!GeminiBatchError::Auth.is_transient());
        assert!(!GeminiBatchError::InvalidRequest("bad".into()).is_transient());
        assert!(!GeminiBatchError::MissingAnnotations.is_transient());
        assert!(!GeminiBatchError::MalformedResponse("x".into()).is_transient());
        assert!(!GeminiBatchError::Transcode("x".into()).is_transient());
    }

    #[test]
    fn cancellation_is_neither_transient_nor_a_failure_to_retry() {
        let err = GeminiBatchError::Cancelled;
        assert!(err.is_cancellation());
        assert!(!err.is_transient());
    }

    #[test]
    fn status_codes_map_to_the_right_variants() {
        assert!(matches!(classify_status(429, ""), GeminiBatchError::QuotaExceeded));
        assert!(matches!(classify_status(401, ""), GeminiBatchError::Auth));
        assert!(matches!(classify_status(403, ""), GeminiBatchError::Auth));
        assert!(matches!(classify_status(422, "no"), GeminiBatchError::InvalidRequest(_)));
        assert!(matches!(classify_status(500, ""), GeminiBatchError::ServerError(500)));
        assert!(matches!(classify_status(503, ""), GeminiBatchError::ServerError(503)));
    }

    #[test]
    fn quota_message_tells_the_user_it_is_retryable() {
        let message = GeminiBatchError::QuotaExceeded.to_string();
        assert!(message.contains("retried later"), "{}", message);
        assert!(message.contains("saved"), "{}", message);
    }
}
