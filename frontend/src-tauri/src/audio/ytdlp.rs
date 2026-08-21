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
// in the app data dir, and finally download a release into that cache as a
// last resort. The download is pinned to one immutable release tag, bounded in
// time and size, and verified against that release's SHA2-256SUMS before it
// becomes runnable. A cached copy is never refreshed automatically: bundled
// and PATH binaries outrank it, so an update policy needs its own design.

use anyhow::{anyhow, Context, Result};
use futures_util::StreamExt;
use log::{debug, info};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tauri::{AppHandle, Manager, Runtime};
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

/// Redirects to `.../releases/tag/<version>`, which pins the version.
const LATEST_RELEASE_URL: &str = "https://github.com/yt-dlp/yt-dlp/releases/latest";
/// `<base>/<version>/<asset>` and `<base>/<version>/SHA2-256SUMS`.
const RELEASE_DOWNLOAD_BASE: &str = "https://github.com/yt-dlp/yt-dlp/releases/download";
const CHECKSUMS_ASSET: &str = "SHA2-256SUMS";

/// Time and size bounds for provisioning. The release binaries are tens of
/// MB; anything far larger is not the file we asked for.
#[derive(Clone, Copy)]
struct DownloadLimits {
    connect: Duration,
    /// Longest gap between received chunks before the download is a stall.
    idle: Duration,
    total: Duration,
    max_bytes: u64,
}

const DOWNLOAD_LIMITS: DownloadLimits = DownloadLimits {
    connect: Duration::from_secs(15),
    idle: Duration::from_secs(30),
    total: Duration::from_secs(5 * 60),
    max_bytes: 160 * 1024 * 1024,
};

/// Where a release is resolved and fetched from (overridable in tests).
struct ReleaseSource<'a> {
    latest_url: &'a str,
    download_base: &'a str,
    asset: &'a str,
}

const GITHUB_RELEASES: ReleaseSource<'static> = ReleaseSource {
    latest_url: LATEST_RELEASE_URL,
    download_base: RELEASE_DOWNLOAD_BASE,
    asset: RELEASE_ASSET,
};

/// Serializes provisioning so concurrent imports never race on the cache.
static PROVISION_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Locate a usable yt-dlp binary, downloading one into the app data dir if
/// none is found. Network access is only used as a last resort.
pub async fn ensure_ytdlp<R: Runtime>(app: &AppHandle<R>) -> Result<PathBuf> {
    if let Some(path) = find_existing(app) {
        debug!("Using yt-dlp at {:?}", path);
        return Ok(path);
    }

    let _provisioning = PROVISION_LOCK.lock().await;
    // Another import may have provisioned it while we waited for the lock.
    if let Some(path) = find_existing(app) {
        return Ok(path);
    }

    info!("yt-dlp not found locally; downloading a verified release");
    let dest = cache_path(app)?;
    download_ytdlp(&dest, &GITHUB_RELEASES, DOWNLOAD_LIMITS)
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

fn http_client(limits: DownloadLimits, follow_redirects: bool) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(limits.connect)
        .timeout(limits.total);
    if !follow_redirects {
        builder = builder.redirect(reqwest::redirect::Policy::none());
    }
    builder.build().context("Failed to build HTTP client")
}

/// Resolve the version the `latest` alias currently points to, so the asset
/// and its checksum list come from the same immutable release.
async fn resolve_release_tag(source: &ReleaseSource<'_>, limits: DownloadLimits) -> Result<String> {
    let response = http_client(limits, false)?
        .get(source.latest_url)
        .send()
        .await
        .context("Could not reach the yt-dlp release page")?;
    let location = response
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| anyhow!("yt-dlp release page did not redirect to a version (status {})", response.status()))?;
    let tag = location
        .trim_end_matches('/')
        .rsplit_once("/tag/")
        .map(|(_, tag)| tag)
        .ok_or_else(|| anyhow!("Unexpected yt-dlp release redirect"))?;
    if tag.is_empty()
        || !tag
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
    {
        return Err(anyhow!("Unexpected yt-dlp release version format"));
    }
    Ok(tag.to_string())
}

