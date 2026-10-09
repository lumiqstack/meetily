use std::sync::{Arc, Mutex};
use tokio::sync::Mutex as AsyncMutex;
use anyhow::Result;
use log::{info, warn, error};
use tauri::{AppHandle, Runtime, Emitter};
use tokio::sync::mpsc;
use serde::{Serialize, Deserialize};
use std::path::PathBuf;
use std::time::Duration;

use super::recording_state::AudioChunk;
use super::audio_processing::create_meeting_folder;
use super::incremental_saver::IncrementalAudioSaver;

/// Structured transcript segment for JSON export
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptSegment {
    pub id: String,
    pub text: String,
    pub audio_start_time: f64, // Seconds from recording start
    pub audio_end_time: f64,   // Seconds from recording start
    pub duration: f64,          // Segment duration in seconds
    pub display_time: String,   // Formatted time for display like "[02:15]"
    pub confidence: f32,
    pub sequence_id: u64,
    /// "mic" / "system" speaker attribution; default keeps old
    /// transcripts.json files readable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker: Option<String>,
}

/// Meeting metadata structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeetingMetadata {
    pub version: String,
    pub meeting_id: Option<String>,
    pub meeting_name: Option<String>,
    pub created_at: String,
    pub completed_at: Option<String>,
    pub duration_seconds: Option<f64>,
    pub devices: DeviceInfo,
    pub audio_file: String,
    pub transcript_file: String,
    pub sample_rate: u32,
    pub status: String,  // "recording", "completed", "error"
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub microphone: Option<String>,
    pub system_audio: Option<String>,
}

/// How long `stop_and_save` waits for the accumulation writer to drain every
/// queued chunk after all producers stopped. Generous: a slow disk must not
/// lose the tail of a meeting, but a stuck writer must not hang Stop forever.
const ACCUMULATION_DRAIN_TIMEOUT: Duration = Duration::from_secs(60);
/// After aborting a timed-out writer, how long to wait for it to terminate.
const ACCUMULATION_ABORT_GRACE: Duration = Duration::from_secs(5);

/// Background writer that owns the recording receiver. Resolves to the number
/// of chunks written, or the first write failure.
type AccumulationTask = tokio::task::JoinHandle<std::result::Result<u64, String>>;

/// How the accumulation writer ended at stop time.
#[derive(Debug, PartialEq)]
pub(crate) enum DrainOutcome {
    /// Every producer closed and every accepted chunk was written.
    Drained(u64),
    /// A write failed or the task panicked; queued chunks were not written.
    WriterFailed(String),
    /// The writer did not finish in time. `writer_stopped` reports whether
    /// it terminated after being aborted; if not it may still hold the saver.
    TimedOut { writer_stopped: bool },
}

/// Consume `receiver` until every sender is dropped, writing each chunk via
/// `sink`. After the first sink failure later chunks are still received (so
/// producers never observe a closed channel) but not written, and the failure
/// is returned once the channel closes.
async fn drain_into<F, Fut>(
    mut receiver: mpsc::UnboundedReceiver<AudioChunk>,
    mut sink: F,
) -> std::result::Result<u64, String>
where
    F: FnMut(AudioChunk) -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    let mut written = 0u64;
    let mut first_error: Option<String> = None;
    while let Some(chunk) = receiver.recv().await {
        if first_error.is_some() {
            continue;
        }
        match sink(chunk).await {
            Ok(()) => written += 1,
            Err(e) => {
                error!("Failed to add chunk to incremental saver: {}", e);
                first_error = Some(e.to_string());
            }
        }
    }
    match first_error {
        Some(e) => Err(e),
        None => Ok(written),
    }
}

/// Wait for the writer to finish draining, bounded by `timeout`. A timed-out
/// writer is aborted and awaited for `grace` so the caller knows whether it is
/// still running; dropping a JoinHandle alone would not stop it.
async fn await_accumulation(
    mut handle: AccumulationTask,
    timeout: Duration,
    grace: Duration,
) -> DrainOutcome {
    match tokio::time::timeout(timeout, &mut handle).await {
        Ok(Ok(Ok(written))) => DrainOutcome::Drained(written),
        Ok(Ok(Err(e))) => DrainOutcome::WriterFailed(e),
        Ok(Err(join_error)) => DrainOutcome::WriterFailed(if join_error.is_panic() {
            "the audio writer task panicked".to_string()
        } else {
            "the audio writer task was cancelled".to_string()
        }),
        Err(_) => {
            handle.abort();
            let writer_stopped = tokio::time::timeout(grace, &mut handle).await.is_ok();
            DrainOutcome::TimedOut { writer_stopped }
        }
    }
}

