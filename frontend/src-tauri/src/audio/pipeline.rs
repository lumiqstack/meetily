use std::sync::Arc;
use std::collections::VecDeque;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use anyhow::Result;
use log::{debug, error, info, warn};
use crate::batch_audio_metric;
use super::batch_processor::AudioMetricsBatcher;
use rubato::{Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction};

use super::devices::AudioDevice;
use super::recording_state::{AudioChunk, AudioError, RecordingState, DeviceType};
use super::audio_processing::{audio_to_mono, LoudnessNormalizer, NoiseSuppressionProcessor, HighPassFilter};
use super::vad::{ContinuousVadProcessor};

/// Ring buffer for synchronized audio mixing
/// Accumulates samples from mic and system streams until we have aligned windows
struct AudioMixerRingBuffer {
    mic_buffer: VecDeque<f32>,
    system_buffer: VecDeque<f32>,
    window_size_samples: usize,  // Fixed mixing window (e.g., 50ms)
    max_buffer_size: usize,  // Safety limit (e.g., 100ms)
    /// Counts `add_samples` calls so diagnostics can be rate-limited. An
    /// overflow persists across many calls, and logging it on each one turns a
    /// stream hiccup into a ~200 lines/second write storm that makes the
    /// underlying stall worse.
    add_calls: u64,
}

impl AudioMixerRingBuffer {
    fn new(sample_rate: u32) -> Self {
        // Use 50ms windows for mixing
        let window_ms = 600.0;
        let window_size_samples = (sample_rate as f32 * window_ms / 1000.0) as usize;

        // CRITICAL FIX: Increase max buffer to 400ms for system audio stability
        // System audio (especially Core Audio on macOS) can have significant jitter
        // due to sample-by-sample streaming → batching → channel transmission
        // Accounts for: RNNoise buffering + Core Audio jitter + processing delays
        let max_buffer_size = window_size_samples * 8;  // 400ms (was 200ms)

        info!("🔊 Ring buffer initialized: window={}ms ({} samples), max={}ms ({} samples)",
              window_ms, window_size_samples,
              window_ms * 8.0, max_buffer_size);

        Self {
            mic_buffer: VecDeque::with_capacity(max_buffer_size),
            system_buffer: VecDeque::with_capacity(max_buffer_size),
            window_size_samples,
            max_buffer_size,
            add_calls: 0,
        }
    }

    fn add_samples(&mut self, device_type: DeviceType, samples: Vec<f32>) {
        self.add_calls += 1;
        let should_report = self.add_calls % 200 == 0;

        // Log buffer health periodically for diagnostics
        if should_report {
            debug!("📊 Ring buffer status: mic={} samples, sys={} samples (max={})",
                   self.mic_buffer.len(), self.system_buffer.len(), self.max_buffer_size);
        }

        match device_type {
            DeviceType::Microphone => self.mic_buffer.extend(samples),
            DeviceType::System => self.system_buffer.extend(samples),
        }

        // CRITICAL FIX: Add warnings before dropping samples
        // This helps diagnose timing issues in production
        if should_report {
            if self.mic_buffer.len() > self.max_buffer_size {
                warn!("⚠️ Microphone buffer overflow: {} > {} samples, dropping oldest {} samples",
                      self.mic_buffer.len(), self.max_buffer_size,
                      self.mic_buffer.len() - self.max_buffer_size);
            }
            if self.system_buffer.len() > self.max_buffer_size {
                error!("🔴 SYSTEM AUDIO BUFFER OVERFLOW: {} > {} samples, dropping {} samples - THIS CAUSES DISTORTION!",
                      self.system_buffer.len(), self.max_buffer_size,
                      self.system_buffer.len() - self.max_buffer_size);
            }
        }

        // Safety: prevent buffer overflow (keep only last 200ms)
        while self.mic_buffer.len() > self.max_buffer_size {
            self.mic_buffer.pop_front();
        }
        while self.system_buffer.len() > self.max_buffer_size {
            self.system_buffer.pop_front();
        }
    }

    fn can_mix(&self) -> bool {
        self.mic_buffer.len() >= self.window_size_samples ||
        self.system_buffer.len() >= self.window_size_samples
    }

    /// Fill `mic_out` and `sys_out` with the next aligned window.
    ///
    /// Writes into caller-owned buffers that the pipeline reuses across windows.
    /// The previous version returned two freshly-allocated `Vec`s, which — with
    /// the mixer's own output buffer — meant three allocations and three full
    /// copies for every 600 ms of audio.
    ///
    /// Both outputs are always exactly `window_size_samples` long, zero-padded
    /// when a stream is short. Zero-padding (silence) is preferred over
    /// last-sample-hold to prevent repetition artifacts, and is inaudible.
    fn extract_window_into(&mut self, mic_out: &mut Vec<f32>, sys_out: &mut Vec<f32>) -> bool {
        if !self.can_mix() {
            return false;
        }

        drain_window(&mut self.mic_buffer, self.window_size_samples, mic_out);
        drain_window(&mut self.system_buffer, self.window_size_samples, sys_out);
        true
    }
}

/// Move up to `window` samples out of `src` into `dst`, zero-padding the tail.
fn drain_window(src: &mut VecDeque<f32>, window: usize, dst: &mut Vec<f32>) {
    dst.clear();
    dst.reserve(window);

    let take = src.len().min(window);
    dst.extend(src.drain(0..take));
    dst.resize(window, 0.0);
}

/// Simple audio mixer without aggressive ducking
/// Combines mic + system audio with basic clipping prevention
struct ProfessionalAudioMixer;

impl ProfessionalAudioMixer {
    fn new(_sample_rate: u32) -> Self {
        Self
    }

    /// Mix into a caller-owned buffer the pipeline reuses across windows.
    ///
    /// `extract_window_into` guarantees both inputs are the same length, so this
    /// zips the slices instead of doing two bounds-checked `get(i)` lookups per
    /// sample (96,000 of them a second).
    fn mix_window_into(&mut self, mic_window: &[f32], sys_window: &[f32], out: &mut Vec<f32>) {
        debug_assert_eq!(mic_window.len(), sys_window.len());

        out.clear();
        out.reserve(mic_window.len());

        // Sum without ducking — mic is already normalized to -23 LUFS by the
        // capture chain, system audio stays at its natural level.
        for (&mic, &sys) in mic_window.iter().zip(sys_window.iter()) {
            let sum = mic + sys;

            // Soft scaling prevents distortion artifacts: if the sum would
            // exceed ±1.0, scale down PROPORTIONALLY rather than hard clipping,
            // which sounds like "radio breaks".
            let sum_abs = sum.abs();
            out.push(if sum_abs > 1.0 { sum / sum_abs } else { sum });
        }
    }
}

