//! One long-lived ffmpeg process per recording, fed mixed audio as it arrives.
//!
//! Replaces a checkpoint-and-merge design that, for every 30 s of audio, held
//! the whole window in RAM (5.8 MB), cloned it, spawned ffmpeg, and encoded —
//! then at Stop spawned ffmpeg once more to concat-remux the entire meeting.
//! For a two-hour meeting that was ~240 process spawns plus a final pass over
//! every byte written.
//!
//! Here the encoder is started once and stdin stays open for the length of the
//! recording, so the steady-state cost is a `write_all` into a pipe. Crash
//! recovery survives because the output is a *fragmented* MP4
//! (`frag_keyframe+empty_moov`): the moov atom is written up front and each
//! fragment is self-describing, so a file whose writer was killed is still
//! playable up to the last flush. The old design needed `.checkpoints/` for
//! exactly that property.

use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use log::{error, info, warn};

use super::ffmpeg::find_ffmpeg_path;

/// 64 kbps AAC-LC is transparent for mono speech. The previous 192 kbps was
/// sized for music and made recordings ~3x larger than they needed to be.
const BITRATE: &str = "64k";

/// stdin is written through a BufWriter this size, so a 600 ms mix window
/// (115 KB) reaches the pipe in a couple of writes rather than one per window.
const WRITE_BUFFER_BYTES: usize = 256 * 1024;

/// Bound on how much audio an unflushed buffer can cost us if the process dies.
const FLUSH_INTERVAL: Duration = Duration::from_secs(5);

/// Keep the tail of ffmpeg's stderr for diagnostics without letting a chatty
/// encoder grow unboundedly in memory.
const STDERR_TAIL_BYTES: usize = 8 * 1024;

/// Backlog cap, in mix windows (~600 ms each), so roughly 40 s of audio ≈ 7 MB.
///
/// The writer thread blocks in `write_all` on ffmpeg's stdin. If ffmpeg ever
/// hangs without exiting, an unbounded queue would grow for the length of the
/// meeting — 1.4 GB over two hours. Dropping the tail of a recording whose
/// encoder has already wedged is the better failure.
const MAX_QUEUED_WINDOWS: usize = 64;

/// Queue payload. There is deliberately no "finish" variant: dropping the
/// sender closes the channel, which `recv()` reports only after draining every
/// queued message. A sentinel would have to be `send`-ed, and `SyncSender::send`
/// blocks when the queue is full — which is exactly the wedged-encoder case
/// where `finalize()` must not hang.
type Msg = Vec<f32>;

/// Handle to a running encoder. Dropping it without calling [`finalize`] stops
/// the writer thread and leaves whatever was flushed on disk — playable, thanks
/// to fragmented MP4.
pub struct StreamingEncoder {
    tx: Option<SyncSender<Msg>>,
    writer: Option<JoinHandle<Result<()>>>,
    output: PathBuf,
    sample_rate: u32,
    samples_written: Arc<AtomicU64>,
    dropped_windows: AtomicU64,
}

impl StreamingEncoder {
    /// Spawn ffmpeg and the thread that feeds it.
    pub fn new(output: PathBuf, sample_rate: u32) -> Result<Self> {
        let ffmpeg_path = find_ffmpeg_path()
            .ok_or_else(|| anyhow!("FFmpeg not found. Please install FFmpeg to save recordings."))?;

        if let Some(parent) = output.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut command = std::process::Command::new(&ffmpeg_path);
        command
            .args([
                "-nostdin",
                "-loglevel",
                "error",
                "-f",
                "f32le",
                "-ar",
                &sample_rate.to_string(),
                "-ac",
                "1",
                "-i",
                "pipe:0",
                "-c:a",
                "aac",
                "-b:a",
                BITRATE,
                "-profile:a",
                "aac_low",
                // Fragmented output: playable even if this process is killed.
                "-movflags",
                "+frag_keyframe+empty_moov+default_base_moof",
                "-f",
                "mp4",
            ])
            .arg(&output)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped());

