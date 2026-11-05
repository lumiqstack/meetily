//! Local speaker diarization: model file management and whole-recording inference.
//!
//! One model, NVIDIA Nemotron-3 Diarization as a single fp32 ONNX file (402 MB), stored
//! under `<models>/diarization/nemotron-3-diarization/`. It is downloaded on demand from a
//! pinned Hugging Face revision, verified by exact byte count like the Parakeet artifacts,
//! and loaded per job: a retranscription runs the model once over the whole recording and
//! drops the session afterwards, so it never competes with the live transcription engines
//! for memory.

use super::assign::{merge_adjacent_spans, SpeakerSpan};
use super::sortformer::{DiarizationConfig, Sortformer, SAMPLE_RATE};
use anyhow::{anyhow, Context, Result};
use futures_util::StreamExt;
use ort::execution_providers::CPUExecutionProvider;
use ort::session::builder::GraphOptimizationLevel;
use ort::session::Session;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

pub const MODEL_NAME: &str = "nemotron-3-diarization";
pub const MODEL_FILE: &str = "nemotron3_step.int8.onnx";
pub const MODEL_BYTES: u64 = 107_243_199;
pub const MODEL_SIZE_MB: u32 = 107;
/// s0undy's int8 build of beshkenadze's opset-17 export of NVIDIA's checkpoint (all
/// OpenMDW-1.1), re-hosted on a Meetily release because corporate networks that block
/// Hugging Face usually allow GitHub. Opset 17 matters: the ONNX Runtime 1.22 the Windows
/// build ships refuses the opset-23 parakeet-rs export. The release notes name the source
/// revisions and the file's SHA-256:
/// https://github.com/lumiqstack/meetily/releases/tag/model-nemotron-3-diarization-v3
pub const MODEL_URL: &str = "https://github.com/lumiqstack/meetily/releases/download/model-nemotron-3-diarization-v3/nemotron3_step.int8.onnx";

/// Two spans of the same speaker closer than this are one utterance split by a window edge.
const MERGE_GAP_MS: f64 = 1.0;
/// Abort a download that receives no bytes for this long.
const DOWNLOAD_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

static DOWNLOAD_IN_PROGRESS: AtomicBool = AtomicBool::new(false);
static DOWNLOAD_PERCENT: AtomicU8 = AtomicU8::new(0);
static DOWNLOAD_CANCELLED: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum ModelStatus {
    Missing,
    Downloading { percent: u8 },
    /// A file exists but is not the expected size (interrupted download or wrong file).
    Corrupted { file_size: u64 },
    Available,
}

/// What the UI needs to offer "Identify speakers": state plus the download size.
#[derive(Debug, Clone, Serialize)]
pub struct ModelReport {
    #[serde(flatten)]
    pub status: ModelStatus,
    pub size_mb: u32,
    pub path: String,
}

pub fn models_dir() -> PathBuf {
    crate::storage::models_dir().join("diarization")
}

pub fn model_dir() -> PathBuf {
    models_dir().join(MODEL_NAME)
}

pub fn model_path() -> PathBuf {
    model_dir().join(MODEL_FILE)
}

pub fn model_status() -> ModelStatus {
    model_status_at(&model_path())
}

fn model_status_at(path: &Path) -> ModelStatus {
    if DOWNLOAD_IN_PROGRESS.load(Ordering::SeqCst) {
        return ModelStatus::Downloading { percent: DOWNLOAD_PERCENT.load(Ordering::SeqCst) };
    }
    match std::fs::metadata(path) {
        Ok(meta) if meta.len() == MODEL_BYTES => ModelStatus::Available,
        Ok(meta) => ModelStatus::Corrupted { file_size: meta.len() },
        Err(_) => ModelStatus::Missing,
    }
}

/// Remove files in the model folder that are not the current model or its download in
/// progress: an earlier Meetily build downloaded a different export there, and 400 MB of
/// unusable file should not linger on the user's disk.
fn sweep_stale_files() {
    let Ok(entries) = std::fs::read_dir(model_dir()) else {
        return;
    };
    let part_name = format!("{MODEL_FILE}.part");
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name == MODEL_FILE || name.to_string_lossy() == part_name {
            continue;
        }
        if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            match std::fs::remove_file(entry.path()) {
                Ok(()) => log::info!("Removed stale speaker model file {}", entry.path().display()),
                Err(e) => log::warn!("Could not remove stale file {}: {e}", entry.path().display()),
            }
        }
    }
}

pub fn model_report() -> ModelReport {
    sweep_stale_files();
    ModelReport {
        status: model_status(),
        size_mb: MODEL_SIZE_MB,
        path: display_path(&model_path()),
    }
}

