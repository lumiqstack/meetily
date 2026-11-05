//! Streaming Sortformer speaker diarization (NVIDIA Nemotron-3 Diarization) on ONNX Runtime.
//!
//! Adapted from the `sortformer` module of parakeet-rs 0.4.4 by Enes Altun
//! (https://github.com/altunenes/parakeet-rs, MIT OR Apache-2.0), reduced to the ONNX
//! Runtime backend and ported to the `ort` release Meetily already links. The model
//! itself is NVIDIA's (OpenMDW-1.1): https://huggingface.co/nvidia/Nemotron-3-Diarization
//!
//! How it works:
//! - The model consumes 128-bin log-mel features and keeps two pieces of state between
//!   calls: a FIFO of recent frame embeddings and an "arrival-order speaker cache" that is
//!   compressed with NeMo's scoring heuristics when it overflows.
//! - Speakers are tracked at 80 ms frames (which drive the cache) and predicted at 10 ms
//!   frames (what this module returns). Up to eight speakers, IDs ordered by first
//!   appearance in the recording.
//! - Two export flavours are accepted. parakeet-rs's has dynamic time axes and names its
//!   outputs `preds_diar` / `preds_hires` / `chunk_pre_encode_embs`; beshkenadze's
//!   (https://huggingface.co/beshkenadze/nemotron-3-diarization-onnx, the one Meetily
//!   ships, opset 17 so it loads on ONNX Runtime 1.22) fixes every axis to the offline
//!   preset and names them `preds` / `preds_hr` / `chunk_embs`. For a fixed graph the
//!   state is zero-padded to the declared length and the real length passed alongside,
//!   which is NeMo's `async_pad_to_max` computation.
//! - Meetily runs the model after a meeting, so it uses NVIDIA's offline preset: a 30.4 s
//!   window, which gives the best accuracy.

use anyhow::{anyhow, Context, Result};
use ndarray::{s, Array1, Array2, Array3, Axis};
use ort::session::Session;
use ort::value::ValueType;
use ort::value::TensorRef;
use realfft::RealFftPlanner;
use std::collections::HashMap;

// Feature extraction (NeMo preprocessor settings baked into the checkpoint)
const N_FFT: usize = 512;
const WIN_LENGTH: usize = 400;
const HOP_LENGTH: usize = 160;
const N_MELS: usize = 128;
const PREEMPH: f32 = 0.97;
const LOG_ZERO_GUARD: f32 = 5.960_464_5e-8;
pub const SAMPLE_RATE: usize = 16000;

// Streaming defaults (NVIDIA's offline preset; the ONNX metadata overrides them)
const CHUNK_LEN: usize = 340;
const FIFO_LEN: usize = 40;
const SPKCACHE_LEN: usize = 264;
const RIGHT_CONTEXT: usize = 40;
const SPKCACHE_UPDATE_PERIOD: usize = 300;
const SUBSAMPLING: usize = 8; // mel frames (10 ms) per model frame (80 ms)
const UPSAMPLE_FACTOR: usize = 8; // model frames -> output frames
const EMB_DIM: usize = 512;
pub const NUM_SPEAKERS: usize = 8;
const FRAME_DURATION: f32 = 0.08;

/// The checkpoint's learned silence embedding (512 little-endian f32), used for unused
/// speaker-cache slots. Exports without it in their metadata fall back to this copy, taken
/// from beshkenadze's export of the same checkpoint.
const SIL_EMB_BYTES: &[u8] = include_bytes!("nemotron3_learnable_sil_emb.f32");

/// Output names of the two accepted exports: (80 ms preds, 10 ms preds, chunk embeddings).
const OUTPUT_NAMES: [[&str; 3]; 2] = [
    ["preds_diar", "preds_hires", "chunk_pre_encode_embs"],
    ["preds", "preds_hr", "chunk_embs"],
];

// Speaker-cache compression parameters (from NeMo)
const SPKCACHE_SIL_FRAMES_PER_SPK: usize = 1;
const PRED_SCORE_THRESHOLD: f32 = 0.25;
const STRONG_BOOST_RATE: f32 = 0.75;
const WEAK_BOOST_RATE: f32 = 1.5;
const MIN_POS_SCORES_RATE: f32 = 0.5;
const SCORES_BOOST_LATEST: f32 = 0.05;
const MAX_INDEX: usize = 99999;

/// Round to the nearest bfloat16 value. The checkpoint stores the STFT window and mel
/// filterbank in bf16, so the features only match NeMo when ours are rounded the same way.
fn to_bf16(x: f32) -> f32 {
    let bits = x.to_bits();
    f32::from_bits((bits + 0x7FFF + ((bits >> 16) & 1)) & 0xFFFF_0000)
}

/// How raw per-frame speaker probabilities become segments.
///
/// The default reproduces NeMo's `diarize()` for this model: a plain 0.5 threshold with no
/// padding, duration filtering or smoothing.
#[derive(Debug, Clone)]
pub struct DiarizationConfig {
    /// Probability above which a speaker segment starts.
    pub onset: f32,
    /// Probability below which a speaker segment ends.
    pub offset: f32,
    pub pad_onset: f32,
    pub pad_offset: f32,
    /// Segments shorter than this (seconds) are dropped.
    pub min_duration_on: f32,
    /// Gaps shorter than this (seconds) between a speaker's segments are merged.
    pub min_duration_off: f32,
    /// Median smoothing window in 10 ms frames; `<= 1` disables it.
    pub median_window: usize,
}