/// Per-callback state for the microphone enhancement chain.
///
/// These were three separate `Arc<Mutex<Option<_>>>` fields, which cost three
/// lock acquisitions on the realtime audio thread for work that is inherently
/// sequential. One mutex covers the whole chain, and `scratch` lets the downmix
/// reuse a buffer instead of allocating one per callback.
struct MicChain {
    noise_suppressor: Option<NoiseSuppressionProcessor>,
    high_pass_filter: Option<HighPassFilter>,
    normalizer: Option<LoudnessNormalizer>,
}

/// Simplified audio capture without broadcast channels
#[derive(Clone)]
pub struct AudioCapture {
    device: Arc<AudioDevice>,
    state: Arc<RecordingState>,
    sample_rate: u32,        // Original device sample rate
    channels: u16,
    chunk_counter: Arc<std::sync::atomic::AtomicU64>,
    device_type: DeviceType,
    needs_resampling: bool,  // Flag if resampling is required
    // CRITICAL FIX: Persistent resampler to preserve energy across chunks
    resampler: Arc<std::sync::Mutex<Option<SincFixedIn<f32>>>>,
    // Buffering for variable-size chunks → fixed-size resampler input
    resampler_input_buffer: Arc<std::sync::Mutex<Vec<f32>>>,
    resampler_chunk_size: usize,  // Fixed chunk size for resampler (512 samples)
    /// Microphone-only enhancement chain; `None` for system audio.
    mic_chain: Option<Arc<std::sync::Mutex<MicChain>>>,
    // Note: Using global recording timestamp for synchronization
}

impl AudioCapture {
    pub fn new(
        device: Arc<AudioDevice>,
        state: Arc<RecordingState>,
        sample_rate: u32,
        channels: u16,
        device_type: DeviceType,
        recording_sender: Option<mpsc::UnboundedSender<AudioChunk>>,
    ) -> Self {
        // CRITICAL FIX: Detect if resampling is needed
        // Pipeline expects 48kHz, but Bluetooth devices often report 8kHz, 16kHz, or 44.1kHz
        const TARGET_SAMPLE_RATE: u32 = 48000;
        let needs_resampling = sample_rate != TARGET_SAMPLE_RATE;

        // Detect device kind (Bluetooth vs Wired) for adaptive processing
        // Use reasonable defaults for buffer size (512 samples is typical)
        let device_kind = super::device_detection::InputDeviceKind::detect(&device.name, 512, sample_rate);

        if needs_resampling {
            warn!(
                "⚠️ SAMPLE RATE MISMATCH DETECTED ⚠️"
            );
            warn!(
                "🔄 [{:?}] Audio device '{}' ({:?}) reports {} Hz (pipeline expects {} Hz)",
                device_type, device.name, device_kind, sample_rate, TARGET_SAMPLE_RATE
            );
            warn!(
                "🔄 Automatic resampling will be applied: {} Hz → {} Hz",
                sample_rate, TARGET_SAMPLE_RATE
            );

            // Log which resampling strategy will be used
            let ratio = TARGET_SAMPLE_RATE as f64 / sample_rate as f64;
            let strategy = if ratio >= 2.0 {
                "High-quality upsampling (sinc_len=512, Cubic interpolation)"
            } else if ratio >= 1.5 {
                "Moderate upsampling (sinc_len=384, Cubic)"
            } else if ratio > 1.0 {
                "Small upsampling (sinc_len=256, Linear)"
            } else if ratio <= 0.5 {
                "Anti-aliased downsampling (sinc_len=512, Cubic)"
            } else {
                "Moderate downsampling (sinc_len=384, Linear)"
            };
            info!("   Resampling strategy: {}", strategy);
        } else {
            info!(
                "✅ [{:?}] Audio device '{}' ({:?}) uses {} Hz (matches pipeline)",
                device_type, device.name, device_kind, sample_rate
            );
        }

        // Initialize audio enhancement processors for MICROPHONE ONLY
        // System audio doesn't need enhancement (already clean)
        let (noise_suppressor, high_pass_filter, normalizer) = if matches!(device_type, DeviceType::Microphone) {
            // Initialize noise suppression (RNNoise) at 48kHz - CONDITIONAL based on flag
            let ns = if super::ffmpeg_mixer::RNNOISE_APPLY_ENABLED {
                match NoiseSuppressionProcessor::new(TARGET_SAMPLE_RATE) {
                    Ok(processor) => {
                        info!("✅ RNNoise noise suppression ENABLED for microphone '{}' (10-15 dB reduction)", device.name);
                        Some(processor)
                    }
                    Err(e) => {
                        warn!("⚠️ Failed to create noise suppressor: {}, continuing without noise suppression", e);
                        None
                    }
                }
            } else {
                info!("ℹ️ RNNoise noise suppression DISABLED for microphone '{}' (flag: RNNOISE_APPLY_ENABLED=false)", device.name);
                info!("   Whisper handles noise well internally - RNNoise is optional");
                None
            };

            // Initialize high-pass filter (removes rumble below 80 Hz)
            let hpf = {
                let filter = HighPassFilter::new(TARGET_SAMPLE_RATE, 80.0);
                info!("✅ High-pass filter initialized for microphone '{}' (cutoff: 80 Hz)", device.name);
                Some(filter)
            };

            // Initialize EBU R128 normalizer (professional loudness standard)
            let norm = match LoudnessNormalizer::new(1, TARGET_SAMPLE_RATE) {
                Ok(normalizer) => {
                    info!("✅ EBU R128 normalizer initialized for microphone '{}' (target: -23 LUFS)", device.name);
                    Some(normalizer)
                }
                Err(e) => {
                    warn!("⚠️ Failed to create normalizer for microphone: {}, normalization disabled", e);
                    None
                }
            };

            (ns, hpf, norm)
        } else {
            // System audio: no enhancement needed
            info!("ℹ️ System audio '{}' captured raw (no enhancement)", device.name);
            (None, None, None)
        };

        // CRITICAL FIX: Initialize persistent resampler to preserve energy across chunks
        // Creating a new resampler per chunk causes energy amplification and incorrect output sizes
        // Use fixed chunk size of 512 samples with buffering for variable-size input
        const RESAMPLER_CHUNK_SIZE: usize = 512;

        let resampler = if needs_resampling {
            let ratio = TARGET_SAMPLE_RATE as f64 / sample_rate as f64;

            // Adaptive parameters based on sample rate ratio (same logic as resample_audio)
            let (sinc_len, interpolation_type, oversampling) = if ratio >= 2.0 {
                (512, SincInterpolationType::Cubic, 512)
            } else if ratio >= 1.5 {
                (384, SincInterpolationType::Cubic, 384)
            } else if ratio > 1.0 {
                (256, SincInterpolationType::Linear, 256)
            } else if ratio <= 0.5 {
                (512, SincInterpolationType::Cubic, 512)
            } else {
                (384, SincInterpolationType::Linear, 384)
            };

            let params = SincInterpolationParameters {
                sinc_len,
                f_cutoff: 0.95,
                interpolation: interpolation_type,
                oversampling_factor: oversampling,
                window: WindowFunction::BlackmanHarris2,
            };

            match SincFixedIn::<f32>::new(
                ratio,
                2.0,  // Maximum relative deviation
                params,
                RESAMPLER_CHUNK_SIZE,
                1,    // Mono
            ) {
                Ok(resampler) => {
                    info!("✅ Persistent resampler initialized for '{}' ({}Hz → {}Hz, chunk_size={})",
                          device.name, sample_rate, TARGET_SAMPLE_RATE, RESAMPLER_CHUNK_SIZE);
                    info!("   Buffering enabled for variable-size chunks (e.g., 320, 512, 1024, etc.)");
                    Some(resampler)
                }
                Err(e) => {
                    warn!("⚠️ Failed to create persistent resampler: {}, will use fallback", e);
                    None
                }
            }
        } else {
            None
        };

        // Raw capture is never sent straight to the recording saver — only the
        // mixed output from AudioPipeline is (see the note in
        // process_audio_data). The parameter is kept for call-site symmetry.
        let _ = recording_sender;

        let mic_chain = if matches!(device_type, DeviceType::Microphone) {
            Some(Arc::new(std::sync::Mutex::new(MicChain {
                noise_suppressor,
                high_pass_filter,
                normalizer,
            })))
        } else {
            None
        };

        Self {
            device,
            state,
            sample_rate,
            channels,
            chunk_counter: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            device_type,
            needs_resampling,
            resampler: Arc::new(std::sync::Mutex::new(resampler)),
            resampler_input_buffer: Arc::new(std::sync::Mutex::new(Vec::with_capacity(RESAMPLER_CHUNK_SIZE * 2))),
            resampler_chunk_size: RESAMPLER_CHUNK_SIZE,
            mic_chain,
            // Using global recording time for sync
        }
    }