/// Cloneable handle to one recording's transcript store. It writes to the same
/// store and folder as the saver it came from.
#[derive(Clone)]
pub struct TranscriptSink {
    segments: Arc<Mutex<Vec<TranscriptSegment>>>,
    meeting_folder: Option<PathBuf>,
}

impl TranscriptSink {
    /// Upsert a segment by sequence_id and persist to the meeting folder if one exists.
    pub fn add_segment(&self, segment: TranscriptSegment) {
        if let Ok(mut segments) = self.segments.lock() {
            // Check if segment with same sequence_id exists (update it)
            if let Some(existing) = segments.iter_mut().find(|s| s.sequence_id == segment.sequence_id) {
                *existing = segment.clone();
                info!("Updated transcript segment {} (seq: {}) - total segments: {}",
                      segment.id, segment.sequence_id, segments.len());
            } else {
                // New segment, add it
                segments.push(segment.clone());
                info!("Added new transcript segment {} (seq: {}) - total segments: {}",
                      segment.id, segment.sequence_id, segments.len());
            }
        } else {
            error!("Failed to lock transcript segments for adding segment {}", segment.id);
        }

        // NEW: Save incrementally to disk
        if let Some(folder) = &self.meeting_folder {
            if let Err(e) = RecordingSaver::write_transcripts_json(&self.segments, folder) {
                warn!("Failed to write incremental transcript update: {}", e);
            }
        }
    }
}

/// New recording saver using incremental saving strategy
pub struct RecordingSaver {
    incremental_saver: Option<Arc<AsyncMutex<IncrementalAudioSaver>>>,
    meeting_folder: Option<PathBuf>,
    /// Base folder for meeting directories, from the user's `save_folder`
    /// preference. `None` falls back to the default under the data root.
    save_folder: Option<PathBuf>,
    meeting_name: Option<String>,
    metadata: Option<MeetingMetadata>,
    transcript_segments: Arc<Mutex<Vec<TranscriptSegment>>>,
    accumulation_task: Option<AccumulationTask>,
    /// True when this recording promised to keep audio (auto_save).
    save_audio: bool,
}

impl RecordingSaver {
    pub fn new() -> Self {
        Self {
            incremental_saver: None,
            meeting_folder: None,
            save_folder: None,
            meeting_name: None,
            metadata: None,
            transcript_segments: Arc::new(Mutex::new(Vec::new())),
            accumulation_task: None,
            save_audio: false,
        }
    }

    /// Set the meeting name for this recording session
    pub fn set_meeting_name(&mut self, name: Option<String>) {
        self.meeting_name = name;
    }

    /// Set the base folder meeting directories are created under.
    ///
    /// Comes from the `save_folder` recording preference. Without this the
    /// saver always used the default folder, so picking a location in Settings
    /// had no effect on where recordings actually landed.
    pub fn set_save_folder(&mut self, folder: Option<PathBuf>) {
        self.save_folder = folder;
    }

    /// Set device information in metadata
    pub fn set_device_info(&mut self, mic_name: Option<String>, sys_name: Option<String>) {
        if let Some(ref mut metadata) = self.metadata {
            metadata.devices.microphone = mic_name;
            metadata.devices.system_audio = sys_name;

            // Write updated metadata to disk if folder exists
            if let Some(folder) = &self.meeting_folder {
                let metadata_clone = metadata.clone();
                if let Err(e) = self.write_metadata(folder, &metadata_clone) {
                    warn!("Failed to update metadata with device info: {}", e);
                }
            }
        }
    }

    /// Add or update a structured transcript segment (upserts based on sequence_id)
    /// Also saves incrementally to disk
    pub fn add_transcript_segment(&self, segment: TranscriptSegment) {
        self.transcript_sink().add_segment(segment);
    }