impl Default for DiarizationConfig {
    fn default() -> Self {
        Self {
            onset: 0.5,
            offset: 0.5,
            pad_onset: 0.0,
            pad_offset: 0.0,
            min_duration_on: 0.0,
            min_duration_off: 0.0,
            median_window: 1,
        }
    }
}

/// A stretch of one speaker's activity, as sample offsets at 16 kHz.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpeakerSegment {
    pub start: u64,
    pub end: u64,
    pub speaker_id: usize,
}

/// Streaming Sortformer engine bound to one ONNX Runtime session.
pub struct Sortformer {
    session: Session,
    config: DiarizationConfig,
    chunk_len: usize,
    fifo_len: usize,
    spkcache_len: usize,
    right_context: usize,
    spkcache_update_period: usize,
    /// Learned embedding for silence / unused cache slots.
    sil_emb: Array1<f32>,
    /// Output names of this export, see [`OUTPUT_NAMES`].
    outputs: [&'static str; 3],
    /// Declared lengths of the `chunk`, `spkcache` and `fifo` inputs when the graph fixes
    /// them; `None` for a dynamic axis.
    fixed_chunk: Option<usize>,
    fixed_spkcache: Option<usize>,
    fixed_fifo: Option<usize>,
    // Streaming state, kept exactly as NeMo does.
    spkcache: Array3<f32>,               // (1, 0..spkcache_len, EMB_DIM)
    spkcache_preds: Option<Array3<f32>>, // (1, 0..spkcache_len, NUM_SPEAKERS), 80 ms
    fifo: Array3<f32>,                   // (1, 0..fifo_len, EMB_DIM)
    fifo_preds: Array3<f32>,             // (1, 0..fifo_len, NUM_SPEAKERS), 80 ms
    // Buffered audio for `feed` / `flush`.
    audio_buffer: Vec<f32>,
    elapsed_samples: usize,
    mel_basis: Array2<f32>,
}

impl Sortformer {
    /// Wrap an already-built session. The caller chooses execution providers and threads.
    pub fn from_session(session: Session, config: DiarizationConfig) -> Result<Self> {
        let has_output = |name: &str| session.outputs.iter().any(|o| o.name == name);
        let outputs = OUTPUT_NAMES
            .into_iter()
            .find(|names| names.iter().all(|n| has_output(n)))
            .ok_or_else(|| {
                anyhow!(
                    "this ONNX has none of the expected output sets {OUTPUT_NAMES:?}; it is \
                     not a Nemotron-3 Diarization step export"
                )
            })?;
        let fixed_len = |input: &str| -> Option<usize> {
            session.inputs.iter().find(|i| i.name == input).and_then(|i| match &i.input_type {
                ValueType::Tensor { shape, .. } => {
                    let dims: &[i64] = shape;
                    dims.get(1).copied().filter(|d| *d > 0).map(|d| d as usize)
                }
                _ => None,
            })
        };
        let fixed_chunk = fixed_len("chunk");
        let fixed_spkcache = fixed_len("spkcache");
        let fixed_fifo = fixed_len("fifo");

        let metadata: HashMap<String, String> = match session.metadata() {
            Ok(meta) => meta
                .custom_keys()
                .unwrap_or_default()
                .into_iter()
                .filter_map(|key| meta.custom(&key).ok().flatten().map(|v| (key, v)))
                .collect(),
            Err(_) => HashMap::new(),
        };

        let get = |key: &str, default: usize| -> usize {
            metadata
                .get(key)
                .and_then(|v| v.parse().ok())
                .unwrap_or(default)
        };
        let sil_emb = metadata
            .get("learnable_sil_emb")
            .map(|raw| {
                raw.split(',')
                    .filter_map(|v| v.trim().parse().ok())
                    .collect::<Vec<f32>>()
            })
            .filter(|v| v.len() == EMB_DIM)
            .map(Array1::from_vec)
            .unwrap_or_else(|| {
                Array1::from_iter(
                    SIL_EMB_BYTES
                        .chunks_exact(4)
                        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])),
                )
            });
        debug_assert_eq!(sil_emb.len(), EMB_DIM);

        let mut instance = Self {
            session,
            config,
            chunk_len: get("chunk_len", CHUNK_LEN),
            fifo_len: get("fifo_len", FIFO_LEN),
            spkcache_len: get("spkcache_len", SPKCACHE_LEN),
            right_context: get("right_context", RIGHT_CONTEXT),
            spkcache_update_period: get("spkcache_update_period", SPKCACHE_UPDATE_PERIOD),
            sil_emb,
            outputs,
            fixed_chunk,
            fixed_spkcache,
            fixed_fifo,
            spkcache: Array3::zeros((1, 0, EMB_DIM)),
            spkcache_preds: None,
            fifo: Array3::zeros((1, 0, EMB_DIM)),
            fifo_preds: Array3::zeros((1, 0, NUM_SPEAKERS)),
            audio_buffer: Vec::new(),
            elapsed_samples: 0,
            mel_basis: create_mel_filterbank(N_FFT, N_MELS, SAMPLE_RATE).mapv(to_bf16),
        };
        if let Some(fixed) = instance.fixed_chunk {
            let window = (instance.chunk_len + instance.right_context) * SUBSAMPLING;
            if fixed != window {
                return Err(anyhow!(
                    "the graph fixes `chunk` to {fixed} mel frames but the streaming preset \
                     needs {window}; the export was made for a different preset"
                ));
            }
        }
        if let Some(fixed) = instance.fixed_spkcache {
            instance.spkcache_len = fixed;
        }
        if let Some(fixed) = instance.fixed_fifo {
            instance.fifo_len = fixed;
        }
        if instance.spkcache_len == 0 || instance.spkcache_len % NUM_SPEAKERS != 0 {
            return Err(anyhow!(
                "spkcache_len ({}) must be a positive multiple of {NUM_SPEAKERS}",
                instance.spkcache_len
            ));
        }
        instance.reset_state();
        Ok(instance)
    }

    /// Window latency in seconds: `(chunk_len + right_context) * 80 ms`.
    pub fn latency_seconds(&self) -> f32 {
        (self.chunk_len + self.right_context) as f32 * FRAME_DURATION
    }

    /// Samples per full streaming window: what `feed` needs before it runs the model once.
    pub fn window_samples(&self) -> usize {
        (self.chunk_len + self.right_context) * SUBSAMPLING * HOP_LENGTH
    }

    pub fn reset_state(&mut self) {
        self.spkcache = Array3::zeros((1, 0, EMB_DIM));
        self.spkcache_preds = None;
        self.fifo = Array3::zeros((1, 0, EMB_DIM));
        self.fifo_preds = Array3::zeros((1, 0, NUM_SPEAKERS));
        self.audio_buffer.clear();
        self.elapsed_samples = 0;
    }

    /// Feed 16 kHz mono samples. Runs the model once per full window and returns the
    /// segments of the windows that completed, with absolute sample offsets. Each window is
    /// binarized on its own, so a segment crossing a window edge comes back as two.
    pub fn feed(&mut self, audio_16k_mono: &[f32]) -> Result<Vec<SpeakerSegment>> {
        self.audio_buffer.extend_from_slice(audio_16k_mono);

        let feed_size = (self.chunk_len + self.right_context) * SUBSAMPLING;
        let stride_samples = self.chunk_len * SUBSAMPLING * HOP_LENGTH;
        let feed_samples = self.window_samples();

        let mut all_segments = Vec::new();
        while self.audio_buffer.len() >= feed_samples {
            let window = &self.audio_buffer[..feed_samples];
            let features = self.extract_mel_features(window)?;
            // STFT with center padding yields feed_size + 1 frames; keep exactly feed_size.
            let chunk_feat = features.slice(s![.., ..feed_size, ..]).to_owned();
            let chunk_preds = self.streaming_update(&chunk_feat, feed_size, false)?;

            let sample_offset = self.elapsed_samples as u64;
            let mut segments =
                Self::post_process(&self.config, &chunk_preds, stride_samples as u64);
            for seg in &mut segments {
                seg.start += sample_offset;
                seg.end += sample_offset;
            }
            all_segments.extend(segments);

            self.audio_buffer.drain(..stride_samples);
            self.elapsed_samples += stride_samples;
        }
        Ok(all_segments)
    }

    /// Process whatever is still buffered at the end of the stream.
    pub fn flush(&mut self) -> Result<Vec<SpeakerSegment>> {
        if self.audio_buffer.is_empty() {
            return Ok(vec![]);
        }
        let feed_size = (self.chunk_len + self.right_context) * SUBSAMPLING;
        let remaining = std::mem::take(&mut self.audio_buffer);

        let features = self.extract_mel_features(&remaining)?;
        let current_len = features.shape()[1].min(feed_size);
        let chunk_feat = Self::pad_chunk(features.slice(s![.., ..current_len, ..]));
        let chunk_preds = self.streaming_update(&chunk_feat, current_len, true)?;

        let sample_offset = self.elapsed_samples as u64;
        let mut segments = Self::post_process(&self.config, &chunk_preds, remaining.len() as u64);
        for seg in &mut segments {
            seg.start += sample_offset;
            seg.end += sample_offset;
        }
        self.elapsed_samples += remaining.len();
        Ok(segments)
    }

    /// Diarize a whole recording, streaming it through the model window by window so the
    /// mel features of a long meeting never sit in memory at once. `on_progress` receives
    /// the fraction of samples consumed and returns `false` to cancel.
    pub fn diarize_all(
        &mut self,
        audio_16k_mono: &[f32],
        mut on_progress: impl FnMut(f32) -> bool,
    ) -> Result<Vec<SpeakerSegment>> {
        self.reset_state();
        let total = audio_16k_mono.len().max(1) as f32;
        let step = self.window_samples();
        let mut segments = Vec::new();
        for (i, slice) in audio_16k_mono.chunks(step).enumerate() {
            segments.extend(self.feed(slice)?);
            let consumed = ((i + 1) * step).min(audio_16k_mono.len()) as f32;
            if !on_progress(consumed / total) {
                return Err(anyhow!("Diarization cancelled"));
            }
        }
        segments.extend(self.flush()?);
        segments.sort_by_key(|s| (s.start, s.speaker_id));
        Ok(segments)
    }

    /// Median-smooth and binarize raw 10 ms speaker probabilities into segments, clipped to
    /// the audio length.
    pub fn post_process(
        config: &DiarizationConfig,
        preds: &Array2<f32>,
        n_audio_samples: u64,
    ) -> Vec<SpeakerSegment> {
        let filtered_owned;
        let filtered = if config.median_window > 1 {
            filtered_owned = Self::median_filter(config, preds);
            &filtered_owned
        } else {
            preds
        };
        let mut segments = Self::binarize(config, filtered);
        for seg in &mut segments {
            seg.end = seg.end.min(n_audio_samples);
        }
        segments.retain(|s| s.end > s.start);
        segments
    }

    /// Zero-pad a short (final) chunk to whole 80 ms frames, as NeMo's pre-encoder does.
    fn pad_chunk(chunk: ndarray::ArrayView3<f32>) -> Array3<f32> {
        let len = chunk.shape()[1];
        let target = len.next_multiple_of(SUBSAMPLING);
        if len == target {
            return chunk.to_owned();
        }
        let mut padded = Array3::zeros((1, target, N_MELS));
        padded.slice_mut(s![.., ..len, ..]).assign(&chunk);
        padded
    }

    /// NeMo's `streaming_update`: one model call, then FIFO and speaker-cache bookkeeping.
    /// Returns the chunk's 10 ms predictions. In a full window the trailing right-context
    /// frames only served as lookahead and belong to the next window; in the final chunk
    /// (`is_last`) nothing follows, so every valid frame is kept.
    fn streaming_update(
        &mut self,
        chunk_feat: &Array3<f32>,
        current_len: usize,
        is_last: bool,
    ) -> Result<Array2<f32>> {
        let spkcache_len = self.spkcache.shape()[1];
        let fifo_len = self.fifo.shape()[1];

        let chunk_lengths = Array1::from_vec(vec![current_len as i64]);
        let spkcache_lengths = Array1::from_vec(vec![spkcache_len as i64]);
        let fifo_lengths = Array1::from_vec(vec![fifo_len as i64]);

        // A fixed-shape graph takes every state tensor at its declared length, zero-padded,
        // with the live length alongside. Inside, the model packs the live cache, live FIFO
        // and chunk back to back and pads the total, so the outputs sit at the live offsets
        // (checked: reading them at the padded offsets drops all but the last 6 s of the
        // first window).
        let chunk_in = pad_axis1(chunk_feat, self.fixed_chunk.unwrap_or(chunk_feat.shape()[1]));
        let spkcache_in = pad_axis1(&self.spkcache, self.fixed_spkcache.unwrap_or(spkcache_len));
        let fifo_in = pad_axis1(&self.fifo, self.fixed_fifo.unwrap_or(fifo_len));

        let (preds_diar, preds_hires, new_embs) = {
            let inputs = ort::inputs![
                "chunk" => TensorRef::from_array_view(chunk_in.view())?,
                "chunk_lengths" => TensorRef::from_array_view(chunk_lengths.view())?,
                "spkcache" => TensorRef::from_array_view(spkcache_in.view())?,
                "spkcache_lengths" => TensorRef::from_array_view(spkcache_lengths.view())?,
                "fifo" => TensorRef::from_array_view(fifo_in.view())?,
                "fifo_lengths" => TensorRef::from_array_view(fifo_lengths.view())?,
            ];
            let outputs = self.session.run(inputs).context("Sortformer inference failed")?;
            (
                extract_3d_f32(&outputs, self.outputs[0])?,
                extract_3d_f32(&outputs, self.outputs[1])?,
                extract_3d_f32(&outputs, self.outputs[2])?,
            )
        };

        let valid_frames = current_len.div_ceil(SUBSAMPLING);
        let keep = if is_last {
            valid_frames.min(self.chunk_len + self.right_context)
        } else {
            self.chunk_len.min(valid_frames)
        };

        let fifo_preds = if fifo_len > 0 {
            preds_diar
                .slice(s![0, spkcache_len..spkcache_len + fifo_len, ..])
                .to_owned()
        } else {
            Array2::zeros((0, NUM_SPEAKERS))
        };
        let chunk_start = spkcache_len + fifo_len;
        let chunk_preds = preds_diar
            .slice(s![0, chunk_start..chunk_start + keep, ..])
            .to_owned();
        let chunk_embs = new_embs.slice(s![0, ..keep, ..]).to_owned();

        let hires_start = chunk_start * UPSAMPLE_FACTOR;
        let chunk_preds_hires = preds_hires
            .slice(s![0, hires_start..hires_start + keep * UPSAMPLE_FACTOR, ..])
            .to_owned();

        self.fifo = concat_axis1(&self.fifo, &chunk_embs.insert_axis(Axis(0)));
        self.fifo_preds = if fifo_len > 0 {
            concat_rows(&fifo_preds, &chunk_preds).insert_axis(Axis(0))
        } else {
            chunk_preds.insert_axis(Axis(0))
        };

        let fifo_len_after = self.fifo.shape()[1];
        if fifo_len_after > self.fifo_len {
            let mut pop_out_len = self.spkcache_update_period;
            pop_out_len = pop_out_len.max((keep + fifo_len).saturating_sub(self.fifo_len));
            pop_out_len = pop_out_len.min(fifo_len_after);

            let pop_out_embs = self.fifo.slice(s![.., ..pop_out_len, ..]).to_owned();
            let pop_out_preds = self.fifo_preds.slice(s![.., ..pop_out_len, ..]).to_owned();
            self.fifo = self.fifo.slice(s![.., pop_out_len.., ..]).to_owned();
            self.fifo_preds = self.fifo_preds.slice(s![.., pop_out_len.., ..]).to_owned();

            self.spkcache = concat_axis1(&self.spkcache, &pop_out_embs);
            if let Some(cache_preds) = &self.spkcache_preds {
                self.spkcache_preds = Some(concat_axis1(cache_preds, &pop_out_preds));
            }

            if self.spkcache.shape()[1] > self.spkcache_len {
                if self.spkcache_preds.is_none() {
                    let initial = preds_diar.slice(s![.., ..spkcache_len, ..]).to_owned();
                    self.spkcache_preds = Some(concat_axis1(&initial, &pop_out_preds));
                }
                self.compress_spkcache();
            }
        }

        Ok(chunk_preds_hires)
    }

    /// NeMo's speaker-cache compression: keep the frames that most confidently belong to
    /// one speaker, favour recent ones slightly, and fill the rest with the silence embedding.
    fn compress_spkcache(&mut self) {
        let Some(cache_preds) = self.spkcache_preds.clone() else {
            return;
        };
        let n_frames = self.spkcache.shape()[1];
        let per_spk = self.spkcache_len / NUM_SPEAKERS;
        if per_spk <= SPKCACHE_SIL_FRAMES_PER_SPK {
            self.spkcache = self.spkcache.slice(s![.., ..self.spkcache_len, ..]).to_owned();
            if let Some(p) = &self.spkcache_preds {
                self.spkcache_preds = Some(p.slice(s![.., ..self.spkcache_len, ..]).to_owned());
            }
            return;
        }
        let spkcache_len_per_spk = per_spk - SPKCACHE_SIL_FRAMES_PER_SPK;
        let strong_boost_per_spk = (spkcache_len_per_spk as f32 * STRONG_BOOST_RATE) as usize;
        let weak_boost_per_spk = (spkcache_len_per_spk as f32 * WEAK_BOOST_RATE) as usize;
        let min_pos_scores_per_spk = (spkcache_len_per_spk as f32 * MIN_POS_SCORES_RATE) as usize;

        let preds_2d = cache_preds.slice(s![0, .., ..]).to_owned();
        let mut scores = log_pred_scores(&preds_2d);
        scores = disable_low_scores(&preds_2d, scores, min_pos_scores_per_spk);

        if SCORES_BOOST_LATEST > 0.0 {
            for t in self.spkcache_len.min(n_frames)..n_frames {
                for sp in 0..NUM_SPEAKERS {
                    scores[[t, sp]] += SCORES_BOOST_LATEST;
                }
            }
        }
        scores = boost_topk_scores(scores, strong_boost_per_spk, 2.0);
        scores = boost_topk_scores(scores, weak_boost_per_spk, 1.0);

        if SPKCACHE_SIL_FRAMES_PER_SPK > 0 {
            let mut padded = Array2::from_elem(
                (n_frames + SPKCACHE_SIL_FRAMES_PER_SPK, NUM_SPEAKERS),
                f32::NEG_INFINITY,
            );
            padded.slice_mut(s![..n_frames, ..]).assign(&scores);
            for i in n_frames..n_frames + SPKCACHE_SIL_FRAMES_PER_SPK {
                for j in 0..NUM_SPEAKERS {
                    padded[[i, j]] = f32::INFINITY;
                }
            }
            scores = padded;
        }

        let (topk_indices, is_disabled) = self.topk_indices(&scores, n_frames);

        let mut new_embs = Array3::zeros((1, self.spkcache_len, EMB_DIM));
        let mut new_preds = Array3::zeros((1, self.spkcache_len, NUM_SPEAKERS));
        for (i, (&idx, &disabled)) in topk_indices.iter().zip(&is_disabled).enumerate() {
            if disabled {
                new_embs.slice_mut(s![0, i, ..]).assign(&self.sil_emb);
            } else if idx < n_frames {
                new_embs
                    .slice_mut(s![0, i, ..])
                    .assign(&self.spkcache.slice(s![0, idx, ..]));
                new_preds
                    .slice_mut(s![0, i, ..])
                    .assign(&cache_preds.slice(s![0, idx, ..]));
            }
        }
        self.spkcache = new_embs;
        self.spkcache_preds = Some(new_preds);
    }

    /// Indices of the `spkcache_len` best-scoring (frame, speaker) cells, flattened
    /// speaker-major as NeMo does, plus which slots hold no real frame.
    fn topk_indices(&self, scores: &Array2<f32>, n_frames_no_sil: usize) -> (Vec<usize>, Vec<bool>) {
        let n_frames = scores.shape()[0];
        let mut flat: Vec<(usize, f32)> = Vec::with_capacity(n_frames * NUM_SPEAKERS);
        for sp in 0..NUM_SPEAKERS {
            for t in 0..n_frames {
                flat.push((sp * n_frames + t, scores[[t, sp]]));
            }
        }
        flat.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        let mut topk: Vec<usize> = flat
            .iter()
            .take(self.spkcache_len)
            .map(|(idx, score)| if *score == f32::NEG_INFINITY { MAX_INDEX } else { *idx })
            .collect();
        topk.sort_unstable();

        let mut is_disabled = vec![false; self.spkcache_len];
        let mut frame_indices = vec![0usize; self.spkcache_len];
        for (i, &flat_idx) in topk.iter().enumerate() {
            if flat_idx == MAX_INDEX {
                is_disabled[i] = true;
                continue;
            }
            let frame_idx = flat_idx % n_frames;
            if frame_idx >= n_frames_no_sil {
                is_disabled[i] = true;
            } else {
                frame_indices[i] = frame_idx;
            }
        }
        (frame_indices, is_disabled)
    }

    fn median_filter(config: &DiarizationConfig, preds: &Array2<f32>) -> Array2<f32> {
        let half = config.median_window / 2;
        let n = preds.shape()[0];
        let mut filtered = preds.clone();
        for spk in 0..NUM_SPEAKERS {
            for t in 0..n {
                let start = t.saturating_sub(half);
                let end = (t + half + 1).min(n);
                let mut values: Vec<f32> = (start..end).map(|i| preds[[i, spk]]).collect();
                values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                filtered[[t, spk]] = values[values.len() / 2];
            }
        }
        filtered
    }

    /// NeMo's `binarization`: a segment starts when p > onset and ends when p < offset, then
    /// padding, `min_duration_on` and `min_duration_off` are applied.
    fn binarize(config: &DiarizationConfig, preds: &Array2<f32>) -> Vec<SpeakerSegment> {
        let num_frames = preds.shape()[0];
        let frame_sec = FRAME_DURATION / UPSAMPLE_FACTOR as f32;
        let to_samples = |sec: f32| (sec * SAMPLE_RATE as f32).round().max(0.0) as u64;
        let pad_onset = (config.pad_onset * SAMPLE_RATE as f32) as u64;
        let pad_offset = (config.pad_offset * SAMPLE_RATE as f32) as u64;
        let min_on = to_samples(config.min_duration_on);
        let merge_gap = to_samples(config.min_duration_off).max((pad_onset + pad_offset > 0) as u64);

        let mut segments = Vec::new();
        for spk in 0..NUM_SPEAKERS {
            let mut runs: Vec<(usize, usize)> = Vec::new();
            let mut in_seg = false;
            let mut seg_start = 0;
            for t in 0..num_frames {
                let p = preds[[t, spk]];
                let active = if p > config.onset {
                    true
                } else if p < config.offset {
                    false
                } else {
                    in_seg
                };
                if active && !in_seg {
                    seg_start = t;
                } else if !active && in_seg {
                    runs.push((seg_start, t));
                }
                in_seg = active;
            }
            if in_seg {
                runs.push((seg_start, num_frames));
            }

            let mut spk_segments: Vec<SpeakerSegment> = Vec::new();
            for (start_f, end_f) in runs {
                let start = to_samples(start_f as f32 * frame_sec).saturating_sub(pad_onset);
                let end = to_samples(end_f as f32 * frame_sec) + pad_offset;
                if end <= start || end - start < min_on {
                    continue;
                }
                spk_segments.push(SpeakerSegment { start, end, speaker_id: spk });
            }

            if merge_gap > 0 && spk_segments.len() > 1 {
                let mut merged = vec![spk_segments[0].clone()];
                for seg in spk_segments.into_iter().skip(1) {
                    let last = merged.last_mut().unwrap();
                    if seg.start.saturating_sub(last.end) < merge_gap {
                        last.end = last.end.max(seg.end);
                    } else {
                        merged.push(seg);
                    }
                }
                segments.extend(merged);
            } else {
                segments.extend(spk_segments);
            }
        }
        segments.sort_by_key(|s| (s.start, s.speaker_id));
        segments
    }

    fn hann_window(window_length: usize) -> Vec<f32> {
        // torch.hann_window(periodic=False): divide by N-1
        let n = (window_length - 1) as f64;
        (0..window_length)
            .map(|i| to_bf16((0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / n).cos()) as f32))
            .collect()
    }

    /// Power spectrogram `(freq_bins, frames)` with librosa/torch centre padding.
    fn stft(audio: &[f32]) -> Result<Array2<f32>> {
        let mut planner = RealFftPlanner::<f32>::new();
        let r2c = planner.plan_fft_forward(N_FFT);

        let hann = Self::hann_window(WIN_LENGTH);
        let win_offset = (N_FFT - WIN_LENGTH) / 2;
        let mut fft_window = vec![0.0f32; N_FFT];
        fft_window[win_offset..win_offset + WIN_LENGTH].copy_from_slice(&hann);

        let pad = N_FFT / 2;
        let mut padded = vec![0.0f32; pad];
        padded.extend_from_slice(audio);
        padded.extend(std::iter::repeat(0.0).take(pad));

        let num_frames = (padded.len() - N_FFT) / HOP_LENGTH + 1;
        let freq_bins = N_FFT / 2 + 1;
        let mut spectrogram = Array2::<f32>::zeros((freq_bins, num_frames));

        let mut input = vec![0.0f32; N_FFT];
        let mut output = r2c.make_output_vec();
        let mut scratch = r2c.make_scratch_vec();
        for frame in 0..num_frames {
            let start = frame * HOP_LENGTH;
            for i in 0..N_FFT {
                input[i] = padded[start + i] * fft_window[i];
            }
            r2c.process_with_scratch(&mut input, &mut output, &mut scratch)
                .map_err(|e| anyhow!("FFT failed: {e}"))?;
            for k in 0..freq_bins {
                spectrogram[[k, frame]] = output[k].norm_sqr();
            }
        }
        Ok(spectrogram)
    }

    /// Log-mel features `(1, frames, 128)` as the model expects them.
    fn extract_mel_features(&self, audio: &[f32]) -> Result<Array3<f32>> {
        // NeMo adds dither; it is skipped for determinism and the log guard covers zeros.
        let preemphasized = apply_preemphasis(audio, PREEMPH);
        let spectrogram = Self::stft(&preemphasized)?;
        let mel = self.mel_basis.dot(&spectrogram);
        let log_mel = mel.mapv(|x| (x + LOG_ZERO_GUARD).ln());
        Ok(log_mel.t().as_standard_layout().into_owned().insert_axis(Axis(0)))
    }
}

