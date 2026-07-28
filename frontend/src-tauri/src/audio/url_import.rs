// Download a SharePoint/Stream recording with yt-dlp.
//
// Given a `cookies.txt` produced by `sharepoint.rs` and a recording URL, this
// drives yt-dlp (with the bundled ffmpeg for muxing) to fetch the media into a
// working directory, streaming download progress and honoring cancellation by
// killing the child process. The resulting file is then handed to the normal
// import pipeline in `import.rs`.

use anyhow::{anyhow, Context, Result};
use log::{debug, warn};
use once_cell::sync::Lazy;
use regex::Regex;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

static DOWNLOAD_PCT: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\[download\]\s+([0-9]+(?:\.[0-9]+)?)%").unwrap());

/// Kill an attempt when no byte reaches the output directory for this long.
///
/// Disk growth is the only trustworthy signal. Process output is not: the
/// infamous "stall at fragment 144" was yt-dlp blocking on a write to its own
/// full 64 KiB stdout pipe (≈143 fragments' worth of progress lines) because
/// the task that should have drained it was itself wedged inside a UI
/// progress emit. The pipes are drained by dedicated tasks now, but a genuine
/// network freeze is still possible, and a false positive merely costs a
/// resume — so keep this tight.
const STALL_TIMEOUT: Duration = Duration::from_secs(120);

/// A stalled attempt is not fatal: yt-dlp records per-fragment progress in a
/// `.ytdl` sidecar, so the next attempt re-extracts (fresh manifest token) and
/// resumes on a fresh connection where the last one stopped.
const MAX_DOWNLOAD_ATTEMPTS: u32 = 20;
/// Give up when this many consecutive attempts add no bytes at all.
const MAX_STALLS_WITHOUT_PROGRESS: u32 = 2;

const STALL_MARKER: &str = "ytdlp-download-stalled";

/// Download the recording at `url` into `out_dir`, returning the path to the
/// downloaded media file. Download completion percent (0–100) is sent through
/// `progress`; the receiver runs on its own task so a slow or wedged UI
/// consumer can never back-pressure the download (see [`STALL_TIMEOUT`]).
/// Cancelling `cancel` kills yt-dlp and returns an error.
pub async fn download_recording(
    ytdlp: &Path,
    ffmpeg: Option<&Path>,
    cookies_txt: &Path,
    url: &str,
    out_dir: &Path,
    progress: tokio::sync::mpsc::UnboundedSender<u32>,
    cancel: &CancellationToken,
) -> Result<PathBuf> {
    tokio::fs::create_dir_all(out_dir).await.ok();

    let mut prev_bytes = dir_bytes(out_dir);
    let mut stalls_without_progress = 0u32;

    for attempt in 1..=MAX_DOWNLOAD_ATTEMPTS {
        match download_attempt(ytdlp, ffmpeg, cookies_txt, url, out_dir, &progress, cancel)
            .await
        {
            Ok(path) => return Ok(path),
            Err(e) if e.to_string().contains(STALL_MARKER) => {
                let bytes = dir_bytes(out_dir);
                if bytes > prev_bytes {
                    stalls_without_progress = 0;
                } else {
                    stalls_without_progress += 1;
                    if stalls_without_progress >= MAX_STALLS_WITHOUT_PROGRESS {
                        return Err(anyhow!(
                            "The download keeps stalling at the same point — SharePoint \
                             appears to be freezing this session. Please try again later."
                        ));
                    }
                }
                prev_bytes = bytes;
                warn!(
                    "Download attempt {attempt} stalled after {bytes} bytes; \
                     resuming on a fresh connection"
                );
            }
            Err(e) => return Err(e),
        }
    }

    Err(anyhow!(
        "The download stalled too many times ({MAX_DOWNLOAD_ATTEMPTS} attempts). Please try again later."
    ))
}

