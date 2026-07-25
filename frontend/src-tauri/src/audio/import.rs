// Audio file import module - allows importing external audio files as new meetings

use crate::api::TranscriptSegment;
use crate::audio::decoder::{decode_audio_file, decode_audio_file_with_progress};
use crate::audio::vad::get_speech_chunks_with_progress;
use crate::config::{DEFAULT_PARAKEET_MODEL, DEFAULT_WHISPER_MODEL};
use crate::parakeet_engine::ParakeetEngine;
use crate::state::AppState;
use crate::whisper_engine::WhisperEngine;
use anyhow::{anyhow, Result};
use log::{debug, error, info, warn};
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager, Runtime};
use tauri_plugin_dialog::DialogExt;
use uuid::Uuid;

use super::audio_processing::create_meeting_folder;
use super::common::{create_transcript_segments, split_segment_at_silence, write_transcripts_json};
use super::constants::AUDIO_EXTENSIONS;
use super::job_registry::{JobGuard, JobRegistry};
use super::recording_preferences::get_default_recordings_folder;

/// Registry of in-flight import jobs, keyed by import ID.
///
/// Remote jobs run concurrently up to the `remote_concurrency` cap; local
/// jobs claim exclusive use of the shared Whisper/Parakeet engines via the
/// engine coordinator. See `job_registry.rs` for the full semantics.
static IMPORT_JOBS: Lazy<JobRegistry> = Lazy::new(|| {
    JobRegistry::new(
        crate::audio::engine_coordinator::LocalEngineUser::Import,
        "Import",
        "import",
        "job",
    )
});

fn is_import_cancelled(import_id: &str) -> bool {
    IMPORT_JOBS.is_cancelled(import_id)
}

/// Cancellation tokens for URL imports still in their download/authentication
/// phase — before the shared import guard is acquired. Once the download
/// finishes the job transitions into `IMPORT_JOBS` like any other import and is
/// removed from here.
static URL_DOWNLOADS: Lazy<dashmap::DashMap<String, tokio_util::sync::CancellationToken>> =
    Lazy::new(dashmap::DashMap::new);

fn is_url_download_active(import_id: &str) -> bool {
    URL_DOWNLOADS.contains_key(import_id)
}

fn cancel_url_download(import_id: Option<&str>) {
    match import_id {
        Some(id) => {
            if let Some(token) = URL_DOWNLOADS.get(id) {
                token.cancel();
            }
        }
        None => {
            for entry in URL_DOWNLOADS.iter() {
                entry.value().cancel();
            }
        }
    }
}

/// VAD redemption time in milliseconds - bridges natural pauses in speech
/// Batch processing needs longer redemption (2000ms) than live pipeline (400ms)
/// because the entire file is processed at once by VAD, and 400ms fragments
/// speech at every natural sentence/topic pause (500ms-2s)
const VAD_REDEMPTION_TIME_MS: u32 = 2000;

/// Maximum file size: 20GB (prevents OOM and excessive processing time)
const MAX_FILE_SIZE_BYTES: u64 = 20 * 1024 * 1024 * 1024; // 20GB

/// Information about a selected audio file
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioFileInfo {
    pub path: String,
    pub filename: String,
    pub duration_seconds: f64,
    pub size_bytes: u64,
    pub format: String,
}

/// Candidate file for a batch import: enough for the selection list; full
/// validation happens per file when its import actually starts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BatchCandidate {
    pub path: String,
    pub file_name: String,
    pub size_bytes: u64,
}

/// Result of scanning a folder for importable audio.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FolderScanResult {
    pub candidates: Vec<BatchCandidate>,
    /// True when the scan stopped at the file cap — more audio exists.
    pub truncated: bool,
}

/// Bounds for recursive folder scans: enough for any sane recordings
/// directory while keeping a mistaken "pick C:\" recoverable.
pub(crate) const FOLDER_SCAN_MAX_DEPTH: usize = 8;
pub(crate) const FOLDER_SCAN_MAX_FILES: usize = 500;

/// Recursively collect audio files under `root` (by extension), skipping
/// dot-directories and dot-files. Deterministic: entries are visited in
/// name order and the result is sorted by path.
pub(crate) fn scan_folder_for_audio(
    root: &Path,
    max_depth: usize,
    max_files: usize,
) -> FolderScanResult {
    fn visit(
        dir: &Path,
        depth: usize,
        max_depth: usize,
        max_files: usize,
        candidates: &mut Vec<BatchCandidate>,
        truncated: &mut bool,
    ) {
        if depth > max_depth || *truncated {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return; // Unreadable subdir: skip rather than fail the scan.
        };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|e| e.file_name());

        for entry in entries {
            if *truncated {
                return;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            let path = entry.path();
            if path.is_dir() {
                visit(&path, depth + 1, max_depth, max_files, candidates, truncated);
            } else {
                let extension = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.to_lowercase())
                    .unwrap_or_default();
                if !AUDIO_EXTENSIONS.contains(&extension.as_str()) {
                    continue;
                }
                if candidates.len() >= max_files {
                    *truncated = true;
                    return;
                }
                let size_bytes = entry.metadata().map(|m| m.len()).unwrap_or(0);
                candidates.push(BatchCandidate {
                    path: path.to_string_lossy().to_string(),
                    file_name: name,
                    size_bytes,
                });
            }
        }
    }

    let mut candidates = Vec::new();
    let mut truncated = false;
    visit(root, 0, max_depth, max_files, &mut candidates, &mut truncated);
    candidates.sort_by(|a, b| a.path.cmp(&b.path));
    FolderScanResult {
        candidates,
        truncated,
    }
}

/// Progress update emitted during import
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportProgress {
    pub import_id: String,
    pub stage: String, // "copying", "decoding", "vad", "transcribing", "saving"
    pub progress_percentage: u32,
    pub message: String,
}

/// Result of import
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportResult {
    pub import_id: String,
    pub meeting_id: String,
    pub title: String,
    pub segments_count: usize,
    pub duration_seconds: f64,
}

/// Error during import
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportError {
    pub import_id: String,
    pub error: String,
}

/// Warning emitted during import (non-fatal)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportWarning {
    pub import_id: String,
    pub warning: String,
    pub details: Option<String>,
}

/// Response when import is started
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportStarted {
    pub import_id: String,
    pub message: String,
}

/// Check if import is currently in progress (including URL imports still
/// downloading, which have not yet claimed the shared import guard).
pub fn is_import_in_progress() -> bool {
    IMPORT_JOBS.has_active_jobs() || !URL_DOWNLOADS.is_empty()
}

/// Cancel ongoing import.
/// If `import_id` is None, all currently active imports are cancelled.
pub fn cancel_import(import_id: Option<&str>) {
    IMPORT_JOBS.cancel(import_id);
}

/// Validate an audio file and return its info using metadata-only approach
/// Falls back to full decode if metadata is unavailable
pub fn validate_audio_file(path: &Path) -> Result<AudioFileInfo> {
    // Check file exists
    if !path.exists() {
        return Err(anyhow!("File does not exist: {}", path.display()));
    }

    // Check extension
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .unwrap_or_default();

    if !AUDIO_EXTENSIONS.contains(&extension.as_str()) {
        return Err(anyhow!(
            "Unsupported format: .{}. Supported: {}",
            extension,
            AUDIO_EXTENSIONS.join(", ")
        ));
    }

    // Get file size
    let metadata = std::fs::metadata(path)
        .map_err(|e| anyhow!("Cannot read file: {}", e))?;
    let size_bytes = metadata.len();

    // Check file size limit
    if size_bytes > MAX_FILE_SIZE_BYTES {
        return Err(anyhow!(
            "File too large: {:.2}GB. Maximum supported size is {}GB",
            size_bytes as f64 / (1024.0 * 1024.0 * 1024.0),
            MAX_FILE_SIZE_BYTES / (1024 * 1024 * 1024)
        ));
    }

    // Get filename without extension for title
    let filename = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Imported Audio")
        .to_string();

    // Try fast metadata-only validation first
    let duration_seconds = match extract_duration_from_metadata(path) {
        Ok(duration) => {
            debug!(
                "Got duration from metadata: {:.2}s (fast path)",
                duration
            );
            duration
        }
        Err(e) => {
            // Fallback to full decode if metadata unavailable
            warn!(
                "Metadata extraction failed: {}, falling back to full decode",
                e
            );
            let decoded = decode_audio_file(path)?;
            decoded.duration_seconds
        }
    };

    Ok(AudioFileInfo {
        path: path.to_string_lossy().to_string(),
        filename,
        duration_seconds,
        size_bytes,
        format: extension.to_uppercase(),
    })
}