/// Zero-pad `a` along axis 1 up to `len` frames; returns it unchanged when already that long.
fn pad_axis1(a: &Array3<f32>, len: usize) -> Array3<f32> {
    let current = a.shape()[1];
    if current >= len {
        return a.clone();
    }
    let mut padded = Array3::zeros((a.shape()[0], len, a.shape()[2]));
    padded.slice_mut(s![.., ..current, ..]).assign(a);
    padded
}

fn extract_3d_f32(outputs: &ort::session::SessionOutputs<'_>, name: &str) -> Result<Array3<f32>> {
    let value = outputs
        .get(name)
        .ok_or_else(|| anyhow!("Sortformer output '{name}' missing"))?;
    let (shape, data) = value
        .try_extract_tensor::<f32>()
        .with_context(|| format!("failed to extract Sortformer output '{name}'"))?;
    let dims: &[i64] = shape;
    if dims.len() != 3 {
        return Err(anyhow!("expected 3D tensor for '{name}', got shape {dims:?}"));
    }
    Array3::from_shape_vec(
        (dims[0] as usize, dims[1] as usize, dims[2] as usize),
        data.to_vec(),
    )
    .with_context(|| format!("failed to reshape Sortformer output '{name}'"))
}

fn concat_axis1(a: &Array3<f32>, b: &Array3<f32>) -> Array3<f32> {
    if a.shape()[1] == 0 {
        return b.clone();
    }
    if b.shape()[1] == 0 {
        return a.clone();
    }
    ndarray::concatenate(Axis(1), &[a.view(), b.view()]).expect("matching trailing dims")
}