/// One yt-dlp run. Resumes any partial download left by a previous attempt
/// (fragment progress lives in yt-dlp's `.ytdl` sidecar next to the `.part`).
async fn download_attempt(
    ytdlp: &Path,
    ffmpeg: Option<&Path>,
    cookies_txt: &Path,
    url: &str,
    out_dir: &Path,
    progress: &tokio::sync::mpsc::UnboundedSender<u32>,
    cancel: &CancellationToken,
) -> Result<PathBuf> {
    let output_template = out_dir.join("recording.%(ext)s");

    let mut cmd = Command::new(ytdlp);
    cmd.arg("--no-playlist")
        .arg("--newline")
        .arg("--no-color")
        // NO --force-overwrites: a stalled attempt must resume, not restart.
        .arg("--continue")
        // Prefer an audio-only representation (smaller/faster); fall back to a
        // combined stream if the manifest has no separate audio.
        .arg("-f")
        .arg("bestaudio/best")
        .arg("--merge-output-format")
        .arg("mp4")
        // One connection with exponential backoff: SharePoint throttles
        // fragment bursts hard (observed: 4 connections wedged at ~150
        // fragments while a single one ramped up fine).
        .arg("-N")
        .arg("1")
        .arg("--retries")
        .arg("10")
        .arg("--fragment-retries")
        .arg("15")
        .arg("--retry-sleep")
        .arg("fragment:exp=1:30")
        .arg("--socket-timeout")
        .arg("30")
        // A skipped fragment is silently missing audio in the meeting — a
        // loud failure beats a quietly truncated transcript.
        .arg("--abort-on-unavailable-fragments")
        .arg("--cookies")
        .arg(cookies_txt)
        .arg("-o")
        .arg(&output_template)
        .arg(url)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    if let Some(ffmpeg_path) = ffmpeg {
        // yt-dlp accepts either the binary or its directory.
        if let Some(dir) = ffmpeg_path.parent() {
            cmd.arg("--ffmpeg-location").arg(dir);
        }
    }

    harden_child_io(&mut cmd);
    no_console_window(&mut cmd);

    debug!("Spawning yt-dlp for {url}");
    let mut child = cmd.spawn().context("Failed to start yt-dlp")?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("yt-dlp produced no stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("yt-dlp produced no stderr"))?;

    // Drain stderr in the background so the pipe never blocks; keep the tail
    // for error reporting. Byte-level + lossy: a strict UTF-8 line reader
    // errors on legacy-codepage output, and a reader that exits early closes
    // the pipe — which yt-dlp experiences as EINVAL on its next print and
    // dies (observed 2026-07-28 on a title containing "Grabación").
    let stderr_task = tokio::spawn(async move {
        let mut reader = BufReader::new(stderr);
        let mut raw: Vec<u8> = Vec::new();
        let mut tail_lines: Vec<String> = Vec::new();
        loop {
            raw.clear();
            match reader.read_until(b'\n', &mut raw).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let line = String::from_utf8_lossy(&raw).trim_end().to_string();
                    tail_lines.push(line);
                    if tail_lines.len() > 40 {
                        tail_lines.remove(0);
                    }
                }
            }
        }
        tail_lines.join("\n")
    });

    // Drain stdout on its own task too, forwarding parsed percentages into
    // the (unbounded, never-blocking) progress channel. This task must never
    // do anything that can block or make it exit while yt-dlp lives: if the
    // 64 KiB stdout pipe fills — or the reader drops the pipe — the download
    // dies with it.
    let progress_tx = progress.clone();
    let stdout_task = tokio::spawn(async move {
        let mut reader = BufReader::new(stdout);
        let mut raw: Vec<u8> = Vec::new();
        let mut last_pct = 0u32;
        loop {
            raw.clear();
            match reader.read_until(b'\n', &mut raw).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let line = String::from_utf8_lossy(&raw);
                    if let Some(pct) = parse_progress(&line) {
                        if pct != last_pct {
                            last_pct = pct;
                            let _ = progress_tx.send(pct);
                        }
                    }
                }
            }
        }
    });

    // Flip the UI off "Preparing downloader…" now — manifest resolution plus
    // throttled fragments can hold the first integer percent for minutes.
    let _ = progress.send(0);

    // Stall watchdog: when did the output directory last change size. The
    // ticker lives outside the select loop so per-iteration future resets
    // can't starve it.
    let mut watched_bytes = dir_bytes(out_dir);
    let mut last_growth = Instant::now();
    let mut watchdog = tokio::time::interval(Duration::from_secs(20));
    watchdog.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let status = loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                kill_child_tree(&mut child).await;
                return Err(anyhow!("Import cancelled"));
            }
            _ = watchdog.tick() => {
                let bytes = dir_bytes(out_dir);
                if bytes != watched_bytes {
                    watched_bytes = bytes;
                    last_growth = Instant::now();
                } else if last_growth.elapsed() >= STALL_TIMEOUT {
                    kill_child_tree(&mut child).await;
                    return Err(anyhow!(
                        "{STALL_MARKER}: nothing written to disk for {}s",
                        last_growth.elapsed().as_secs()
                    ));
                }
            }
            status = child.wait() => {
                break status.context("Failed to wait for yt-dlp")?;
            }
        }
    };

    let _ = stdout_task.await;
    let stderr_text = stderr_task.await.unwrap_or_default();

    if !status.success() {
        // The user-facing error keeps the last few lines; the extractor's own
        // warnings arrive earlier and are what actually explain a failure, so
        // log the full tail (it may carry tenant manifest URLs — acceptable in
        // the local log file, and failures here are undiagnosable without it).
        warn!("yt-dlp failed ({status}); captured stderr:\n{stderr_text}");
        return Err(anyhow!(
            "Could not download the recording. It may be inaccessible, deleted, or require different permissions.\n\nyt-dlp: {}",
            tail(&stderr_text, 8)
        ));
    }

    find_output_file(out_dir)
        .ok_or_else(|| anyhow!("Download completed but no media file was produced"))
}

