// Model manager for built-in AI models - handles downloads and lifecycle
// Follows the same pattern as whisper_engine/whisper_engine.rs for consistency

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use anyhow::{anyhow, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::fs::{self, OpenOptions};
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::sync::RwLock;
use tokio::time::timeout;

use super::models::{get_available_models, get_model_by_name, ModelDef};

/// Longest silence from the server (awaiting headers or between body chunks)
/// before a download is treated as stalled.
const DOWNLOAD_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

// ============================================================================
// Model Status Types
// ============================================================================

/// Detailed download progress info (MB-based with speed)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadProgress {
    /// Bytes downloaded so far
    pub downloaded_bytes: u64,
    /// Total file size in bytes
    pub total_bytes: u64,
    /// Downloaded in MB (for display)
    pub downloaded_mb: f64,
    /// Total size in MB (for display)
    pub total_mb: f64,
    /// Download speed in MB/s
    pub speed_mbps: f64,
    /// Percentage complete (0-100)
    pub percent: u8,
}

impl DownloadProgress {
    pub fn new(downloaded: u64, total: u64, speed_mbps: f64) -> Self {
        let percent = if total > 0 {
            ((downloaded as f64 / total as f64) * 100.0) as u8
        } else {
            0
        };
        Self {
            downloaded_bytes: downloaded,
            total_bytes: total,
            downloaded_mb: downloaded as f64 / (1024.0 * 1024.0),
            total_mb: total as f64 / (1024.0 * 1024.0),
            speed_mbps,
            percent,
        }
    }
}

/// Model status in the system
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModelStatus {
    /// Model is not yet downloaded
    NotDownloaded,

    /// Model is currently being downloaded (progress 0-100)
    Downloading { progress: u8 },

    /// Model is downloaded and ready to use
    Available,

    /// Model file is corrupted and needs redownload
    Corrupted { file_size: u64, expected_min_size: u64 },

    /// Error occurred with the model
    Error(String),
}

/// Model information for UI display
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    /// Model name (e.g., "gemma3:1b")
    pub name: String,

    /// Display name for UI
    pub display_name: String,

    /// Current status
    pub status: ModelStatus,

    /// File path (if available)
    pub path: PathBuf,

    /// Size in MB
    pub size_mb: u64,

    /// Context window size in tokens
    pub context_size: u32,

    /// Description
    pub description: String,

    /// GGUF filename on disk
    pub gguf_file: String,
}

// ============================================================================
// Model Manager
// ============================================================================

/// A model's entry in `active_downloads`, removed on drop so that every way out of a
/// download, `?` included, frees the model for a retry.
struct ActiveDownload {
    downloads: Arc<Mutex<HashSet<String>>>,
    model_name: String,
}

impl ActiveDownload {
    /// Returns `None` when a download of `model_name` is already running.
    fn claim(downloads: &Arc<Mutex<HashSet<String>>>, model_name: &str) -> Option<Self> {
        let inserted = downloads
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(model_name.to_string());
        inserted.then(|| Self {
            downloads: Arc::clone(downloads),
            model_name: model_name.to_string(),
        })
    }
}

impl Drop for ActiveDownload {
    fn drop(&mut self) {
        self.downloads
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.model_name);
    }
}

pub struct ModelManager {
    /// Directory where models are stored
    models_dir: PathBuf,

    /// Currently available models with their status
    available_models: Arc<RwLock<HashMap<String, ModelInfo>>>,

    /// Active downloads (model names)
    active_downloads: Arc<Mutex<HashSet<String>>>,

    /// Cancellation flag for current download
    cancel_download_flag: Arc<RwLock<Option<String>>>,
}

impl ModelManager {
    /// Create a new model manager with default models directory
    pub fn new() -> Result<Self> {
        Self::new_with_models_dir(None)
    }

    /// Create a new model manager with custom models directory
    pub fn new_with_models_dir(models_dir: Option<PathBuf>) -> Result<Self> {
        let models_dir = if let Some(dir) = models_dir {
            dir
        } else {
            // Fallback: Use current directory in development
            let current_dir = std::env::current_dir()
                .map_err(|e| anyhow!("Failed to get current directory: {}", e))?;

            if cfg!(debug_assertions) {
                // Development mode
                current_dir.join("models").join("summary")
            } else {
                // Production mode fallback (caller should provide path)
                log::warn!("ModelManager: No models directory provided, using fallback path");
                dirs::data_dir()
                    .or_else(|| dirs::home_dir())
                    .ok_or_else(|| anyhow!("Could not find system data directory"))?
                    .join("Meetily")
                    .join("models")
                    .join("summary")
            }
        };

        log::info!(
            "Built-in AI ModelManager using directory: {}",
            models_dir.display()
        );

        Ok(Self {
            models_dir,
            available_models: Arc::new(RwLock::new(HashMap::new())),
            active_downloads: Arc::new(Mutex::new(HashSet::new())),
            cancel_download_flag: Arc::new(RwLock::new(None)),
        })
    }

