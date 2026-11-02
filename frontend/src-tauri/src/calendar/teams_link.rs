//! Finds Teams join links in event text. ICS bodies arrive with `&amp;`,
//! HTML tags, and iCalendar escaping, so the matcher works on a cleaned copy
//! and stops at anything that cannot be part of a URL.

const PREFIXES: [&str; 3] = [
    "https://teams.microsoft.com/l/meetup-join/",
    "https://teams.microsoft.com/meet/",
    "https://teams.live.com/meet/",
];

pub fn extract_teams_join_url(text: &str) -> Option<String> {
    let cleaned = text.replace("&amp;", "&");
    let lower = cleaned.to_ascii_lowercase();

    let mut best: Option<(usize, &str)> = None;
    for prefix in PREFIXES {
        if let Some(index) = lower.find(prefix) {
            if best.map_or(true, |(current, _)| index < current) {
                best = Some((index, prefix));
            }
        }
    }
    let (start, matched_prefix) = best?;

    let rest = &cleaned[start..];
    let end = rest
        .find(|c: char| {
            c.is_whitespace() || matches!(c, '<' | '>' | '"' | '\'' | ')' | ']' | '\\' | '\n')
        })
        .unwrap_or(rest.len());
    let url = rest[..end].trim_end_matches(['.', ',', ';']);

    if url.len() <= matched_prefix.len() {
        return None;
    }
    Some(url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_meetup_join_link_in_html_body() {
        let body = r#"<a href="https://teams.microsoft.com/l/meetup-join/19%3ameeting_abc%40thread.v2/0?context=%7b%22Tid%22%3a%22t%22%7d">Join</a>"#;
        assert_eq!(
            extract_teams_join_url(body).as_deref(),
            Some("https://teams.microsoft.com/l/meetup-join/19%3ameeting_abc%40thread.v2/0?context=%7b%22Tid%22%3a%22t%22%7d")
        );
    }

    #[test]
    fn decodes_html_ampersands_and_stops_at_whitespace() {
        let body = "Join: https://teams.microsoft.com/meet/123456789?p=abc&amp;x=1 Meeting ID: 123";
        assert_eq!(
            extract_teams_join_url(body).as_deref(),
            Some("https://teams.microsoft.com/meet/123456789?p=abc&x=1")
        );
    }

    #[test]
    fn strips_trailing_punctuation() {
        assert_eq!(
            extract_teams_join_url("see https://teams.live.com/meet/9876543210?p=x.").as_deref(),
            Some("https://teams.live.com/meet/9876543210?p=x")
        );
    }

    #[test]
    fn ignores_text_without_a_link() {
        assert_eq!(extract_teams_join_url("Weekly sync in room 4"), None);
        assert_eq!(extract_teams_join_url("https://teams.microsoft.com/meet/"), None);
    }

    #[test]
    fn picks_the_earliest_link() {
        let body = "https://teams.live.com/meet/1 then https://teams.microsoft.com/l/meetup-join/2";
        assert_eq!(
            extract_teams_join_url(body).as_deref(),
            Some("https://teams.live.com/meet/1")
        );
    }
}
