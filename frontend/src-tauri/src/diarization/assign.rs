//! Turning diarization output into speaker labels on transcript segments.
//!
//! Pure logic, no audio or ONNX dependencies, so every rule here is unit-tested.
//!
//! Two inputs meet here: the diarizer's speaker spans (who spoke when, at 10 ms
//! resolution) and the transcript, which is either word-timed (Parakeet gives a
//! start time per token) or segment-timed (Whisper gives text per VAD segment).
//! Word-timed text is split wherever the speaker changes; segment-timed text gets
//! the speaker who dominates the segment.

use serde::Serialize;

/// One speaker's activity in milliseconds from the start of the recording.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SpeakerSpan {
    pub start_ms: f64,
    pub end_ms: f64,
    /// Raw diarizer speaker index (0-based, in order of first appearance).
    pub speaker: usize,
}

/// A word with its start time and the start of the next word (or segment end).
#[derive(Debug, Clone, PartialEq)]
pub struct TimedWord {
    pub text: String,
    pub start_ms: f64,
    pub end_ms: f64,
}

/// A transcript piece ready for persistence: text, bounds and raw speaker index.
#[derive(Debug, Clone, PartialEq)]
pub struct SpeakerPiece {
    pub text: String,
    pub start_ms: f64,
    pub end_ms: f64,
    pub speaker: Option<usize>,
}

/// Merge spans of the same speaker that touch or overlap, or are separated by at most
/// `max_gap_ms`. The streaming diarizer binarizes each 30 s window on its own, so one
/// utterance that straddles a window edge arrives as two spans.
pub fn merge_adjacent_spans(mut spans: Vec<SpeakerSpan>, max_gap_ms: f64) -> Vec<SpeakerSpan> {
    spans.sort_by(|a, b| {
        a.speaker
            .cmp(&b.speaker)
            .then(a.start_ms.partial_cmp(&b.start_ms).unwrap_or(std::cmp::Ordering::Equal))
    });
    let mut merged: Vec<SpeakerSpan> = Vec::with_capacity(spans.len());
    for span in spans {
        match merged.last_mut() {
            Some(last) if last.speaker == span.speaker && span.start_ms - last.end_ms <= max_gap_ms => {
                last.end_ms = last.end_ms.max(span.end_ms);
            }
            _ => merged.push(span),
        }
    }
    merged.sort_by(|a, b| {
        a.start_ms
            .partial_cmp(&b.start_ms)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.speaker.cmp(&b.speaker))
    });
    merged
}

fn overlap_ms(span: &SpeakerSpan, start_ms: f64, end_ms: f64) -> f64 {
    (span.end_ms.min(end_ms) - span.start_ms.max(start_ms)).max(0.0)
}

/// The speaker with the most speech inside `[start_ms, end_ms]`, or `None` when no span
/// touches the interval. Ties go to the speaker who started first.
pub fn dominant_speaker(spans: &[SpeakerSpan], start_ms: f64, end_ms: f64) -> Option<usize> {
    let mut totals: Vec<(usize, f64, f64)> = Vec::new(); // (speaker, overlap, first start)
    for span in spans {
        let overlap = overlap_ms(span, start_ms, end_ms);
        if overlap <= 0.0 {
            continue;
        }
        match totals.iter_mut().find(|(s, _, _)| *s == span.speaker) {
            Some(entry) => {
                entry.1 += overlap;
                entry.2 = entry.2.min(span.start_ms);
            }
            None => totals.push((span.speaker, overlap, span.start_ms)),
        }
    }
    totals
        .into_iter()
        .max_by(|a, b| {
            a.1.partial_cmp(&b.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal))
        })
        .map(|(speaker, _, _)| speaker)
}

/// Group Parakeet tokens into words. Tokens carry a leading space where SentencePiece had a
/// word boundary (the vocabulary loader rewrites `▁` to a space), so a token that starts with
/// a space starts a word. Timestamps are token start offsets in seconds from the segment
/// start; a word ends where the next one starts, the last at the segment end.
pub fn words_from_tokens(
    tokens: &[String],
    token_offsets_s: &[f32],
    segment_start_ms: f64,
    segment_end_ms: f64,
) -> Vec<TimedWord> {
    let mut words: Vec<TimedWord> = Vec::new();
    for (token, &offset) in tokens.iter().zip(token_offsets_s) {
        if token.trim().is_empty() {
            continue;
        }
        let starts_word = token.starts_with(' ') || words.is_empty();
        let piece = token.trim_start();
        if starts_word {
            // Offsets sit on the model's 80 ms grid; rounding to whole milliseconds removes
            // f32 noise without losing anything.
            words.push(TimedWord {
                text: piece.to_string(),
                start_ms: segment_start_ms + (offset as f64 * 1000.0).round(),
                end_ms: segment_end_ms,
            });
        } else if let Some(last) = words.last_mut() {
            last.text.push_str(piece);
        }
    }
    for i in 0..words.len().saturating_sub(1) {
        words[i].end_ms = words[i + 1].start_ms.max(words[i].start_ms);
    }
    if let Some(last) = words.last_mut() {
        last.end_ms = last.end_ms.max(last.start_ms);
    }
    words
}

