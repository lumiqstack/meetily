use std::path::PathBuf;
use anyhow::{Result, anyhow};
use log::{info, error};
use super::recording_state::AudioChunk;
use super::stream_encoder::StreamingEncoder;
use serde::{Serialize, Deserialize};

use super::ffmpeg::find_ffmpeg_path;

/// Writes meeting audio straight into `audio.mp4` as it arrives.
///
/// This used to buffer 30 s in RAM, encode it to `.checkpoints/audio_chunk_NNN.mp4`,
/// and concat-remux the lot at Stop. The checkpoints existed purely so a crash
/// left something recoverable; a fragmented MP4 gives that property for free, so
/// the buffer, the per-checkpoint ffmpeg spawn, and the final full-file remux are
/// all gone. See [`super::stream_encoder`].
///
/// The `.checkpoints/` reader in this file stays for meetings recorded by older
/// builds — see [`recover_audio_from_checkpoints`].
pub struct IncrementalAudioSaver {
    encoder: Option<StreamingEncoder>,
    meeting_folder: PathBuf,
}

impl IncrementalAudioSaver {
    /// Create a saver and start its encoder.
    ///
    /// # Arguments
    /// * `meeting_folder` - Path to the meeting folder
    /// * `sample_rate` - Sample rate of the mixed audio (typically 48000)
    pub fn new(meeting_folder: PathBuf, sample_rate: u32) -> Result<Self> {
        let output = meeting_folder.join("audio.mp4");
        let encoder = StreamingEncoder::new(output, sample_rate)?;

        Ok(Self {
            encoder: Some(encoder),
            meeting_folder,
        })
    }

    /// Hand a mixed window to the encoder. Returns as soon as it is queued.
    pub fn add_chunk(&mut self, chunk: AudioChunk) -> Result<()> {
        let Some(encoder) = &self.encoder else {
            return Err(anyhow!("Encoder already finalized"));
        };
        encoder.write(chunk.data)
    }

    /// Close the encoder and return the finished `audio.mp4`.
    pub async fn finalize(&mut self) -> Result<PathBuf> {
        let Some(encoder) = self.encoder.take() else {
            return Err(anyhow!("Recording already finalized"));
        };

        // ffmpeg has to drain its input and write the trailer; keep that off the
        // async worker it would otherwise park.
        let path = tokio::task::spawn_blocking(move || encoder.finalize())
            .await
            .map_err(|e| anyhow!("Encoder finalize task failed: {}", e))??;

        info!("Finalized recording: {}", path.display());
        Ok(path)
    }

    /// Get the meeting folder path
    pub fn get_meeting_folder(&self) -> &PathBuf {
        &self.meeting_folder
    }

    /// Seconds of audio committed so far. Reported to the UI as recording stats.
    pub fn duration_seconds(&self) -> f64 {
        self.encoder
            .as_ref()
            .map(|e| e.duration_seconds())
            .unwrap_or(0.0)
    }
}

/// Audio recovery status for transcript recovery feature
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioRecoveryStatus {
    pub status: String, // "success" | "partial" | "failed" | "none"
    pub chunk_count: u32,
    pub estimated_duration_seconds: f64,
    pub audio_file_path: Option<String>,
    pub message: String,
}

/// Duration of an existing audio file, via ffprobe/ffmpeg. Best-effort: a
/// fragmented MP4 from an interrupted recording may not carry a duration, in
/// which case we report 0 rather than failing the recovery.
fn probe_duration_seconds(path: &PathBuf) -> f64 {
    let Some(ffmpeg) = find_ffmpeg_path() else {
        return 0.0;
    };
    // ffprobe sits next to ffmpeg in every distribution we bundle or find.
    let ffprobe = ffmpeg.with_file_name(if cfg!(windows) {
        "ffprobe.exe"
    } else {
        "ffprobe"
    });
    if !ffprobe.exists() {
        return 0.0;
    }

    let mut command = std::process::Command::new(ffprobe);
    command.args([
        "-v",
        "error",
        "-show_entries",
        "format=duration",
        "-of",
        "default=noprint_wrappers=1:nokey=1",
    ]);
    command.arg(path);

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    command
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse::<f64>().ok())
        .unwrap_or(0.0)
}