        // Hide the console window on Windows; without this every recording
        // flashes a CMD window.
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x08000000;
            command.creation_flags(CREATE_NO_WINDOW);
        }

        let mut child = command
            .spawn()
            .map_err(|e| anyhow!("Failed to start FFmpeg encoder: {}", e))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("FFmpeg stdin unavailable"))?;

        // Drain stderr on its own thread. Without this a full stderr pipe blocks
        // ffmpeg, which blocks our writes into stdin, and the recording wedges.
        let stderr = child.stderr.take();
        std::thread::spawn(move || {
            let Some(mut stderr) = stderr else { return };
            let mut tail = Vec::with_capacity(STDERR_TAIL_BYTES);
            let mut buf = [0u8; 4096];
            loop {
                match stderr.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        tail.extend_from_slice(&buf[..n]);
                        if tail.len() > STDERR_TAIL_BYTES {
                            let excess = tail.len() - STDERR_TAIL_BYTES;
                            tail.drain(0..excess);
                        }
                    }
                }
            }
            if !tail.is_empty() {
                warn!("FFmpeg encoder stderr: {}", String::from_utf8_lossy(&tail));
            }
        });

        let (tx, rx) = mpsc::sync_channel::<Msg>(MAX_QUEUED_WINDOWS);
        let samples_written = Arc::new(AtomicU64::new(0));

        let writer = {
            let counter = samples_written.clone();
            std::thread::Builder::new()
                .name("meetily-audio-encoder".into())
                .spawn(move || write_loop(stdin, rx, child, counter))?
        };

        info!(
            "Streaming encoder started: {} ({} Hz mono, AAC {})",
            output.display(),
            sample_rate,
            BITRATE
        );

        Ok(Self {
            tx: Some(tx),
            writer: Some(writer),
            output,
            sample_rate,
            samples_written,
            dropped_windows: AtomicU64::new(0),
        })
    }

    /// Queue a mixed window. Never blocks — the caller is the audio pipeline
    /// task, and stalling it would back pressure all the way to the realtime
    /// capture threads. A full queue means the encoder is wedged, so the window
    /// is dropped and counted rather than buffered forever.
    pub fn write(&self, samples: Vec<f32>) -> Result<()> {
        if samples.is_empty() {
            return Ok(());
        }
        let Some(tx) = &self.tx else {
            return Err(anyhow!("Encoder already finalized"));
        };

        match tx.try_send(samples) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                let dropped = self.dropped_windows.fetch_add(1, Ordering::Relaxed) + 1;
                // Log the first drop and then sparsely: a wedged encoder would
                // otherwise produce a line per window for the whole meeting.
                if dropped == 1 || dropped % 100 == 0 {
                    warn!(
                        "Audio encoder backlog full ({} windows); dropped {} window(s) so far",
                        MAX_QUEUED_WINDOWS, dropped
                    );
                }
                Ok(())
            }
            Err(TrySendError::Disconnected(_)) => {
                Err(anyhow!("Encoder writer thread has stopped"))
            }
        }
    }

    /// Approximate audio duration committed so far, in seconds.
    pub fn duration_seconds(&self) -> f64 {
        let samples = self.samples_written.load(Ordering::Relaxed);
        samples as f64 / self.sample_rate as f64
    }

    pub fn output_path(&self) -> &Path {
        &self.output
    }

    /// Close stdin, wait for ffmpeg to write its trailer, and return the file.
    pub fn finalize(mut self) -> Result<PathBuf> {
        // Dropping the sender is the shutdown signal; see the `Msg` doc comment.
        drop(self.tx.take());

        if let Some(writer) = self.writer.take() {
            match writer.join() {
                Ok(Ok(())) => {}
                Ok(Err(e)) => return Err(e),
                Err(_) => return Err(anyhow!("Encoder writer thread panicked")),
            }
        }

        if !self.output.exists() {
            return Err(anyhow!(
                "Encoder produced no output at {}",
                self.output.display()
            ));
        }

        // ffmpeg accepts an empty input stream and writes a header-only mp4, so
        // "the file exists" is not enough. Left in place that file would look
        // like a recoverable recording to has_audio_checkpoints, which only
        // checks for a non-empty audio.mp4.
        if self.samples_written.load(Ordering::Relaxed) == 0 {
            let _ = std::fs::remove_file(&self.output);
            return Err(anyhow!("No audio was captured during this recording"));
        }

        info!(
            "Streaming encoder finalized: {} ({:.1}s)",
            self.output.display(),
            self.duration_seconds()
        );
        Ok(self.output.clone())
    }
}

impl Drop for StreamingEncoder {
    fn drop(&mut self) {
        // finalize() takes both by Option::take, so this only fires on an
        // abandoned encoder — stop the child rather than leaking the process.
        drop(self.tx.take());
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
    }
}