    /// Process audio data directly from callback
    pub fn process_audio_data(&self, data: &[f32]) {
        // Check if still recording
        if !self.state.is_recording() {
            return;
        }

        // Convert to mono if needed. This buffer is eventually moved into the
        // AudioChunk, so it has to be owned — but the filter and normalizer
        // below now reuse it rather than each allocating their own copy.
        let mut mono_data = if self.channels > 1 {
            audio_to_mono(data, self.channels)
        } else {
            data.to_vec()
        };

        // CRITICAL FIX: Resample to 48kHz if device uses different sample rate
        // This fixes Bluetooth devices (like Sony WH-1000XM4) that report 16kHz or 44.1kHz
        // Without this, audio is sped up 3x and VAD fails
        //
        // IMPORTANT: Uses PERSISTENT resampler with BUFFERING to preserve energy across chunks
        // Creating a new resampler per chunk causes energy amplification (173.5% RMS)
        // Buffering handles variable chunk sizes (320, 512, 1024, etc.) by accumulating to fixed 512-sample chunks
        const TARGET_SAMPLE_RATE: u32 = 48000;
        if self.needs_resampling {
            // The counter only advances at the end of this callback, so this is
            // the same id the logging block below reads.
            let chunk_id = self.chunk_counter.load(std::sync::atomic::Ordering::SeqCst);
            // Release caps the log level at Info, so these `debug!`s never emit
            // there — and the RMS pass and buffer lock that feed them must not
            // run on the realtime thread either.
            let will_log_resampling = cfg!(debug_assertions) && chunk_id % 100 == 0;

            let before_len = mono_data.len();
            // Sum-of-squares over the whole buffer, on the realtime thread, for
            // a diagnostic printed once every 100 chunks — so only pay for it
            // on the chunks that actually log.
            let before_rms = if will_log_resampling && !mono_data.is_empty() {
                (mono_data.iter().map(|&x| x * x).sum::<f32>() / mono_data.len() as f32).sqrt()
            } else {
                0.0
            };

            // Use persistent resampler with buffering to handle variable chunk sizes
            let mut resampled_output = Vec::new();
            let mut used_persistent_resampler = false;

            if let Ok(mut buffer_lock) = self.resampler_input_buffer.lock() {
                // Add new samples to buffer
                buffer_lock.extend_from_slice(&mono_data);

                // Process complete chunks through the resampler
                if let Ok(mut resampler_lock) = self.resampler.lock() {
                    if let Some(ref mut resampler) = *resampler_lock {
                        used_persistent_resampler = true;

                        // Process as many complete chunks as we have
                        while buffer_lock.len() >= self.resampler_chunk_size {
                            // Extract exactly chunk_size samples
                            let chunk: Vec<f32> = buffer_lock.drain(0..self.resampler_chunk_size).collect();

                            // Rubato expects input as Vec<Vec<f32>> (one Vec per channel)
                            let waves_in = vec![chunk];

                            match resampler.process(&waves_in, None) {
                                Ok(mut waves_out) => {
                                    if let Some(output) = waves_out.pop() {
                                        resampled_output.extend_from_slice(&output);
                                    }
                                }
                                Err(e) => {
                                    warn!("⚠️ Persistent resampler processing failed: {}", e);
                                    used_persistent_resampler = false;
                                    break;
                                }
                            }
                        }
                        // Remaining samples in buffer will be processed in next iteration
                    }
                }
            }

            // CRITICAL: Only update mono_data if we got output from persistent resampler
            // If buffer is accumulating (< 512 samples), skip this chunk - data is safely buffered
            // and will be processed in next iteration with proper resampling
            let has_resampled_output = !resampled_output.is_empty();

            if has_resampled_output {
                mono_data = resampled_output;
            } else if !used_persistent_resampler {
                // Only fallback if persistent resampler is not available at all
                mono_data = super::audio_processing::resample_audio(
                    &mono_data,
                    self.sample_rate,
                    TARGET_SAMPLE_RATE,
                );
            } else {
                // Buffering: samples are accumulating in buffer, waiting for 512-sample chunk
                // Don't send partial/unprocessed data - return early
                // Audio is NOT lost - it's in the buffer and will be processed next iteration
                return;
            }

            // Log resampling only occasionally to avoid spam
            if will_log_resampling && has_resampled_output {
                let after_len = mono_data.len();
                let after_rms = if !mono_data.is_empty() {
                    (mono_data.iter().map(|&x| x * x).sum::<f32>() / mono_data.len() as f32).sqrt()
                } else {
                    0.0
                };
                let ratio = TARGET_SAMPLE_RATE as f64 / self.sample_rate as f64;
                let rms_preservation = if before_rms > 0.0 { (after_rms / before_rms) * 100.0 } else { 100.0 };

                let buffer_size = if let Ok(buf) = self.resampler_input_buffer.lock() {
                    buf.len()
                } else {
                    0
                };

                debug!(
                    "🔄 [{:?}] Persistent buffered resampler: {}Hz → {}Hz (ratio: {:.2}x)",
                    self.device_type,
                    self.sample_rate,
                    TARGET_SAMPLE_RATE,
                    ratio
                );
                debug!(
                    "   Chunk {}: {} → {} samples, RMS preservation: {:.1}%, buffer: {}",
                    chunk_id,
                    before_len,
                    after_len,
                    rms_preservation,
                    buffer_size
                );
            }
        }

        // AUDIO ENHANCEMENT PIPELINE (Microphone Only)
        // Processing order is critical: high-pass → noise suppression → normalization
        // This ensures noise is removed before being amplified by the normalizer
        //
        // All three stages live behind one mutex now: they always run together
        // on the same buffer, so three separate lock/unlock pairs per callback
        // bought nothing. The filter and normalizer also work in place, leaving
        // the mono downmix above as the only allocation on this path.
        if let Some(chain) = &self.mic_chain {
            if let Ok(mut chain) = chain.lock() {
                // STEP 1: Apply high-pass filter to remove low-frequency rumble (< 80 Hz)
                if let Some(ref mut filter) = chain.high_pass_filter {
                    filter.process_in_place(&mut mono_data);
                }

                // STEP 2: Apply RNNoise noise suppression (10-15 dB reduction) - CONDITIONAL
                // Still allocating: RNNoise buffers into 480-sample frames, so its
                // output length differs from its input and it cannot work in place.
                if super::ffmpeg_mixer::RNNOISE_APPLY_ENABLED {
                    if let Some(ref mut suppressor) = chain.noise_suppressor {
                        let before_len = mono_data.len();
                        mono_data = suppressor.process(&mono_data);
                        let after_len = mono_data.len();

                        // CRITICAL MONITORING: Track buffer health
                        let chunk_id = self.chunk_counter.load(std::sync::atomic::Ordering::Relaxed);
                        if chunk_id % 100 == 0 {
                            let buffered = suppressor.buffered_samples();
                            let length_delta = (before_len as i32 - after_len as i32).abs();

                            debug!("🔇 Noise suppression health: in={}, out={}, delta={}, buffered={}",
                                   before_len, after_len, length_delta, buffered);

                            // WARN if accumulating samples (potential latency buildup)
                            if buffered > 1000 {
                                warn!("⚠️ RNNoise accumulating samples: {} buffered (potential latency issue!)",
                                      buffered);
                            }

                            // WARN if significant length mismatch
                            if length_delta > 50 {
                                warn!("⚠️ RNNoise length mismatch: input={} output={} (delta={})",
                                      before_len, after_len, length_delta);
                            }
                        }
                    }
                }

                // STEP 3: Apply EBU R128 normalization (professional loudness standard)
                if let Some(ref mut normalizer) = chain.normalizer {
                    normalizer.normalize_in_place(&mut mono_data);
                }
            }
        }

        // Create audio chunk with stream-specific timestamp (get ID first for logging)
        let chunk_id = self.chunk_counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        // RAW AUDIO: No gain applied here - will be applied AFTER mixing
        // This prevents amplifying system audio bleed-through in the microphone

        // DIAGNOSTIC: Log audio levels for debugging (especially mic issues)
        // if chunk_id % 100 == 0 && !mono_data.is_empty() {
        //     let raw_rms = (mono_data.iter().map(|&x| x * x).sum::<f32>() / mono_data.len() as f32).sqrt();
        //     let raw_peak = mono_data.iter().map(|&x| x.abs()).fold(0.0f32, f32::max);

        //         info!("🎙️ [{:?}] Chunk {} - Raw: RMS={:.6}, Peak={:.6}",
        //               self.device_type, chunk_id, raw_rms, raw_peak);

        //     // Warn if microphone is completely silent
        //     if matches!(self.device_type, DeviceType::Microphone) && raw_rms == 0.0 && raw_peak == 0.0 {
        //         warn!("⚠️ Microphone producing ZERO audio - check permissions or hardware!");
        //     }
        // }
        // else if chunk_id % 100 == 0 && matches!(self.device_type, DeviceType::System) {
        //     let raw_rms = (mono_data.iter().map(|&x| x * x).sum::<f32>() / mono_data.len() as f32).sqrt();
        //     let raw_peak = mono_data.iter().map(|&x| x.abs()).fold(0.0f32, f32::max);
        //     info!("🔊 [{:?}] Chunk {} - Raw: RMS={:.6}, Peak={:.6}",
        //       self.device_type, chunk_id, raw_rms, raw_peak);
            
        //     // Warn if system audio is completely silent
        //     if raw_rms == 0.0 && raw_peak == 0.0 {
        //         warn!("⚠️ System audio producing ZERO audio - check permissions or hardware!");
        //     }
        // }

        // Use global recording timestamp for proper synchronization
        let timestamp = self.state.get_recording_duration().unwrap_or(0.0);

        // RAW AUDIO CHUNK: No gain applied - will be mixed and gained downstream
        // Use 48kHz if we resampled, otherwise use original rate
        let audio_chunk = AudioChunk {
            data: mono_data,  // Raw audio (resampled if needed), no gain yet
            sample_rate: if self.needs_resampling { 48000 } else { self.sample_rate },
            timestamp,
            chunk_id,
            device_type: self.device_type.clone(),
            dominant_source: None,
        };

        // NOTE: Raw audio is NOT sent to recording saver to prevent echo
        // Only the mixed audio (from AudioPipeline) is saved to file (see pipeline.rs:726-736)
        // This ensures we only record once: mic + system properly mixed
        // Individual raw streams go only to the transcription pipeline below

        // Send to processing pipeline for transcription
        if let Err(e) = self.state.send_audio_chunk(audio_chunk) {
            // Check if this is the "pipeline not ready" error
            if e.to_string().contains("Audio pipeline not ready") {
                // This is expected during initialization, just log it as debug
                debug!("Audio pipeline not ready yet, skipping chunk {}", chunk_id);
                return;
            }

            warn!("Failed to send audio chunk: {}", e);
            // More specific error handling based on failure reason
            let error = if e.to_string().contains("channel closed") {
                AudioError::ChannelClosed
            } else if e.to_string().contains("full") {
                AudioError::BufferOverflow
            } else {
                AudioError::ProcessingFailed
            };
            self.state.report_error(error);
        } else {
            // ~200 calls/second on the realtime audio thread; compiled out of
            // release builds entirely.
            perf_trace!("Sent audio chunk {} ({} samples)", chunk_id, data.len());
        }
    }

