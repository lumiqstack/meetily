//! Bounded 16 kHz mono PCM reader backed by ffmpeg.
//!
//! The read-side twin of [`super::stream_encoder`]. That module pipes f32
//! samples *into* ffmpeg to produce an mp4; this one pipes them back *out*,
//! already downmixed and resampled, so nothing upstream of VAD ever holds a
//! whole recording.
//!
//! The alternative — Symphonia into one `Vec<f32>`, then `audio_to_mono`, then a
//! chunked rubato resample — needed ~21 GB for a 10.8-hour file and aborted the
//! process seven times. ffmpeg's `swresample` does the downmix and the 48k→16k
//! conversion in a single anti-aliased pass, which is both bounded and the part
//! VAD is most sensitive to.
//!
//! Unlike the encoder there is no bounded channel. Here *we* are the consumer
//! and ffmpeg is the producer, so a full pipe blocking ffmpeg is exactly the
//! backpressure that keeps memory flat; a plain synchronous read is enough.

use anyhow::{anyhow, Result};
use log::warn;
use std::io::{BufReader, Read};
use std::path::Path;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};

use super::ffmpeg::find_ffmpeg_path;

/// Whisper, Silero VAD and the remote providers all want 16 kHz mono.
pub const TARGET_SAMPLE_RATE: u32 = 16_000;

/// Frames handed over per [`PcmStream::next_block`]. One second of audio —
/// 64 KB — which is far below anything that matters for memory and large enough
/// that per-read overhead disappears. VAD re-buffers into its own 480-sample
/// frames, so this size does not affect segmentation.
const BLOCK_FRAMES: usize = TARGET_SAMPLE_RATE as usize;

/// How much of ffmpeg's stderr to keep for the error message.
const STDERR_TAIL_BYTES: usize = 4096;

/// A running ffmpeg decode, pulled one block at a time.
pub struct PcmStream {
    child: Child,
    stdout: BufReader<ChildStdout>,
    stderr_tail: Arc<Mutex<Vec<u8>>>,
    frames_read: u64,
    /// Reused across blocks so the read loop does not allocate.
    byte_buf: Vec<u8>,
}

impl PcmStream {
    /// Start decoding `path` to 16 kHz mono f32.
    ///
    /// `start_seconds` skips ahead, for resuming a partially transcribed file.
    /// It is placed after `-i` so the seek is sample-accurate: seeking before
    /// `-i` is faster but lands on a packet boundary, and every transcript
    /// timestamp after the resume point would inherit that error.
    pub fn open(path: &Path, start_seconds: f64) -> Result<Self> {
        let ffmpeg_path = find_ffmpeg_path()
            .ok_or_else(|| anyhow!("FFmpeg not found; cannot stream audio for transcription"))?;

        let mut command = Command::new(&ffmpeg_path);
        command.args(["-nostdin", "-loglevel", "error", "-i"]);
        command.arg(path);
        if start_seconds > 0.0 {
            command.args(["-ss", &format!("{:.3}", start_seconds)]);
        }
        command
            .args([
                "-vn",
                "-ac",
                "1",
                "-ar",
                &TARGET_SAMPLE_RATE.to_string(),
                "-f",
                "f32le",
                "pipe:1",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // Hide the console window on Windows, as every other spawn site does.
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x08000000;
            command.creation_flags(CREATE_NO_WINDOW);
        }

        let mut child = command
            .spawn()
            .map_err(|e| anyhow!("Failed to start FFmpeg decoder: {}", e))?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("FFmpeg stdout unavailable"))?;

        // Drain stderr on its own thread. A full stderr pipe blocks ffmpeg,
        // which stops it writing stdout, which deadlocks our reads.
        let stderr_tail = Arc::new(Mutex::new(Vec::new()));
        let stderr = child.stderr.take();
        {
            let tail = Arc::clone(&stderr_tail);
            std::thread::spawn(move || {
                let Some(mut stderr) = stderr else { return };
                let mut buf = [0u8; 4096];
                loop {
                    match stderr.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let Ok(mut tail) = tail.lock() else { break };
                            tail.extend_from_slice(&buf[..n]);
                            if tail.len() > STDERR_TAIL_BYTES {
                                let excess = tail.len() - STDERR_TAIL_BYTES;
                                tail.drain(0..excess);
                            }
                        }
                    }
                }
            });
        }

        Ok(Self {
            child,
            stdout: BufReader::with_capacity(BLOCK_FRAMES * 4, stdout),
            stderr_tail,
            frames_read: 0,
            byte_buf: vec![0u8; BLOCK_FRAMES * 4],
        })
    }

    /// Fill `out` with the next block, returning `false` at end of stream.
    ///
    /// The final block may be short. `out` is cleared first and keeps its
    /// capacity between calls.
    pub fn next_block(&mut self, out: &mut Vec<f32>) -> Result<bool> {
        out.clear();

        // Partial reads are normal on a pipe; keep going until the buffer is
        // full or the stream ends, so a block is only short at true EOF.
        let mut filled = 0usize;
        while filled < self.byte_buf.len() {
            match self.stdout.read(&mut self.byte_buf[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(anyhow!("Reading FFmpeg output failed: {}", e)),
            }
        }

        if filled == 0 {
            return Ok(false);
        }

        // A trailing fragment of a sample means ffmpeg died mid-write; the exit
        // status collected by `finish` will say why.
        let usable = filled - (filled % 4);
        out.reserve(usable / 4);
        out.extend(
            self.byte_buf[..usable]
                .chunks_exact(4)
                .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])),
        );

        self.frames_read += (usable / 4) as u64;
        Ok(!out.is_empty())
    }

    /// Seconds of audio produced so far.
    pub fn seconds_read(&self) -> f64 {
        self.frames_read as f64 / TARGET_SAMPLE_RATE as f64
    }

    /// Wait for ffmpeg and fail if it did not exit cleanly.
    ///
    /// Call this after the stream ends: a decode that dies partway still looks
    /// like a normal EOF to the reader, and silently transcribing half a meeting
    /// is worse than reporting the failure.
    pub fn finish(mut self) -> Result<()> {
        let status = self
            .child
            .wait()
            .map_err(|e| anyhow!("Waiting for FFmpeg failed: {}", e))?;

        if status.success() {
            return Ok(());
        }

        let tail = self
            .stderr_tail
            .lock()
            .ok()
            .map(|t| String::from_utf8_lossy(&t).trim().to_string())
            .unwrap_or_default();

        Err(anyhow!(
            "FFmpeg decode failed ({}): {}",
            status,
            if tail.is_empty() { "no stderr output" } else { &tail }
        ))
    }
}

