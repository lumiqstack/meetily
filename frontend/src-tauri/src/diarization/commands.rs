//! Tauri commands for the local speaker model: status, download, cancel, delete.

use super::engine::{self, ModelReport};
use serde::Serialize;
use tauri::{command, AppHandle, Emitter, Runtime};

#[derive(Debug, Clone, Serialize)]
struct DownloadProgressEvent {
    percent: u8,
}

#[derive(Debug, Clone, Serialize)]
struct DownloadFailedEvent {
    error: String,
}

#[command]
pub async fn diarization_model_status() -> Result<ModelReport, String> {
    Ok(engine::model_report())
}

/// Download the speaker model, emitting `diarization-download-progress` with the percent
/// as it goes and `diarization-download-complete` or `diarization-download-failed` at the
/// end. Resolves when the download finishes.
#[command]
pub async fn diarization_download_model<R: Runtime>(app: AppHandle<R>) -> Result<ModelReport, String> {
    let progress_app = app.clone();
    let result = engine::download_model(move |percent| {
        let _ = progress_app.emit("diarization-download-progress", DownloadProgressEvent { percent });
    })
    .await;
    match result {
        Ok(()) => {
            let report = engine::model_report();
            let _ = app.emit("diarization-download-complete", report.clone());
            Ok(report)
        }
        Err(e) => {
            let error = e.to_string();
            let _ = app.emit("diarization-download-failed", DownloadFailedEvent { error: error.clone() });
            Err(error)
        }
    }
}

#[command]
pub async fn diarization_cancel_download() -> Result<(), String> {
    engine::cancel_download();
    Ok(())
}

/// Open the folder the speaker model is read from, creating it first, so the file can be
/// put there by hand on a PC that cannot reach the download host.
#[command]
pub async fn diarization_open_model_folder() -> Result<(), String> {
    let dir = engine::model_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("Failed to create {}: {e}", dir.display()))?;
    let folder = dir.to_string_lossy().to_string();
    #[cfg(target_os = "windows")]
    let opener = "explorer";
    #[cfg(target_os = "macos")]
    let opener = "open";
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    let opener = "xdg-open";
    std::process::Command::new(opener)
        .arg(&folder)
        .spawn()
        .map_err(|e| format!("Failed to open {folder}: {e}"))?;
    Ok(())
}

#[command]
pub async fn diarization_delete_model() -> Result<ModelReport, String> {
    engine::delete_model().map_err(|e| e.to_string())?;
    Ok(engine::model_report())
}