    /// Handle stream errors with enhanced disconnect detection
    pub fn handle_stream_error(&self, error: cpal::StreamError) {
        error!("Audio stream error for {}: {}", self.device.name, error);

        let error_str = error.to_string().to_lowercase();

        // Enhanced error detection for device disconnection
        let audio_error = if error_str.contains("device is no longer available")
            || error_str.contains("device not found")
            || error_str.contains("device disconnected")
            || error_str.contains("no such device")
            || error_str.contains("device unavailable")
            || error_str.contains("device removed")
        {
            warn!("🔌 Device disconnect detected for: {}", self.device.name);
            AudioError::DeviceDisconnected
        } else if error_str.contains("permission") || error_str.contains("access denied") {
            AudioError::PermissionDenied
        } else if error_str.contains("channel closed") {
            AudioError::ChannelClosed
        } else if error_str.contains("stream") && error_str.contains("failed") {
            AudioError::StreamFailed
        } else {
            warn!("Unknown audio error: {}", error);
            AudioError::StreamFailed
        };

        self.state.report_error(audio_error);
    }
}

/// VAD-driven audio processing pipeline
/// Uses Voice Activity Detection to segment speech in real-time and send only speech to Whisper
pub struct AudioPipeline {
    receiver: mpsc::UnboundedReceiver<AudioChunk>,
    transcription_sender: mpsc::UnboundedSender<AudioChunk>,
    state: Arc<RecordingState>,
    /// `None` when realtime transcription is off. VAD is only useful for
    /// producing transcription segments, and Silero runs a neural inference per
    /// 30ms frame — so with transcription disabled we skip building it at all
    /// rather than feed a channel whose receiver has already been dropped.
    vad_processor: Option<ContinuousVadProcessor>,
    sample_rate: u32,
    chunk_id_counter: u64,
    // Performance optimization: reduce logging frequency
    last_summary_time: std::time::Instant,
    processed_chunks: u64,
    // Smart batching for audio metrics
    metrics_batcher: Option<AudioMetricsBatcher>,
    // PROFESSIONAL AUDIO MIXING: Ring buffer + RMS-based mixer
    ring_buffer: AudioMixerRingBuffer,
    mixer: ProfessionalAudioMixer,
    // Recording sender for pre-mixed audio
    recording_sender_for_mixed: Option<mpsc::UnboundedSender<AudioChunk>>,
    /// Continuous mixed audio for a streaming transcription provider (Gemini
    /// Live). Deliberately fed from the mixed stream rather than the VAD
    /// segments: a streaming recognizer runs its own endpointing and needs
    /// unbroken audio, and silence-stripped segments would delay every interim
    /// caption until after the utterance had already finished. `None` for
    /// every non-streaming provider.
    live_sender_for_mixed: Option<mpsc::UnboundedSender<AudioChunk>>,
    // Me/Others speaker attribution: label each pre-mix window by dominant
    // source, aggregate across the windows behind each VAD segment.
    window_labeler: super::source_attribution::WindowLabeler,
    segment_aggregator: super::source_attribution::SegmentAggregator,
    /// Reused across mix windows so the hot loop allocates nothing. The mixed
    /// buffer is still moved out per window (it becomes the recording chunk),
    /// but these two no longer are.
    mic_window: Vec<f32>,
    sys_window: Vec<f32>,
}