/// Stream `url` into `sink`, enforcing the idle/total/size limits.
async fn fetch_bounded(
    url: &str,
    limits: DownloadLimits,
    mut sink: impl FnMut(&[u8]) -> Result<()>,
) -> Result<u64> {
    let deadline = tokio::time::Instant::now() + limits.total;
    let response = http_client(limits, true)?
        .get(url)
        .send()
        .await
        .context("Request for yt-dlp release file failed")?
        .error_for_status()
        .context("yt-dlp release download returned an error status")?;
    if let Some(length) = response.content_length() {
        if length > limits.max_bytes {
            return Err(anyhow!("yt-dlp release file is unexpectedly large ({} bytes)", length));
        }
    }
    let expected = response.content_length();
    let mut stream = response.bytes_stream();
    let mut received = 0u64;
    loop {
        let wait = limits.idle.min(deadline.saturating_duration_since(tokio::time::Instant::now()));
        let next = match tokio::time::timeout(wait, stream.next()).await {
            Ok(next) => next,
            Err(_) if tokio::time::Instant::now() >= deadline => {
                return Err(anyhow!("yt-dlp download exceeded {}s", limits.total.as_secs()))
            }
            Err(_) => return Err(anyhow!("yt-dlp download stalled for {}s", limits.idle.as_secs())),
        };
        let Some(chunk) = next else { break };
        let chunk = chunk.context("Error while downloading yt-dlp")?;
        received += chunk.len() as u64;
        if received > limits.max_bytes {
            return Err(anyhow!("yt-dlp release file exceeded {} bytes", limits.max_bytes));
        }
        sink(&chunk)?;
    }
    if let Some(expected) = expected {
        if received != expected {
            return Err(anyhow!("yt-dlp download was truncated ({} of {} bytes)", received, expected));
        }
    }
    Ok(received)
}

/// Find the hex SHA-256 for exactly `asset` in a SHA2-256SUMS listing.
fn expected_sha256(sums: &str, asset: &str) -> Result<String> {
    for line in sums.lines() {
        let mut parts = line.split_whitespace();
        let (Some(hash), Some(name)) = (parts.next(), parts.next()) else {
            continue;
        };
        if name.trim_start_matches('*') == asset {
            if hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()) {
                return Ok(hash.to_ascii_lowercase());
            }
            return Err(anyhow!("Malformed checksum entry for {}", asset));
        }
    }
    Err(anyhow!("No checksum is published for {}", asset))
}

/// Download one pinned release of yt-dlp into `dest`, verifying its SHA-256
/// before it becomes runnable. The file is written to a unique temporary file
/// beside `dest` and only renamed into place after verification, so a failure
/// at any point leaves an existing working binary untouched and no partial
/// executable behind. (The checksum list arrives over the same HTTPS channel:
/// it detects corrupted or mismatched downloads, not a compromised release.)
async fn download_ytdlp(dest: &Path, source: &ReleaseSource<'_>, limits: DownloadLimits) -> Result<()> {
    let parent = dest
        .parent()
        .ok_or_else(|| anyhow!("Invalid yt-dlp cache path"))?;
    tokio::fs::create_dir_all(parent)
        .await
        .with_context(|| format!("Failed to create {}", parent.display()))?;

    let tag = resolve_release_tag(source, limits).await?;
    debug!("Provisioning yt-dlp release {}", tag);

    let mut sums = Vec::new();
    let sums_limits = DownloadLimits { max_bytes: 1024 * 1024, ..limits };
    fetch_bounded(
        &format!("{}/{}/{}", source.download_base, tag, CHECKSUMS_ASSET),
        sums_limits,
        |chunk| {
            sums.extend_from_slice(chunk);
            Ok(())
        },
    )
    .await
    .context("Could not download the yt-dlp checksum list")?;
    let expected = expected_sha256(&String::from_utf8_lossy(&sums), source.asset)?;

    let mut temp = tempfile::Builder::new()
        .prefix(".yt-dlp-download-")
        .tempfile_in(parent)
        .context("Failed to create a temporary download file")?;
    let mut hasher = Sha256::new();
    fetch_bounded(
        &format!("{}/{}/{}", source.download_base, tag, source.asset),
        limits,
        |chunk| {
            hasher.update(chunk);
            temp.write_all(chunk).context("Failed to write yt-dlp download")
        },
    )
    .await?;
    let actual = format!("{:x}", hasher.finalize());
    if actual != expected {
        return Err(anyhow!("yt-dlp download failed checksum verification"));
    }
    temp.as_file_mut().flush()?;
    temp.as_file().sync_all()?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temp.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o755))?;
    }

    // Atomic replace (MoveFileEx with replace-existing on Windows).
    temp.persist(dest)
        .map_err(|e| anyhow!("Failed to move yt-dlp into {}: {}", dest.display(), e.error))?;

    info!("yt-dlp {} downloaded and verified at {:?}", tag, dest);
    Ok(())
}