    /// Handle for recording segments into this saver's store. Lets a caller keep
    /// writing after the saver has been taken out of the global manager.
    pub fn transcript_sink(&self) -> TranscriptSink {
        TranscriptSink {
            segments: Arc::clone(&self.transcript_segments),
            meeting_folder: self.meeting_folder.clone(),
        }
    }

    /// Legacy method for backward compatibility - converts text to basic segment
    pub fn add_transcript_chunk(&self, text: String) {
        let segment = TranscriptSegment {
            id: format!("seg_{}", chrono::Utc::now().timestamp_millis()),
            text,
            audio_start_time: 0.0,
            audio_end_time: 0.0,
            duration: 0.0,
            display_time: "[00:00]".to_string(),
            confidence: 1.0,
            sequence_id: 0,
            speaker: None,
        };
        self.add_transcript_segment(segment);
    }

    /// Create the meeting folder (and, with `auto_save`, the checkpoint
    /// writer) before any audio is captured. Both modes promise persistence —
    /// audio checkpoints, or transcripts + metadata — so a failure here must
    /// stop the recording from starting rather than surface only at Stop.
    pub fn prepare_storage(&mut self, auto_save: bool) -> Result<()> {
        // User's chosen folder, falling back to the default under the data root
        let base_folder = self
            .save_folder
            .clone()
            .unwrap_or_else(super::recording_preferences::get_default_recordings_folder);
        self.prepare_storage_in(&base_folder, auto_save)
    }

    fn prepare_storage_in(&mut self, base_folder: &PathBuf, auto_save: bool) -> Result<()> {
        if auto_save {
            info!("Initializing incremental audio saver for recording (auto-save ENABLED)");
        } else {
            info!("Starting recording without audio saving (auto-save DISABLED - transcripts only)");
        }
        self.save_audio = auto_save;
        let name = self
            .meeting_name
            .clone()
            .ok_or_else(|| anyhow::anyhow!("No meeting name was set for the recording"))?;
        // Without auto_save the folder still holds transcripts/metadata, but
        // no audio encoder is started.
        self.initialize_meeting_folder(base_folder, &name, auto_save)
            .map_err(|e| {
                anyhow::anyhow!(
                    "Cannot create the recording folder in {}: {}",
                    base_folder.display(),
                    e
                )
            })?;
        info!(
            "Successfully initialized meeting folder ({})",
            if auto_save { "with audio encoder" } else { "transcripts only" }
        );
        Ok(())
    }

    /// Start the background writer for a recording whose storage was prepared
    /// with [`prepare_storage`](Self::prepare_storage).
    ///
    /// # Arguments
    /// * `auto_save` - If true, audio chunks go to the audio encoder. If false, they are discarded.
    pub fn start_accumulation(
        &mut self,
        auto_save: bool,
        receiver: mpsc::UnboundedReceiver<AudioChunk>,
    ) {
        // Start the accumulation writer. It owns the receiver and ends only
        // when every producer has dropped its sender, so stop_and_save can
        // await it and know every accepted chunk reached the saver.
        let incremental_saver_arc = self.incremental_saver.clone();
        let save_audio = auto_save;

        self.accumulation_task = Some(tokio::spawn(async move {
            info!("Recording saver accumulation task started (save_audio: {})", save_audio);
            let result = drain_into(receiver, |chunk| {
                let saver = incremental_saver_arc.clone();
                async move {
                    if !save_audio {
                        // Transcript-only mode: transcription already happened
                        // in the pipeline, the audio itself is not kept.
                        return Ok(());
                    }
                    let saver = saver
                        .ok_or_else(|| anyhow::anyhow!("Incremental saver not available while accumulating"))?;
                    // add_chunk just hands the buffer to the encoder's writer
                    // thread; the encode happens in ffmpeg, off this runtime.
                    let mut guard = saver.lock().await;
                    guard.add_chunk(chunk)
                }
            })
            .await;
            info!("Recording saver accumulation task ended");
            result
        }));
    }