impl AudioPipeline {
    pub fn new(
        receiver: mpsc::UnboundedReceiver<AudioChunk>,
        transcription_sender: mpsc::UnboundedSender<AudioChunk>,
        state: Arc<RecordingState>,
        target_chunk_duration_ms: u32,
        sample_rate: u32,
        mic_device_name: String,
        mic_device_kind: super::device_detection::InputDeviceKind,
        system_device_name: String,
        system_device_kind: super::device_detection::InputDeviceKind,
        vad_enabled: bool,
    ) -> Self {
        // Log device characteristics for adaptive buffering
        info!("🎛️ AudioPipeline initializing with device characteristics:");
        info!("   Mic: '{}' ({:?}) - Buffer: {:?}",
              mic_device_name, mic_device_kind, mic_device_kind.buffer_timeout());
        info!("   System: '{}' ({:?}) - Buffer: {:?}",
              system_device_name, system_device_kind, system_device_kind.buffer_timeout());

        // Device kind information can be used for adaptive buffering in the future
        // For now, we log it for monitoring and potential optimization
        let _ = (mic_device_name, mic_device_kind, system_device_name, system_device_kind);

        // Create VAD processor with balanced redemption time for speech accumulation
        // The VAD processor now handles 48kHz->16kHz resampling internally
        // This bridges natural pauses without excessive fragmentation
        // For mac os core audio, 900ms, for windows 400ms seems good

        let redemption_time = if cfg!(target_os = "macos") { 400 } else { 400 };

        let vad_processor = if vad_enabled {
            match ContinuousVadProcessor::new(sample_rate, redemption_time) {
                Ok(processor) => {
                    info!("VAD-driven pipeline: VAD segments will be sent directly to Whisper (no time-based accumulation)");
                    Some(processor)
                }
                Err(e) => {
                    error!("Failed to create VAD processor: {}", e);
                    panic!("VAD processor creation failed: {}", e);
                }
            }
        } else {
            info!("Realtime transcription disabled: skipping VAD, speaker attribution, and segment dispatch (recording/mixing unaffected)");
            None
        };

        // Initialize professional audio mixing components
        let ring_buffer = AudioMixerRingBuffer::new(sample_rate);
        let mixer = ProfessionalAudioMixer::new(sample_rate);

        // Note: target_chunk_duration_ms is ignored - VAD controls segmentation now
        let _ = target_chunk_duration_ms;

        Self {
            receiver,
            transcription_sender,
            state,
            vad_processor,
            sample_rate,
            chunk_id_counter: 0,
            // Performance optimization: reduce logging frequency
            last_summary_time: std::time::Instant::now(),
            processed_chunks: 0,
            // Disabled: the batcher costs a full-buffer pass, an Instant::now()
            // and an unbounded-channel send on every chunk (~200/s), and the
            // summaries it accumulates have no reader — get_summaries() and
            // clear_summaries() are never called. Re-enable alongside a consumer.
            metrics_batcher: None,
            // Initialize professional audio mixing
            ring_buffer,
            mixer,
            recording_sender_for_mixed: None,  // Will be set by manager
            live_sender_for_mixed: None,       // Will be set by manager
            window_labeler: super::source_attribution::WindowLabeler::new(),
            segment_aggregator: super::source_attribution::SegmentAggregator::new(),
            mic_window: Vec::new(),
            sys_window: Vec::new(),
        }
    }