/// Split a word-timed segment wherever the speaker changes.
///
/// Each word takes the speaker active at its midpoint. A word nobody is speaking over
/// (a diarizer gap) keeps the previous word's speaker so one utterance is not shredded by
/// a short miss; if there is no previous word it takes the segment's dominant speaker.
pub fn split_words_by_speaker(
    words: &[TimedWord],
    spans: &[SpeakerSpan],
    segment_start_ms: f64,
    segment_end_ms: f64,
) -> Vec<SpeakerPiece> {
    let fallback = dominant_speaker(spans, segment_start_ms, segment_end_ms);
    let mut pieces: Vec<SpeakerPiece> = Vec::new();
    let mut previous: Option<usize> = None;
    for word in words {
        let mid = (word.start_ms + word.end_ms) / 2.0;
        let speaker = speaker_at(spans, mid, word.start_ms, word.end_ms)
            .or(previous)
            .or(fallback);
        previous = speaker;
        match pieces.last_mut() {
            Some(last) if last.speaker == speaker => {
                if !last.text.is_empty() && !word.text.is_empty() {
                    last.text.push(' ');
                }
                last.text.push_str(&word.text);
                last.end_ms = word.end_ms.max(last.end_ms);
            }
            _ => pieces.push(SpeakerPiece {
                text: word.text.clone(),
                start_ms: word.start_ms,
                end_ms: word.end_ms,
                speaker,
            }),
        }
    }
    pieces
}