fn concat_rows(a: &Array2<f32>, b: &Array2<f32>) -> Array2<f32> {
    if a.shape()[0] == 0 {
        return b.clone();
    }
    if b.shape()[0] == 0 {
        return a.clone();
    }
    ndarray::concatenate(Axis(0), &[a.view(), b.view()]).expect("matching column count")
}

/// NeMo's frame quality score: log-odds of the speaker against the others, each probability
/// clamped before the log.
fn log_pred_scores(preds: &Array2<f32>) -> Array2<f32> {
    let mut scores = Array2::zeros(preds.dim());
    for t in 0..preds.shape()[0] {
        let mut log_1_probs_sum = 0.0f32;
        for sp in 0..NUM_SPEAKERS {
            log_1_probs_sum += (1.0 - preds[[t, sp]]).max(PRED_SCORE_THRESHOLD).ln();
        }
        for sp in 0..NUM_SPEAKERS {
            let log_p = preds[[t, sp]].max(PRED_SCORE_THRESHOLD).ln();
            let log_1_p = (1.0 - preds[[t, sp]]).max(PRED_SCORE_THRESHOLD).ln();
            scores[[t, sp]] = log_p - log_1_p + log_1_probs_sum - 0.5f32.ln();
        }
    }
    scores
}

/// Drop non-speech frames and, for speakers with enough confident frames, overlapped ones.
fn disable_low_scores(preds: &Array2<f32>, mut scores: Array2<f32>, min_pos_scores_per_spk: usize) -> Array2<f32> {
    let mut pos_count = [0usize; NUM_SPEAKERS];
    for t in 0..scores.shape()[0] {
        for sp in 0..NUM_SPEAKERS {
            if scores[[t, sp]] > 0.0 {
                pos_count[sp] += 1;
            }
        }
    }
    for t in 0..preds.shape()[0] {
        for sp in 0..NUM_SPEAKERS {
            let non_speech = preds[[t, sp]] <= 0.5;
            let low_overlap = scores[[t, sp]] <= 0.0 && pos_count[sp] >= min_pos_scores_per_spk;
            if non_speech || low_overlap {
                scores[[t, sp]] = f32::NEG_INFINITY;
            }
        }
    }
    scores
}