    /// Run the VAD-driven audio processing pipeline
    pub async fn run(mut self) -> Result<()> {
        info!("VAD-driven audio pipeline started - segments sent in real-time based on speech detection");

        // CRITICAL FIX: Continue processing until channel is closed, not based on recording state
        // This ensures ALL chunks are processed during shutdown, fixing premature meeting completion
        // Previous bug: Loop checked `while self.state.is_recording()` which caused early exit when
        // stop_recording() was called, losing flush signals and remaining chunks in the pipeline
        loop {
            // Block until the next chunk. There is no periodic work to do here —
            // VAD drives all segmentation — so the previous 50ms timeout only
            // armed and cancelled a timer per chunk (~200/s) and woke this task
            // 20x/s through silence.
            match self.receiver.recv().await {
                Some(chunk) => {
                    // PERFORMANCE: Check for flush signal (special chunk with ID >= u64::MAX - 10)
                    // Multiple flush signals may be sent to ensure processing
                    if chunk.chunk_id >= u64::MAX - 10 {
                        info!("📥 Received FLUSH signal #{} - flushing VAD processor", u64::MAX - chunk.chunk_id);
                        self.flush_remaining_audio()?;
                        // Continue processing to handle any remaining chunks
                        continue;
                    }

                    // PERFORMANCE OPTIMIZATION: Eliminate per-chunk logging overhead
                    // Logging in hot paths causes severe performance degradation
                    self.processed_chunks += 1;

                    // Smart batching: collect metrics instead of logging every chunk
                    if let Some(ref batcher) = self.metrics_batcher {
                        let avg_level = chunk.data.iter().map(|&x| x.abs()).sum::<f32>() / chunk.data.len() as f32;
                        let duration_ms = chunk.data.len() as f64 / chunk.sample_rate as f64 * 1000.0;

                        batch_audio_metric!(
                            Some(batcher),
                            chunk.chunk_id,
                            chunk.data.len(),
                            duration_ms,
                            avg_level
                        );
                    }

                    // CRITICAL: Log summary only every 200 chunks OR every 60 seconds (99.5% reduction)
                    // This eliminates I/O overhead in the audio processing hot path
                    // Use performance-optimized debug macro that compiles to nothing in release builds
                    if self.processed_chunks % 200 == 0 || self.last_summary_time.elapsed().as_secs() >= 60 {
                        perf_debug!("Pipeline processed {} chunks, current chunk: {} ({} samples)",
                                   self.processed_chunks, chunk.chunk_id, chunk.data.len());
                        self.last_summary_time = std::time::Instant::now();
                    }

                    // Nobody downstream: no VAD to feed, no encoder to write to,
                    // and no live stream to send. Mixing here would be pure heat
                    // — drop the samples and keep draining so the capture
                    // threads never block.
                    //
                    // The live sender must be part of this test: with Gemini
                    // Live the VAD processor is deliberately absent, so omitting
                    // it here would skip mixing entirely and stream silence.
                    if self.vad_processor.is_none()
                        && self.recording_sender_for_mixed.is_none()
                        && self.live_sender_for_mixed.is_none()
                    {
                        continue;
                    }

                    // STEP 1: Add raw audio to ring buffer for mixing
                    // Microphone audio is already normalized at capture level (AudioCapture)
                    // System audio remains raw
                    self.ring_buffer.add_samples(chunk.device_type.clone(), chunk.data);

                    // STEP 2: Mix audio in fixed windows when both streams have sufficient data
                    while self.ring_buffer.can_mix() {
                        // `mic_window`/`sys_window` are reused buffers owned by
                        // self; move them out for the duration of the body so
                        // the mixer and labeler can borrow self mutably.
                        let mut mic_window = std::mem::take(&mut self.mic_window);
                        let mut sys_window = std::mem::take(&mut self.sys_window);

                        let extracted = self
                            .ring_buffer
                            .extract_window_into(&mut mic_window, &mut sys_window);

                        if extracted {
                            // Speaker attribution only labels transcript segments,
                            // so it costs two RMS passes we can skip when there
                            // will be no transcripts.
                            let window_label = if self.vad_processor.is_some() {
                                Some(self.window_labeler.label(&mic_window, &sys_window))
                            } else {
                                None
                            };

                            // Simple mixing without aggressive ducking.
                            // NO POST-GAIN NEEDED: Microphone already normalized by EBU R128 to -23 LUFS
                            // (broadcast-standard loudness); system audio at natural levels.
                            let mut mixed_with_gain = Vec::new();
                            self.mixer
                                .mix_window_into(&mic_window, &sys_window, &mut mixed_with_gain);

                            // Weight the window's label by its (mixed) energy so
                            // loud speech outvotes quiet crosstalk per segment.
                            if let Some(window_label) = window_label {
                                let window_energy: f32 =
                                    mixed_with_gain.iter().map(|s| s * s).sum();
                                self.segment_aggregator.add(window_label, window_energy);
                            }

                            // STEP 3: Send mixed audio for transcription (VAD + Whisper).
                            // Skipped entirely when realtime transcription is off.
                            // `map` ends the borrow of `self.vad_processor` before
                            // the body touches `self.segment_aggregator`.
                            let vad_result = self
                                .vad_processor
                                .as_mut()
                                .map(|vad| vad.process_audio(&mixed_with_gain));

                            match vad_result {
                                Some(Ok(speech_segments)) => {
                                    // One label per emission batch: the windows
                                    // accumulated since the last VAD segment(s)
                                    // back everything emitted now. (VAD segment
                                    // boundaries can drift a window either way —
                                    // acceptable for a binary Me/Others label.)
                                    let batch_source = if speech_segments.is_empty() {
                                        None
                                    } else {
                                        self.segment_aggregator.finalize()
                                    };
                                    for segment in speech_segments {
                                        let duration_ms = segment.end_timestamp_ms - segment.start_timestamp_ms;

                                        if segment.samples.len() >= 800 {  // Minimum 50ms at 16kHz - matches Parakeet capability
                                            info!("📤 Sending VAD segment: {:.1}ms, {} samples",
                                                  duration_ms, segment.samples.len());

                                            let transcription_chunk = AudioChunk {
                                                data: segment.samples,
                                                sample_rate: 16000,
                                                timestamp: segment.start_timestamp_ms / 1000.0,
                                                chunk_id: self.chunk_id_counter,
                                                device_type: DeviceType::Microphone,  // Mixed audio
                                                dominant_source: batch_source.clone(),
                                            };

                                            if let Err(e) = self.transcription_sender.send(transcription_chunk) {
                                                warn!("Failed to send VAD segment: {}", e);
                                            } else {
                                                self.chunk_id_counter += 1;
                                            }
                                        } else {
                                            debug!("⏭️ Dropping short VAD segment: {:.1}ms ({} samples < 800)",
                                                   duration_ms, segment.samples.len());
                                        }
                                    }
                                }
                                Some(Err(e)) => {
                                    warn!("⚠️ VAD error: {}", e);
                                }
                                // Realtime transcription disabled — no VAD.
                                None => {}
                            }

                            // STEP 3b: Send the continuous mixed window to a
                            // streaming transcription provider. Read by
                            // reference — STEP 4 still needs to move the buffer
                            // — and resampled by the live session, which owns
                            // the wire format.
                            if let Some(ref sender) = self.live_sender_for_mixed {
                                let live_chunk = AudioChunk {
                                    data: mixed_with_gain.clone(),
                                    sample_rate: self.sample_rate,
                                    timestamp: chunk.timestamp,
                                    chunk_id: self.chunk_id_counter,
                                    device_type: DeviceType::Microphone,  // Mixed audio
                                    // Mixed audio has no trustworthy per-window
                                    // attribution, and the streaming provider
                                    // does not use one.
                                    dominant_source: None,
                                };
                                let _ = sender.send(live_chunk);
                            }

                            // STEP 4: Send mixed audio to the encoder.
                            // Last use of the window, so move it instead of
                            // cloning another 115 KB per window.
                            if let Some(ref sender) = self.recording_sender_for_mixed {
                                let recording_chunk = AudioChunk {
                                    data: mixed_with_gain,
                                    sample_rate: self.sample_rate,
                                    timestamp: chunk.timestamp,
                                    chunk_id: self.chunk_id_counter,
                                    device_type: DeviceType::Microphone,  // Mixed audio
                                    dominant_source: None,
                                };
                                let _ = sender.send(recording_chunk);
                            }
                        }

                        // Hand the window buffers back for the next iteration.
                        // Their capacity is what makes this loop allocation-free.
                        self.mic_window = mic_window;
                        self.sys_window = sys_window;

                        if !extracted {
                            break;
                        }
                    }
                }
                None => {
                    info!("Audio pipeline: sender closed after processing {} chunks", self.processed_chunks);
                    break;
                }
            }
        }

        // Flush any remaining VAD segments
        self.flush_remaining_audio()?;

        info!("VAD-driven audio pipeline ended");
        Ok(())
    }