    /// Initialize meeting folder structure and metadata
    ///
    /// # Arguments
    /// * `meeting_name` - Name of the meeting
    /// * `save_audio` - Whether to start an audio encoder for this meeting
    fn initialize_meeting_folder(
        &mut self,
        base_folder: &PathBuf,
        meeting_name: &str,
        save_audio: bool,
    ) -> Result<()> {
        // No .checkpoints/ any more: the encoder writes audio.mp4 directly and
        // that file is already crash-recoverable (fragmented MP4).
        let meeting_folder = create_meeting_folder(base_folder, meeting_name, false)?;

        if save_audio {
            let incremental_saver = IncrementalAudioSaver::new(meeting_folder.clone(), 48000)?;
            self.incremental_saver = Some(Arc::new(AsyncMutex::new(incremental_saver)));
            info!("✅ Audio encoder started for meeting: {}", meeting_name);
        } else {
            info!("⚠️  Skipped audio encoder (auto-save disabled)");
        }

        // Create initial metadata
        let metadata = MeetingMetadata {
            version: "1.0".to_string(),
            meeting_id: None,  // Will be set by backend
            meeting_name: Some(meeting_name.to_string()),
            created_at: chrono::Utc::now().to_rfc3339(),
            completed_at: None,
            duration_seconds: None,
            devices: DeviceInfo {
                microphone: None,  // Could be enhanced to store actual device names
                system_audio: None,
            },
            audio_file: if save_audio { "audio.mp4".to_string() } else { "".to_string() },
            transcript_file: "transcripts.json".to_string(),
            sample_rate: 48000,
            status: "recording".to_string(),
        };

        // Write initial metadata.json
        self.write_metadata(&meeting_folder, &metadata)?;

        self.meeting_folder = Some(meeting_folder);
        self.metadata = Some(metadata);

        Ok(())
    }

    /// Write metadata.json to disk (atomic write with temp file)
    fn write_metadata(&self, folder: &PathBuf, metadata: &MeetingMetadata) -> Result<()> {
        let metadata_path = folder.join("metadata.json");
        let temp_path = folder.join(".metadata.json.tmp");

        let json_string = serde_json::to_string_pretty(metadata)?;
        std::fs::write(&temp_path, json_string)?;
        std::fs::rename(&temp_path, &metadata_path)?;  // Atomic

        Ok(())
    }

    /// Write transcripts.json to disk (atomic write with temp file and validation)
    fn write_transcripts_json(transcript_segments: &Mutex<Vec<TranscriptSegment>>, folder: &PathBuf) -> Result<()> {
        // Clone segments to avoid holding lock during I/O
        let segments_clone = if let Ok(segments) = transcript_segments.lock() {
            segments.clone()
        } else {
            error!("Failed to lock transcript segments for writing");
            return Err(anyhow::anyhow!("Failed to lock transcript segments"));
        };

        info!("Writing {} transcript segments to JSON", segments_clone.len());

        let transcript_path = folder.join("transcripts.json");
        let temp_path = folder.join(".transcripts.json.tmp");

        // Create JSON structure
        let json = serde_json::json!({
            "version": "1.0",
            "segments": segments_clone,
            "last_updated": chrono::Utc::now().to_rfc3339(),
            "total_segments": segments_clone.len()
        });

        // Serialize to pretty JSON string
        let json_string = serde_json::to_string_pretty(&json)
            .map_err(|e| {
                error!("Failed to serialize transcripts to JSON: {}", e);
                anyhow::anyhow!("JSON serialization failed: {}", e)
            })?;

        // Write to temp file with error handling
        std::fs::write(&temp_path, &json_string)
            .map_err(|e| {
                error!("Failed to write transcript temp file to {}: {}", temp_path.display(), e);
                anyhow::anyhow!("Failed to write temp file: {}", e)
            })?;

        // Verify temp file was written correctly
        if !temp_path.exists() {
            error!("Temp transcript file does not exist after write: {}", temp_path.display());
            return Err(anyhow::anyhow!("Temp file verification failed"));
        }

        // Atomic rename
        std::fs::rename(&temp_path, &transcript_path)
            .map_err(|e| {
                error!("Failed to rename transcript file from {} to {}: {}",
                       temp_path.display(), transcript_path.display(), e);
                anyhow::anyhow!("Failed to rename transcript file: {}", e)
            })?;

        info!("✅ Successfully wrote transcripts.json with {} segments", segments_clone.len());
        Ok(())
    }