fn boost_topk_scores(mut scores: Array2<f32>, n_boost_per_spk: usize, scale: f32) -> Array2<f32> {
    for sp in 0..NUM_SPEAKERS {
        let mut sorted: Vec<(usize, f32)> = (0..scores.shape()[0]).map(|t| (t, scores[[t, sp]])).collect();
        sorted.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        for &(t, _) in sorted.iter().take(n_boost_per_spk.min(sorted.len())) {
            if scores[[t, sp]] != f32::NEG_INFINITY {
                scores[[t, sp]] -= scale * 0.5f32.ln();
            }
        }
    }
    scores
}

fn apply_preemphasis(audio: &[f32], coef: f32) -> Vec<f32> {
    let mut out = Vec::with_capacity(audio.len());
    if let Some(&first) = audio.first() {
        out.push(first);
        out.extend(audio.windows(2).map(|w| w[1] - coef * w[0]));
    }
    out
}

// Slaney mel scale, as librosa and NeMo use it.
const F_SP: f64 = 200.0 / 3.0;
const MIN_LOG_HZ: f64 = 1000.0;
const MIN_LOG_MEL: f64 = MIN_LOG_HZ / F_SP;
const LOG_STEP: f64 = 0.068_751_777_42; // ln(6.4) / 27

fn hz_to_mel_slaney(hz: f64) -> f64 {
    if hz < MIN_LOG_HZ {
        hz / F_SP
    } else {
        MIN_LOG_MEL + (hz / MIN_LOG_HZ).ln() / LOG_STEP
    }
}