fn write_loop(
    stdin: std::process::ChildStdin,
    rx: Receiver<Msg>,
    mut child: std::process::Child,
    counter: Arc<AtomicU64>,
) -> Result<()> {
    let mut out = BufWriter::with_capacity(WRITE_BUFFER_BYTES, stdin);
    let mut last_flush = Instant::now();
    let mut write_failed = false;

    // Ends when the sender is dropped, after every queued message is drained.
    while let Ok(samples) = rx.recv() {
        if write_failed {
            continue;
        }

        let bytes: &[u8] = bytemuck::cast_slice(&samples);
        if let Err(e) = out.write_all(bytes) {
            // Typically a broken pipe because ffmpeg exited. Stop writing but
            // keep draining the channel so producers never block on a dead
            // consumer.
            error!("Encoder write failed: {}", e);
            write_failed = true;
            continue;
        }
        counter.fetch_add(samples.len() as u64, Ordering::Relaxed);

        if last_flush.elapsed() >= FLUSH_INTERVAL {
            if let Err(e) = out.flush() {
                error!("Encoder flush failed: {}", e);
                write_failed = true;
            }
            last_flush = Instant::now();
        }
    }

    // Dropping the BufWriter closes stdin, which is how ffmpeg knows the stream
    // ended and it should write the trailer.
    let flush_result = out.flush();
    drop(out);

    let status = child.wait()?;

    if let Err(e) = flush_result {
        return Err(anyhow!("Failed flushing audio to encoder: {}", e));
    }
    if !status.success() {
        return Err(anyhow!("FFmpeg encoder exited with status {}", status));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn ffmpeg_available() -> bool {
        find_ffmpeg_path().is_some()
    }

    #[test]
    fn encodes_a_playable_file() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not found");
            return;
        }
        let dir = tempdir().unwrap();
        let out = dir.path().join("audio.mp4");

        let encoder = StreamingEncoder::new(out.clone(), 48000).unwrap();
        // 2 seconds of a quiet tone, delivered in 600 ms-ish windows.
        let mut phase = 0.0f32;
        for _ in 0..4 {
            let mut window = Vec::with_capacity(24_000);
            for _ in 0..24_000 {
                phase += 440.0 * std::f32::consts::TAU / 48_000.0;
                window.push(phase.sin() * 0.25);
            }
            encoder.write(window).unwrap();
        }

        // Don't assert duration_seconds() here — the writer thread drains
        // asynchronously, so it races. duration_tracks_samples_written covers
        // the counter with a proper wait.
        let path = encoder.finalize().unwrap();

        assert!(path.exists());
        assert!(
            std::fs::metadata(&path).unwrap().len() > 1024,
            "expected a non-trivial mp4"
        );
    }

    #[test]
    fn empty_writes_are_ignored() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not found");
            return;
        }
        let dir = tempdir().unwrap();
        let out = dir.path().join("audio.mp4");

        let encoder = StreamingEncoder::new(out.clone(), 48000).unwrap();
        encoder.write(Vec::new()).unwrap();
        assert_eq!(encoder.duration_seconds(), 0.0);

        // ffmpeg would happily emit a header-only mp4 here; finalize rejects it
        // and removes the file so it cannot be mistaken for a recoverable
        // recording.
        assert!(encoder.finalize().is_err());
        assert!(!out.exists(), "empty output should have been removed");
    }

    #[test]
    fn a_full_backlog_drops_windows_instead_of_blocking() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not found");
            return;
        }
        let dir = tempdir().unwrap();
        let out = dir.path().join("audio.mp4");

        let encoder = StreamingEncoder::new(out, 48000).unwrap();

        // Far more than MAX_QUEUED_WINDOWS. Even if ffmpeg were wedged and the
        // writer thread never drained a single message, none of these may block.
        for _ in 0..(MAX_QUEUED_WINDOWS * 8) {
            encoder.write(vec![0.1f32; 4800]).unwrap();
        }

        let _ = encoder.finalize();
    }

    #[test]
    fn duration_tracks_samples_written() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not found");
            return;
        }
        let dir = tempdir().unwrap();
        let out = dir.path().join("audio.mp4");

        let encoder = StreamingEncoder::new(out, 16000).unwrap();
        encoder.write(vec![0.0; 16_000]).unwrap();

        // The writer thread is asynchronous; give it a moment to drain.
        for _ in 0..50 {
            if encoder.duration_seconds() >= 1.0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!((encoder.duration_seconds() - 1.0).abs() < 0.01);
        let _ = encoder.finalize();
    }
}