    /// (whole seconds of audio committed, sample rate). Reported to the UI; the
    /// first field used to be a checkpoint count, which no longer exists.
    pub fn get_stats(&self) -> (usize, u32) {
        if let Some(ref saver) = self.incremental_saver {
            if let Ok(guard) = saver.try_lock() {
                return (guard.duration_seconds() as usize, 48000);
            }
        }
        (0, 48000)
    }

    /// Stop and save using incremental saving approach
    ///
    /// # Arguments
    /// * `app` - Tauri app handle for emitting events
    /// * `recording_duration` - Actual recording duration in seconds (from RecordingState)
    pub async fn stop_and_save<R: Runtime>(
        &mut self,
        app: &AppHandle<R>,
        recording_duration: Option<f64>
    ) -> Result<Option<String>, String> {
        info!("Stopping recording saver");

        // Callers stop and await every producer (streams, then the pipeline
        // task that owns the mixed-audio sender) before calling this, so the
        // writer ends once it has drained the queue.
        let drain = match self.accumulation_task.take() {
            Some(handle) => {
                await_accumulation(handle, ACCUMULATION_DRAIN_TIMEOUT, ACCUMULATION_ABORT_GRACE).await
            }
            None => DrainOutcome::Drained(0),
        };
        match drain {
            DrainOutcome::Drained(written) => {
                info!("Recording writer drained: {} chunks accepted", written);
            }
            outcome => return Err(self.preserve_incomplete_recording(outcome)),
        }

        // Audio was promised but no writer exists: never report that as
        // "auto-save disabled".
        if self.save_audio && self.incremental_saver.is_none() {
            return Err(self.preserve_incomplete_recording(DrainOutcome::WriterFailed(
                "no audio writer was initialized".to_string(),
            )));
        }

        if self.incremental_saver.is_none() {
            info!("⚠️  No audio saver initialized (auto-save was disabled) - skipping audio finalization");
            info!("✅ Transcripts and metadata already saved incrementally");
            return Ok(None);
        }

        // Finalize incremental saver (merge checkpoints into final audio.mp4)
        let final_audio_path = if let Some(saver_arc) = &self.incremental_saver {
            let mut saver = saver_arc.lock().await;
            match saver.finalize().await {
                Ok(path) => {
                    info!("✅ Successfully finalized audio: {}", path.display());
                    path
                }
                Err(e) => {
                    error!("❌ Failed to finalize incremental saver: {}", e);
                    return Err(format!("Failed to finalize audio: {}", e));
                }
            }
        } else {
            error!("No incremental saver initialized - cannot save recording");
            return Err("No incremental saver initialized".to_string());
        };

        // Save final transcripts.json with validation
        if let Some(folder) = &self.meeting_folder {
            if let Err(e) = Self::write_transcripts_json(&self.transcript_segments, folder) {
                error!("❌ Failed to write final transcripts: {}", e);
                return Err(format!("Failed to save transcripts: {}", e));
            }

            // Verify transcripts were written correctly
            let transcript_path = folder.join("transcripts.json");
            if !transcript_path.exists() {
                error!("❌ Transcript file was not created at: {}", transcript_path.display());
                return Err("Transcript file verification failed".to_string());
            }
            info!("✅ Transcripts saved and verified at: {}", transcript_path.display());
        }

        // Update metadata to completed status with actual recording duration
        if let (Some(folder), Some(mut metadata)) = (&self.meeting_folder, self.metadata.clone()) {
            metadata.status = "completed".to_string();
            metadata.completed_at = Some(chrono::Utc::now().to_rfc3339());

            // Use actual recording duration from RecordingState (more accurate than transcript segments)
            // Falls back to last transcript segment if duration not provided
            metadata.duration_seconds = recording_duration.or_else(|| {
                if let Ok(segments) = self.transcript_segments.lock() {
                    segments.last().map(|seg| seg.audio_end_time)
                } else {
                    None
                }
            });

            if let Err(e) = self.write_metadata(folder, &metadata) {
                error!("❌ Failed to update metadata to completed: {}", e);
                return Err(format!("Failed to update metadata: {}", e));
            }

            info!("✅ Metadata updated with duration: {:?}s", metadata.duration_seconds);
        }

        // Emit save event with audio and transcript paths
        let save_event = serde_json::json!({
            "audio_file": final_audio_path.to_string_lossy(),
            "transcript_file": self.meeting_folder.as_ref()
                .map(|f| f.join("transcripts.json").to_string_lossy().to_string()),
            "meeting_name": self.meeting_name,
            "meeting_folder": self.meeting_folder.as_ref()
                .map(|f| f.to_string_lossy().to_string())
        });

        if let Err(e) = app.emit("recording-saved", &save_event) {
            warn!("Failed to emit recording-saved event: {}", e);
        }

        // Clean up transcript segments
        if let Ok(mut segments) = self.transcript_segments.lock() {
            segments.clear();
        }

        Ok(Some(final_audio_path.to_string_lossy().to_string()))
    }