fn mel_to_hz_slaney(mel: f64) -> f64 {
    if mel < MIN_LOG_MEL {
        mel * F_SP
    } else {
        MIN_LOG_HZ * ((mel - MIN_LOG_MEL) * LOG_STEP).exp()
    }
}

/// librosa-style triangular mel filterbank with Slaney normalisation, `(n_mels, n_fft/2+1)`.
fn create_mel_filterbank(n_fft: usize, n_mels: usize, sample_rate: usize) -> Array2<f32> {
    let freq_bins = n_fft / 2 + 1;
    let mut filterbank = Array2::<f32>::zeros((n_mels, freq_bins));

    let mel_min = hz_to_mel_slaney(0.0);
    let mel_max = hz_to_mel_slaney(sample_rate as f64 / 2.0);
    let mel_points: Vec<f64> = (0..=n_mels + 1)
        .map(|i| mel_to_hz_slaney(mel_min + (mel_max - mel_min) * i as f64 / (n_mels + 1) as f64))
        .collect();
    let fft_freqs: Vec<f64> = (0..freq_bins)
        .map(|i| i as f64 * sample_rate as f64 / n_fft as f64)
        .collect();
    let fdiff: Vec<f64> = mel_points.windows(2).map(|w| w[1] - w[0]).collect();

    for i in 0..n_mels {
        for (k, &freq) in fft_freqs.iter().enumerate() {
            let lower = (freq - mel_points[i]) / fdiff[i];
            let upper = (mel_points[i + 2] - freq) / fdiff[i + 1];
            filterbank[[i, k]] = 0.0f64.max(lower.min(upper)) as f32;
        }
        let enorm = 2.0 / (mel_points[i + 2] - mel_points[i]);
        for k in 0..freq_bins {
            filterbank[[i, k]] *= enorm as f32;
        }
    }
    filterbank
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;

    fn sine_wave(freq_hz: f32, num_samples: usize) -> Vec<f32> {
        (0..num_samples)
            .map(|i| (2.0 * PI * freq_hz * i as f32 / SAMPLE_RATE as f32).sin())
            .collect()
    }

    #[test]
    fn stft_concentrates_power_at_expected_bin() {
        let spec = Sortformer::stft(&sine_wave(1000.0, SAMPLE_RATE)).unwrap();
        // 1000 Hz * 512 / 16000 = bin 32
        let num_frames = spec.shape()[1];
        let mut correct = 0;
        for frame in 2..num_frames - 2 {
            let col = spec.column(frame);
            let (max_bin, _) = col
                .iter()
                .enumerate()
                .fold((0, f32::MIN), |acc, (i, &v)| if v > acc.1 { (i, v) } else { acc });
            if max_bin == 32 {
                correct += 1;
            }
        }
        assert!(correct > (num_frames - 4) / 2, "{correct} of {num_frames} frames peaked at bin 32");
    }

    #[test]
    fn stft_frame_count_matches_centred_hop() {
        let spec = Sortformer::stft(&vec![0.0f32; SAMPLE_RATE]).unwrap();
        assert_eq!(spec.shape()[0], N_FFT / 2 + 1);
        assert_eq!(spec.shape()[1], SAMPLE_RATE / HOP_LENGTH + 1);
    }

    #[test]
    fn mel_filterbank_rows_cover_the_spectrum() {
        let fb = create_mel_filterbank(N_FFT, N_MELS, SAMPLE_RATE);
        assert_eq!(fb.shape(), &[N_MELS, N_FFT / 2 + 1]);
        for row in fb.rows() {
            assert!(row.iter().any(|&v| v > 0.0), "every mel band has at least one bin");
        }
    }

    #[test]
    fn binarize_uses_hysteresis_and_sample_offsets() {
        // speaker 0 active for 10 ms frames 10..30 -> samples 1600..4800
        let mut preds = Array2::zeros((50, NUM_SPEAKERS));
        for t in 10..30 {
            preds[[t, 0]] = 0.9;
        }
        let segs = Sortformer::post_process(&DiarizationConfig::default(), &preds, 1_000_000);
        assert_eq!(
            segs,
            vec![SpeakerSegment { start: 1600, end: 4800, speaker_id: 0 }]
        );
    }

    #[test]
    fn binarize_clips_to_audio_length_and_drops_empty() {
        let mut preds = Array2::zeros((50, NUM_SPEAKERS));
        for t in 40..50 {
            preds[[t, 2]] = 0.9;
        }
        let segs = Sortformer::post_process(&DiarizationConfig::default(), &preds, 6500);
        assert_eq!(segs, vec![SpeakerSegment { start: 6400, end: 6500, speaker_id: 2 }]);
        let none = Sortformer::post_process(&DiarizationConfig::default(), &preds, 6400);
        assert!(none.is_empty());
    }

    #[test]
    fn binarize_merges_gaps_shorter_than_min_duration_off() {
        let mut preds = Array2::zeros((100, NUM_SPEAKERS));
        for t in (0..20).chain(25..50) {
            preds[[t, 1]] = 0.9;
        }
        let config = DiarizationConfig { min_duration_off: 0.1, ..Default::default() };
        let segs = Sortformer::post_process(&config, &preds, 1_000_000);
        assert_eq!(segs, vec![SpeakerSegment { start: 0, end: 8000, speaker_id: 1 }]);
    }

    #[test]
    fn preemphasis_keeps_first_sample_and_differences_the_rest() {
        let out = apply_preemphasis(&[1.0, 1.0, 1.0], 0.97);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0], 1.0);
        for v in &out[1..] {
            assert!((v - 0.03).abs() < 1e-5, "got {v}");
        }
        assert!(apply_preemphasis(&[], 0.97).is_empty());
    }

    #[test]
    fn cache_compression_scoring_prefers_confident_single_speaker_frames() {
        let mut preds = Array2::zeros((3, NUM_SPEAKERS));
        preds[[0, 0]] = 0.95; // confident speaker 0
        preds[[1, 0]] = 0.6; // weak
        preds[[1, 1]] = 0.6; // overlapped with speaker 1
        // frame 2: silence
        let scores = log_pred_scores(&preds);
        assert!(scores[[0, 0]] > scores[[1, 0]]);
        let scores = disable_low_scores(&preds, scores, 0);
        assert_eq!(scores[[2, 0]], f32::NEG_INFINITY, "silence is never kept");
        assert!(scores[[0, 0]].is_finite());
    }
}
