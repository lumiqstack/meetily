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

#[command]
pub async fn diarization_delete_model() -> Result<ModelReport, String> {
    engine::delete_model().map_err(|e| e.to_string())?;
    Ok(engine::model_report())
}