/// Extract duration from audio file metadata without full decode
/// Returns error if metadata is unavailable, triggering fallback to full decode
fn extract_duration_from_metadata(path: &Path) -> Result<f64> {
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;

    // Open the file
    let file = std::fs::File::open(path)
        .map_err(|e| anyhow!("Failed to open audio file: {}", e))?;

    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    // Set up format hint based on file extension
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    // Probe the file format (lightweight operation)
    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| anyhow!("Failed to probe audio format: {}", e))?;

    let format = probed.format;

    // Find the first audio track
    use symphonia::core::codecs::CODEC_TYPE_NULL;
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or_else(|| anyhow!("No audio track found in file"))?;

    // Extract duration from metadata
    let sample_rate = track
        .codec_params
        .sample_rate
        .ok_or_else(|| anyhow!("Unknown sample rate"))?;

    let n_frames = track
        .codec_params
        .n_frames
        .ok_or_else(|| anyhow!("Frame count not available in metadata"))?;

    let duration_seconds = n_frames as f64 / sample_rate as f64;

    debug!(
        "Extracted metadata: {}Hz, {} frames, {:.2}s",
        sample_rate, n_frames, duration_seconds
    );

    Ok(duration_seconds)
}

/// Start import of an audio file
pub async fn start_import<R: Runtime>(
    app: AppHandle<R>,
    source_path: String,
    title: String,
    language: Option<String>,
    model: Option<String>,
    provider: Option<String>,
) -> Result<ImportResult> {
    let import_id = format!("import-{}", Uuid::new_v4());
    let use_remote = provider.as_deref() == Some("openaiCompatible");
    let guard = IMPORT_JOBS
        .acquire(import_id.clone(), use_remote)
        .map_err(|e| anyhow!(e))?;

    start_import_with_guard(
        app,
        import_id,
        source_path,
        title,
        language,
        model,
        provider,
        None,
        None,
        guard,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn start_import_with_guard<R: Runtime>(
    app: AppHandle<R>,
    import_id: String,
    source_path: String,
    title: String,
    language: Option<String>,
    model: Option<String>,
    provider: Option<String>,
    source_url: Option<String>,
    mode: Option<String>,
    _guard: JobGuard<'static>,
) -> Result<ImportResult> {
    let use_parakeet = provider.as_deref() == Some("parakeet");
    let use_remote = provider.as_deref() == Some("openaiCompatible");

    // Journal the job so a crash mid-import is detected (and its orphaned
    // folder cleaned up) on the next launch. See job_persistence.rs.
    super::job_persistence::try_record_job_started(
        &app,
        &super::job_persistence::PersistedJob {
            id: import_id.clone(),
            kind: "import".to_string(),
            title: title.clone(),
            source_path: Some(source_path.clone()),
            source_url,
            mode,
            folder_path: None,
            meeting_id: None,
            language: language.clone(),
            model: model.clone(),
            provider: provider.clone(),
            created_at: chrono::Utc::now().to_rfc3339(),
        },
    )
    .await;

    let result = run_import(
        app.clone(),
        import_id.clone(),
        source_path,
        title,
        language,
        model,
        provider,
    )
    .await;

    // Unload the engine after the batch job (success, failure, or cancellation).
    // Remote transcription loads no local model, so there is nothing to unload.
    if !use_remote {
        super::common::unload_engine_after_batch(use_parakeet).await;
    }

    match &result {
        Ok(res) => {
            let _ = app.emit(
                "import-complete",
                serde_json::json!({
                    "import_id": res.import_id,
                    "meeting_id": res.meeting_id,
                    "title": res.title,
                    "segments_count": res.segments_count,
                    "duration_seconds": res.duration_seconds
                }),
            );
        }
        Err(e) => {
            let _ = app.emit(
                "import-error",
                ImportError {
                    import_id: import_id.clone(),
                    error: e.to_string(),
                },
            );
        }
    }

    // The job finished in-process (either way, the user was told), so it
    // must not be reported as interrupted on the next launch.
    super::job_persistence::try_clear_job(&app, &import_id).await;

    result
}

/// Internal function to run import
async fn run_import<R: Runtime>(
    app: AppHandle<R>,
    import_id: String,
    source_path: String,
    title: String,
    language: Option<String>,
    model: Option<String>,
    provider: Option<String>,
) -> Result<ImportResult> {
    let source = PathBuf::from(&source_path);

    // Validate source file
    if !source.exists() {
        return Err(anyhow!("Source file not found: {}", source.display()));
    }

    info!(
        "Starting import for '{}' from {} with language {:?}, model {:?}, provider {:?}",
        title, source_path, language, model, provider
    );

    // Determine which provider to use (default to whisper)
    let use_parakeet = provider.as_deref() == Some("parakeet");
    let use_remote = provider.as_deref() == Some("openaiCompatible");

    emit_progress(&app, &import_id, "copying", 5, "Creating meeting folder...");

    // Check for cancellation
    if is_import_cancelled(&import_id) {
        return Err(anyhow!("Import cancelled"));
    }

    // Create meeting folder
    let base_folder = get_default_recordings_folder();
    let meeting_folder = create_meeting_folder(&base_folder, &title, false)?;

    // From here on a crash would leave this folder half-built; journal it so
    // startup reconciliation can clean it up.
    super::job_persistence::try_set_job_folder(
        &app,
        &import_id,
        &meeting_folder.to_string_lossy(),
    )
    .await;

    // Copy audio file to meeting folder
    emit_progress(&app, &import_id, "copying", 10, "Copying audio file...");

    let dest_filename = format!(
        "audio.{}",
        source
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("mp4")
    );
    let dest_path = meeting_folder.join(&dest_filename);

    let src = source.clone();
    let dst = dest_path.clone();
    tokio::task::spawn_blocking(move || std::fs::copy(&src, &dst))
        .await
        .map_err(|e| anyhow!("Copy task join error: {}", e))?
        .map_err(|e| anyhow!("Failed to copy audio file: {}", e))?;

    info!("Copied audio to: {}", dest_path.display());

    // Check for cancellation
    if is_import_cancelled(&import_id) {
        // Cleanup: remove the meeting folder
        let _ = std::fs::remove_dir_all(&meeting_folder);
        return Err(anyhow!("Import cancelled"));
    }

    emit_progress(&app, &import_id, "decoding", 15, "Decoding audio file...");

    // Decode the audio file with progress updates
    let app_for_decode = app.clone();
    let import_id_for_decode = import_id.clone();
    let decode_progress = Box::new(move |progress: u32, msg: &str| {
        // Map decode progress: 15% + (progress * 0.05) to go from 15% to 20%
        let overall_progress = 15 + ((progress as f32 * 0.05) as u32);
        emit_progress(&app_for_decode, &import_id_for_decode, "decoding", overall_progress, msg);
    });

    let path_for_decode = dest_path.clone();
    let decoded = tokio::task::spawn_blocking(move || {
        decode_audio_file_with_progress(&path_for_decode, Some(decode_progress))
    })
    .await
    .map_err(|e| anyhow!("Decode task join error: {}", e))??;
    let duration_seconds = decoded.duration_seconds;

    info!(
        "Decoded audio: {:.2}s, {}Hz, {} channels",
        duration_seconds, decoded.sample_rate, decoded.channels
    );

    emit_progress(&app, &import_id, "resampling", 20, "Converting audio format...");

    // Check for cancellation
    if is_import_cancelled(&import_id) {
        let _ = std::fs::remove_dir_all(&meeting_folder);
        return Err(anyhow!("Import cancelled"));
    }

    // Convert to 16kHz mono format with progress updates
    let app_for_resample = app.clone();
    let import_id_for_resample = import_id.clone();
    let resample_progress = Box::new(move |progress: u32, msg: &str| {
        // Map resample progress: 20% + (progress * 0.05) to go from 20% to 25%
        let overall_progress = 20 + ((progress as f32 * 0.05) as u32);
        emit_progress(&app_for_resample, &import_id_for_resample, "resampling", overall_progress, msg);
    });

    let audio_samples = tokio::task::spawn_blocking(move || {
        decoded.to_whisper_format_with_progress(Some(resample_progress))
    })
    .await
    .map_err(|e| anyhow!("Resample task join error: {}", e))?;
    info!(
        "Converted to 16kHz mono format: {} samples",
        audio_samples.len()
    );

    emit_progress(&app, &import_id, "vad", 25, "Detecting speech segments...");

    // Check for cancellation
    if is_import_cancelled(&import_id) {
        let _ = std::fs::remove_dir_all(&meeting_folder);
        return Err(anyhow!("Import cancelled"));
    }

    // Use VAD to find speech segments
    let app_for_vad = app.clone();
    let import_id_for_vad = import_id.clone();

    let speech_segments = tokio::task::spawn_blocking(move || {
        get_speech_chunks_with_progress(
            &audio_samples,
            VAD_REDEMPTION_TIME_MS,
            |vad_progress, segments_found| {
                let overall_progress = 25 + (vad_progress as f32 * 0.05) as u32;
                emit_progress(
                    &app_for_vad,
                    &import_id_for_vad,
                    "vad",
                    overall_progress,
                    &format!(
                        "Detecting speech segments... {}% ({} found)",
                        vad_progress, segments_found
                    ),
                );
                !is_import_cancelled(&import_id_for_vad)
            },
        )
    })
    .await
    .map_err(|e| anyhow!("VAD task panicked: {}", e))?
    .map_err(|e| anyhow!("VAD processing failed: {}", e))?;

    let total_segments = speech_segments.len();
    info!("VAD detected {} speech segments (redemption_time={}ms)", total_segments, VAD_REDEMPTION_TIME_MS);

    // Diagnostic: log segment duration distribution
    if !speech_segments.is_empty() {
        let durations_ms: Vec<f64> = speech_segments.iter()
            .map(|s| s.end_timestamp_ms - s.start_timestamp_ms)
            .collect();
        let total_speech_ms: f64 = durations_ms.iter().sum();
        let avg_duration = total_speech_ms / durations_ms.len() as f64;
        let min_duration = durations_ms.iter().cloned().fold(f64::INFINITY, f64::min);
        let max_duration = durations_ms.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        info!(
            "VAD segment stats: avg={:.0}ms, min={:.0}ms, max={:.0}ms, total_speech={:.1}s/{:.1}s ({:.0}%)",
            avg_duration, min_duration, max_duration,
            total_speech_ms / 1000.0, duration_seconds,
            (total_speech_ms / 1000.0 / duration_seconds) * 100.0
        );
        // Log first 10 segments for detailed inspection
        for (i, seg) in speech_segments.iter().take(10).enumerate() {
            let dur = seg.end_timestamp_ms - seg.start_timestamp_ms;
            debug!("  Segment {}: {:.0}ms-{:.0}ms ({:.0}ms, {} samples)",
                i, seg.start_timestamp_ms, seg.end_timestamp_ms, dur, seg.samples.len());
        }
        if total_segments > 10 {
            debug!("  ... and {} more segments", total_segments - 10);
        }
    }

    if total_segments == 0 {
        warn!("No speech detected in audio");

        // Emit warning to frontend
        let _ = app.emit(
            "import-warning",
            ImportWarning {
                import_id: import_id.clone(),
                warning: "No speech detected in audio file".to_string(),
                details: Some(
                    "The file was imported successfully, but VAD did not detect any speech. \
                     The meeting was created but contains no transcripts.".to_string()
                ),
            },
        );
        // Still create the meeting, just with no transcripts
    }

    // Check for cancellation
    if is_import_cancelled(&import_id) {
        let _ = std::fs::remove_dir_all(&meeting_folder);
        return Err(anyhow!("Import cancelled"));
    }

    emit_progress(&app, &import_id, "transcribing", 30, "Loading transcription engine...");

    // Initialize the appropriate engine
    let whisper_engine = if !use_parakeet && !use_remote && total_segments > 0 {
        Some(get_or_init_whisper(&app, model.as_deref()).await?)
    } else {
        None
    };
    let parakeet_engine = if use_parakeet && total_segments > 0 {
        Some(get_or_init_parakeet(&app, model.as_deref()).await?)
    } else {
        None
    };
    let remote_provider = if use_remote && total_segments > 0 {
        Some(
            crate::audio::transcription::OpenAICompatibleProvider::from_saved_settings(
                &app,
                model.clone(),
            )
            .await
            .map_err(|e| anyhow!(e))?,
        )
    } else {
        None
    };

    // Split very long segments at silence boundaries for better transcription quality.
    // Hard cuts at arbitrary sample positions lose words at boundaries. Instead, scan
    // for the lowest-energy window near the target split point and cut there.
    const MAX_SEGMENT_SAMPLES: usize = 25 * 16000; // 25 seconds at 16kHz

    let mut processable_segments: Vec<crate::audio::vad::SpeechSegment> = Vec::new();
    for segment in &speech_segments {
        if segment.samples.len() > MAX_SEGMENT_SAMPLES {
            debug!(
                "Splitting large segment ({:.0}ms, {} samples) at silence boundaries",
                segment.end_timestamp_ms - segment.start_timestamp_ms,
                segment.samples.len()
            );

            let sub_segments = split_segment_at_silence(segment, MAX_SEGMENT_SAMPLES);
            debug!("Split into {} sub-segments", sub_segments.len());
            processable_segments.extend(sub_segments);
        } else {
            processable_segments.push(segment.clone());
        }
    }

    let processable_count = processable_segments.len();
    info!("Processing {} segments (after splitting)", processable_count);

    // Process each speech segment
    let mut all_transcripts: Vec<(String, f64, f64)> = Vec::new();
    let mut total_confidence = 0.0f32;

    for (i, segment) in processable_segments.iter().enumerate() {
        if is_import_cancelled(&import_id) {
            let _ = std::fs::remove_dir_all(&meeting_folder);
            return Err(anyhow!("Import cancelled"));
        }

        let progress = 30 + ((i as f32 / processable_count.max(1) as f32) * 50.0) as u32;
        let segment_duration_sec = (segment.end_timestamp_ms - segment.start_timestamp_ms) / 1000.0;
        emit_progress(
            &app,
            &import_id,
            "transcribing",
            progress,
            &format!(
                "Transcribing segment {} of {} ({:.1}s)...",
                i + 1,
                processable_count,
                segment_duration_sec
            ),
        );

        // Skip very short segments
        if segment.samples.len() < 1600 {
            debug!(
                "Skipping short segment {} with {} samples",
                i,
                segment.samples.len()
            );
            continue;
        }

        // Transcribe
        let (text, conf) = if use_remote {
            use crate::audio::transcription::TranscriptionProvider;
            let engine = remote_provider.as_ref().unwrap();
            let result = engine
                .transcribe(segment.samples.clone(), language.clone())
                .await
                .map_err(|e| anyhow!("Remote transcription failed on segment {}: {}", i, e))?;
            (result.text, result.confidence.unwrap_or(0.9))
        } else if use_parakeet {
            let engine = parakeet_engine.as_ref().unwrap();
            let text = engine
                .transcribe_audio(segment.samples.clone())
                .await
                .map_err(|e| anyhow!("Parakeet transcription failed on segment {}: {}", i, e))?;
            (text, 0.9f32)
        } else {
            let engine = whisper_engine.as_ref().unwrap();
            let (text, conf, _) = engine
                .transcribe_audio_with_confidence(segment.samples.clone(), language.clone())
                .await
                .map_err(|e| anyhow!("Whisper transcription failed on segment {}: {}", i, e))?;
            (text, conf)
        };

        let trimmed = text.trim();
        if !trimmed.is_empty() {
            debug!(
                "Segment {}/{}: {:.1}s, conf={:.2}, text='{}'",
                i + 1, processable_count, segment_duration_sec, conf,
                if trimmed.len() > 80 { let mut end = 80; while !trimmed.is_char_boundary(end) { end -= 1; } &trimmed[..end] } else { trimmed }
            );
            all_transcripts.push((text, segment.start_timestamp_ms, segment.end_timestamp_ms));
            total_confidence += conf;
        } else {
            debug!("Segment {}/{}: {:.1}s — empty transcription", i + 1, processable_count, segment_duration_sec);
        }
    }

    let transcribed_count = all_transcripts.len();
    let avg_confidence = if transcribed_count > 0 {
        total_confidence / transcribed_count as f32
    } else {
        0.0
    };

    info!(
        "Transcription complete: {} segments transcribed out of {}, avg confidence: {:.2}",
        transcribed_count, processable_count, avg_confidence
    );

    // Check for cancellation
    if is_import_cancelled(&import_id) {
        let _ = std::fs::remove_dir_all(&meeting_folder);
        return Err(anyhow!("Import cancelled"));
    }

    emit_progress(&app, &import_id, "saving", 85, "Creating meeting...");

    // Create transcript segments
    let segments = create_transcript_segments(&all_transcripts);

    // Save to database
    let app_state = app
        .try_state::<AppState>()
        .ok_or_else(|| anyhow!("App state not available"))?;

    let meeting_id = create_meeting_with_transcripts(
        app_state.db_manager.pool(),
        &title,
        &segments,
        meeting_folder.to_string_lossy().to_string(),
    )
    .await?;

    // Write transcripts.json and metadata.json to the meeting folder
    emit_progress(&app, &import_id, "saving", 90, "Writing transcript files...");

    if let Err(e) = write_transcripts_json(&meeting_folder, &segments) {
        warn!("Failed to write transcripts.json: {}", e);
    }

    if let Err(e) = write_import_metadata(
        &meeting_folder,
        &meeting_id,
        &title,
        duration_seconds,
        &dest_filename,
        "import",
    ) {
        warn!("Failed to write metadata.json: {}", e);
    }

    emit_progress(&app, &import_id, "complete", 100, "Import complete");

    Ok(ImportResult {
        import_id,
        meeting_id,
        title,
        segments_count: segments.len(),
        duration_seconds,
    })
}

/// Emit progress event
fn emit_progress<R: Runtime>(
    app: &AppHandle<R>,
    import_id: &str,
    stage: &str,
    progress: u32,
    message: &str,
) {
    let _ = app.emit(
        "import-progress",
        ImportProgress {
            import_id: import_id.to_string(),
            stage: stage.to_string(),
            progress_percentage: progress,
            message: message.to_string(),
        },
    );
}


/// Create a new meeting with transcripts in the database
async fn create_meeting_with_transcripts(
    pool: &sqlx::SqlitePool,
    title: &str,
    segments: &[TranscriptSegment],
    folder_path: String,
) -> Result<String> {
    let meeting_id = format!("meeting-{}", Uuid::new_v4());
    let now = chrono::Utc::now();

    // Start transaction
    let mut conn = pool.acquire().await.map_err(|e| anyhow!("DB error: {}", e))?;
    let mut tx = sqlx::Connection::begin(&mut *conn)
        .await
        .map_err(|e| anyhow!("Failed to start transaction: {}", e))?;

    // Insert meeting
    sqlx::query(
        "INSERT INTO meetings (id, title, created_at, updated_at, folder_path)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(&meeting_id)
    .bind(title)
    .bind(now)
    .bind(now)
    .bind(&folder_path)
    .execute(&mut *tx)
    .await
    .map_err(|e| anyhow!("Failed to create meeting: {}", e))?;

    // Insert transcripts
    for segment in segments {
        sqlx::query(
            "INSERT INTO transcripts (id, meeting_id, transcript, timestamp, audio_start_time, audio_end_time, duration, speaker)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&segment.id)
        .bind(&meeting_id)
        .bind(&segment.text)
        .bind(&segment.timestamp)
        .bind(segment.audio_start_time)
        .bind(segment.audio_end_time)
        .bind(segment.duration)
        .bind(&segment.speaker)
        .execute(&mut *tx)
        .await
        .map_err(|e| anyhow!("Failed to insert transcript: {}", e))?;
    }

    tx.commit()
        .await
        .map_err(|e| anyhow!("Failed to commit transaction: {}", e))?;

    info!(
        "Created meeting '{}' with {} transcripts",
        meeting_id,
        segments.len()
    );

    Ok(meeting_id)
}

/// Get or initialize the Whisper engine
async fn get_or_init_whisper<R: Runtime>(
    app: &AppHandle<R>,
    requested_model: Option<&str>,
) -> Result<Arc<WhisperEngine>> {
    use crate::whisper_engine::commands::WHISPER_ENGINE;

    let engine = {
        let guard = WHISPER_ENGINE.lock().unwrap_or_else(|e| e.into_inner());
        guard.as_ref().cloned()
    };

    match engine {
        Some(e) => {
            let target_model = match requested_model {
                Some(model) => model.to_string(),
                None => get_configured_model(app, "whisper").await?,
            };

            let current_model = e.get_current_model().await;
            let needs_load = match &current_model {
                Some(loaded) => loaded != &target_model,
                None => true,
            };

            if needs_load {
                info!(
                    "Loading Whisper model '{}' (current: {:?})",
                    target_model, current_model
                );

                if let Err(e) = e.discover_models().await {
                    warn!("Model discovery error (continuing): {}", e);
                }

                e.load_model(&target_model)
                    .await
                    .map_err(|e| anyhow!("Failed to load model '{}': {}", target_model, e))?;
            }

            Ok(e)
        }
        None => Err(anyhow!("Whisper engine not initialized")),
    }
}

/// Get or initialize the Parakeet engine
async fn get_or_init_parakeet<R: Runtime>(
    app: &AppHandle<R>,
    requested_model: Option<&str>,
) -> Result<Arc<ParakeetEngine>> {
    use crate::parakeet_engine::commands::PARAKEET_ENGINE;

    let engine = {
        let guard = PARAKEET_ENGINE.lock().unwrap_or_else(|e| e.into_inner());
        guard.as_ref().cloned()
    };

    match engine {
        Some(e) => {
            let target_model = match requested_model {
                Some(model) => model.to_string(),
                None => get_configured_model(app, "parakeet").await?,
            };

            let current_model = e.get_current_model().await;
            let needs_load = match &current_model {
                Some(loaded) => loaded != &target_model,
                None => true,
            };

            if needs_load {
                info!(
                    "Loading Parakeet model '{}' (current: {:?})",
                    target_model, current_model
                );

                if let Err(e) = e.discover_models().await {
                    warn!("Model discovery error (continuing): {}", e);
                }

                e.load_model(&target_model)
                    .await
                    .map_err(|e| anyhow!("Failed to load model '{}': {}", target_model, e))?;
            }

            Ok(e)
        }
        None => Err(anyhow!("Parakeet engine not initialized")),
    }
}

/// Get the configured model from database
async fn get_configured_model<R: Runtime>(app: &AppHandle<R>, provider_type: &str) -> Result<String> {
    let app_state = app
        .try_state::<AppState>()
        .ok_or_else(|| anyhow!("App state not available"))?;

    let result: Option<(String, String)> = sqlx::query_as(
        "SELECT provider, model FROM transcript_settings WHERE id = '1'",
    )
    .fetch_optional(app_state.db_manager.pool())
    .await
    .map_err(|e| anyhow!("Failed to query config: {}", e))?;

    match result {
        Some((provider, model)) => {
            if (provider_type == "whisper" && (provider == "localWhisper" || provider == "whisper"))
                || (provider_type == "parakeet" && provider == "parakeet")
            {
                Ok(model)
            } else {
                // Return default model for the requested type
                Ok(if provider_type == "parakeet" {
                    DEFAULT_PARAKEET_MODEL.to_string()
                } else {
                    DEFAULT_WHISPER_MODEL.to_string()
                })
            }
        }
        None => Ok(if provider_type == "parakeet" {
            DEFAULT_PARAKEET_MODEL.to_string()
        } else {
            DEFAULT_WHISPER_MODEL.to_string()
        }),
    }
}

/// Write metadata.json to a meeting folder (atomic write with temp file)
fn write_import_metadata(
    folder: &Path,
    meeting_id: &str,
    title: &str,
    duration_seconds: f64,
    audio_filename: &str,
    source: &str,
) -> Result<()> {
    let metadata_path = folder.join("metadata.json");
    let temp_path = folder.join(".metadata.json.tmp");
    let now = chrono::Utc::now().to_rfc3339();

    let json = serde_json::json!({
        "version": "1.0",
        "meeting_id": meeting_id,
        "meeting_name": title,
        "created_at": now,
        "completed_at": now,
        "duration_seconds": duration_seconds,
        "audio_file": audio_filename,
        "transcript_file": "transcripts.json",
        "status": "completed",
        "source": source
    });

    let json_string = serde_json::to_string_pretty(&json)?;
    std::fs::write(&temp_path, &json_string)?;
    std::fs::rename(&temp_path, &metadata_path)?;

    info!("Wrote metadata.json to {}", metadata_path.display());
    Ok(())
}

// ============================================================================
// Tauri Commands
// ============================================================================

/// Select an audio file and validate it
#[tauri::command]
pub async fn select_and_validate_audio_command<R: Runtime>(
    app: AppHandle<R>,
) -> Result<Option<AudioFileInfo>, String> {
    info!("Opening file dialog for audio import");

    // Use spawn_blocking to avoid blocking async runtime
    let app_clone = app.clone();
    let file_path = tokio::task::spawn_blocking(move || {
        app_clone
            .dialog()
            .file()
            .add_filter("Audio Files", &AUDIO_EXTENSIONS.iter().map(|s| *s).collect::<Vec<_>>())
            .blocking_pick_file()
    })
    .await
    .map_err(|e| format!("File dialog task failed: {}", e))?;

    match file_path {
        Some(path) => {
            let path_str = path.to_string();
            info!("User selected: {}", path_str);

            match validate_audio_file(Path::new(&path_str)) {
                Ok(info) => Ok(Some(info)),
                Err(e) => {
                    error!("Validation failed: {}", e);
                    Err(e.to_string())
                }
            }
        }
        None => {
            info!("User cancelled file selection");
            Ok(None)
        }
    }
}

/// Validate an audio file from a given path (for drag-drop)
#[tauri::command]
pub async fn validate_audio_file_command(path: String) -> Result<AudioFileInfo, String> {
    info!("Validating audio file: {}", path);
    validate_audio_file(Path::new(&path)).map_err(|e| e.to_string())
}

/// Select multiple audio files for a batch import. Files that fail
/// validation are skipped with a log line — the user reviews the returned
/// list in the dialog before anything starts.
#[tauri::command]
pub async fn select_audio_files_command<R: Runtime>(
    app: AppHandle<R>,
) -> Result<Vec<BatchCandidate>, String> {
    info!("Opening multi-file dialog for batch audio import");

    let app_clone = app.clone();
    let picked = tokio::task::spawn_blocking(move || {
        app_clone
            .dialog()
            .file()
            .add_filter(
                "Audio Files",
                &AUDIO_EXTENSIONS.iter().map(|s| *s).collect::<Vec<_>>(),
            )
            .blocking_pick_files()
    })
    .await
    .map_err(|e| format!("File dialog task failed: {}", e))?;

    let Some(paths) = picked else {
        return Ok(Vec::new());
    };

    let mut candidates = Vec::new();
    for picked_path in paths {
        let path_str = picked_path.to_string();
        match validate_audio_file(Path::new(&path_str)) {
            Ok(info) => candidates.push(BatchCandidate {
                path: info.path,
                file_name: info.filename,
                size_bytes: info.size_bytes,
            }),
            Err(e) => warn!("Skipping unimportable selection {}: {}", path_str, e),
        }
    }
    Ok(candidates)
}

/// Pick a folder and scan it recursively for importable audio files.
/// Returns None when the user cancels the picker.
#[tauri::command]
pub async fn select_audio_folder_command<R: Runtime>(
    app: AppHandle<R>,
) -> Result<Option<FolderScanResult>, String> {
    info!("Opening folder dialog for batch audio import");

    let app_clone = app.clone();
    let picked = tokio::task::spawn_blocking(move || app_clone.dialog().file().blocking_pick_folder())
        .await
        .map_err(|e| format!("Folder dialog task failed: {}", e))?;

    let Some(folder) = picked else {
        return Ok(None);
    };
    let root = PathBuf::from(folder.to_string());
    let result = tokio::task::spawn_blocking(move || {
        scan_folder_for_audio(&root, FOLDER_SCAN_MAX_DEPTH, FOLDER_SCAN_MAX_FILES)
    })
    .await
    .map_err(|e| format!("Folder scan task failed: {}", e))?;

    info!(
        "Folder scan found {} audio file(s){}",
        result.candidates.len(),
        if result.truncated { " (truncated)" } else { "" }
    );
    Ok(Some(result))
}

/// Start importing an audio file (Beta gated using configContext.betaFeatures)
#[tauri::command]
pub async fn start_import_audio_command<R: Runtime>(
    app: AppHandle<R>,
    import_id: Option<String>,
    source_path: String,
    title: String,
    language: Option<String>,
    model: Option<String>,
    provider: Option<String>,
) -> Result<ImportStarted, String> {
    let import_id = import_id.unwrap_or_else(|| format!("import-{}", Uuid::new_v4()));
    let use_remote = provider.as_deref() == Some("openaiCompatible");
    let guard = IMPORT_JOBS.acquire(import_id.clone(), use_remote)?;

    let import_id_for_task = import_id.clone();

    // Spawn import in background
    tauri::async_runtime::spawn(async move {
        let result = start_import_with_guard(
            app,
            import_id_for_task.clone(),
            source_path,
            title,
            language,
            model,
            provider,
            None,
            None,
            guard,
        )
        .await;

        if let Err(e) = result {
            error!("Import {} failed: {}", import_id_for_task, e);
        }
    });

    Ok(ImportStarted {
        import_id,
        message: "Import started".to_string(),
    })
}

/// Cancel ongoing import. Handles both file/URL imports that hold the shared
/// import guard and URL imports still in their download/authentication phase.
#[tauri::command]
pub async fn cancel_import_command(import_id: Option<String>) -> Result<(), String> {
    let url_active = match import_id.as_deref() {
        Some(id) => is_url_download_active(id),
        None => !URL_DOWNLOADS.is_empty(),
    };

    if let Some(import_id) = import_id.as_deref() {
        if !url_active && !IMPORT_JOBS.is_active(import_id) {
            return Err("No import in progress for this job".to_string());
        }
    } else if !url_active && !is_import_in_progress() {
        return Err("No import in progress".to_string());
    }

    cancel_url_download(import_id.as_deref());
    cancel_import(import_id.as_deref());
    Ok(())
}

/// Start importing a meeting recording from a SharePoint / Microsoft Stream URL.
///
/// Authenticates via an embedded webview (reusing a persisted session so login
/// is usually silent), downloads the recording with yt-dlp, then runs it
/// through the normal import pipeline. Progress, completion, error, and
/// cancellation all use the same `import-*` events as file imports, so the
/// frontend needs no special handling.
#[tauri::command]
pub async fn start_import_from_url_command<R: Runtime>(
    app: AppHandle<R>,
    import_id: Option<String>,
    url: String,
    title: String,
    language: Option<String>,
    model: Option<String>,
    provider: Option<String>,
    mode: Option<String>,
) -> Result<ImportStarted, String> {
    let import_id = import_id.unwrap_or_else(|| format!("import-{}", Uuid::new_v4()));

    if is_url_download_active(&import_id) || IMPORT_JOBS.is_active(&import_id) {
        return Err("This import is already in progress".to_string());
    }

    let cancel = tokio_util::sync::CancellationToken::new();
    URL_DOWNLOADS.insert(import_id.clone(), cancel.clone());

    let app_task = app.clone();
    let id_task = import_id.clone();
    tauri::async_runtime::spawn(async move {
        let result = run_url_import(
            app_task.clone(),
            id_task.clone(),
            url,
            title,
            language,
            model,
            provider,
            mode,
            cancel,
        )
        .await;

        // Ensure the download registry entry is gone even on early error paths.
        URL_DOWNLOADS.remove(&id_task);

        // The job finished in-process (success, failure, or cancellation — the
        // user was told either way), so its journal row must not survive to be
        // reported as interrupted on the next launch. Idempotent: the audio
        // pipeline clears its own row on the success path.
        super::job_persistence::try_clear_job(&app_task, &id_task).await;

        if let Err(e) = result {
            error!("URL import {} failed: {}", id_task, e);
            // The import pipeline (once reached) emits its own import-error and
            // journals cleanup. Only pre-pipeline failures (auth/download/engine
            // acquisition) surface here — and cancellation is not an error the
            // user needs to see twice.
            let msg = e.to_string();
            if !msg.contains("cancelled") {
                let _ = app_task.emit(
                    "import-error",
                    ImportError {
                        import_id: id_task.clone(),
                        error: msg,
                    },
                );
            }
        }
    });

    Ok(ImportStarted {
        import_id,
        message: "Import started".to_string(),
    })
}

/// Orchestrate a URL import: authenticate → download → hand off to the shared
/// import pipeline. Returns `Ok(())` once the download has been handed to the
/// pipeline (which then owns success/error reporting); returns `Err` only for
/// failures that occur before the pipeline takes over.
#[allow(clippy::too_many_arguments)]
async fn run_url_import<R: Runtime>(
    app: AppHandle<R>,
    import_id: String,
    url: String,
    title: String,
    language: Option<String>,
    model: Option<String>,
    provider: Option<String>,
    mode: Option<String>,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<()> {
    use super::{sharepoint, url_import, ytdlp};

    let is_transcript = mode.as_deref() == Some("transcript");

    // Journal the job before any long-running phase: a crash during login,
    // download, or transcript fetch must surface an interrupted notice on the
    // next launch, and retry needs the original URL + mode (there is no local
    // source file to fall back on). Cleared by the spawn wrapper on every
    // in-process finish.
    super::job_persistence::try_record_job_started(
        &app,
        &super::job_persistence::PersistedJob {
            id: import_id.clone(),
            kind: "import".to_string(),
            title: title.clone(),
            source_path: None,
            source_url: Some(url.clone()),
            mode: mode.clone(),
            folder_path: None,
            meeting_id: None,
            language: language.clone(),
            model: model.clone(),
            provider: provider.clone(),
            created_at: chrono::Utc::now().to_rfc3339(),
        },
    )
    .await;

    let work_dir = std::env::temp_dir().join(format!("meetily-url-{}", import_id));

    // Direct-download path: URLs pointing straight at a media file on a
    // SharePoint host (the sync scan's file URLs, or a stream.aspx link
    // wrapping one) skip yt-dlp — we hold the host's cookies, so a plain
    // authenticated GET works and is immune to yt-dlp's Stream page
    // scraping breaking on newer SharePoint UIs.
    let direct_media = if is_transcript {
        None
    } else {
        super::sharepoint_sync::direct_sharepoint_media_url(&url)
    };

    let media_path = if let Some(file_url) = direct_media {
        emit_progress(&app, &import_id, "downloading", 0, "Connecting to SharePoint…");
        let host = file_url.host_str().unwrap_or_default().to_ascii_lowercase();
        let auth = sharepoint::ensure_multi_host_auth(
            &app,
            &format!("https://{host}/"),
            &[],
            |msg| emit_progress(&app, &import_id, "downloading", 0, msg),
        )
        .await?;
        if cancel.is_cancelled() {
            return Err(anyhow!("Import cancelled"));
        }
        let cookies = auth
            .host_cookies
            .get(&host)
            .filter(|c| !c.is_empty())
            .ok_or_else(|| anyhow!("SharePoint sign-in did not produce cookies for {host}"))?;
        let dl_result = super::sharepoint_sync::download_direct_file(
            &file_url,
            &super::sharepoint_sync::build_cookie_header(cookies),
            &work_dir,
            |pct| {
                emit_progress(
                    &app,
                    &import_id,
                    "downloading",
                    pct,
                    &format!("Downloading recording… {pct}%"),
                )
            },
            &cancel,
        )
        .await;
        match dl_result {
            Ok(p) => p,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&work_dir);
                return Err(e);
            }
        }
    } else {
        // Phase 1: authenticate. The engine is deliberately NOT held during
        // login or download so recording and other jobs remain usable
        // meanwhile.
        emit_progress(&app, &import_id, "downloading", 0, "Connecting to SharePoint…");
        let auth = sharepoint::ensure_auth_cookies(&app, &url, |msg| {
            emit_progress(&app, &import_id, "downloading", 0, msg);
        })
        .await?;

        if cancel.is_cancelled() {
            auth.cleanup();
            return Err(anyhow!("Import cancelled"));
        }

        // Phase 2: locate yt-dlp (downloaded on first use).
        emit_progress(&app, &import_id, "downloading", 0, "Preparing downloader…");
        let ytdlp_path = match ytdlp::ensure_ytdlp(&app).await {
            Ok(p) => p,
            Err(e) => {
                auth.cleanup();
                return Err(e);
            }
        };
        let ffmpeg_path = super::ffmpeg::find_ffmpeg_path();

        // Transcript mode: fetch the Teams transcript (VTT) and build the
        // meeting from it directly — no download, no Whisper, no engine guard.
        if is_transcript {
            let result =
                run_transcript_import(&app, &import_id, &url, &title, &ytdlp_path, ffmpeg_path.as_deref(), &auth, &cancel)
                    .await;
            auth.cleanup();
            return result;
        }

        // Phase 3: download into a dedicated working directory.
        let dl_result = url_import::download_recording(
            &ytdlp_path,
            ffmpeg_path.as_deref(),
            &auth.cookies_txt,
            &url,
            &work_dir,
            |pct| {
                emit_progress(
                    &app,
                    &import_id,
                    "downloading",
                    pct,
                    &format!("Downloading recording… {pct}%"),
                )
            },
            &cancel,
        )
        .await;
        auth.cleanup();

        match dl_result {
            Ok(p) => p,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&work_dir);
                return Err(e);
            }
        }
    };

    // Phase 4: claim the shared engine guard now (after the long download) and
    // hand off to the normal pipeline, which journals + emits its own events.
    let use_remote = provider.as_deref() == Some("openaiCompatible");
    let guard = match IMPORT_JOBS.acquire(import_id.clone(), use_remote) {
        Ok(g) => g,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&work_dir);
            return Err(anyhow!(e));
        }
    };

    // The import guard now owns this job; cancellation flows through IMPORT_JOBS.
    URL_DOWNLOADS.remove(&import_id);

    let media_path_str = media_path.to_string_lossy().to_string();
    let _ = start_import_with_guard(
        app.clone(),
        import_id.clone(),
        media_path_str,
        title,
        language,
        model,
        provider,
        Some(url),
        mode,
        guard,
    )
    .await;

    let _ = std::fs::remove_dir_all(&work_dir);
    Ok(())
}