/// The path as the user knows it: with the models root resolved through any junction or
/// symlink (on Windows the app data folder is often a junction onto another drive), and
/// without the `\\?\` prefix canonicalization adds there.
fn display_path(path: &Path) -> String {
    let root = crate::storage::models_dir();
    let resolved = match (std::fs::canonicalize(&root), path.strip_prefix(&root)) {
        (Ok(real_root), Ok(rest)) => real_root.join(rest),
        _ => path.to_path_buf(),
    };
    let text = resolved.display().to_string();
    text.strip_prefix(r"\\?\").map(str::to_owned).unwrap_or(text)
}

pub fn is_model_available() -> bool {
    model_status() == ModelStatus::Available
}

pub fn cancel_download() {
    DOWNLOAD_CANCELLED.store(true, Ordering::SeqCst);
}

/// Download the model to a `.part` file, verify its size, then move it into place.
/// Only one download runs at a time; a second call while one is active is an error.
pub async fn download_model(progress: impl Fn(u8) + Send) -> Result<()> {
    if DOWNLOAD_IN_PROGRESS.swap(true, Ordering::SeqCst) {
        return Err(anyhow!("The speaker model is already downloading"));
    }
    DOWNLOAD_CANCELLED.store(false, Ordering::SeqCst);
    DOWNLOAD_PERCENT.store(0, Ordering::SeqCst);
    let result = download_model_inner(&progress).await;
    DOWNLOAD_IN_PROGRESS.store(false, Ordering::SeqCst);
    result
}

async fn download_model_inner(progress: &(impl Fn(u8) + Send)) -> Result<()> {
    let dir = model_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let final_path = dir.join(MODEL_FILE);
    let part_path = dir.join(format!("{MODEL_FILE}.part"));

    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .build()?;
    let response = client
        .get(MODEL_URL)
        .send()
        .await
        .context("requesting the speaker model")?
        .error_for_status()
        .context("speaker model download refused")?;
    if let Some(len) = response.content_length() {
        if len != MODEL_BYTES {
            return Err(anyhow!(
                "speaker model download is {len} bytes, expected {MODEL_BYTES}; the pinned file changed"
            ));
        }
    }

    let mut file = tokio::fs::File::create(&part_path)
        .await
        .with_context(|| format!("creating {}", part_path.display()))?;
    let mut stream = response.bytes_stream();
    let mut downloaded: u64 = 0;
    let mut last_percent = 0u8;
    loop {
        let next = tokio::time::timeout(DOWNLOAD_IDLE_TIMEOUT, stream.next()).await;
        let chunk = match next {
            Ok(Some(chunk)) => chunk.context("reading speaker model download")?,
            Ok(None) => break,
            Err(_) => {
                let _ = tokio::fs::remove_file(&part_path).await;
                return Err(anyhow!("speaker model download stalled for 60 s"));
            }
        };
        if DOWNLOAD_CANCELLED.load(Ordering::SeqCst) {
            drop(file);
            let _ = tokio::fs::remove_file(&part_path).await;
            return Err(anyhow!("Speaker model download cancelled"));
        }
        tokio::io::AsyncWriteExt::write_all(&mut file, &chunk).await?;
        downloaded += chunk.len() as u64;
        let percent = ((downloaded * 100) / MODEL_BYTES).min(99) as u8;
        if percent != last_percent {
            last_percent = percent;
            DOWNLOAD_PERCENT.store(percent, Ordering::SeqCst);
            progress(percent);
        }
    }
    tokio::io::AsyncWriteExt::flush(&mut file).await?;
    drop(file);

    if downloaded != MODEL_BYTES {
        let _ = tokio::fs::remove_file(&part_path).await;
        return Err(anyhow!(
            "speaker model download ended at {downloaded} bytes, expected {MODEL_BYTES}"
        ));
    }
    tokio::fs::rename(&part_path, &final_path)
        .await
        .with_context(|| format!("moving {} into place", part_path.display()))?;
    DOWNLOAD_PERCENT.store(100, Ordering::SeqCst);
    progress(100);
    Ok(())
}

/// Remove the model file (and any partial download) so it can be fetched again.
pub fn delete_model() -> Result<()> {
    let dir = model_dir();
    if dir.exists() {
        std::fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
    }
    Ok(())
}

fn build_session(path: &Path) -> Result<Session> {
    crate::ensure_onnx_runtime_available()?;
    let session = Session::builder()?
        .with_optimization_level(GraphOptimizationLevel::Level3)?
        .with_execution_providers([CPUExecutionProvider::default().build()])?
        .commit_from_file(path)
        .with_context(|| format!("loading speaker model from {}", path.display()))?;
    Ok(session)
}

/// Diarize a 16 kHz mono recording on a blocking thread. `on_progress` gets 0.0..=1.0 and
/// returns `false` to cancel. Returns speaker spans in milliseconds, merged across the
/// model's window edges and sorted by start time.
pub async fn diarize_recording(
    audio_16k_mono: Arc<Vec<f32>>,
    on_progress: impl FnMut(f32) -> bool + Send + 'static,
) -> Result<Vec<SpeakerSpan>> {
    let path = model_path();
    if model_status_at(&path) != super::engine::ModelStatus::Available {
        return Err(anyhow!(
            "The speaker model is not downloaded. Download it from the retranscribe dialog first."
        ));
    }
    tokio::task::spawn_blocking(move || diarize_blocking(&path, &audio_16k_mono, on_progress))
        .await
        .map_err(|e| anyhow!("Diarization task panicked: {e}"))?
}

fn diarize_blocking(
    model_path: &Path,
    audio: &[f32],
    on_progress: impl FnMut(f32) -> bool,
) -> Result<Vec<SpeakerSpan>> {
    let started = std::time::Instant::now();
    let session = build_session(model_path)?;
    let mut diarizer = Sortformer::from_session(session, DiarizationConfig::default())?;
    log::info!(
        "Diarizing {:.1}s of audio with Nemotron-3 Diarization (window {:.1}s)",
        audio.len() as f64 / SAMPLE_RATE as f64,
        diarizer.latency_seconds()
    );
    let segments = diarizer.diarize_all(audio, on_progress)?;
    let ms_per_sample = 1000.0 / SAMPLE_RATE as f64;
    let spans: Vec<SpeakerSpan> = segments
        .into_iter()
        .map(|s| SpeakerSpan {
            start_ms: s.start as f64 * ms_per_sample,
            end_ms: s.end as f64 * ms_per_sample,
            speaker: s.speaker_id,
        })
        .collect();
    let spans = merge_adjacent_spans(spans, MERGE_GAP_MS);
    let speakers = {
        let mut ids: Vec<usize> = spans.iter().map(|s| s.speaker).collect();
        ids.sort_unstable();
        ids.dedup();
        ids.len()
    };
    log::info!(
        "Diarization found {} speakers in {} spans ({:.1}s)",
        speakers,
        spans.len(),
        started.elapsed().as_secs_f64()
    );
    Ok(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_reflects_file_presence_and_exact_size() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(MODEL_FILE);
        assert_eq!(model_status_at(&path), ModelStatus::Missing);

        std::fs::write(&path, b"not a model").unwrap();
        assert_eq!(model_status_at(&path), ModelStatus::Corrupted { file_size: 11 });
    }

    #[test]
    fn report_serializes_status_flat_for_the_ui() {
        let report = ModelReport {
            status: ModelStatus::Downloading { percent: 42 },
            size_mb: MODEL_SIZE_MB,
            path: "x".into(),
        };
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["status"], "downloading");
        assert_eq!(json["percent"], 42);
        assert_eq!(json["size_mb"], 107);
        let json = serde_json::to_value(ModelStatus::Available).unwrap();
        assert_eq!(json["status"], "available");
    }
}

