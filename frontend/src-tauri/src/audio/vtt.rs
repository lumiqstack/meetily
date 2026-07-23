// Minimal WebVTT parser for Microsoft Teams meeting transcripts.
//
// Teams exports transcripts as WebVTT where each cue carries the speaker in a
// voice span, e.g.:
//
//     WEBVTT
//
//     00:00:03.120 --> 00:00:06.740
//     <v Jane Smith>Thanks everyone for joining.</v>
//
// Some tenants prefix each cue with a GUID identifier line, omit the closing
// </v>, or drop the <v> tag entirely. We parse defensively: a missing voice
// tag yields a speakerless cue rather than an error.

use once_cell::sync::Lazy;
use regex::Regex;

/// One parsed transcript cue.
#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptCue {
    pub speaker: Option<String>,
    pub start_s: f64,
    pub end_s: f64,
    pub text: String,
}

static VOICE_TAG: Lazy<Regex> = Lazy::new(|| Regex::new(r"<v\s+([^>]+)>").unwrap());
static ANY_TAG: Lazy<Regex> = Lazy::new(|| Regex::new(r"<[^>]*>").unwrap());

/// Parse WebVTT text into cues. Cues with empty text are dropped. Returns an
/// error only if no cue could be parsed at all (so the caller can fall back).
pub fn parse_vtt(content: &str) -> Result<Vec<TranscriptCue>, String> {
    // Normalize line endings and split into blank-line-separated blocks.
    let normalized = content.replace("\r\n", "\n").replace('\r', "\n");
    let mut cues = Vec::new();

    for block in normalized.split("\n\n") {
        let block = block.trim_matches('\n');
        if block.is_empty() {
            continue;
        }

        // Find the timing line (the one containing "-->"); everything before it
        // is an optional identifier, everything after is cue text.
        let lines: Vec<&str> = block.lines().collect();
        let timing_idx = match lines.iter().position(|l| l.contains("-->")) {
            Some(i) => i,
            None => continue, // header ("WEBVTT ...", "NOTE ...") or junk block
        };

        let (start_s, end_s) = match parse_timing(lines[timing_idx]) {
            Some(t) => t,
            None => continue,
        };

        let raw_text = lines[timing_idx + 1..].join(" ");
        let (speaker, text) = extract_speaker_and_text(&raw_text);
        if text.is_empty() {
            continue;
        }

        cues.push(TranscriptCue {
            speaker,
            start_s,
            end_s,
            text,
        });
    }

    if cues.is_empty() {
        return Err("No transcript cues found in VTT".to_string());
    }
    Ok(cues)
}

/// Parse a timing line like `00:00:03.120 --> 00:00:06.740 position:50%`.
fn parse_timing(line: &str) -> Option<(f64, f64)> {
    let mut parts = line.split("-->");
    let start = parts.next()?.trim();
    // The end may be followed by cue settings; take the first whitespace token.
    let end = parts.next()?.trim().split_whitespace().next()?;
    Some((parse_timestamp(start)?, parse_timestamp(end)?))
}

/// Parse `HH:MM:SS.mmm` or `MM:SS.mmm` into seconds.
fn parse_timestamp(ts: &str) -> Option<f64> {
    let ts = ts.trim();
    let (hms, millis) = match ts.split_once('.') {
        Some((a, b)) => (a, b),
        None => (ts, "0"),
    };
    let comps: Vec<&str> = hms.split(':').collect();
    let (h, m, s) = match comps.as_slice() {
        [h, m, s] => (h.parse::<f64>().ok()?, m.parse::<f64>().ok()?, s.parse::<f64>().ok()?),
        [m, s] => (0.0, m.parse::<f64>().ok()?, s.parse::<f64>().ok()?),
        _ => return None,
    };
    let frac = format!("0.{}", millis).parse::<f64>().unwrap_or(0.0);
    Some(h * 3600.0 + m * 60.0 + s + frac)
}

/// Pull the speaker from the first `<v Name>` tag (if any), strip all tags, and
/// decode common HTML entities.
fn extract_speaker_and_text(raw: &str) -> (Option<String>, String) {
    let speaker = VOICE_TAG
        .captures(raw)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().trim().to_string())
        .filter(|s| !s.is_empty());

    let stripped = ANY_TAG.replace_all(raw, "");
    let text = decode_entities(stripped.trim());
    (speaker, text)
}

fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_teams_voice_cues() {
        let vtt = "WEBVTT\n\n\
            00:00:03.120 --> 00:00:06.740\n\
            <v Jane Smith>Thanks everyone for joining.</v>\n\n\
            0d2c-1\n\
            00:00:07.000 --> 00:00:09.500\n\
            <v John Doe>No problem, glad to be here.</v>";
        let cues = parse_vtt(vtt).unwrap();
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[0].speaker.as_deref(), Some("Jane Smith"));
        assert!((cues[0].start_s - 3.12).abs() < 1e-6);
        assert!((cues[0].end_s - 6.74).abs() < 1e-6);
        assert_eq!(cues[0].text, "Thanks everyone for joining.");
        assert_eq!(cues[1].speaker.as_deref(), Some("John Doe"));
    }

    #[test]
    fn handles_missing_voice_tag_and_entities() {
        let vtt = "WEBVTT\n\n\
            00:01:00.000 --> 00:01:02.000\n\
            Tom &amp; Jerry discussed the plan.";
        let cues = parse_vtt(vtt).unwrap();
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].speaker, None);
        assert_eq!(cues[0].text, "Tom & Jerry discussed the plan.");
        assert!((cues[0].start_s - 60.0).abs() < 1e-6);
    }

    #[test]
    fn handles_mm_ss_and_multiline() {
        let vtt = "WEBVTT\n\n\
            01:05.500 --> 01:08.000\n\
            <v Ann>First line\nsecond line</v>";
        let cues = parse_vtt(vtt).unwrap();
        assert_eq!(cues[0].text, "First line second line");
        assert!((cues[0].start_s - 65.5).abs() < 1e-6);
    }

    #[test]
    fn empty_input_errors() {
        assert!(parse_vtt("WEBVTT\n\n").is_err());
    }
}