    fn flush_remaining_audio(&mut self) -> Result<()> {
        info!("Flushing remaining audio from pipeline (processed {} chunks)", self.processed_chunks);

        // Flush any remaining audio from VAD processor and send segments to
        // transcription. No VAD means no pending segments to flush.
        let flushed = self.vad_processor.as_mut().map(|vad| vad.flush());
        let Some(flushed) = flushed else {
            return Ok(());
        };

        match flushed {
            Ok(final_segments) => {
                let batch_source = if final_segments.is_empty() {
                    None
                } else {
                    self.segment_aggregator.finalize()
                };
                for segment in final_segments {
                    let duration_ms = segment.end_timestamp_ms - segment.start_timestamp_ms;

                    // Send segments >= 50ms (800 samples at 16kHz) - matches main pipeline filter
                    if segment.samples.len() >= 800 {
                        info!("📤 Sending final VAD segment to Whisper: {:.1}ms duration, {} samples",
                              duration_ms, segment.samples.len());

                        let transcription_chunk = AudioChunk {
                            data: segment.samples,
                            sample_rate: 16000,
                            timestamp: segment.start_timestamp_ms / 1000.0,
                            chunk_id: self.chunk_id_counter,
                            device_type: DeviceType::Microphone,
                            dominant_source: batch_source.clone(),
                        };

                        if let Err(e) = self.transcription_sender.send(transcription_chunk) {
                            warn!("Failed to send final VAD segment: {}", e);
                        } else {
                            self.chunk_id_counter += 1;
                        }
                    } else {
                        info!("⏭️ Skipping short final segment: {:.1}ms ({} samples < 800)",
                              duration_ms, segment.samples.len());
                    }
                }
            }
            Err(e) => {
                warn!("Failed to flush VAD processor: {}", e);
            }
        }

        Ok(())
    }

}