    /// Wind down after a start that failed once the writer was running: the
    /// caller has already closed every producer, so the writer finishes; the
    /// folder is kept (never deleted) but marked as a failed start.
    pub async fn discard_after_failed_start(&mut self) {
        if let Some(handle) = self.accumulation_task.take() {
            let outcome =
                await_accumulation(handle, ACCUMULATION_ABORT_GRACE, ACCUMULATION_ABORT_GRACE).await;
            info!("Writer after failed start ended: {:?}", outcome);
        }
        if let (Some(folder), Some(mut metadata)) = (&self.meeting_folder, self.metadata.clone()) {
            metadata.status = "start_failed".to_string();
            if let Err(e) = self.write_metadata(folder, &metadata) {
                warn!("Failed to mark recording metadata as a failed start: {}", e);
            }
        }
    }

    /// The writer did not drain cleanly: never finalize (merging checkpoints
    /// while a writer may still touch them, or presenting a truncated file as
    /// complete). Keep checkpoints and transcripts for recovery, mark the
    /// metadata incomplete and describe what happened.
    fn preserve_incomplete_recording(&self, outcome: DrainOutcome) -> String {
        let reason = match outcome {
            DrainOutcome::WriterFailed(e) => format!("the audio writer failed ({})", e),
            DrainOutcome::TimedOut { writer_stopped: true } => format!(
                "the audio writer did not finish within {}s and was stopped",
                ACCUMULATION_DRAIN_TIMEOUT.as_secs()
            ),
            DrainOutcome::TimedOut { writer_stopped: false } => format!(
                "the audio writer did not finish within {}s and is still running; the recording was quarantined",
                ACCUMULATION_DRAIN_TIMEOUT.as_secs()
            ),
            DrainOutcome::Drained(_) => unreachable!("drained recordings are finalized"),
        };
        error!("❌ Recording audio not finalized: {}", reason);

        if let Some(folder) = &self.meeting_folder {
            if let Err(e) = Self::write_transcripts_json(&self.transcript_segments, folder) {
                warn!("Failed to write transcripts for incomplete recording: {}", e);
            }
            if let Some(mut metadata) = self.metadata.clone() {
                metadata.status = "incomplete".to_string();
                if let Err(e) = self.write_metadata(folder, &metadata) {
                    warn!("Failed to mark recording metadata incomplete: {}", e);
                }
            }
        }

        let location = self
            .meeting_folder
            .as_ref()
            .map(|f| format!(" Audio checkpoints were kept in {} for recovery.", f.display()))
            .unwrap_or_default();
        format!("The recording audio could not be fully saved: {}.{}", reason, location)
    }

    /// Get the meeting folder path (for passing to backend)
    pub fn get_meeting_folder(&self) -> Option<&PathBuf> {
        self.meeting_folder.as_ref()
    }

    /// Get accumulated transcript segments (for reload sync)
    pub fn get_transcript_segments(&self) -> Vec<TranscriptSegment> {
        if let Ok(segments) = self.transcript_segments.lock() {
            segments.clone()
        } else {
            Vec::new()
        }
    }

    /// Get meeting name (for reload sync)
    pub fn get_meeting_name(&self) -> Option<String> {
        self.meeting_name.clone()
    }

    pub fn audio_saving_enabled(&self) -> bool {
        self.save_audio
    }
}

impl Default for RecordingSaver {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod drain_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn chunk(id: u64) -> AudioChunk {
        AudioChunk {
            capture_gap: None,
            data: vec![0.0; 480],
            sample_rate: 48000,
            timestamp: id as f64 * 0.01,
            chunk_id: id,
            device_type: super::super::recording_state::DeviceType::Microphone,
            dominant_source: None,
        }
    }

