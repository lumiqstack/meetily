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
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

static DOWNLOAD_PCT: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\[download\]\s+([0-9]+(?:\.[0-9]+)?)%").unwrap());

/// Download the recording at `url` into `out_dir`, returning the path to the
/// downloaded media file. `on_progress` receives download completion percent
/// (0–100). Cancelling `cancel` kills yt-dlp and returns an error.
pub async fn download_recording<F>(
    ytdlp: &Path,
    ffmpeg: Option<&Path>,
    cookies_txt: &Path,
    url: &str,
    out_dir: &Path,
    on_progress: F,
    cancel: &CancellationToken,
) -> Result<PathBuf>
where
    F: Fn(u32),
{
    tokio::fs::create_dir_all(out_dir).await.ok();

    let output_template = out_dir.join("recording.%(ext)s");

    let mut cmd = Command::new(ytdlp);
    cmd.arg("--no-playlist")
        .arg("--newline")
        .arg("--no-color")
        .arg("--force-overwrites")
        // Prefer an audio-only representation (smaller/faster); fall back to a
        // combined stream if the manifest has no separate audio.
        .arg("-f")
        .arg("bestaudio/best")
        .arg("--merge-output-format")
        .arg("mp4")
        .arg("-N")
        .arg("4")
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

    #[cfg(windows)]
    {
        // CREATE_NO_WINDOW — don't flash a console window on each spawn.
        cmd.creation_flags(0x0800_0000);
    }

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
    // for error reporting.
    let stderr_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        let mut buf: Vec<String> = Vec::new();
        while let Ok(Some(line)) = lines.next_line().await {
            buf.push(line);
            if buf.len() > 40 {
                buf.remove(0);
            }
        }
        buf.join("\n")
    });

    let mut reader = BufReader::new(stdout).lines();
    let mut last_pct = 0u32;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Err(anyhow!("Import cancelled"));
            }
            line = reader.next_line() => {
                match line {
                    Ok(Some(l)) => {
                        if let Some(pct) = parse_progress(&l) {
                            if pct != last_pct {
                                last_pct = pct;
                                on_progress(pct);
                            }
                        }
                    }
                    Ok(None) => break, // stdout closed — download finished
                    Err(e) => {
                        warn!("Error reading yt-dlp output: {e}");
                        break;
                    }
                }
            }
        }
    }

    let status = child.wait().await.context("Failed to wait for yt-dlp")?;
    let stderr_text = stderr_task.await.unwrap_or_default();

    if !status.success() {
        return Err(anyhow!(
            "Could not download the recording. It may be inaccessible, deleted, or require different permissions.\n\nyt-dlp: {}",
            tail(&stderr_text, 8)
        ));
    }

    find_output_file(out_dir)
        .ok_or_else(|| anyhow!("Download completed but no media file was produced"))
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
        let mut lines = BufReader::new(r).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            buf.push_str(&line);
            buf.push('\n');
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