/// Speaker active at `at_ms`; when several overlap there, the one covering most of
/// `[start_ms, end_ms]`.
fn speaker_at(spans: &[SpeakerSpan], at_ms: f64, start_ms: f64, end_ms: f64) -> Option<usize> {
    let covering: Vec<&SpeakerSpan> = spans
        .iter()
        .filter(|s| s.start_ms <= at_ms && at_ms < s.end_ms)
        .collect();
    match covering.len() {
        0 => None,
        1 => Some(covering[0].speaker),
        _ => covering
            .into_iter()
            .max_by(|a, b| {
                overlap_ms(a, start_ms, end_ms)
                    .partial_cmp(&overlap_ms(b, start_ms, end_ms))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|s| s.speaker),
    }
}

/// Replace raw speaker indices with `Speaker N` labels numbered by first appearance in
/// time order, the same canonical form the Gemini batch path produces, so the UI treats
/// both alike. Pieces are returned sorted by start time.
pub fn label_pieces(mut pieces: Vec<SpeakerPiece>) -> Vec<(String, f64, f64, Option<String>)> {
    pieces.sort_by(|a, b| a.start_ms.partial_cmp(&b.start_ms).unwrap_or(std::cmp::Ordering::Equal));
    let mut order: Vec<usize> = Vec::new();
    pieces
        .into_iter()
        .map(|piece| {
            let label = piece.speaker.map(|raw| {
                let n = match order.iter().position(|&s| s == raw) {
                    Some(i) => i,
                    None => {
                        order.push(raw);
                        order.len() - 1
                    }
                };
                format!("Speaker {}", n + 1)
            });
            (piece.text, piece.start_ms, piece.end_ms, label)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(speaker: usize, start_ms: f64, end_ms: f64) -> SpeakerSpan {
        SpeakerSpan { start_ms, end_ms, speaker }
    }

    fn word(text: &str, start_ms: f64, end_ms: f64) -> TimedWord {
        TimedWord { text: text.into(), start_ms, end_ms }
    }

    #[test]
    fn merge_joins_same_speaker_across_window_edges_only() {
        let spans = vec![
            span(0, 0.0, 30400.0),
            span(0, 30400.0, 31000.0), // continues at the window edge
            span(1, 30400.0, 32000.0),
            span(0, 40000.0, 41000.0), // a real gap stays separate
        ];
        let merged = merge_adjacent_spans(spans, 1.0);
        assert_eq!(
            merged,
            vec![span(0, 0.0, 31000.0), span(1, 30400.0, 32000.0), span(0, 40000.0, 41000.0)]
        );
    }

    #[test]
    fn dominant_speaker_counts_total_overlap_not_span_count() {
        let spans = vec![span(0, 0.0, 1000.0), span(0, 1000.0, 2000.0), span(1, 500.0, 5000.0)];
        assert_eq!(dominant_speaker(&spans, 0.0, 5000.0), Some(1));
        assert_eq!(dominant_speaker(&spans, 0.0, 1200.0), Some(0));
        assert_eq!(dominant_speaker(&spans, 9000.0, 9500.0), None);
    }

    #[test]
    fn dominant_speaker_tie_goes_to_whoever_started_first() {
        let spans = vec![span(3, 1000.0, 2000.0), span(1, 0.0, 1000.0)];
        assert_eq!(dominant_speaker(&spans, 0.0, 2000.0), Some(1));
    }

    #[test]
    fn words_from_tokens_groups_on_leading_space_and_fills_end_times() {
        let tokens: Vec<String> = [" Hel", "lo", " there", " friend"].iter().map(|s| s.to_string()).collect();
        let offsets = [0.08f32, 0.16, 0.48, 0.80];
        let words = words_from_tokens(&tokens, &offsets, 10_000.0, 11_500.0);
        assert_eq!(
            words,
            vec![
                word("Hello", 10_080.0, 10_480.0),
                word("there", 10_480.0, 10_800.0),
                word("friend", 10_800.0, 11_500.0),
            ]
        );
    }

    #[test]
    fn words_from_tokens_handles_no_leading_space_on_first_token_and_blank_tokens() {
        let tokens: Vec<String> = ["Hi", " ", " you"].iter().map(|s| s.to_string()).collect();
        let offsets = [0.0f32, 0.1, 0.2];
        let words = words_from_tokens(&tokens, &offsets, 0.0, 1000.0);
        assert_eq!(words, vec![word("Hi", 0.0, 200.0), word("you", 200.0, 1000.0)]);
    }

    #[test]
    fn split_words_breaks_segment_where_speaker_changes() {
        let spans = vec![span(0, 0.0, 1000.0), span(1, 1000.0, 2000.0)];
        let words = vec![
            word("I", 0.0, 300.0),
            word("agree", 300.0, 900.0),
            word("Great", 1100.0, 1500.0),
            word("thanks", 1500.0, 2000.0),
        ];
        let pieces = split_words_by_speaker(&words, &spans, 0.0, 2000.0);
        assert_eq!(
            pieces,
            vec![
                SpeakerPiece { text: "I agree".into(), start_ms: 0.0, end_ms: 900.0, speaker: Some(0) },
                SpeakerPiece { text: "Great thanks".into(), start_ms: 1100.0, end_ms: 2000.0, speaker: Some(1) },
            ]
        );
    }

    #[test]
    fn split_words_carries_previous_speaker_over_diarizer_gaps() {
        let spans = vec![span(2, 0.0, 400.0), span(2, 900.0, 2000.0)];
        let words = vec![word("one", 0.0, 400.0), word("two", 400.0, 900.0), word("three", 900.0, 2000.0)];
        let pieces = split_words_by_speaker(&words, &spans, 0.0, 2000.0);
        assert_eq!(pieces.len(), 1);
        assert_eq!(pieces[0].text, "one two three");
        assert_eq!(pieces[0].speaker, Some(2));
    }

    #[test]
    fn split_words_without_any_speaker_leaves_label_empty() {
        let words = vec![word("hello", 0.0, 500.0)];
        let pieces = split_words_by_speaker(&words, &[], 0.0, 500.0);
        assert_eq!(pieces[0].speaker, None);
    }

    #[test]
    fn overlapping_speakers_at_a_word_pick_the_one_covering_more_of_it() {
        let spans = vec![span(0, 0.0, 1000.0), span(1, 450.0, 3000.0)];
        let words = vec![word("x", 400.0, 1000.0)]; // mid 700: both active; 1 covers 550ms vs 0 covers 600ms
        assert_eq!(split_words_by_speaker(&words, &spans, 400.0, 1000.0)[0].speaker, Some(0));
    }

    #[test]
    fn labels_number_speakers_by_first_appearance_in_time() {
        let pieces = vec![
            SpeakerPiece { text: "b".into(), start_ms: 5000.0, end_ms: 6000.0, speaker: Some(0) },
            SpeakerPiece { text: "a".into(), start_ms: 0.0, end_ms: 1000.0, speaker: Some(4) },
            SpeakerPiece { text: "c".into(), start_ms: 7000.0, end_ms: 8000.0, speaker: Some(4) },
            SpeakerPiece { text: "d".into(), start_ms: 9000.0, end_ms: 9500.0, speaker: None },
        ];
        let labelled = label_pieces(pieces);
        let labels: Vec<Option<String>> = labelled.iter().map(|p| p.3.clone()).collect();
        assert_eq!(
            labels,
            vec![Some("Speaker 1".into()), Some("Speaker 2".into()), Some("Speaker 1".into()), None]
        );
        assert_eq!(labelled[0].0, "a");
    }
}