impl Drop for PcmStream {
    fn drop(&mut self) {
        // An abandoned stream must not leave ffmpeg writing into a pipe nobody
        // reads. `kill` on an already-exited child is a no-op.
        if let Err(e) = self.child.kill() {
            warn!("Failed to stop FFmpeg decoder: {}", e);
        }
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ffmpeg_available() -> bool {
        find_ffmpeg_path().is_some()
    }

    #[test]
    fn reports_a_missing_file_as_an_error() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not available");
            return;
        }

        let mut stream = match PcmStream::open(Path::new("definitely-not-audio.xyz"), 0.0) {
            Ok(s) => s,
            // Spawning may fail outright, which is also a correct outcome.
            Err(_) => return,
        };

        let mut block = Vec::new();
        while stream.next_block(&mut block).unwrap_or(false) {}
        assert!(
            stream.finish().is_err(),
            "a failed decode must not look like a clean EOF"
        );
    }

    /// Round trip through both halves of the ffmpeg plumbing: encode a known
    /// tone with `stream_encoder`, read it back here, and check the duration and
    /// amplitude survive.
    #[test]
    fn round_trips_a_tone_written_by_the_encoder() {
        use super::super::stream_encoder::StreamingEncoder;

        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not available");
            return;
        }

        let dir = std::env::temp_dir().join("meetily-pcm-stream-test");
        let _ = std::fs::create_dir_all(&dir);
        let out = dir.join("tone.mp4");
        let _ = std::fs::remove_file(&out);

        const SRC_RATE: u32 = 48_000;
        const SECONDS: u32 = 2;

        let encoder = StreamingEncoder::new(out.clone(), SRC_RATE).expect("start encoder");
        for _ in 0..SECONDS {
            let second: Vec<f32> = (0..SRC_RATE)
                .map(|i| {
                    (2.0 * std::f32::consts::PI * 440.0 * i as f32 / SRC_RATE as f32).sin() * 0.5
                })
                .collect();
            encoder.write(second).expect("write tone");
        }
        encoder.finalize().expect("finalize");

        let mut stream = PcmStream::open(&out, 0.0).expect("open decoded stream");
        let mut block = Vec::new();
        let mut peak = 0.0f32;
        while stream.next_block(&mut block).expect("read block") {
            for s in &block {
                peak = peak.max(s.abs());
            }
        }
        let seconds = stream.seconds_read();
        stream.finish().expect("ffmpeg exits cleanly");

        assert!(
            (seconds - SECONDS as f64).abs() < 0.2,
            "expected ~{}s at 16kHz, got {:.3}s",
            SECONDS,
            seconds
        );
        // AAC is lossy and the encoder is mono 64 kbps, so allow a wide band —
        // this is checking the signal survived, not that it is bit-exact.
        assert!(
            peak > 0.2 && peak < 0.9,
            "expected a ~0.5 peak to survive the round trip, got {:.3}",
            peak
        );

        let _ = std::fs::remove_file(&out);
    }
}
