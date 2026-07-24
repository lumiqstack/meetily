// Per-window "who is speaking" attribution for live recordings.
//
// Whisper transcribes the mixed mic+system stream, so speaker identity must
// be decided *before* mixing: the ring buffer yields time-aligned
// (mic_window, system_window) pairs, and comparing their energy tells us
// which source dominates each window. A transcription chunk spans several
// windows; its label is the energy-weighted winner across them. Stored
// values are "mic" / "system" (see migration 20251110000001), rendered as
// "Me" / "Others" in the UI.
//
// Pure logic only — no audio-pipeline dependencies — so it is unit-testable
// without capture devices.

use super::recording_state::DeviceType;

/// How much louder (in dB) one source must be than the other to claim a
/// window outright. Inside this dead band the previous window's label is
/// carried forward so near-equal crosstalk does not flap between speakers.
const DOMINANCE_MARGIN_DB: f32 = 6.0;

/// RMS below which a window counts as silent. Silence in both sources yields
/// no label and breaks the carry chain (a gap ends the previous speaker's
/// claim).
const NOISE_FLOOR_RMS: f32 = 1e-4;

/// Labels each 600 ms mixer window with its dominant source.
pub struct WindowLabeler {
    prev: Option<DeviceType>,
}

impl WindowLabeler {
    pub fn new() -> Self {
        Self { prev: None }
    }

    /// Decide the dominant source for one time-aligned window pair.
    pub fn label(&mut self, mic_window: &[f32], system_window: &[f32]) -> Option<DeviceType> {
        let mic_rms = rms(mic_window);
        let system_rms = rms(system_window);

        if mic_rms < NOISE_FLOOR_RMS && system_rms < NOISE_FLOOR_RMS {
            self.prev = None;
            return None;
        }

        let ratio_db =
            20.0 * (mic_rms.max(f32::EPSILON) / system_rms.max(f32::EPSILON)).log10();
        let label = if ratio_db >= DOMINANCE_MARGIN_DB {
            Some(DeviceType::Microphone)
        } else if ratio_db <= -DOMINANCE_MARGIN_DB {
            Some(DeviceType::System)
        } else {
            self.prev.clone()
        };
        self.prev = label.clone();
        label
    }
}

impl Default for WindowLabeler {
    fn default() -> Self {
        Self::new()
    }
}

/// Accumulates window labels for the transcription chunk being built and
/// yields the chunk's overall label on `finalize()`.
pub struct SegmentAggregator {
    mic_energy: f32,
    system_energy: f32,
}

impl SegmentAggregator {
    pub fn new() -> Self {
        Self {
            mic_energy: 0.0,
            system_energy: 0.0,
        }
    }

    /// Record one window's label with the energy that backed it.
    pub fn add(&mut self, label: Option<DeviceType>, energy: f32) {
        match label {
            Some(DeviceType::Microphone) => self.mic_energy += energy.max(0.0),
            Some(DeviceType::System) => self.system_energy += energy.max(0.0),
            None => {}
        }
    }

    /// The chunk's label: the source that carried more labeled energy.
    /// Resets the aggregator for the next chunk.
    pub fn finalize(&mut self) -> Option<DeviceType> {
        let (mic, system) = (self.mic_energy, self.system_energy);
        self.mic_energy = 0.0;
        self.system_energy = 0.0;

        if mic <= 0.0 && system <= 0.0 {
            None
        } else if mic >= system {
            Some(DeviceType::Microphone)
        } else {
            Some(DeviceType::System)
        }
    }
}

impl Default for SegmentAggregator {
    fn default() -> Self {
        Self::new()
    }
}

