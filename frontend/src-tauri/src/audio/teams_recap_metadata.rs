//! Teams' filename timestamp is local time with no supplied timezone.
//! Preserve it as such; never manufacture a UTC offset.

#[derive(Debug, PartialEq)]
pub struct RecordingMetadata {
    pub title: String,
    pub recorded_at: String,
}

pub fn valid_recorded_at(value: &str) -> bool {
    if value.len() != 19 || !value.is_ascii() {
        return false;
    }
    let bytes = value.as_bytes();
    if bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return false;
    }
    let parts: Option<Vec<u32>> = [0..4, 5..7, 8..10, 11..13, 14..16, 17..19]
        .iter()
        .map(|r| {
            let part = &value[r.clone()];
            if part.bytes().all(|b| b.is_ascii_digit()) {
                part.parse().ok()
            } else {
                None
            }
        })
        .collect();
    let Some(p) = parts else { return false };
    let leap = p[0] % 4 == 0 && (p[0] % 100 != 0 || p[0] % 400 == 0);
    let days = match p[1] {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        _ => 0,
    };
    p[0] > 0 && p[2] > 0 && p[2] <= days && p[3] < 24 && p[4] < 60 && p[5] < 60
}

pub fn from_filename(filename: &str) -> Option<RecordingMetadata> {
    let decoded_spaces = filename.replace("%20", " ");
    let mut name = decoded_spaces.as_str();
    for extension in [".mp4", ".vtt"] {
        if name.to_ascii_lowercase().ends_with(extension) {
            name = &name[..name.len() - extension.len()];
            break;
        }
    }
    let lower = name.to_ascii_lowercase();
    let suffix = ["-meeting recording", "-meeting transcript"]
        .into_iter()
        .find(|suffix| lower.ends_with(suffix))?;
    let (title, stamp) = name[..name.len() - suffix.len()].rsplit_once('-')?;
    if stamp.len() != 15 || !stamp.is_ascii() || stamp.as_bytes()[8] != b'_' {
        return None;
    }
    let recorded_at = format!(
        "{}-{}-{}T{}:{}:{}",
        &stamp[..4],
        &stamp[4..6],
        &stamp[6..8],
        &stamp[9..11],
        &stamp[11..13],
        &stamp[13..]
    );
    if title.trim().is_empty() || !valid_recorded_at(&recorded_at) {
        return None;
    }
    Some(RecordingMetadata {
        title: title.trim().to_owned(),
        recorded_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn extracts_recording_and_transcript_local_timestamps() {
        for kind in ["Recording", "Transcript"] {
            let parsed = from_filename(&format!(
                "Monthly Review-20260924_140734-Meeting%20{kind}.mp4"
            ))
            .unwrap();
            assert_eq!(parsed.title, "Monthly Review");
            assert_eq!(parsed.recorded_at, "2026-09-24T14:07:34");
        }
    }
    #[test]
    fn validates_real_dates_without_inventing_missing_metadata() {
        assert!(valid_recorded_at("2024-02-29T23:59:59"));
        assert!(!valid_recorded_at("2026-02-29T14:00:00"));
        assert!(!valid_recorded_at("2026-09-24T25:00:00"));
        assert!(from_filename("Ordinary title.mp4").is_none());
        assert!(from_filename("Review-20260230_140734-Meeting Recording.mp4").is_none());
    }
}