/// Build a meeting directly from a Teams transcript (VTT) — no audio download,
/// no Whisper, no engine guard. Emits the same `import-*` events as the audio
/// path so the frontend toast and sidebar refresh work unchanged.
#[allow(clippy::too_many_arguments)]
async fn run_transcript_import<R: Runtime>(
    app: &AppHandle<R>,
    import_id: &str,
    url: &str,
    title: &str,
    ytdlp_path: &Path,
    ffmpeg_path: Option<&Path>,
    auth: &super::sharepoint::AuthCookies,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<()> {
    emit_progress(app, import_id, "downloading", 10, "Fetching Teams transcript…");

    let work_dir = std::env::temp_dir().join(format!("meetily-vtt-{}", import_id));
    let vtt_path = match super::url_import::fetch_transcript(
        ytdlp_path,
        ffmpeg_path,
        &auth.cookies_txt,
        url,
        &work_dir,
        cancel,
    )
    .await
    {
        Ok(p) => p,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&work_dir);
            return Err(e);
        }
    };

    emit_progress(app, import_id, "saving", 60, "Parsing transcript…");
    let content = match tokio::fs::read_to_string(&vtt_path).await {
        Ok(c) => c,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&work_dir);
            return Err(anyhow!("Failed to read transcript file: {e}"));
        }
    };
    let cues = match super::vtt::parse_vtt(&content) {
        Ok(c) => super::vtt::merge_cues(c),
        Err(e) => {
            let _ = std::fs::remove_dir_all(&work_dir);
            return Err(anyhow!(e));
        }
    };

    let now = chrono::Utc::now().to_rfc3339();
    let segments: Vec<TranscriptSegment> = cues
        .iter()
        .map(|c| TranscriptSegment {
            id: format!("transcript-{}", Uuid::new_v4()),
            text: c.text.clone(),
            timestamp: now.clone(),
            audio_start_time: Some(c.start_s),
            audio_end_time: Some(c.end_s),
            duration: Some((c.end_s - c.start_s).max(0.0)),
            speaker: c.speaker.clone(),
        })
        .collect();

    if segments.is_empty() {
        let _ = std::fs::remove_dir_all(&work_dir);
        return Err(anyhow!("The transcript contained no readable text."));
    }
    if cancel.is_cancelled() {
        let _ = std::fs::remove_dir_all(&work_dir);
        return Err(anyhow!("Import cancelled"));
    }

    // Resolve app state before creating the meeting folder so a missing state
    // can't leave an orphaned folder behind.
    let app_state = match app.try_state::<AppState>() {
        Some(s) => s,
        None => {
            let _ = std::fs::remove_dir_all(&work_dir);
            return Err(anyhow!("App state not available"));
        }
    };

    emit_progress(app, import_id, "saving", 85, "Creating meeting…");
    let base_folder = get_default_recordings_folder();
    let meeting_folder = match create_meeting_folder(&base_folder, title, false) {
        Ok(f) => f,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&work_dir);
            return Err(e.into());
        }
    };
    // From here a folder exists on disk: record it in the journal so a crash
    // before the DB commit is reconciled (and the orphan removed) at startup.
    super::job_persistence::try_set_job_folder(
        app,
        import_id,
        &meeting_folder.to_string_lossy(),
    )
    .await;
    // Keep the raw VTT alongside the meeting for reference.
    let _ = std::fs::copy(&vtt_path, meeting_folder.join("transcript.vtt"));

    let meeting_id = match create_meeting_with_transcripts(
        app_state.db_manager.pool(),
        title,
        &segments,
        meeting_folder.to_string_lossy().to_string(),
    )
    .await
    {
        Ok(id) => id,
        Err(e) => {
            // The meeting never reached the DB: remove the folder we just
            // created (mirrors the audio pipeline's failure cleanup).
            let _ = std::fs::remove_dir_all(&meeting_folder);
            let _ = std::fs::remove_dir_all(&work_dir);
            return Err(e.into());
        }
    };

    let duration_seconds = cues.last().map(|c| c.end_s).unwrap_or(0.0);

    if let Err(e) = write_transcripts_json(&meeting_folder, &segments) {
        warn!("Failed to write transcripts.json: {}", e);
    }
    if let Err(e) = write_import_metadata(
        &meeting_folder,
        &meeting_id,
        title,
        duration_seconds,
        "transcript.vtt",
        "transcript-import",
    ) {
        warn!("Failed to write metadata.json: {}", e);
    }

    let _ = std::fs::remove_dir_all(&work_dir);

    emit_progress(app, import_id, "complete", 100, "Transcript imported");
    let _ = app.emit(
        "import-complete",
        serde_json::json!({
            "import_id": import_id,
            "meeting_id": meeting_id,
            "title": title,
            "segments_count": segments.len(),
            "duration_seconds": duration_seconds,
        }),
    );

    Ok(())
}