/// Root-mean-square of a sample window; 0.0 for an empty window.
fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq: f32 = samples.iter().map(|s| s * s).sum();
    (sum_sq / samples.len() as f32).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(amplitude: f32) -> Vec<f32> {
        vec![amplitude; 480] // 10 ms at 48 kHz; length is irrelevant to RMS
    }

    const LOUD: f32 = 0.5;
    const QUIET: f32 = 0.01; // 34 dB below LOUD — clearly outside the margin
    const SILENT: f32 = 0.0;

    #[test]
    fn mic_dominant_window_is_labeled_microphone() {
        let mut labeler = WindowLabeler::new();
        assert_eq!(
            labeler.label(&window(LOUD), &window(QUIET)),
            Some(DeviceType::Microphone)
        );
    }

    #[test]
    fn system_dominant_window_is_labeled_system() {
        let mut labeler = WindowLabeler::new();
        assert_eq!(
            labeler.label(&window(QUIET), &window(LOUD)),
            Some(DeviceType::System)
        );
    }

    #[test]
    fn silent_window_has_no_label() {
        let mut labeler = WindowLabeler::new();
        assert_eq!(labeler.label(&window(SILENT), &window(SILENT)), None);
    }

    #[test]
    fn first_ambiguous_window_has_no_label() {
        let mut labeler = WindowLabeler::new();
        // Equal energy on both sides, nothing to carry from.
        assert_eq!(labeler.label(&window(LOUD), &window(LOUD)), None);
    }

    #[test]
    fn ambiguous_window_carries_previous_label() {
        let mut labeler = WindowLabeler::new();
        assert_eq!(
            labeler.label(&window(LOUD), &window(QUIET)),
            Some(DeviceType::Microphone)
        );
        // Crosstalk: both loud, inside the dead band — keep the mic label.
        assert_eq!(
            labeler.label(&window(LOUD), &window(LOUD)),
            Some(DeviceType::Microphone)
        );
    }

    #[test]
    fn near_equal_energy_does_not_flap() {
        let mut labeler = WindowLabeler::new();
        labeler.label(&window(LOUD), &window(QUIET)); // establish mic
        for _ in 0..10 {
            // Tiny alternating imbalance, always inside ±6 dB.
            assert_eq!(
                labeler.label(&window(LOUD), &window(LOUD * 0.9)),
                Some(DeviceType::Microphone)
            );
            assert_eq!(
                labeler.label(&window(LOUD * 0.9), &window(LOUD)),
                Some(DeviceType::Microphone)
            );
        }
    }

    #[test]
    fn silence_breaks_the_carry_chain() {
        let mut labeler = WindowLabeler::new();
        labeler.label(&window(LOUD), &window(QUIET)); // mic
        assert_eq!(labeler.label(&window(SILENT), &window(SILENT)), None);
        // Ambiguous after a gap: nothing to carry.
        assert_eq!(labeler.label(&window(LOUD), &window(LOUD)), None);
    }

    #[test]
    fn one_sided_signal_over_silence_is_labeled() {
        let mut labeler = WindowLabeler::new();
        assert_eq!(
            labeler.label(&window(QUIET * 2.0), &window(SILENT)),
            Some(DeviceType::Microphone)
        );
    }

    #[test]
    fn aggregator_picks_energy_weighted_winner() {
        let mut agg = SegmentAggregator::new();
        // Long quiet mic speech vs one loud system burst: system carries
        // more energy overall and wins.
        agg.add(Some(DeviceType::Microphone), 1.0);
        agg.add(Some(DeviceType::Microphone), 1.0);
        agg.add(Some(DeviceType::System), 5.0);
        assert_eq!(agg.finalize(), Some(DeviceType::System));
    }

    #[test]
    fn aggregator_majority_energy_wins_over_count() {
        let mut agg = SegmentAggregator::new();
        agg.add(Some(DeviceType::System), 0.1);
        agg.add(Some(DeviceType::System), 0.1);
        agg.add(Some(DeviceType::System), 0.1);
        agg.add(Some(DeviceType::Microphone), 10.0);
        assert_eq!(agg.finalize(), Some(DeviceType::Microphone));
    }

    #[test]
    fn aggregator_with_no_labeled_windows_yields_none() {
        let mut agg = SegmentAggregator::new();
        agg.add(None, 3.0);
        agg.add(None, 1.0);
        assert_eq!(agg.finalize(), None);
    }

    #[test]
    fn aggregator_resets_after_finalize() {
        let mut agg = SegmentAggregator::new();
        agg.add(Some(DeviceType::Microphone), 1.0);
        assert_eq!(agg.finalize(), Some(DeviceType::Microphone));
        // A fresh chunk with no windows must not inherit the previous label.
        assert_eq!(agg.finalize(), None);
    }

    #[test]
    fn rms_of_empty_window_is_zero() {
        assert_eq!(rms(&[]), 0.0);
    }
}