/// Kill yt-dlp and its whole process tree, then reap it. The Windows yt-dlp
/// exe is a PyInstaller launcher whose Python child does the actual work —
/// killing only the direct child leaves the worker alive, still downloading
/// and still holding the `.part` file the next resume attempt needs.
async fn kill_child_tree(child: &mut tokio::process::Child) {
    #[cfg(windows)]
    if let Some(pid) = child.id() {
        let mut cmd = std::process::Command::new("taskkill");
        cmd.args(["/PID", &pid.to_string(), "/T", "/F"]);
        {
            use std::os::windows::process::CommandExt;
            // CREATE_NO_WINDOW
            cmd.creation_flags(0x0800_0000);
        }
        let _ = tokio::task::spawn_blocking(move || cmd.output()).await;
    }
    let _ = child.start_kill();
    let _ = child.wait().await;
}

/// Total bytes on disk in `dir` — the stall-watchdog and resume-progress
/// metric. Sizes are read through an opened handle: on Windows, directory
/// metadata for a file another process holds open lags far behind the real
/// size (a `.part` mid-download can list as 0 bytes for its whole life).
fn dir_bytes(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.path().is_file())
                .filter_map(|e| std::fs::File::open(e.path()).ok())
                .filter_map(|f| f.metadata().ok())
                .map(|m| m.len())
                .sum()
        })
        .unwrap_or(0)
}

/// Fetch the meeting's transcript track (a WebVTT) via yt-dlp's subtitle
/// support, without downloading the video. Returns the path to a `.vtt` file.
/// If no transcript track exists, returns an error that includes yt-dlp's
/// `--list-subs` output for diagnosis.
pub async fn fetch_transcript(
    ytdlp: &Path,
    ffmpeg: Option<&Path>,
    cookies_txt: &Path,
    url: &str,
    out_dir: &Path,
    cancel: &CancellationToken,
) -> Result<PathBuf> {
    tokio::fs::create_dir_all(out_dir).await.ok();
    let out_template = out_dir.join("transcript.%(ext)s");

    let mut cmd = Command::new(ytdlp);
    cmd.arg("--no-playlist")
        .arg("--skip-download")
        .arg("--write-subs")
        .arg("--write-auto-subs")
        .arg("--sub-langs")
        .arg("all")
        .arg("--sub-format")
        .arg("vtt/best")
        .arg("--convert-subs")
        .arg("vtt")
        .arg("--cookies")
        .arg(cookies_txt)
        .arg("-o")
        .arg(&out_template)
        .arg(url)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    apply_ffmpeg_location(&mut cmd, ffmpeg);
    harden_child_io(&mut cmd);
    no_console_window(&mut cmd);

    debug!("Fetching transcript subtitles for {url}");
    let (_ok, output) = run_capture(cmd, cancel).await?;

    if let Some(vtt) = find_vtt(out_dir) {
        return Ok(vtt);
    }

    // No subtitle track was written. Ask yt-dlp what (if anything) is available
    // so the error is actionable and we can see it in the logs.
    let listed = list_subs(ytdlp, ffmpeg, cookies_txt, url, cancel)
        .await
        .unwrap_or_else(|e| format!("(could not list subtitles: {e})"));
    // Raw yt-dlp output can contain tenant manifest URLs — keep it at debug;
    // the warn line carries only the track list.
    warn!("No transcript track found. Available subs:\n{listed}");
    debug!("yt-dlp transcript-fetch output:\n{output}");

    Err(anyhow!(
        "No transcript was found for this link. Teams may not have generated a transcript for this meeting, or it isn't exposed for download.\n\nAvailable subtitle tracks:\n{}",
        listed.trim()
    ))
}