/// Simple audio pipeline manager
pub struct AudioPipelineManager {
    pipeline_handle: Option<JoinHandle<Result<()>>>,
    audio_sender: Option<mpsc::UnboundedSender<AudioChunk>>,
}

impl AudioPipelineManager {
    pub fn new() -> Self {
        Self {
            pipeline_handle: None,
            audio_sender: None,
        }
    }

    /// Start the audio pipeline with device information for adaptive buffering
    pub fn start(
        &mut self,
        state: Arc<RecordingState>,
        transcription_sender: mpsc::UnboundedSender<AudioChunk>,
        target_chunk_duration_ms: u32,
        sample_rate: u32,
        recording_sender: Option<mpsc::UnboundedSender<AudioChunk>>,
        mic_device_name: String,
        mic_device_kind: super::device_detection::InputDeviceKind,
        system_device_name: String,
        system_device_kind: super::device_detection::InputDeviceKind,
        vad_enabled: bool,
        streaming_live: bool,
    ) -> Result<()> {
        // Log device information for adaptive buffering
        info!("🎙️ Starting pipeline with device info:");
        info!("   Microphone: '{}' ({:?})", mic_device_name, mic_device_kind);
        info!("   System Audio: '{}' ({:?})", system_device_name, system_device_kind);

        // Create audio processing channel
        let (audio_sender, audio_receiver) = mpsc::unbounded_channel::<AudioChunk>();

        // Set sender in state for audio captures to use
        state.set_audio_sender(audio_sender.clone());

        // A streaming provider runs its own endpointing on continuous audio, so
        // local VAD is both unnecessary and harmful (it would strip the silence
        // the recognizer uses to detect utterance boundaries).
        let vad_enabled = vad_enabled && !streaming_live;

        // Create and start pipeline with device information for adaptive mixing
        let mut pipeline = AudioPipeline::new(
            audio_receiver,
            transcription_sender.clone(),
            state.clone(),
            target_chunk_duration_ms,
            sample_rate,
            mic_device_name,
            mic_device_kind,
            system_device_name,
            system_device_kind,
            vad_enabled,
        );

        // CRITICAL FIX: Connect recording sender to receive pre-mixed audio
        // This ensures both mic AND system audio are captured in recordings
        pipeline.recording_sender_for_mixed = recording_sender;

        // In streaming mode the transcription channel carries the continuous
        // mixed stream instead of VAD segments; the consumer
        // (start_transcription_task) branches on the same configuration.
        if streaming_live {
            info!("🔊 Streaming transcription: feeding continuous mixed audio to the live session (VAD bypassed)");
            pipeline.live_sender_for_mixed = Some(transcription_sender);
        }

        let handle = tokio::spawn(async move {
            pipeline.run().await
        });

        self.pipeline_handle = Some(handle);
        self.audio_sender = Some(audio_sender);

        info!("Audio pipeline manager started with mixed audio recording");
        Ok(())
    }

    /// Stop the audio pipeline
    pub async fn stop(&mut self) -> Result<()> {
        // Drop the sender to close the pipeline
        self.audio_sender = None;

        // Wait for pipeline to finish
        if let Some(handle) = self.pipeline_handle.take() {
            match handle.await {
                Ok(result) => result,
                Err(e) => {
                    error!("Pipeline task failed: {}", e);
                    Ok(())
                }
            }
        } else {
            Ok(())
        }
    }

    /// Force immediate flush of accumulated audio and stop pipeline
    /// PERFORMANCE CRITICAL: Eliminates 30+ second shutdown delays
    pub async fn force_flush_and_stop(&mut self) -> Result<()> {
        info!("🚀 Force flushing pipeline - processing ALL accumulated audio immediately");

        // If we have a sender, send a special flush signal first
        if let Some(sender) = &self.audio_sender {
            // Create a special flush chunk to trigger immediate processing
            let flush_chunk = AudioChunk {
                data: vec![], // Empty data signals flush
                sample_rate: 16000,
                timestamp: 0.0,
                chunk_id: u64::MAX, // Special ID to indicate flush
                device_type: super::recording_state::DeviceType::Microphone,
                dominant_source: None,
            };

            if let Err(e) = sender.send(flush_chunk) {
                warn!("Failed to send flush signal: {}", e);
            } else {
                info!("📤 Sent flush signal to pipeline");

                // PERFORMANCE OPTIMIZATION: Reduced wait time from 50ms to 20ms
                // Pipeline should process flush signal very quickly
                tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;

                // Send multiple flush signals to ensure the pipeline catches it
                // This aggressive approach eliminates shutdown delay issues
                for i in 0..3 {
                    let additional_flush = AudioChunk {
                        data: vec![],
                        sample_rate: 16000,
                        timestamp: 0.0,
                        chunk_id: u64::MAX - (i as u64),
                        device_type: super::recording_state::DeviceType::Microphone,
                        dominant_source: None,
                    };
                    let _ = sender.send(additional_flush);
                }

                info!("📤 Sent additional flush signals for reliability");
                tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
            }
        }

        // Now stop normally
        self.stop().await
    }
}

impl Default for AudioPipelineManager {
    fn default() -> Self {
        Self::new()
    }
}