#[cfg(test)]
mod provisioning_tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    const ASSET: &str = "yt-dlp_test";
    const TAG: &str = "2026.10.01";

    #[derive(Clone)]
    enum Reply {
        Body(Vec<u8>),
        Status(u16),
        Redirect(String),
        /// Send headers announcing `len` bytes, then this prefix, then hang.
        Stall { len: usize, prefix: Vec<u8> },
        /// Send `chunks` one byte at a time, `gap` apart.
        Trickle { chunks: usize, gap: Duration },
        /// Announce `len` bytes but close after `body`.
        Truncated { len: usize, body: Vec<u8> },
    }

    /// Minimal HTTP/1.1 fixture server: one reply per path, Connection: close.
    async fn serve(routes: HashMap<String, Reply>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let routes = Arc::new(routes);
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else { return };
                let routes = routes.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let n = socket.read(&mut buf).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&buf[..n]).to_string();
                    let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
                    let reply = routes.get(&path).cloned().unwrap_or(Reply::Status(404));
                    let head = |status: &str, len: usize, extra: &str| {
                        format!("HTTP/1.1 {status}\r\nContent-Length: {len}\r\n{extra}Connection: close\r\n\r\n")
                    };
                    match reply {
                        Reply::Body(body) => {
                            let _ = socket.write_all(head("200 OK", body.len(), "").as_bytes()).await;
                            let _ = socket.write_all(&body).await;
                        }
                        Reply::Status(code) => {
                            let _ = socket.write_all(head(&format!("{code} X"), 0, "").as_bytes()).await;
                        }
                        Reply::Redirect(location) => {
                            let extra = format!("Location: {location}\r\n");
                            let _ = socket.write_all(head("302 Found", 0, &extra).as_bytes()).await;
                        }
                        Reply::Stall { len, prefix } => {
                            let _ = socket.write_all(head("200 OK", len, "").as_bytes()).await;
                            let _ = socket.write_all(&prefix).await;
                            tokio::time::sleep(Duration::from_secs(30)).await;
                        }
                        Reply::Trickle { chunks, gap } => {
                            let _ = socket.write_all(head("200 OK", chunks, "").as_bytes()).await;
                            for _ in 0..chunks {
                                let _ = socket.write_all(b"x").await;
                                let _ = socket.flush().await;
                                tokio::time::sleep(gap).await;
                            }
                        }
                        Reply::Truncated { len, body } => {
                            let _ = socket.write_all(head("200 OK", len, "").as_bytes()).await;
                            let _ = socket.write_all(&body).await;
                        }
                    }
                    let _ = socket.shutdown().await;
                });
            }
        });
        format!("http://{addr}")
    }

    fn sha(body: &[u8]) -> String {
        format!("{:x}", Sha256::digest(body))
    }

    fn routes(base_asset: Reply, sums: Reply) -> HashMap<String, Reply> {
        HashMap::from([
            ("/latest".to_string(), Reply::Redirect(format!("https://github.com/yt-dlp/yt-dlp/releases/tag/{TAG}"))),
            (format!("/download/{TAG}/{CHECKSUMS_ASSET}"), sums),
            (format!("/download/{TAG}/{ASSET}"), base_asset),
        ])
    }

    fn sums_for(body: &[u8]) -> Reply {
        Reply::Body(format!("{}  other_asset\n{}  {}\n", "0".repeat(64), sha(body), ASSET).into_bytes())
    }

    const FAST: DownloadLimits = DownloadLimits {
        connect: Duration::from_secs(2),
        idle: Duration::from_millis(300),
        total: Duration::from_secs(5),
        max_bytes: 1024 * 1024,
    };

    async fn provision(base: &str, dest: &Path, limits: DownloadLimits) -> Result<()> {
        let latest = format!("{base}/latest");
        let download = format!("{base}/download");
        let source = ReleaseSource { latest_url: &latest, download_base: &download, asset: ASSET };
        download_ytdlp(dest, &source, limits).await
    }

    /// Existing working binary plus no stray temp files after a failure.
    fn assert_untouched(dir: &Path, dest: &Path) {
        assert_eq!(std::fs::read(dest).unwrap(), b"working binary");
        let leftovers: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(".yt-dlp-download-"))
            .collect();
        assert!(leftovers.is_empty(), "partial download left behind");
    }

    async fn failing(asset: Reply, sums: Reply, limits: DownloadLimits) -> String {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("yt-dlp");
        std::fs::write(&dest, b"working binary").unwrap();
        let base = serve(routes(asset, sums)).await;
        let error = provision(&base, &dest, limits).await.unwrap_err();
        assert_untouched(dir.path(), &dest);
        format!("{error:#}")
    }

    #[tokio::test]
    async fn verified_release_is_installed_from_a_pinned_tag() {
        let body = b"fake yt-dlp binary".to_vec();
        let base = serve(routes(Reply::Body(body.clone()), sums_for(&body))).await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("yt-dlp");
        provision(&base, &dest, FAST).await.unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), body);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777, 0o755);
        }
    }

    #[tokio::test]
    async fn bad_status_keeps_existing_binary() {
        let body = b"x".to_vec();
        let error = failing(Reply::Status(500), sums_for(&body), FAST).await;
        assert!(error.contains("error status"), "{error}");
    }

    #[tokio::test]
    async fn checksum_mismatch_is_rejected() {
        let error = failing(Reply::Body(b"tampered".to_vec()), sums_for(b"genuine"), FAST).await;
        assert!(error.contains("checksum verification"), "{error}");
    }

    #[tokio::test]
    async fn missing_and_malformed_checksum_entries_are_rejected() {
        let body = b"x".to_vec();
        let missing = failing(Reply::Body(body.clone()), Reply::Body(b"abc  other\n".to_vec()), FAST).await;
        assert!(missing.contains("No checksum"), "{missing}");
        let malformed = failing(
            Reply::Body(body.clone()),
            Reply::Body(format!("nothex  {ASSET}\n").into_bytes()),
            FAST,
        )
        .await;
        assert!(malformed.contains("Malformed"), "{malformed}");
        let unavailable = failing(Reply::Body(body), Reply::Status(404), FAST).await;
        assert!(unavailable.contains("checksum list"), "{unavailable}");
    }

    #[tokio::test]
    async fn oversized_and_truncated_downloads_are_rejected() {
        let small = DownloadLimits { max_bytes: 8, ..FAST };
        let big = b"0123456789abcdef".to_vec();
        let error = failing(Reply::Body(big.clone()), sums_for(&big), small).await;
        assert!(error.contains("large") || error.contains("exceeded"), "{error}");

        let error = failing(
            Reply::Truncated { len: 100, body: b"short".to_vec() },
            sums_for(b"short"),
            FAST,
        )
        .await;
        assert!(error.contains("truncated") || error.contains("Error while downloading"), "{error}");
    }

    #[tokio::test]
    async fn stalled_and_slow_downloads_time_out() {
        let error = failing(Reply::Stall { len: 50, prefix: b"abc".to_vec() }, sums_for(b"x"), FAST).await;
        assert!(error.contains("stalled"), "{error}");

        let slow = DownloadLimits { idle: Duration::from_secs(2), total: Duration::from_millis(400), ..FAST };
        let error = failing(
            Reply::Trickle { chunks: 40, gap: Duration::from_millis(50) },
            sums_for(b"x"),
            slow,
        )
        .await;
        assert!(error.contains("exceeded") || error.contains("timed out") || error.contains("Error while downloading"), "{error}");
    }

    #[tokio::test]
    async fn unreachable_release_server_fails_cleanly() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        drop(listener); // nothing listens there now
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("yt-dlp");
        std::fs::write(&dest, b"working binary").unwrap();
        assert!(provision(&base, &dest, FAST).await.is_err());
        assert_untouched(dir.path(), &dest);
    }

    #[tokio::test]
    async fn concurrent_provisioning_never_produces_a_partial_binary() {
        let body = vec![7u8; 64 * 1024];
        let base = serve(routes(Reply::Body(body.clone()), sums_for(&body))).await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("yt-dlp");
        let (a, b) = tokio::join!(provision(&base, &dest, FAST), provision(&base, &dest, FAST));
        a.unwrap();
        b.unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), body);
    }

    #[test]
    fn checksum_lookup_matches_the_exact_asset_name() {
        let sums = format!("{}  {ASSET}.zip\n{}  *{ASSET}\n", "a".repeat(64), "B".repeat(64));
        assert_eq!(expected_sha256(&sums, ASSET).unwrap(), "b".repeat(64));
    }
}