/// Check if import is in progress
#[tauri::command]
pub async fn is_import_in_progress_command() -> bool {
    is_import_in_progress()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::engine_coordinator::{
        global_coordinator_test_lock, LocalEngineUser, LOCAL_ENGINE_COORDINATOR,
    };

    #[test]
    fn local_import_is_rejected_while_recording_holds_engine() {
        let _serial = global_coordinator_test_lock();
        let recording_claim = LOCAL_ENGINE_COORDINATOR
            .try_claim(LocalEngineUser::Recording)
            .unwrap();

        let local = IMPORT_JOBS.acquire("import-under-recording".to_string(), false);
        let err = match local {
            Ok(_) => panic!("local import must not run while recording holds the engine"),
            Err(err) => err,
        };
        assert!(
            err.contains("recording"),
            "error should name the conflicting consumer, got: {err}"
        );
        // The failed acquire must not leave the job registered as active.
        assert!(!is_import_in_progress());

        // Remote imports don't touch the local engine and stay allowed.
        let remote = IMPORT_JOBS.acquire("import-under-recording-remote".to_string(), true);
        assert!(remote.is_ok());
        drop(remote);

        // Once recording releases the engine, local imports work again.
        drop(recording_claim);
        let local_after = IMPORT_JOBS.acquire("import-after-recording".to_string(), false);
        assert!(local_after.is_ok());
    }

    #[test]
    fn remote_imports_beyond_cap_are_rejected_until_a_slot_frees() {
        use crate::audio::remote_concurrency::MAX_CONCURRENT_REMOTE_JOBS;

        let _serial = global_coordinator_test_lock();

        let mut guards: Vec<JobGuard<'static>> = (0..MAX_CONCURRENT_REMOTE_JOBS)
            .map(|i| {
                IMPORT_JOBS.acquire(format!("import-remote-cap-{i}"), true)
                    .unwrap_or_else(|e| panic!("remote import {i} within the cap must acquire: {e}"))
            })
            .collect();

        let over_cap = IMPORT_JOBS.acquire("import-remote-over-cap".to_string(), true);
        let err = match over_cap {
            Ok(_) => panic!("remote import beyond the cap must be rejected"),
            Err(err) => err,
        };
        assert!(
            err.to_lowercase().contains("remote"),
            "error should explain the remote-job limit, got: {err}"
        );
        // The rejected job must not be left registered as active.
        assert!(!IMPORT_JOBS.is_active("import-remote-over-cap"));

        // Finishing one job frees a slot for the next.
        guards.pop();
        let after_free = IMPORT_JOBS.acquire("import-remote-after-free".to_string(), true);
        assert!(
            after_free.is_ok(),
            "remote import must acquire once a slot frees: {:?}",
            after_free.err()
        );
    }

    #[test]
    fn local_import_does_not_consume_remote_slots() {
        use crate::audio::remote_concurrency::MAX_CONCURRENT_REMOTE_JOBS;

        let _serial = global_coordinator_test_lock();

        let _remote_guards: Vec<JobGuard<'static>> = (0..MAX_CONCURRENT_REMOTE_JOBS)
            .map(|i| {
                IMPORT_JOBS.acquire(format!("import-remote-full-{i}"), true)
                    .expect("remote import within the cap must acquire")
            })
            .collect();

        // The remote cap is exhausted, but a local import uses the local
        // engine, not a remote slot, so it must still acquire.
        let local = IMPORT_JOBS.acquire("import-local-while-remote-full".to_string(), false);
        assert!(
            local.is_ok(),
            "local import must not be blocked by the remote cap: {:?}",
            local.err()
        );
    }

    #[test]
    fn test_audio_extensions() {
        assert!(AUDIO_EXTENSIONS.contains(&"mp4"));
        assert!(AUDIO_EXTENSIONS.contains(&"wav"));
        assert!(AUDIO_EXTENSIONS.contains(&"mp3"));
        assert!(!AUDIO_EXTENSIONS.contains(&"txt"));
    }

    #[test]
    fn test_create_transcript_segments_empty() {
        let transcripts: Vec<(String, f64, f64)> = vec![];
        let segments = create_transcript_segments(&transcripts);
        assert!(segments.is_empty());
    }

    #[test]
    fn test_create_transcript_segments_single() {
        let transcripts = vec![("Hello world".to_string(), 0.0, 1500.0)];
        let segments = create_transcript_segments(&transcripts);

        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].text, "Hello world");
        assert_eq!(segments[0].audio_start_time, Some(0.0));
        assert_eq!(segments[0].audio_end_time, Some(1.5));
    }

    #[test]
    fn job_started_during_cancel_all_drain_is_not_cancelled() {
        let _serial = global_coordinator_test_lock();
        // Remote jobs skip the engine coordinator, so this exercises only the
        // cancellation bookkeeping.
        let draining = IMPORT_JOBS.acquire("import-drain-old".to_string(), true)
            .expect("first remote import should acquire");

        cancel_import(None);
        assert!(is_import_cancelled("import-drain-old"));

        // The user cancelled the jobs active at the time, not future ones. A job
        // started while the cancel-all is still draining must run normally.
        let fresh = IMPORT_JOBS.acquire("import-drain-new".to_string(), true)
            .expect("new import should acquire while old job drains");
        assert!(
            !is_import_cancelled("import-drain-new"),
            "job started after cancel-all must not inherit the cancellation"
        );

        drop(draining);
        assert!(
            !is_import_cancelled("import-drain-new"),
            "draining the cancelled job must not affect the new job"
        );
        drop(fresh);
        assert!(!is_import_in_progress());
    }

    #[test]
    fn test_cancellation_flag() {
        let _serial = global_coordinator_test_lock();

        let guard = IMPORT_JOBS
            .acquire("import-1".to_string(), true)
            .expect("import should acquire");

        cancel_import(Some("import-1"));
        assert!(is_import_cancelled("import-1"));
        assert!(!is_import_cancelled("import-2"));

        // Draining the job clears its cancel flag.
        drop(guard);
        assert!(!is_import_cancelled("import-1"));
        assert!(!is_import_in_progress());
    }

    #[test]
    fn test_extract_duration_from_metadata_wav() {
        // Test with sample WAV file if available
        let test_path = Path::new("../../backend/whisper.cpp/samples/jfk.wav");
        if test_path.exists() {
            let result = extract_duration_from_metadata(test_path);
            // Should succeed and return a reasonable duration
            assert!(result.is_ok());
            let duration = result.unwrap();
            assert!(duration > 0.0 && duration < 60.0, "Duration {} seems unreasonable", duration);
        }
    }

    #[test]
    fn test_extract_duration_from_metadata_mp3() {
        // Test with sample MP3 file if available
        let test_path = Path::new("../../backend/whisper.cpp/samples/jfk.mp3");
        if test_path.exists() {
            let result = extract_duration_from_metadata(test_path);
            // MP3 files may not have n_frames metadata, so fallback is expected
            // We just verify it doesn't panic
            let _ = result;
        }
    }

    #[test]
    fn test_validate_audio_file_with_metadata() {
        // Test validation with actual audio file
        let test_path = Path::new("../../backend/whisper.cpp/samples/jfk.wav");
        if test_path.exists() {
            let result = validate_audio_file(test_path);
            assert!(result.is_ok());
            let info = result.unwrap();
            assert_eq!(info.format, "WAV");
            assert!(info.duration_seconds > 0.0);
            assert!(info.size_bytes > 0);
        }
    }

    #[test]
    fn test_validate_audio_file_nonexistent() {
        let result = validate_audio_file(Path::new("/nonexistent/file.mp4"));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("does not exist"));
    }

    #[test]
    fn test_validate_audio_file_wrong_extension() {
        // Create a temporary file with wrong extension
        let temp_dir = std::env::temp_dir();
        let temp_file = temp_dir.join("test_audio.txt");
        let _ = std::fs::write(&temp_file, b"dummy content");

        let result = validate_audio_file(&temp_file);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Unsupported format"));

        // Cleanup
        let _ = std::fs::remove_file(temp_file);
    }

    #[test]
    fn test_split_segment_at_silence_short_segment() {
        // Segment shorter than max — returned as-is
        let segment = crate::audio::vad::SpeechSegment {
            samples: vec![0.1; 16000], // 1 second
            start_timestamp_ms: 0.0,
            end_timestamp_ms: 1000.0,
            confidence: 0.9,
        };
        let result = split_segment_at_silence(&segment, 25 * 16000);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].samples.len(), 16000);
    }

    #[test]
    fn test_split_segment_at_silence_splits_long_segment() {
        // 60-second segment of low-level noise with a silent gap at ~25s
        let mut samples = vec![0.01f32; 60 * 16000];
        // Insert silence at 25 seconds (sample 400000)
        for i in (25 * 16000)..(25 * 16000 + 3200) {
            samples[i] = 0.0;
        }
        let segment = crate::audio::vad::SpeechSegment {
            samples,
            start_timestamp_ms: 0.0,
            end_timestamp_ms: 60_000.0,
            confidence: 0.9,
        };

        let result = split_segment_at_silence(&segment, 25 * 16000);
        assert!(result.len() >= 2, "Should split into at least 2 segments, got {}", result.len());

        // All sub-segments should have samples
        for (i, seg) in result.iter().enumerate() {
            assert!(!seg.samples.is_empty(), "Segment {} is empty", i);
            assert!(
                seg.start_timestamp_ms < seg.end_timestamp_ms,
                "Segment {} has invalid timestamps: {} >= {}",
                i, seg.start_timestamp_ms, seg.end_timestamp_ms
            );
        }
    }

    #[test]
    fn test_split_segment_at_silence_no_silence_uses_overlap() {
        // Continuous speech (constant energy) — should still split with overlap
        let segment = crate::audio::vad::SpeechSegment {
            samples: vec![0.5f32; 60 * 16000], // 60 seconds of "speech"
            start_timestamp_ms: 0.0,
            end_timestamp_ms: 60_000.0,
            confidence: 0.9,
        };

        let result = split_segment_at_silence(&segment, 25 * 16000);
        assert!(result.len() >= 2);

        // Total samples should exceed input due to overlap
        let total_samples: usize = result.iter().map(|s| s.samples.len()).sum();
        assert!(total_samples >= 60 * 16000, "Overlap should not lose samples");
    }

    #[test]
    fn test_write_transcripts_json() {
        let dir = tempfile::tempdir().unwrap();
        let segments = vec![
            TranscriptSegment {
                id: "t-1".to_string(),
                text: "Hello world".to_string(),
                timestamp: "2024-01-01T00:00:00Z".to_string(),
                audio_start_time: Some(0.0),
                audio_end_time: Some(1.5),
                duration: Some(1.5),
                speaker: None,
            },
            TranscriptSegment {
                id: "t-2".to_string(),
                text: "Second segment".to_string(),
                timestamp: "2024-01-01T00:00:01Z".to_string(),
                audio_start_time: Some(2.0),
                audio_end_time: Some(3.5),
                duration: Some(1.5),
                speaker: None,
            },
        ];

        let result = write_transcripts_json(dir.path(), &segments);
        assert!(result.is_ok(), "write_transcripts_json failed: {:?}", result);

        // Verify file exists and is valid JSON
        let path = dir.path().join("transcripts.json");
        assert!(path.exists());

        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed["total_segments"], 2);
        assert_eq!(parsed["version"], "1.0");
        assert_eq!(parsed["segments"][0]["text"], "Hello world");
        assert_eq!(parsed["segments"][1]["text"], "Second segment");
        assert_eq!(parsed["segments"][0]["sequence_id"], 0);
        assert_eq!(parsed["segments"][1]["sequence_id"], 1);

        // Verify temp file was cleaned up
        assert!(!dir.path().join(".transcripts.json.tmp").exists());
    }

    #[test]
    fn test_write_import_metadata() {
        let dir = tempfile::tempdir().unwrap();

        let result = write_import_metadata(
            dir.path(),
            "meeting-123",
            "Test Meeting",
            1800.0,
            "audio.mp4",
            "import",
        );
        assert!(result.is_ok(), "write_import_metadata failed: {:?}", result);

        let path = dir.path().join("metadata.json");
        assert!(path.exists());

        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed["version"], "1.0");
        assert_eq!(parsed["meeting_id"], "meeting-123");
        assert_eq!(parsed["meeting_name"], "Test Meeting");
        assert_eq!(parsed["duration_seconds"], 1800.0);
        assert_eq!(parsed["audio_file"], "audio.mp4");
        assert_eq!(parsed["status"], "completed");
        assert_eq!(parsed["source"], "import");
    }

    /// Integration test that decodes a real audio file and runs VAD.
    /// Run with: TEST_AUDIO_PATH=/path/to/audio.mp4 cargo test -- --ignored --nocapture
    #[test]
    #[ignore]
    fn test_import_pipeline_decode_vad() {
        let audio_path = std::env::var("TEST_AUDIO_PATH")
            .expect("Set TEST_AUDIO_PATH to run this integration test");

        let path = Path::new(&audio_path);
        assert!(path.exists(), "Audio file not found: {}", audio_path);

        // Step 1: Decode
        println!("Decoding {}...", audio_path);
        let decoded = crate::audio::decoder::decode_audio_file(path)
            .expect("Failed to decode audio file");
        println!(
            "Decoded: {:.2}s, {}Hz, {} channels, {} samples",
            decoded.duration_seconds,
            decoded.sample_rate,
            decoded.channels,
            decoded.samples.len()
        );

        // Step 2: Resample to 16kHz mono
        println!("Resampling to 16kHz mono...");
        let samples = decoded.to_whisper_format();
        println!("Resampled: {} samples ({:.2}s at 16kHz)", samples.len(), samples.len() as f64 / 16000.0);

        // Step 3: Run VAD with both redemption times and compare
        for redemption_ms in [400u32, 2000] {
            println!("\n--- VAD with redemption_time={}ms ---", redemption_ms);
            let segments = crate::audio::vad::get_speech_chunks_with_progress(
                &samples,
                redemption_ms,
                |progress, count| {
                    if progress % 20 == 0 {
                        println!("  VAD progress: {}% ({} segments)", progress, count);
                    }
                    true
                },
            ).expect("VAD failed");

            let total_segments = segments.len();
            println!("Found {} segments", total_segments);

            if !segments.is_empty() {
                let durations: Vec<f64> = segments.iter()
                    .map(|s| s.end_timestamp_ms - s.start_timestamp_ms)
                    .collect();
                let total_speech: f64 = durations.iter().sum();
                let avg = total_speech / durations.len() as f64;
                let min = durations.iter().cloned().fold(f64::INFINITY, f64::min);
                let max = durations.iter().cloned().fold(f64::NEG_INFINITY, f64::max);

                println!(
                    "Stats: avg={:.0}ms, min={:.0}ms, max={:.0}ms, total_speech={:.1}s/{:.1}s ({:.0}%)",
                    avg, min, max,
                    total_speech / 1000.0,
                    decoded.duration_seconds,
                    (total_speech / 1000.0 / decoded.duration_seconds) * 100.0
                );

                // Segments over 25s that would be split
                let oversized = durations.iter().filter(|d| **d > 25_000.0).count();
                println!("Segments >25s (would be split): {}", oversized);

                // Basic sanity checks
                assert!(total_speech > 0.0, "No speech detected");
                for (i, seg) in segments.iter().enumerate() {
                    assert!(!seg.samples.is_empty(), "Segment {} has no samples", i);
                    assert!(
                        seg.end_timestamp_ms > seg.start_timestamp_ms,
                        "Segment {} has invalid timestamps",
                        i
                    );
                }
            }
        }
    }

    mod folder_scan {
        use super::super::{scan_folder_for_audio, FOLDER_SCAN_MAX_DEPTH};

        fn touch(path: &std::path::Path) {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(path, b"x").unwrap();
        }

        #[test]
        fn finds_audio_recursively_and_skips_non_audio_and_dot_entries() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            touch(&root.join("a.mp3"));
            touch(&root.join("notes.txt"));
            touch(&root.join(".hidden.mp3"));
            touch(&root.join("nested/deep/b.WAV")); // extension match is case-insensitive
            touch(&root.join(".git/c.mp3")); // dot-dir skipped entirely

            let result = scan_folder_for_audio(root, FOLDER_SCAN_MAX_DEPTH, 500);
            let names: Vec<&str> = result
                .candidates
                .iter()
                .map(|c| c.file_name.as_str())
                .collect();
            assert_eq!(names, vec!["a.mp3", "b.WAV"]);
            assert!(!result.truncated);
        }

        #[test]
        fn respects_the_depth_cap() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            touch(&root.join("l1/l2/shallow.mp3"));
            touch(&root.join("l1/l2/l3/too-deep.mp3"));

            let result = scan_folder_for_audio(root, 2, 500);
            let names: Vec<&str> = result
                .candidates
                .iter()
                .map(|c| c.file_name.as_str())
                .collect();
            assert_eq!(names, vec!["shallow.mp3"]);
        }

        #[test]
        fn truncates_at_the_file_cap() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            for i in 0..5 {
                touch(&root.join(format!("file-{i}.mp3")));
            }

            let result = scan_folder_for_audio(root, FOLDER_SCAN_MAX_DEPTH, 3);
            assert_eq!(result.candidates.len(), 3);
            assert!(result.truncated, "hitting the cap must be reported");
        }

        #[test]
        fn empty_folder_yields_empty_untruncated_result() {
            let dir = tempfile::tempdir().unwrap();
            let result = scan_folder_for_audio(dir.path(), FOLDER_SCAN_MAX_DEPTH, 500);
            assert!(result.candidates.is_empty());
            assert!(!result.truncated);
        }
    }
}