    fn prefilled(n: u64) -> mpsc::UnboundedReceiver<AudioChunk> {
        let (tx, rx) = mpsc::unbounded_channel();
        for id in 0..n {
            tx.send(chunk(id)).unwrap();
        }
        rx // tx dropped: every producer has stopped
    }

    #[tokio::test]
    async fn drains_every_queued_chunk_in_order_after_producers_stop() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink_seen = seen.clone();
        let handle: AccumulationTask = tokio::spawn(drain_into(prefilled(250), move |c| {
            let seen = sink_seen.clone();
            async move {
                seen.lock().unwrap().push(c.chunk_id);
                Ok(())
            }
        }));
        let outcome = await_accumulation(handle, Duration::from_secs(5), Duration::from_secs(1)).await;
        assert_eq!(outcome, DrainOutcome::Drained(250));
        assert_eq!(*seen.lock().unwrap(), (0..250).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn slow_sink_still_receives_the_whole_tail() {
        let written = Arc::new(AtomicU64::new(0));
        let counter = written.clone();
        let handle: AccumulationTask = tokio::spawn(drain_into(prefilled(20), move |_| {
            let counter = counter.clone();
            async move {
                tokio::time::sleep(Duration::from_millis(5)).await;
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        }));
        let outcome = await_accumulation(handle, Duration::from_secs(5), Duration::from_secs(1)).await;
        assert_eq!(outcome, DrainOutcome::Drained(20));
        assert_eq!(written.load(Ordering::SeqCst), 20);
    }

    #[tokio::test]
    async fn sink_failure_is_reported_and_producers_never_see_a_closed_channel() {
        let (tx, rx) = mpsc::unbounded_channel();
        let calls = Arc::new(AtomicU64::new(0));
        let counter = calls.clone();
        let handle: AccumulationTask = tokio::spawn(drain_into(rx, move |c| {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                if c.chunk_id == 2 {
                    Err(anyhow::anyhow!("disk full"))
                } else {
                    Ok(())
                }
            }
        }));
        for id in 0..10 {
            tx.send(chunk(id)).expect("receiver must stay open after a write failure");
            tokio::task::yield_now().await;
        }
        drop(tx);
        let outcome = await_accumulation(handle, Duration::from_secs(5), Duration::from_secs(1)).await;
        assert_eq!(outcome, DrainOutcome::WriterFailed("disk full".to_string()));
        assert_eq!(calls.load(Ordering::SeqCst), 3, "no writes after the first failure");
    }

    #[tokio::test]
    async fn writer_panic_is_a_failure_not_success() {
        let handle: AccumulationTask = tokio::spawn(drain_into(prefilled(3), |c| async move {
            if c.chunk_id == 1 {
                panic!("writer bug");
            }
            Ok(())
        }));
        let outcome = await_accumulation(handle, Duration::from_secs(5), Duration::from_secs(1)).await;
        assert_eq!(outcome, DrainOutcome::WriterFailed("the audio writer task panicked".to_string()));
    }

    #[tokio::test]
    async fn stuck_producer_times_out_and_writer_is_stopped() {
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send(chunk(0)).unwrap();
        let handle: AccumulationTask = tokio::spawn(drain_into(rx, |_| async { Ok(()) }));
        let outcome = await_accumulation(handle, Duration::from_millis(50), Duration::from_secs(1)).await;
        assert_eq!(outcome, DrainOutcome::TimedOut { writer_stopped: true });
        drop(tx);
    }

    #[test]
    fn incomplete_recording_keeps_artifacts_and_marks_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let mut saver = RecordingSaver::new();
        saver.meeting_folder = Some(dir.path().to_path_buf());
        saver.metadata = Some(MeetingMetadata {
            version: "1.0".to_string(),
            meeting_id: None,
            meeting_name: Some("Test".to_string()),
            created_at: "2026-10-04T00:00:00Z".to_string(),
            completed_at: None,
            duration_seconds: None,
            devices: DeviceInfo { microphone: None, system_audio: None },
            audio_file: "audio.mp4".to_string(),
            transcript_file: "transcripts.json".to_string(),
            sample_rate: 48000,
            status: "recording".to_string(),
        });
        let checkpoints = dir.path().join(".checkpoints");
        std::fs::create_dir_all(&checkpoints).unwrap();
        std::fs::write(checkpoints.join("audio_chunk_000.mp4"), b"chunk").unwrap();

        let message = saver.preserve_incomplete_recording(DrainOutcome::TimedOut { writer_stopped: false });

        assert!(message.contains("quarantined"), "{message}");
        assert!(message.contains("kept"), "{message}");
        assert!(checkpoints.join("audio_chunk_000.mp4").exists());
        assert!(!dir.path().join("audio.mp4").exists(), "must not finalize");
        assert!(dir.path().join("transcripts.json").exists());
        let metadata = std::fs::read_to_string(dir.path().join("metadata.json")).unwrap();
        assert!(metadata.contains("\"status\": \"incomplete\""), "{metadata}");
    }
}

