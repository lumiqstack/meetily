// yt-dlp sidecar discovery and on-demand provisioning.
//
// yt-dlp is used to turn an authenticated SharePoint/Stream "stream.aspx"
// recording page into a downloadable media file. It encapsulates the
// (Microsoft-specific, frequently changing) logic for locating the video
// manifest behind a view-only recording, so we don't have to maintain that
// ourselves. ffmpeg (already bundled via `ffmpeg.rs`) does the muxing.
//
// Discovery mirrors `ffmpeg.rs::find_ffmpeg_path`: prefer a bundled binary
// next to the executable (Tauri `externalBin`), then PATH, then a copy cached
// in the app data dir, and finally download the latest release into that
// cache as a last resort.

use anyhow::{Context, Result};
use futures_util::StreamExt;
use log::{debug, info};
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Runtime};
use tokio::io::AsyncWriteExt;
use which::which;

#[cfg(windows)]
const EXECUTABLE_NAME: &str = "yt-dlp.exe";
#[cfg(not(windows))]
const EXECUTABLE_NAME: &str = "yt-dlp";

/// GitHub release asset name for the current platform.
#[cfg(windows)]
const RELEASE_ASSET: &str = "yt-dlp.exe";
#[cfg(target_os = "macos")]
const RELEASE_ASSET: &str = "yt-dlp_macos";
#[cfg(all(unix, not(target_os = "macos")))]
const RELEASE_ASSET: &str = "yt-dlp_linux";

const RELEASE_URL_BASE: &str = "https://github.com/yt-dlp/yt-dlp/releases/latest/download";

/// Locate a usable yt-dlp binary, downloading one into the app data dir if
/// none is found. Network access is only used as a last resort.
pub async fn ensure_ytdlp<R: Runtime>(app: &AppHandle<R>) -> Result<PathBuf> {
    if let Some(path) = find_existing(app) {
        debug!("Using yt-dlp at {:?}", path);
        return Ok(path);
    }

    info!("yt-dlp not found locally; downloading latest release");
    let dest = cache_path(app)?;
    download_ytdlp(&dest)
        .await
        .context("Failed to download yt-dlp. A network connection is required the first time you import from a link.")?;
    Ok(dest)
}

/// Search the bundled location, PATH, and app-data cache. Never touches the
/// network.
fn find_existing<R: Runtime>(app: &AppHandle<R>) -> Option<PathBuf> {
    // 1. Bundled next to the executable (Tauri externalBin / resources).
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let bundled = dir.join(EXECUTABLE_NAME);
            if bundled.is_file() {
                return Some(bundled);
            }
        }
    }

    // 2. On PATH (developer machines, corporate images that ship it).
    if let Ok(path) = which(EXECUTABLE_NAME) {
        return Some(path);
    }

    // 3. Previously downloaded into the app data dir.
    if let Ok(cached) = cache_path(app) {
        if cached.is_file() {
            return Some(cached);
        }
    }

    None
}

/// `<data_root>/bin/yt-dlp[.exe]`
fn cache_path<R: Runtime>(_app: &AppHandle<R>) -> Result<PathBuf> {
    Ok(crate::storage::bin_dir().join(EXECUTABLE_NAME))
}

async fn download_ytdlp(dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }

    let url = format!("{RELEASE_URL_BASE}/{RELEASE_ASSET}");
    debug!("Downloading yt-dlp from {url}");

    let client = reqwest::Client::builder()
        .build()
        .context("Failed to build HTTP client")?;
    let response = client
        .get(&url)
        .send()
        .await
        .context("Request for yt-dlp release failed")?
        .error_for_status()
        .context("yt-dlp release download returned an error status")?;

    // Stream to a temporary file, then atomically move into place so a partial
    // download never looks like a valid binary.
    let tmp = dest.with_extension("download");
    let mut file = tokio::fs::File::create(&tmp)
        .await
        .with_context(|| format!("Failed to create {}", tmp.display()))?;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("Error while downloading yt-dlp")?;
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    drop(file);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = tokio::fs::metadata(&tmp).await?.permissions();
        perms.set_mode(0o755);
        tokio::fs::set_permissions(&tmp, perms).await?;
    }

    tokio::fs::rename(&tmp, dest)
        .await
        .with_context(|| format!("Failed to move yt-dlp into {}", dest.display()))?;

    info!("yt-dlp downloaded to {:?}", dest);
    Ok(())
}