/// Recover audio after a crash.
///
/// Two shapes to handle. Recordings from the streaming encoder already have a
/// playable fragmented `audio.mp4` on disk, so recovery is just confirming it is
/// there. Recordings from older builds have `.checkpoints/audio_chunk_NNN.mp4`
/// that still need the concat merge.
#[tauri::command]
pub async fn recover_audio_from_checkpoints(
    meeting_folder: String,
    _sample_rate: u32
) -> Result<AudioRecoveryStatus, String> {
    info!("Starting audio recovery for folder: {}", meeting_folder);

    let folder_path = PathBuf::from(&meeting_folder);
    let checkpoints_dir = folder_path.join(".checkpoints");

    // Streaming-encoder recording: the file is already complete enough to play.
    // Checked first so a folder holding both (an interrupted upgrade) prefers
    // the newer, more complete artifact.
    let audio_path = folder_path.join("audio.mp4");
    if !checkpoints_dir.exists() {
        let usable = std::fs::metadata(&audio_path)
            .map(|m| m.len() > 0)
            .unwrap_or(false);

        if usable {
            let duration = probe_duration_seconds(&audio_path);
            info!(
                "Recovered streamed audio directly: {} ({:.1}s)",
                audio_path.display(),
                duration
            );
            return Ok(AudioRecoveryStatus {
                status: "success".to_string(),
                chunk_count: 1,
                estimated_duration_seconds: duration,
                audio_file_path: Some(audio_path.to_string_lossy().to_string()),
                message: "Recovered audio written during the interrupted recording".to_string(),
            });
        }

        info!("No checkpoints directory found at: {}", checkpoints_dir.display());
        return Ok(AudioRecoveryStatus {
            status: "none".to_string(),
            chunk_count: 0,
            estimated_duration_seconds: 0.0,
            audio_file_path: None,
            message: "No audio checkpoints found".to_string(),
        });
    }

    // Scan for checkpoint files
    let mut checkpoint_files: Vec<_> = std::fs::read_dir(&checkpoints_dir)
        .map_err(|e| format!("Failed to read checkpoints directory: {}", e))?
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry.path().extension().and_then(|s| s.to_str()) == Some("mp4")
        })
        .collect();

    if checkpoint_files.is_empty() {
        info!("No checkpoint files found in: {}", checkpoints_dir.display());
        return Ok(AudioRecoveryStatus {
            status: "none".to_string(),
            chunk_count: 0,
            estimated_duration_seconds: 0.0,
            audio_file_path: None,
            message: "No audio checkpoint files found".to_string(),
        });
    }

    // Sort by filename (audio_chunk_000.mp4, audio_chunk_001.mp4, etc.)
    checkpoint_files.sort_by_key(|entry| entry.path());

    let chunk_count = checkpoint_files.len() as u32;
    let estimated_duration = (chunk_count as f64) * 30.0; // 30 seconds per chunk

    info!("Found {} checkpoint files, estimated duration: {:.2}s", chunk_count, estimated_duration);

    // Create FFmpeg concat file
    let concat_file_path = checkpoints_dir.join("concat_list.txt");
    let mut concat_content = String::new();

    for entry in &checkpoint_files {
        let path = entry.path().canonicalize()
            .map_err(|e| format!("Failed to canonicalize path: {}", e))?;
        concat_content.push_str(&format!("file '{}'\n", path.display()));
    }

    std::fs::write(&concat_file_path, concat_content)
        .map_err(|e| format!("Failed to write concat file: {}", e))?;

    // Run FFmpeg to merge chunks
    let output_path = folder_path.join("audio.mp4");
    let output_path_str = output_path.to_str()
        .ok_or("Invalid output path")?
        .to_string();

    let ffmpeg_path = find_ffmpeg_path()
        .ok_or_else(|| "FFmpeg not found. Please install FFmpeg to recover audio.".to_string())?;
    info!("Using FFmpeg at: {:?}", ffmpeg_path);

    let mut command = std::process::Command::new(ffmpeg_path);

    command.args(&[
        "-f", "concat",
        "-safe", "0",
        "-i", concat_file_path.to_str().unwrap(),
        "-c", "copy",
        "-y", // Overwrite if exists
        &output_path_str
    ]);

    // Hide console window on Windows
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    let ffmpeg_result = command.output();

    match ffmpeg_result {
        Ok(output) if output.status.success() => {
            // Clean up concat file
            let _ = std::fs::remove_file(concat_file_path);

            info!("Successfully recovered audio: {}", output_path_str);

            Ok(AudioRecoveryStatus {
                status: "success".to_string(),
                chunk_count,
                estimated_duration_seconds: estimated_duration,
                audio_file_path: Some(output_path_str),
                message: format!("Successfully recovered {} audio chunks", chunk_count),
            })
        }
        Ok(output) => {
            let error = String::from_utf8_lossy(&output.stderr);
            error!("FFmpeg recovery failed: {}", error);
            Ok(AudioRecoveryStatus {
                status: "failed".to_string(),
                chunk_count,
                estimated_duration_seconds: estimated_duration,
                audio_file_path: None,
                message: format!("FFmpeg failed: {}", error),
            })
        }
        Err(e) => {
            error!("Failed to run FFmpeg: {}", e);
            Ok(AudioRecoveryStatus {
                status: "failed".to_string(),
                chunk_count,
                estimated_duration_seconds: estimated_duration,
                audio_file_path: None,
                message: format!("Failed to run FFmpeg: {}", e),
            })
        }
    }
}