    /// Initialize and scan for existing models
    pub async fn init(&self) -> Result<()> {
        // Create models directory if it doesn't exist
        if !self.models_dir.exists() {
            fs::create_dir_all(&self.models_dir).await?;
            log::info!("Created models directory: {}", self.models_dir.display());
        }

        // Scan for existing models
        self.scan_models().await?;

        Ok(())
    }

    /// Scan models directory and update status
    pub async fn scan_models(&self) -> Result<()> {
        let start = std::time::Instant::now();

        log::info!(
            "Starting model scan in directory: {}",
            self.models_dir.display()
        );

        let model_defs = get_available_models();
        let mut models_map = HashMap::new();

        for model_def in model_defs {
            let model_path = self.models_dir.join(&model_def.gguf_file);
            log::debug!(
                "Checking model '{}' at path: {}",
                model_def.name,
                model_path.display()
            );

            let is_actively_downloading = self
                .active_downloads
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .contains(&model_def.name);

            // If actively downloading, preserve existing status from memory
            if is_actively_downloading {
                let existing_info = {
                    let models = self.available_models.read().await;
                    models.get(&model_def.name).cloned()
                };

                if let Some(info) = existing_info {
                    // Preserve existing status (should be Downloading)
                    models_map.insert(model_def.name.clone(), info);
                    log::debug!(
                        "Model '{}': Preserving Downloading status during scan",
                        model_def.name
                    );
                    continue;
                }
            }

            let status = if model_path.exists() {
                // Check if file size matches expected size (basic validation)
                match fs::metadata(&model_path).await {
                    Ok(metadata) => {
                        let file_size_mb = metadata.len() / (1024 * 1024);

                        // Allow 10% variance for file size check
                        let expected_min = (model_def.size_mb as f64 * 0.9) as u64;
                        let expected_max = (model_def.size_mb as f64 * 1.1) as u64;

                        log::info!(
                            "Model '{}': found {} MB (expected {}-{} MB)",
                            model_def.name,
                            file_size_mb,
                            expected_min,
                            expected_max
                        );

                        if file_size_mb >= expected_min && file_size_mb <= expected_max {
                            log::info!("Model '{}': AVAILABLE", model_def.name);
                            ModelStatus::Available
                        } else {
                            log::warn!(
                                "Model '{}': CORRUPTED (size mismatch: {} MB, expected {} MB)",
                                model_def.name,
                                file_size_mb,
                                model_def.size_mb
                            );
                            ModelStatus::Corrupted {
                                file_size: file_size_mb,
                                expected_min_size: expected_min,
                            }
                        }
                    }
                    Err(e) => {
                        log::error!(
                            "Model '{}': Failed to read metadata: {}",
                            model_def.name,
                            e
                        );
                        ModelStatus::Error(format!("Failed to read metadata: {}", e))
                    }
                }
            } else {
                log::debug!("Model '{}': NOT FOUND", model_def.name);
                ModelStatus::NotDownloaded
            };

            let model_info = ModelInfo {
                name: model_def.name.clone(),
                display_name: model_def.display_name.clone(),
                status,
                path: model_path,
                size_mb: model_def.size_mb,
                context_size: model_def.context_size,
                description: model_def.description.clone(),
                gguf_file: model_def.gguf_file.clone(),
            };

            models_map.insert(model_def.name.clone(), model_info);
        }

        let model_count = models_map.len();

        let mut models = self.available_models.write().await;
        *models = models_map;

        let elapsed = start.elapsed();
        log::info!(
            "Model scan complete: {} models checked in {:?}",
            model_count,
            elapsed
        );
        Ok(())
    }

    /// Get list of all models with their status
    pub async fn list_models(&self) -> Vec<ModelInfo> {
        self.available_models
            .read()
            .await
            .values()
            .cloned()
            .collect()
    }

    /// Get info for a specific model
    pub async fn get_model_info(&self, model_name: &str) -> Option<ModelInfo> {
        self.available_models
            .read()
            .await
            .get(model_name)
            .cloned()
    }

    /// Check if a model is ready to use
    /// If refresh=true, scans filesystem before checking (slower but accurate)
    pub async fn is_model_ready(&self, model_name: &str, refresh: bool) -> bool {
        if refresh {
            if let Err(e) = self.scan_models().await {
                log::error!("Failed to scan models: {}", e);
                return false;
            }
        }

        if let Some(info) = self.get_model_info(model_name).await {
            info.status == ModelStatus::Available
        } else {
            false
        }
    }