#[cfg(test)]
mod storage_tests {
    use super::*;

    fn named_saver() -> RecordingSaver {
        let mut saver = RecordingSaver::new();
        saver.set_meeting_name(Some("Weekly".to_string()));
        saver
    }

    #[test]
    fn audio_mode_fails_when_the_recordings_folder_is_unusable() {
        let dir = tempfile::tempdir().unwrap();
        // A regular file where the recordings folder should be: portable
        // (unlike read-only directories, which Windows does not enforce).
        let blocked = dir.path().join("recordings");
        std::fs::write(&blocked, b"not a directory").unwrap();

        let mut saver = named_saver();
        let error = saver.prepare_storage_in(&blocked, true).unwrap_err().to_string();
        assert!(error.contains("Cannot create the recording folder"), "{error}");
        assert!(saver.incremental_saver.is_none());
    }

    #[test]
    fn transcript_only_mode_also_requires_its_folder() {
        let dir = tempfile::tempdir().unwrap();
        let blocked = dir.path().join("recordings");
        std::fs::write(&blocked, b"not a directory").unwrap();
        assert!(named_saver().prepare_storage_in(&blocked, false).is_err());
    }

    #[test]
    fn transcript_only_mode_creates_folder_without_audio_writer() {
        let dir = tempfile::tempdir().unwrap();
        let mut saver = named_saver();
        saver.prepare_storage_in(&dir.path().to_path_buf(), false).unwrap();
        assert!(!saver.audio_saving_enabled());
        let folder = saver.meeting_folder.clone().unwrap();
        assert!(folder.join("metadata.json").exists());
        assert!(!folder.join(".checkpoints").exists());
        assert!(saver.incremental_saver.is_none());
    }

    #[test]
    fn audio_mode_starts_the_streaming_encoder() {
        let dir = tempfile::tempdir().unwrap();
        let mut saver = named_saver();
        saver.prepare_storage_in(&dir.path().to_path_buf(), true).unwrap();
        assert!(saver.audio_saving_enabled());
        assert!(saver.incremental_saver.is_some());
        // The encoder writes a crash-recoverable audio.mp4 directly; the
        // .checkpoints/ directory is only read for meetings from older builds.
        assert!(!saver.meeting_folder.clone().unwrap().join(".checkpoints").exists());
    }

    #[test]
    fn missing_meeting_name_fails_instead_of_recording_unsaved() {
        let dir = tempfile::tempdir().unwrap();
        let mut saver = RecordingSaver::new();
        assert!(saver.prepare_storage_in(&dir.path().to_path_buf(), true).is_err());
    }

    #[tokio::test]
    async fn failed_start_ends_the_writer_and_marks_the_folder() {
        let dir = tempfile::tempdir().unwrap();
        let mut saver = named_saver();
        saver.prepare_storage_in(&dir.path().to_path_buf(), true).unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        saver.start_accumulation(true, rx);
        drop(tx); // the pipeline was stopped
        saver.discard_after_failed_start().await;
        assert!(saver.accumulation_task.is_none());
        let folder = saver.meeting_folder.clone().unwrap();
        let metadata = std::fs::read_to_string(folder.join("metadata.json")).unwrap();
        assert!(metadata.contains("start_failed"), "{metadata}");
        // A retry gets a fresh saver; nothing here blocks it.
        let mut retry = named_saver();
        assert!(retry.prepare_storage_in(&dir.path().to_path_buf(), true).is_ok());
    }
}