/// Clean up checkpoint files after successful recording or recovery
/// This command is called by the frontend after successful save to clean up checkpoint files
#[tauri::command]
pub async fn cleanup_checkpoints(meeting_folder: String) -> Result<(), String> {
    info!("Cleaning up checkpoints for folder: {}", meeting_folder);

    let folder_path = PathBuf::from(&meeting_folder);
    let checkpoints_dir = folder_path.join(".checkpoints");

    if checkpoints_dir.exists() {
        std::fs::remove_dir_all(&checkpoints_dir)
            .map_err(|e| format!("Failed to remove checkpoints directory: {}", e))?;
        info!("Successfully cleaned up checkpoints directory");
    } else {
        info!("No checkpoints directory to clean up");
    }

    Ok(())
}

/// Whether a meeting folder holds audio that survived an interrupted recording.
///
/// True for either shape: a non-empty `audio.mp4` written by the streaming
/// encoder, or legacy `.checkpoints/*.mp4` awaiting a merge.
#[tauri::command]
pub async fn has_audio_checkpoints(meeting_folder: String) -> Result<bool, String> {
    let folder_path = PathBuf::from(&meeting_folder);

    // Streaming encoder writes here continuously, so a non-empty file means
    // there is audio to recover even though the recording never finished.
    if std::fs::metadata(folder_path.join("audio.mp4"))
        .map(|m| m.len() > 0)
        .unwrap_or(false)
    {
        return Ok(true);
    }

    let checkpoints_dir = folder_path.join(".checkpoints");
    if !checkpoints_dir.exists() {
        return Ok(false);
    }

    // Legacy layout: scan for .mp4 checkpoint files
    let has_mp4_files = std::fs::read_dir(&checkpoints_dir)
        .map_err(|e| format!("Failed to read checkpoints directory: {}", e))?
        .filter_map(|entry| entry.ok())
        .any(|entry| {
            entry.path().extension().and_then(|s| s.to_str()) == Some("mp4")
        });

    Ok(has_mp4_files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use super::super::recording_state::DeviceType;

    fn ffmpeg_available() -> bool {
        find_ffmpeg_path().is_some()
    }

    fn chunk(samples: usize, id: u64) -> AudioChunk {
        AudioChunk {
            data: vec![0.25f32; samples],
            sample_rate: 48000,
            timestamp: id as f64 * 0.5,
            chunk_id: id,
            device_type: DeviceType::Microphone,
            dominant_source: None,
        }
    }

    #[tokio::test]
    async fn streams_audio_straight_to_the_meeting_folder() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not found");
            return;
        }
        let temp_dir = tempdir().unwrap();
        let meeting_folder = temp_dir.path().join("Test_Meeting");
        std::fs::create_dir_all(&meeting_folder).unwrap();

        let mut saver = IncrementalAudioSaver::new(meeting_folder.clone(), 48000).unwrap();

        // 60 seconds of audio — twice the old checkpoint interval, to prove
        // nothing here depends on that boundary any more.
        for i in 0..120 {
            saver.add_chunk(chunk(24_000, i)).unwrap();
        }

        let final_path = saver.finalize().await.unwrap();
        assert_eq!(final_path, meeting_folder.join("audio.mp4"));
        assert!(final_path.exists());

        // No checkpoint scratch directory is created at all now.
        assert!(!meeting_folder.join(".checkpoints").exists());
    }

    #[tokio::test]
    async fn finalizing_twice_is_an_error_not_a_panic() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not found");
            return;
        }
        let temp_dir = tempdir().unwrap();
        let meeting_folder = temp_dir.path().join("Double_Finalize");
        std::fs::create_dir_all(&meeting_folder).unwrap();

        let mut saver = IncrementalAudioSaver::new(meeting_folder, 48000).unwrap();
        saver.add_chunk(chunk(48_000, 0)).unwrap();

        assert!(saver.finalize().await.is_ok());
        assert!(saver.finalize().await.is_err());
    }

    #[tokio::test]
    async fn empty_recording_reports_an_error() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not found");
            return;
        }
        let temp_dir = tempdir().unwrap();
        let meeting_folder = temp_dir.path().join("Empty_Test");
        std::fs::create_dir_all(&meeting_folder).unwrap();

        let mut saver = IncrementalAudioSaver::new(meeting_folder, 48000).unwrap();

        // No samples at all: ffmpeg rejects the empty input stream.
        assert!(saver.finalize().await.is_err());
    }

    #[tokio::test]
    async fn has_audio_checkpoints_sees_a_streamed_file() {
        let temp_dir = tempdir().unwrap();
        let folder = temp_dir.path().join("Streamed");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("audio.mp4"), b"not really an mp4, but non-empty").unwrap();

        let found = has_audio_checkpoints(folder.to_string_lossy().to_string())
            .await
            .unwrap();
        assert!(found);
    }

    #[tokio::test]
    async fn has_audio_checkpoints_sees_legacy_checkpoints() {
        let temp_dir = tempdir().unwrap();
        let folder = temp_dir.path().join("Legacy");
        std::fs::create_dir_all(folder.join(".checkpoints")).unwrap();
        std::fs::write(folder.join(".checkpoints/audio_chunk_000.mp4"), b"x").unwrap();

        let found = has_audio_checkpoints(folder.to_string_lossy().to_string())
            .await
            .unwrap();
        assert!(found);
    }

    #[tokio::test]
    async fn has_audio_checkpoints_ignores_an_empty_file() {
        let temp_dir = tempdir().unwrap();
        let folder = temp_dir.path().join("Nothing");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("audio.mp4"), b"").unwrap();

        let found = has_audio_checkpoints(folder.to_string_lossy().to_string())
            .await
            .unwrap();
        assert!(!found);
    }
}