    /// Download a model with simple percentage callback (backward compatible)
    pub async fn download_model(
        &self,
        model_name: &str,
        progress_callback: Option<Box<dyn Fn(u8) + Send>>,
    ) -> Result<()> {
        // Wrap the simple callback to use detailed progress internally
        let detailed_callback: Option<Box<dyn Fn(DownloadProgress) + Send>> =
            progress_callback.map(|cb| {
                Box::new(move |p: DownloadProgress| cb(p.percent)) as Box<dyn Fn(DownloadProgress) + Send>
            });
        self.download_model_detailed(model_name, detailed_callback).await
    }

    /// Download a model with detailed progress (MB, speed, etc.)
    pub async fn download_model_detailed(
        &self,
        model_name: &str,
        progress_callback: Option<Box<dyn Fn(DownloadProgress) + Send>>,
    ) -> Result<()> {
        let model_def = get_model_by_name(model_name)
            .ok_or_else(|| anyhow!("Unknown model: {}", model_name))?;
        self.download_model_def(&model_def, progress_callback, DOWNLOAD_IDLE_TIMEOUT)
            .await
    }

    /// Downloads `model_def`, failing if the server goes quiet for `idle_timeout`.
    /// A total request timeout would bound the whole body, which fails any download
    /// longer than that limit, so only connection setup and per-chunk silence are bounded.
    async fn download_model_def(
        &self,
        model_def: &ModelDef,
        progress_callback: Option<Box<dyn Fn(DownloadProgress) + Send>>,
        idle_timeout: Duration,
    ) -> Result<()> {
        let model_name = model_def.name.as_str();
        log::info!("Starting download for model: {}", model_name);

        let Some(_active) = ActiveDownload::claim(&self.active_downloads, model_name) else {
            log::warn!("Download already in progress for model: {}", model_name);
            return Err(anyhow!("Download already in progress"));
        };

        let result = self
            .download_claimed_model(model_def, progress_callback, idle_timeout)
            .await;

        if let Err(e) = &result {
            let mut models = self.available_models.write().await;
            if let Some(model_info) = models.get_mut(model_name) {
                // Failures that already set a status (cancel, friendlier error text) keep it.
                if matches!(model_info.status, ModelStatus::Downloading { .. }) {
                    model_info.status = ModelStatus::Error(e.to_string());
                }
            }
        }

        result
    }

    /// Body of `download_model_def`, run while the caller holds the model's `ActiveDownload`.
    async fn download_claimed_model(
        &self,
        model_def: &ModelDef,
        progress_callback: Option<Box<dyn Fn(DownloadProgress) + Send>>,
        idle_timeout: Duration,
    ) -> Result<()> {
        let model_name = model_def.name.as_str();

        // Clear cancellation flag
        {
            let mut cancel_flag = self.cancel_download_flag.write().await;
            *cancel_flag = None;
        }

        // Update status to downloading
        {
            let mut models = self.available_models.write().await;
            if let Some(model_info) = models.get_mut(model_name) {
                model_info.status = ModelStatus::Downloading { progress: 0 };
            }
        }

        let file_path = self.models_dir.join(&model_def.gguf_file);
        // Bytes land here and only move to `file_path` once the whole body has arrived,
        // so a file under the final name is always a finished download.
        let part_path = self.models_dir.join(format!("{}.part", model_def.gguf_file));

        // Check if model already exists and is valid (skip re-download)
        if file_path.exists() {
            if let Ok(metadata) = fs::metadata(&file_path).await {
                let file_size_mb = metadata.len() / (1024 * 1024);
                let expected_min = (model_def.size_mb as f64 * 0.9) as u64;
                let expected_max = (model_def.size_mb as f64 * 1.1) as u64;

                if file_size_mb >= expected_min && file_size_mb <= expected_max {
                    log::info!(
                        "Model '{}' already exists and is valid ({} MB), skipping download",
                        model_name,
                        file_size_mb
                    );

                    // Update status to available
                    {
                        let mut models = self.available_models.write().await;
                        if let Some(model_info) = models.get_mut(model_name) {
                            model_info.status = ModelStatus::Available;
                        }
                    }

                    // Report 100% progress
                    if let Some(ref callback) = progress_callback {
                        let total = metadata.len();
                        callback(DownloadProgress::new(total, total, 0.0));
                    }

                    return Ok(());
                } else if file_size_mb > expected_max {
                    // File is LARGER than expected - possibly corrupted or wrong file
                    // Delete and re-download in this case
                    log::warn!(
                        "Model '{}' exists but is too large ({} MB, expected max {} MB), deleting and re-downloading",
                        model_name,
                        file_size_mb,
                        expected_max
                    );
                    if let Err(e) = fs::remove_file(&file_path).await {
                        log::warn!("Failed to delete oversized model file: {}", e);
                    }
                } else {
                    log::info!(
                        "Model '{}' exists but is too small ({} MB, expected min {} MB), will replace it",
                        model_name,
                        file_size_mb,
                        expected_min
                    );
                }
            }
        }

        log::info!("Downloading from: {}", model_def.download_url);
        log::info!("Saving to: {}", part_path.display());

        // Create models directory if needed
        if !self.models_dir.exists() {
            fs::create_dir_all(&self.models_dir).await?;
        }

        // Check for existing partial download to resume
        let mut existing_size: u64 = if part_path.exists() {
            fs::metadata(&part_path)
                .await
                .map(|m| m.len())
                .unwrap_or(0)
        } else {
            0
        };

        // Download the file with optimized client settings
        let client = Client::builder()
            .tcp_nodelay(true) // Disable Nagle's algorithm for faster streaming
            .pool_max_idle_per_host(1) // Keep connection alive
            .connect_timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| anyhow!("Failed to create HTTP client: {}", e))?;