/// Run yt-dlp `--list-subs` and return its stdout (for diagnostics).
async fn list_subs(
    ytdlp: &Path,
    ffmpeg: Option<&Path>,
    cookies_txt: &Path,
    url: &str,
    cancel: &CancellationToken,
) -> Result<String> {
    let mut cmd = Command::new(ytdlp);
    cmd.arg("--no-playlist")
        .arg("--skip-download")
        .arg("--list-subs")
        .arg("--cookies")
        .arg(cookies_txt)
        .arg(url)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    apply_ffmpeg_location(&mut cmd, ffmpeg);
    harden_child_io(&mut cmd);
    no_console_window(&mut cmd);
    let (_ok, output) = run_capture(cmd, cancel).await?;
    Ok(output)
}

/// Spawn a yt-dlp command, drain both pipes concurrently (avoids deadlock),
/// and wait — killing the child if `cancel` fires. Returns (success, combined
/// stdout+stderr).
async fn run_capture(mut cmd: Command, cancel: &CancellationToken) -> Result<(bool, String)> {
    let mut child = cmd.spawn().context("Failed to start yt-dlp")?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let out_task = tokio::spawn(async move { drain(stdout).await });
    let err_task = tokio::spawn(async move { drain(stderr).await });

    let status = tokio::select! {
        _ = cancel.cancelled() => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Err(anyhow!("Import cancelled"));
        }
        st = child.wait() => st.context("Failed to wait for yt-dlp")?,
    };

    let out = out_task.await.unwrap_or_default();
    let err = err_task.await.unwrap_or_default();
    Ok((status.success(), format!("{out}{err}")))
}

async fn drain<R: tokio::io::AsyncRead + Unpin>(pipe: Option<R>) -> String {
    let mut buf = String::new();
    if let Some(r) = pipe {
        let mut reader = BufReader::new(r);
        let mut raw: Vec<u8> = Vec::new();
        loop {
            raw.clear();
            match reader.read_until(b'\n', &mut raw).await {
                Ok(0) | Err(_) => break,
                Ok(_) => buf.push_str(&String::from_utf8_lossy(&raw)),
            }
        }
    }
    buf
}

fn apply_ffmpeg_location(cmd: &mut Command, ffmpeg: Option<&Path>) {
    if let Some(ffmpeg_path) = ffmpeg {
        if let Some(dir) = ffmpeg_path.parent() {
            cmd.arg("--ffmpeg-location").arg(dir);
        }
    }
}

/// Make yt-dlp's Python I/O safe to pipe from a GUI parent.
///
/// Without this, Python wraps stdout in the legacy locale codec (cp1252 on
/// Windows). The first non-ASCII character in the *recording's own title*
/// (e.g. "Grabación") then reaches us as bytes that are not valid UTF-8 — and
/// worse, a title character outside cp1252 makes yt-dlp itself crash with
/// UnicodeEncodeError. UTF-8 mode fixes both directions. stdin is nulled
/// because a windows-subsystem parent has no valid stdin handle to inherit.
fn harden_child_io(cmd: &mut Command) {
    cmd.env("PYTHONUTF8", "1")
        .env("PYTHONIOENCODING", "utf-8")
        .stdin(Stdio::null());
}

fn no_console_window(cmd: &mut Command) {
    #[cfg(windows)]
    {
        // CREATE_NO_WINDOW
        cmd.creation_flags(0x0800_0000);
    }
    #[cfg(not(windows))]
    {
        let _ = cmd;
    }
}

/// Find a downloaded `.vtt` in `dir`, preferring an English track.
fn find_vtt(dir: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut fallback: Option<PathBuf> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        let is_vtt = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("vtt"))
            .unwrap_or(false);
        if !is_vtt {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if name.contains(".en") {
            return Some(path);
        }
        fallback = fallback.or(Some(path));
    }
    fallback
}

fn parse_progress(line: &str) -> Option<u32> {
    let caps = DOWNLOAD_PCT.captures(line)?;
    let pct: f32 = caps.get(1)?.as_str().parse().ok()?;
    Some(pct.round().clamp(0.0, 100.0) as u32)
}

fn tail(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    let start = all.len().saturating_sub(lines);
    all[start..].join("\n")
}

/// Find the single downloaded media file in `dir`, ignoring yt-dlp temp files.
fn find_output_file(dir: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if matches!(ext.as_str(), "part" | "ytdl" | "download" | "tmp") {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        if best.as_ref().map(|(t, _)| modified >= *t).unwrap_or(true) {
            best = Some((modified, path));
        }
    }
    best.map(|(_, p)| p)
}