#[cfg(test)]
mod load_probe {
    /// Diarizes the 16 kHz mono 16-bit PCM WAV named by `DIAR_WAV_PATH` with the model
    /// named by `DIAR_MODEL_PATH` and prints the spans, for checking a real export by hand.
    #[test]
    #[ignore]
    fn diarize_real_wav() {
        let model = std::path::PathBuf::from(std::env::var("DIAR_MODEL_PATH").expect("DIAR_MODEL_PATH"));
        let wav = std::fs::read(std::env::var("DIAR_WAV_PATH").expect("DIAR_WAV_PATH")).expect("read wav");
        // Minimal RIFF walk: find the "data" chunk, assume what ffmpeg wrote (s16le mono 16 kHz).
        let mut pos = 12;
        let samples: Vec<f32> = loop {
            let id = &wav[pos..pos + 4];
            let len = u32::from_le_bytes([wav[pos + 4], wav[pos + 5], wav[pos + 6], wav[pos + 7]]) as usize;
            if id == b"data" {
                break wav[pos + 8..pos + 8 + len]
                    .chunks_exact(2)
                    .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0)
                    .collect();
            }
            pos += 8 + len + (len & 1);
        };
        let started = std::time::Instant::now();
        let spans = super::diarize_blocking(&model, &samples, |_| true).unwrap_or_else(|e| panic!("{e:#}"));
        println!("DIARIZED {:.1}s of audio in {:.1}s", samples.len() as f64 / 16000.0, started.elapsed().as_secs_f64());
        for s in &spans {
            println!("SPAN speaker {} {:>7.2}s - {:>7.2}s", s.speaker, s.start_ms / 1000.0, s.end_ms / 1000.0);
        }
    }

    /// Loads a real model file named by `DIAR_MODEL_PATH`, so a load failure seen on
    /// another machine can be reproduced here with the same ort version.
    #[test]
    #[ignore]
    fn load_real_model() {
        let path = std::path::PathBuf::from(std::env::var("DIAR_MODEL_PATH").expect("DIAR_MODEL_PATH"));
        match super::build_session(&path) {
            Ok(session) => {
                let s = session;
                let sf = super::super::sortformer::Sortformer::from_session(s, Default::default());
                match sf {
                    Ok(_) => println!("LOAD_OK"),
                    Err(e) => panic!("sortformer init failed: {e:#}"),
                }
            }
            Err(e) => panic!("session load failed: {e:#}"),
        }
    }
}