        // `None` means the .part already holds the whole file and there is no body to read.
        let response = loop {
            // Build request with Range header if resuming
            let mut request = client.get(&model_def.download_url);
            if existing_size > 0 {
                log::info!(
                    "Resuming download from byte {} ({:.1} MB)",
                    existing_size,
                    existing_size as f64 / (1024.0 * 1024.0)
                );
                request = request.header("Range", format!("bytes={}-", existing_size));
            }

            let response = match timeout(idle_timeout, request.send()).await {
                Err(_) => {
                    return Err(anyhow!("Download stalled - No response for {:?}", idle_timeout));
                }
                Ok(response) => response.map_err(|e| anyhow!("Failed to start download: {}", e))?,
            };

            // A request without Range always breaks here, so this retries at most once.
            if existing_size == 0
                || response.status() != reqwest::StatusCode::RANGE_NOT_SATISFIABLE
            {
                break Some(response);
            }
            let server_size = response
                .headers()
                .get(reqwest::header::CONTENT_RANGE)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.strip_prefix("bytes */"))
                .and_then(|total| total.parse::<u64>().ok());
            if server_size == Some(existing_size) {
                log::info!("Partial file already holds all {} bytes", existing_size);
                break None;
            }
            log::warn!(
                "Server cannot resume from byte {} (file size {:?}), restarting download",
                existing_size,
                server_size
            );
            fs::remove_file(&part_path)
                .await
                .map_err(|e| anyhow!("Failed to remove stale partial download: {}", e))?;
            existing_size = 0;
        };

        // Check response status - 200 OK (full download) or 206 Partial Content (resume)
        let (total_size, resuming) = match &response {
            None => (existing_size, true),
            Some(response) if response.status() == reqwest::StatusCode::PARTIAL_CONTENT => {
                // Server supports resume - total size = existing + remaining
                let remaining = response.content_length().unwrap_or(0);
                log::info!("Server supports resume, {} MB remaining", remaining / (1024 * 1024));
                (existing_size + remaining, true)
            }
            Some(response) if response.status().is_success() => {
                // Server doesn't support resume or fresh download
                if existing_size > 0 {
                    log::warn!("Server doesn't support resume, starting fresh download");
                }
                (response.content_length().unwrap_or(0), false)
            }
            Some(response) => {
                return Err(anyhow!("Download failed with status: {}", response.status()));
            }
        };

        log::info!("Total size: {} MB", total_size / (1024 * 1024));

        // Open file for append if resuming, or create new
        let file = if resuming {
            OpenOptions::new()
                .write(true)
                .append(true)
                .open(&part_path)
                .await
                .map_err(|e| anyhow!("Failed to open file for append: {}", e))?
        } else {
            fs::File::create(&part_path)
                .await
                .map_err(|e| anyhow!("Failed to create file: {}", e))?
        };

        // Use 8MB buffer to reduce disk I/O syscalls (major performance improvement)
        let mut writer = BufWriter::with_capacity(8 * 1024 * 1024, file);

        let mut downloaded: u64 = if resuming { existing_size } else { 0 };

        // Emit initial progress (showing resumed position if applicable)
        if let Some(ref callback) = progress_callback {
            callback(DownloadProgress::new(downloaded, total_size, 0.0));
        }
        log::info!(
            "Starting at {:.1} MB / {:.1} MB",
            downloaded as f64 / (1024.0 * 1024.0),
            total_size as f64 / (1024.0 * 1024.0)
        );

        let mut last_progress_percent = if total_size > 0 {
            ((downloaded as f64 / total_size as f64) * 100.0) as u8
        } else {
            0
        };
        let mut last_report_time = std::time::Instant::now();
        let mut bytes_since_last_report: u64 = 0;
        let download_start_time = std::time::Instant::now();
        let start_downloaded = downloaded;

        use futures_util::StreamExt;
        let mut stream = match response {
            Some(response) => response.bytes_stream().boxed(),
            None => futures_util::stream::empty().boxed(),
        };

        loop {
            // Check for cancellation
            {
                let cancel_flag = self.cancel_download_flag.read().await;
                if cancel_flag.as_ref() == Some(&model_name.to_string()) {
                    log::info!("Download cancelled for model: {}", model_name);

                    // Flush and keep partial file for resume on next attempt
                    let _ = writer.flush().await;
                    drop(writer);

                    // Update status
                    {
                        let mut models = self.available_models.write().await;
                        if let Some(model_info) = models.get_mut(model_name) {
                            model_info.status = ModelStatus::NotDownloaded;
                        }
                    }

                    // Use special marker prefix to distinguish cancellation from other errors
                    return Err(anyhow!("CANCELLED: Download cancelled by user"));
                }
            }

            // Per-chunk timeout to detect stalled connections
            let next_result = timeout(idle_timeout, stream.next()).await;

            let chunk = match next_result {
                // Timeout - no data received within the idle limit
                Err(_) => {
                    let message = format!("Download stalled - No data received for {:?}", idle_timeout);
                    log::warn!("{} ({})", message, model_name);
                    let _ = writer.flush().await;
                    return Err(anyhow!(message));
                },
                // Stream ended
                Ok(None) => break,
                // Got chunk result
                Ok(Some(chunk_result)) => {
                    match chunk_result {
                        Ok(c) => c,
                        // Detect error type for better user feedback
                        Err(e) => {
                            log::error!("Download error for {}: {:?}", model_name, e);
                            let _ = writer.flush().await;

                            // Categorize error for user-friendly message
                            let error_msg = if e.is_timeout() {
                                "Connection timeout - Check your internet"
                            } else if e.is_connect() {
                                "Connection failed - Check your internet"
                            } else if e.is_body() {
                                "Stream interrupted - Network unstable"
                            } else {
                                "Download error"
                            };

                            // Set model status to Error (NOT NotDownloaded) so UI can show retry button
                            {
                                let mut models = self.available_models.write().await;
                                if let Some(model_info) = models.get_mut(model_name) {
                                    model_info.status = ModelStatus::Error(error_msg.to_string());
                                }
                            }

                            return Err(anyhow!("{}: {}", error_msg, e));
                        }
                    }
                }
            };
            let chunk_len = chunk.len() as u64;
            writer
                .write_all(&chunk)
                .await
                .map_err(|e| anyhow!("Error writing to file: {}", e))?;

            downloaded += chunk_len;
            bytes_since_last_report += chunk_len;

            // Calculate progress
            let progress_percent = if total_size > 0 {
                let exact_percent = (downloaded as f64 / total_size as f64) * 100.0;
                exact_percent.min(100.0) as u8
            } else {
                0
            };

            let elapsed_since_report = last_report_time.elapsed();
            let is_download_complete = downloaded >= total_size;
            let should_report = progress_percent > last_progress_percent
                || is_download_complete  // Force report on completion
                || elapsed_since_report.as_millis() >= 500;

            if should_report {
                // Calculate speed based on bytes downloaded since last report
                let speed_mbps = if elapsed_since_report.as_secs_f64() > 0.0 {
                    (bytes_since_last_report as f64 / (1024.0 * 1024.0)) / elapsed_since_report.as_secs_f64()
                } else {
                    // Fallback to overall average speed
                    let total_elapsed = download_start_time.elapsed().as_secs_f64();
                    if total_elapsed > 0.0 {
                        ((downloaded - start_downloaded) as f64 / (1024.0 * 1024.0)) / total_elapsed
                    } else {
                        0.0
                    }
                };

                log::info!(
                    "Download: {:.1} MB / {:.1} MB ({:.1} MB/s)",
                    downloaded as f64 / (1024.0 * 1024.0),
                    total_size as f64 / (1024.0 * 1024.0),
                    speed_mbps
                );

                // Update status
                {
                    let mut models = self.available_models.write().await;
                    if let Some(model_info) = models.get_mut(model_name) {
                        model_info.status = ModelStatus::Downloading {
                            progress: if is_download_complete { 100 } else { progress_percent }
                        };
                    }
                }

                // Call progress callback with detailed info
                if let Some(ref callback) = progress_callback {
                    callback(DownloadProgress::new(downloaded, total_size, speed_mbps));
                }

                last_progress_percent = progress_percent;
                last_report_time = std::time::Instant::now();
                bytes_since_last_report = 0;
            }
        }

        writer.flush().await?;
        drop(writer);

        if total_size > 0 && downloaded != total_size {
            let message = format!(
                "Download incomplete - received {} of {} bytes",
                downloaded, total_size
            );
            log::warn!("{} ({})", message, model_name);
            return Err(anyhow!(message));
        }

        log::info!("Download completed for model: {}", model_name);

        {
            let mut models = self.available_models.write().await;
            if let Some(model_info) = models.get_mut(model_name) {
                model_info.status = ModelStatus::Downloading { progress: 100 };
            }
        }

        if let Some(ref callback) = progress_callback {
            callback(DownloadProgress::new(total_size, total_size, 0.0));
        }

        // Small delay to ensure UI receives 100% event
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        if let Err(e) = self.validate_gguf_file(&part_path).await {
            log::error!("Downloaded file failed validation: {}", e);

            // Clean up invalid file
            let _ = fs::remove_file(&part_path).await;

            return Err(anyhow!("File validation failed: {}", e));
        }

        fs::rename(&part_path, &file_path)
            .await
            .map_err(|e| anyhow!("Failed to move downloaded model into place: {}", e))?;

        // Update status to available
        {
            let mut models = self.available_models.write().await;
            if let Some(model_info) = models.get_mut(model_name) {
                model_info.status = ModelStatus::Available;
                model_info.path = file_path.clone();
            }
        }

        Ok(())
    }

    /// Validate that a file is a valid GGUF model
    async fn validate_gguf_file(&self, path: &PathBuf) -> Result<()> {
        let mut file = fs::File::open(path).await?;

        // Read first 4 bytes to check for GGUF magic number
        use tokio::io::AsyncReadExt;
        let mut magic = [0u8; 4];
        file.read_exact(&mut magic).await?;

        // GGUF magic number is "GGUF" (0x47475546)
        if &magic == b"GGUF" {
            Ok(())
        } else if &magic == b"ggjt" || &magic == b"ggla" || &magic == b"ggml" {
            // Older formats (GGML, GGJT)
            Ok(())
        } else {
            Err(anyhow!(
                "Invalid model file: magic number {:?} doesn't match GGUF/GGML",
                magic
            ))
        }
    }

    /// Cancel an ongoing download
    pub async fn cancel_download(&self, model_name: &str) -> Result<()> {
        log::info!("Cancelling download for model: {}", model_name);

        // Set cancellation flag - download loop will detect this and handle cleanup
        {
            let mut cancel_flag = self.cancel_download_flag.write().await;
            *cancel_flag = Some(model_name.to_string());
        }

        // Note: active_downloads cleanup is handled by the download loop when it detects
        // the cancellation flag. This avoids double-removal race condition.

        // Update status immediately for UI responsiveness
        {
            let mut models = self.available_models.write().await;
            if let Some(model_info) = models.get_mut(model_name) {
                model_info.status = ModelStatus::NotDownloaded;
            }
        }

        // Brief delay to let download loop detect cancellation
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        Ok(())
    }

    /// Delete a corrupted or available model file
    pub async fn delete_model(&self, model_name: &str) -> Result<()> {
        log::info!("Deleting model: {}", model_name);

        let model_def = get_model_by_name(model_name)
            .ok_or_else(|| anyhow!("Unknown model: {}", model_name))?;

        let file_path = self.models_dir.join(&model_def.gguf_file);
        let part_path = self.models_dir.join(format!("{}.part", model_def.gguf_file));

        for path in [&file_path, &part_path] {
            if path.exists() {
                fs::remove_file(path).await?;
                log::info!("Deleted model file: {}", path.display());
            }
        }

        // Update status
        {
            let mut models = self.available_models.write().await;
            if let Some(model_info) = models.get_mut(model_name) {
                model_info.status = ModelStatus::NotDownloaded;
            }
        }

        Ok(())
    }

    /// Get models directory path
    pub fn get_models_directory(&self) -> PathBuf {
        self.models_dir.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    const IDLE_LIMIT: Duration = Duration::from_millis(500);

    /// Answers one request with `head` followed by each piece after its delay,
    /// and yields the raw request it received.
    async fn serve(
        head: String,
        pieces: Vec<(Duration, &'static [u8])>,
    ) -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = socket.read(&mut buffer).await.unwrap();
                assert_ne!(read, 0, "request ended before its headers");
                request.extend_from_slice(&buffer[..read]);
            }
            let _ = socket.write_all(head.as_bytes()).await;
            for (delay, piece) in pieces {
                tokio::time::sleep(delay).await;
                let _ = socket.write_all(piece).await;
                let _ = socket.flush().await;
            }
            String::from_utf8_lossy(&request).to_string()
        });
        (format!("http://{address}/model.gguf"), handle)
    }

    /// Answers one request per connection with each full response in turn,
    /// and yields the raw requests it received.
    async fn serve_each(responses: Vec<String>) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let mut requests = Vec::new();
            for response in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut buffer = [0u8; 1024];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    let read = socket.read(&mut buffer).await.unwrap();
                    assert_ne!(read, 0, "request ended before its headers");
                    request.extend_from_slice(&buffer[..read]);
                }
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
                requests.push(String::from_utf8_lossy(&request).to_ascii_lowercase());
            }
            requests
        });
        (format!("http://{address}/model.gguf"), handle)
    }

    /// Serves one GGUF body with its full Content-Length up front, writing each
    /// piece after its delay, so the body spans the sum of the delays.
    async fn serve_drip(pieces: Vec<(Duration, &'static [u8])>) -> String {
        let length: usize = pieces.iter().map(|(_, piece)| piece.len()).sum();
        let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n");
        serve(head, pieces).await.0
    }

    fn test_model(download_url: String) -> ModelDef {
        let mut model = get_available_models().into_iter().next().unwrap();
        model.gguf_file = "drip-test.gguf".to_string();
        model.download_url = download_url;
        model
    }

    #[tokio::test]
    async fn download_survives_slow_stream_longer_than_idle_limit() {
        let models_dir = tempfile::tempdir().unwrap();
        let manager = ModelManager::new_with_models_dir(Some(models_dir.path().to_path_buf())).unwrap();
        let url = serve_drip(vec![
            (Duration::ZERO, b"GG"),
            (Duration::from_millis(200), b"UF"),
            (Duration::from_millis(200), b"-body"),
            (Duration::from_millis(200), b"-end"),
        ])
        .await;
        manager
            .download_model_def(&test_model(url), None, IDLE_LIMIT)
            .await
            .expect("a steady stream whose gaps stay under the idle limit must succeed");
        assert_eq!(
            fs::read(models_dir.path().join("drip-test.gguf")).await.unwrap(),
            b"GGUF-body-end"
        );
    }

    #[tokio::test]
    async fn download_fails_when_stream_stalls_past_idle_limit() {
        let models_dir = tempfile::tempdir().unwrap();
        let manager = ModelManager::new_with_models_dir(Some(models_dir.path().to_path_buf())).unwrap();
        let url = serve_drip(vec![
            (Duration::ZERO, b"GGUF"),
            (Duration::from_millis(1500), b"-end"),
        ])
        .await;
        let error = manager
            .download_model_def(&test_model(url), None, IDLE_LIMIT)
            .await
            .expect_err("a stream that stalls past the idle limit must fail");
        assert!(error.to_string().contains("stalled"), "unexpected error: {error}");
    }

    #[tokio::test]
    async fn partial_part_file_is_not_reported_available() {
        let models_dir = tempfile::tempdir().unwrap();
        let manager = ModelManager::new_with_models_dir(Some(models_dir.path().to_path_buf())).unwrap();
        let model = get_available_models().into_iter().next().unwrap();
        let part_path = models_dir.path().join(format!("{}.part", model.gguf_file));
        let mut part = std::fs::File::create(&part_path).unwrap();
        std::io::Write::write_all(&mut part, b"GGUF").unwrap();
        part.set_len(model.size_mb * 1024 * 1024 * 95 / 100).unwrap();

        manager.scan_models().await.unwrap();

        let status = manager.get_model_info(&model.name).await.unwrap().status;
        assert_ne!(status, ModelStatus::Available);
    }

    #[tokio::test]
    async fn interrupted_download_stays_incomplete_and_resumes_on_retry() {
        let models_dir = tempfile::tempdir().unwrap();
        let manager = ModelManager::new_with_models_dir(Some(models_dir.path().to_path_buf())).unwrap();
        let final_path = models_dir.path().join("resume-test.gguf");

        let (url, _) = serve(
            "HTTP/1.1 200 OK\r\nContent-Length: 20\r\nConnection: close\r\n\r\n".to_string(),
            vec![
                (Duration::ZERO, b"GGUF-partial"),
                (Duration::from_millis(1500), b"-the-end"),
            ],
        )
        .await;
        let mut model = test_model(url);
        model.gguf_file = "resume-test.gguf".to_string();
        // Small enough that the 12 bytes on disk fall inside the size tolerance.
        model.size_mb = 1;
        manager
            .download_model_def(&model, None, IDLE_LIMIT)
            .await
            .expect_err("a stalled download must fail");
        assert!(
            !final_path.exists(),
            "an interrupted download must not leave a file under the final model name"
        );

        let (url, request) = serve(
            "HTTP/1.1 206 Partial Content\r\nContent-Length: 8\r\nConnection: close\r\n\r\n".to_string(),
            vec![(Duration::ZERO, b"-the-end")],
        )
        .await;
        model.download_url = url;
        manager
            .download_model_def(&model, None, IDLE_LIMIT)
            .await
            .expect("retry must resume the partial download");
        let request = request.await.unwrap().to_ascii_lowercase();
        assert!(request.contains("range: bytes=12-"), "retry did not resume: {request}");
        assert_eq!(fs::read(&final_path).await.unwrap(), b"GGUF-partial-the-end");
    }

    #[tokio::test]
    async fn complete_part_file_answered_416_is_moved_into_place() {
        let models_dir = tempfile::tempdir().unwrap();
        let manager = ModelManager::new_with_models_dir(Some(models_dir.path().to_path_buf())).unwrap();
        manager.scan_models().await.unwrap();
        let mut model = test_model(String::new());
        model.size_mb = 1;
        let part_path = models_dir.path().join(format!("{}.part", model.gguf_file));
        std::fs::write(&part_path, b"GGUF-partial-the-end").unwrap();
        let (url, requests) = serve_each(vec![
            "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */20\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
        ])
        .await;
        model.download_url = url;

        manager
            .download_model_def(&model, None, IDLE_LIMIT)
            .await
            .expect("a .part already holding the whole file must complete");

        assert!(requests.await.unwrap()[0].contains("range: bytes=20-"));
        assert_eq!(
            fs::read(models_dir.path().join(&model.gguf_file)).await.unwrap(),
            b"GGUF-partial-the-end"
        );
        assert!(!part_path.exists());
        assert_eq!(
            manager.get_model_info(&model.name).await.unwrap().status,
            ModelStatus::Available
        );
    }

    #[tokio::test]
    async fn stale_part_file_answered_416_restarts_fresh() {
        let models_dir = tempfile::tempdir().unwrap();
        let manager = ModelManager::new_with_models_dir(Some(models_dir.path().to_path_buf())).unwrap();
        manager.scan_models().await.unwrap();
        let mut model = test_model(String::new());
        model.size_mb = 1;
        let part_path = models_dir.path().join(format!("{}.part", model.gguf_file));
        std::fs::write(&part_path, b"GGUF-partial").unwrap();
        let (url, requests) = serve_each(vec![
            "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */8\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
            "HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\nGGUF-new".to_string(),
        ])
        .await;
        model.download_url = url;

        manager
            .download_model_def(&model, None, IDLE_LIMIT)
            .await
            .expect("a .part the server cannot extend must be replaced by a fresh download");

        let requests = requests.await.unwrap();
        assert!(requests[0].contains("range: bytes=12-"), "first request: {}", requests[0]);
        assert!(!requests[1].contains("range:"), "fresh retry must not send Range: {}", requests[1]);
        assert_eq!(
            fs::read(models_dir.path().join(&model.gguf_file)).await.unwrap(),
            b"GGUF-new"
        );
        assert!(!part_path.exists());
    }

    #[tokio::test]
    async fn failed_download_start_does_not_block_retry() {
        let models_dir = tempfile::tempdir().unwrap();
        let manager = ModelManager::new_with_models_dir(Some(models_dir.path().to_path_buf())).unwrap();
        manager.scan_models().await.unwrap();
        let model = test_model("http://127.0.0.1:1/model.gguf".to_string());

        let first = manager
            .download_model_def(&model, None, IDLE_LIMIT)
            .await
            .expect_err("an unreachable server must fail the download");
        assert!(
            !first.to_string().contains("already in progress"),
            "unexpected error: {first}"
        );

        let second = manager
            .download_model_def(&model, None, IDLE_LIMIT)
            .await
            .expect_err("an unreachable server must fail the download");
        assert!(
            !second.to_string().contains("already in progress"),
            "retry was blocked by the failed attempt: {second}"
        );
        assert!(matches!(
            manager.get_model_info(&model.name).await.unwrap().status,
            ModelStatus::Error(_)
        ));
    }

    #[tokio::test]
    async fn rejected_download_is_marked_failed_and_can_be_retried() {
        let models_dir = tempfile::tempdir().unwrap();
        let manager = ModelManager::new_with_models_dir(Some(models_dir.path().to_path_buf())).unwrap();
        manager.scan_models().await.unwrap();
        let (url, _) = serve(
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
            vec![],
        )
        .await;
        let mut model = test_model(url);

        manager
            .download_model_def(&model, None, IDLE_LIMIT)
            .await
            .expect_err("a 404 must fail the download");
        assert!(matches!(
            manager.get_model_info(&model.name).await.unwrap().status,
            ModelStatus::Error(_)
        ));

        model.download_url = "http://127.0.0.1:1/model.gguf".to_string();
        let retry = manager
            .download_model_def(&model, None, IDLE_LIMIT)
            .await
            .expect_err("an unreachable server must fail the download");
        assert!(
            !retry.to_string().contains("already in progress"),
            "retry was blocked by the failed attempt: {retry}"
        );
    }
}
