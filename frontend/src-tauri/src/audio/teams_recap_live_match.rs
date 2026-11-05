//! Whether a Teams recap or recording belongs to a meeting Meetily recorded
//! live. A live recording has no file URL for the identity key to find, so the
//! recap or file is compared with the calendar event the recording matched. The
//! rules and the reasons for them are in docs/TEAMS_RECAP_SYNC_SPEC.md, under
//! "Matching a recap to a live recording" and "Matching a recording to a live
//! recording". Everything here is pure; the caller reads the windows.

use chrono::{DateTime, Duration, Local, NaiveDateTime, TimeZone, Utc};
use url::Url;

use super::teams_recap_list::UNTITLED_ROW_TITLE;

/// How far outside its scheduled slot a recap's time may fall.
pub const TOLERANCE_MINUTES: i64 = 15;
/// A shorter title is too generic to count as the start of a longer one.
const MIN_TITLE_PREFIX_CHARS: usize = 8;
/// Teams cuts a title in a file name at about 160 characters. A recording has
/// no thread id to veto a wrong meeting, so only a title this long may be the
/// start of the subject.
const MIN_TRUNCATED_FILE_TITLE_CHARS: usize = 100;

/// A live recording's calendar event, as stored on its meeting row.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveWindow {
    pub meeting_id: String,
    /// The event's subject, or the meeting's own title for a meeting saved
    /// before the subject was stored.
    pub title: String,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub join_url: Option<String>,
}

/// What a recap says about the meeting it belongs to.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RecapFacts {
    /// The times the recap carries, most reliable first. A later one is tried
    /// only when the earlier ones fit no live recording.
    pub times: Vec<DateTime<Utc>>,
    /// The title it was listed or pasted with, and the one in its file name.
    pub titles: Vec<String>,
    pub thread_id: Option<String>,
}

/// What a recording found on SharePoint says about the meeting it belongs to.
#[derive(Debug, PartialEq)]
pub struct RecordingFacts {
    /// The times it carries, most reliable first, as in [`RecapFacts`].
    pub times: Vec<DateTime<Utc>>,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum LiveMatch {
    /// No live recording fits, so the recap or file stands alone.
    None,
    Attach(String),
    /// Live recordings fit, but nothing settles which of them is this meeting
    /// or whether any is. The recap or file stands alone.
    Ambiguous(Vec<String>),
}

impl RecapFacts {
    /// `recorded_at` is the stamp from the recording's file name; `listed_start`
    /// is the RFC 3339 start time the Meet app row showed.
    pub fn from_link(
        link: &str,
        title: &str,
        recorded_at: Option<&str>,
        listed_start: Option<&str>,
    ) -> Self {
        let file_title = Url::parse(link)
            .ok()
            .and_then(|url| super::teams_recap::recording_metadata(&url))
            .map(|metadata| metadata.title);
        let mut titles = Vec::new();
        if !title.trim().is_empty() && title.trim() != UNTITLED_ROW_TITLE {
            titles.push(title.trim().to_owned());
        }
        titles.extend(file_title);

        // Teams shows the row's time in this user's zone. The file stamp's zone
        // is an assumption, so it comes second.
        let mut times = Vec::new();
        times.extend(
            listed_start
                .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
                .map(|value| value.with_timezone(&Utc)),
        );
        times.extend(recorded_at.and_then(|stamp| stamp_to_instant(stamp, &Local)));
        Self {
            times,
            titles,
            thread_id: thread_id_from_url(link),
        }
    }
}

impl RecordingFacts {
    /// `None` when the file name is not a Teams recording's, as with any other
    /// video tenant-wide search finds. `created` is SharePoint's creation time.
    pub fn from_file(file_url: &str, created: &str) -> Option<Self> {
        let url = Url::parse(file_url.trim()).ok()?;
        let file_name = crate::audio::sharepoint_sync::percent_decode_component(
            url.path().rsplit('/').next()?,
        );
        let name = super::teams_recap::metadata::read_stamped_name(&file_name)?;

        // SharePoint's time is an instant. An unmarked stamp comes last because
        // it is in the zone of whoever recorded it, often not this machine's.
        let mut times = Vec::new();
        if name.is_utc {
            times.extend(stamp_to_instant(&name.stamp, &Utc));
        }
        times.extend(
            DateTime::parse_from_rfc3339(created.trim())
                .ok()
                .map(|value| value.with_timezone(&Utc)),
        );
        if !name.is_utc {
            times.extend(stamp_to_instant(&name.stamp, &Local));
        }
        Some(Self { times, title: name.title })
    }
}

/// The instant of a Teams file name stamp (`2026-07-21T10:02:21`), read in
/// `zone`. A clock time skipped by a daylight saving change has no instant.
fn stamp_to_instant<Tz: TimeZone>(stamp: &str, zone: &Tz) -> Option<DateTime<Utc>> {
    let naive = NaiveDateTime::parse_from_str(stamp.trim(), "%Y-%m-%dT%H:%M:%S").ok()?;
    zone.from_local_datetime(&naive)
        .earliest()
        .map(|instant| instant.with_timezone(&Utc))
}

/// The Teams thread in a recap link's `threadId` or a `meetup-join` link's
/// path, lowercased for comparison. `/meet/<code>` links carry none.
pub fn thread_id_from_url(link: &str) -> Option<String> {
    let url = Url::parse(link.trim()).ok()?;
    let raw = url
        .query_pairs()
        .find(|(key, _)| key.eq_ignore_ascii_case("threadId"))
        .map(|(_, value)| value.into_owned())
        .or_else(|| {
            let mut segments = url.path_segments()?;
            segments.find(|segment| *segment == "meetup-join")?;
            segments
                .next()
                .map(crate::audio::sharepoint_sync::percent_decode_component)
        })?;
    let thread = raw.trim().to_lowercase();
    thread.starts_with("19:").then_some(thread)
}

/// Lowercase letters and digits only, because Teams drops punctuation from the
/// recording's file name.
fn title_key(title: &str) -> String {
    title
        .chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// What confirms a window that fits in time, weakest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Evidence {
    Unconfirmed,
    /// One title starts the other: long titles get truncated in file names, and
    /// people add to a meeting's title after the call.
    TitlePrefix,
    ExactTitle,
    SameThread,
}

fn title_agreement(a: &str, b: &str) -> Evidence {
    let (a, b) = (title_key(a), title_key(b));
    if a.is_empty() || b.is_empty() {
        return Evidence::Unconfirmed;
    }
    if a == b {
        return Evidence::ExactTitle;
    }
    let (short, long) = if a.chars().count() < b.chars().count() {
        (&a, &b)
    } else {
        (&b, &a)
    };
    if short.chars().count() >= MIN_TITLE_PREFIX_CHARS && long.starts_with(short.as_str()) {
        Evidence::TitlePrefix
    } else {
        Evidence::Unconfirmed
    }
}

/// The stricter comparison for a recording, which has nothing to veto a meeting
/// that merely starts with the same words.
fn file_title_agreement(file_title: &str, subject: &str) -> Evidence {
    let (file, subject) = (title_key(file_title), title_key(subject));
    if file.is_empty() || subject.is_empty() {
        Evidence::Unconfirmed
    } else if file == subject {
        Evidence::ExactTitle
    } else if file.chars().count() >= MIN_TRUNCATED_FILE_TITLE_CHARS && subject.starts_with(file.as_str()) {
        Evidence::TitlePrefix
    } else {
        Evidence::Unconfirmed
    }
}

/// Seconds outside the scheduled slot, when that is within the tolerance.
fn window_fit(time: DateTime<Utc>, window: &LiveWindow) -> Option<i64> {
    let distance = if time < window.start {
        (window.start - time).num_seconds()
    } else if time > window.end {
        (time - window.end).num_seconds()
    } else {
        0
    };
    (distance <= Duration::minutes(TOLERANCE_MINUTES).num_seconds()).then_some(distance)
}

/// The closest confirmed candidate, or every candidate tied for closest.
fn closest(candidates: Vec<(i64, &str)>) -> LiveMatch {
    let Some(best) = candidates.iter().map(|(distance, _)| *distance).min() else {
        return LiveMatch::None;
    };
    let mut tied: Vec<String> = candidates
        .iter()
        .filter(|(distance, _)| *distance == best)
        .map(|(_, id)| (*id).to_owned())
        .collect();
    if tied.len() == 1 {
        LiveMatch::Attach(tied.remove(0))
    } else {
        LiveMatch::Ambiguous(tied)
    }
}

/// Apply the rule: a recap's time must fit the window, and a thread id or
/// title must confirm the meeting. Time alone never attaches, because
/// double-booked meetings overlap by definition.
pub fn match_live_meeting(recap: &RecapFacts, windows: &[LiveWindow]) -> LiveMatch {
    decide(&recap.times, windows, |window| {
        let window_thread = window.join_url.as_deref().and_then(thread_id_from_url);
        match (recap.thread_id.as_deref(), window_thread.as_deref()) {
            (Some(ours), Some(theirs)) if ours == theirs => Some(Evidence::SameThread),
            // Both know their thread and they differ: another meeting.
            (Some(_), Some(_)) => None,
            _ => Some(
                recap
                    .titles
                    .iter()
                    .map(|title| title_agreement(title, &window.title))
                    .max()
                    .unwrap_or(Evidence::Unconfirmed),
            ),
        }
    })
}

/// The same rule for a recording, which can only offer its file name's title.
pub fn match_live_recording(recording: &RecordingFacts, windows: &[LiveWindow]) -> LiveMatch {
    decide(&recording.times, windows, |window| {
        Some(file_title_agreement(&recording.title, &window.title))
    })
}

/// The first time that fits any window decides. `evidence` returns `None` for a
/// window that is certainly another meeting.
fn decide(
    times: &[DateTime<Utc>],
    windows: &[LiveWindow],
    evidence: impl Fn(&LiveWindow) -> Option<Evidence>,
) -> LiveMatch {
    times
        .iter()
        .map(|time| decide_at_time(*time, windows, &evidence))
        .find(|outcome| *outcome != LiveMatch::None)
        .unwrap_or(LiveMatch::None)
}

fn decide_at_time(
    time: DateTime<Utc>,
    windows: &[LiveWindow],
    evidence: &impl Fn(&LiveWindow) -> Option<Evidence>,
) -> LiveMatch {
    let fitting: Vec<(Evidence, i64, &str)> = windows
        .iter()
        .filter_map(|window| {
            let distance = window_fit(time, window)?;
            Some((evidence(window)?, distance, window.meeting_id.as_str()))
        })
        .collect();
    let Some(strongest) = fitting.iter().map(|(evidence, ..)| *evidence).max() else {
        return LiveMatch::None;
    };
    // Strongest evidence first. A closer slot never outranks a better match:
    // "Weekly sync" must not take the recap of "Weekly sync with vendors".
    let candidates: Vec<(i64, &str)> = fitting
        .iter()
        .filter(|(evidence, ..)| *evidence == strongest)
        .map(|(_, distance, id)| (*distance, *id))
        .collect();
    if strongest == Evidence::Unconfirmed {
        LiveMatch::Ambiguous(candidates.iter().map(|(_, id)| (*id).to_owned()).collect())
    } else {
        closest(candidates)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;

    const JOIN_A: &str = "https://teams.microsoft.com/l/meetup-join/19%3ameeting_AAA%40thread.v2/0?context=%7b%22Tid%22%3a%22t%22%7d";
    const JOIN_B: &str = "https://teams.microsoft.com/l/meetup-join/19%3ameeting_BBB%40thread.v2/0";
    const THREAD_A: &str = "19:meeting_aaa@thread.v2";

    fn at(h: u32, m: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 3, h, m, 0).unwrap()
    }

    fn window(id: &str, title: &str, start: (u32, u32), end: (u32, u32), join: Option<&str>) -> LiveWindow {
        LiveWindow {
            meeting_id: id.to_owned(),
            title: title.to_owned(),
            start: at(start.0, start.1),
            end: at(end.0, end.1),
            join_url: join.map(str::to_owned),
        }
    }

    fn recap(time: DateTime<Utc>, title: &str, thread: Option<&str>) -> RecapFacts {
        RecapFacts {
            times: vec![time],
            titles: vec![title.to_owned()],
            thread_id: thread.map(str::to_owned),
        }
    }

    fn attach(id: &str) -> LiveMatch {
        LiveMatch::Attach(id.to_owned())
    }

    #[test]
    fn same_thread_inside_the_slot_attaches() {
        let windows = [window("m1", "Weekly sync", (10, 0), (10, 30), Some(JOIN_A))];
        assert_eq!(
            match_live_meeting(&recap(at(10, 2), "Anything", Some(THREAD_A)), &windows),
            attach("m1")
        );
    }

    #[test]
    fn the_tolerance_is_fifteen_minutes_on_each_side() {
        let windows = [window("m1", "Weekly sync", (10, 0), (10, 30), Some(JOIN_A))];
        let fits = |time| match_live_meeting(&recap(time, "x", Some(THREAD_A)), &windows);
        assert_eq!(fits(at(9, 45)), attach("m1"));
        assert_eq!(fits(at(10, 45)), attach("m1"));
        assert_eq!(fits(at(9, 44)), LiveMatch::None);
        assert_eq!(fits(at(10, 46)), LiveMatch::None);
    }

    #[test]
    fn a_different_thread_is_another_meeting_whatever_the_clock_and_title_say() {
        let windows = [window("m1", "Weekly sync", (10, 0), (10, 30), Some(JOIN_B))];
        assert_eq!(
            match_live_meeting(&recap(at(10, 2), "Weekly sync", Some(THREAD_A)), &windows),
            LiveMatch::None
        );
    }

    #[test]
    fn a_recurring_series_attaches_only_to_its_own_occurrence() {
        let monday = window("mon", "Standup", (9, 0), (9, 15), Some(JOIN_A));
        let mut tuesday = window("tue", "Standup", (9, 0), (9, 15), Some(JOIN_A));
        tuesday.start += Duration::days(1);
        tuesday.end += Duration::days(1);
        let windows = [monday, tuesday];
        assert_eq!(
            match_live_meeting(&recap(at(9, 1), "Standup", Some(THREAD_A)), &windows),
            attach("mon")
        );
        assert_eq!(
            match_live_meeting(
                &recap(at(9, 1) + Duration::days(1), "Standup", Some(THREAD_A)),
                &windows
            ),
            attach("tue")
        );
        assert_eq!(
            match_live_meeting(
                &recap(at(9, 1) + Duration::days(2), "Standup", Some(THREAD_A)),
                &windows
            ),
            LiveMatch::None
        );
    }

    #[test]
    fn without_a_thread_on_either_side_the_title_confirms() {
        let meet_link = Some("https://teams.microsoft.com/meet/123456789?p=abc");
        let windows = [window("m1", "Q4 Review: Planning", (10, 0), (11, 0), meet_link)];
        // Teams dropped the punctuation from the file name.
        assert_eq!(
            match_live_meeting(&recap(at(10, 5), "Q4 Review Planning", Some(THREAD_A)), &windows),
            attach("m1")
        );
        let no_link = [window("m2", "Q4 Review: Planning", (10, 0), (11, 0), None)];
        assert_eq!(
            match_live_meeting(&recap(at(10, 5), "q4 review_ planning", Some(THREAD_A)), &no_link),
            attach("m2")
        );
        // Teams truncated it, or the user added to it.
        assert_eq!(
            match_live_meeting(&recap(at(10, 5), "Q4 Review", None), &no_link),
            attach("m2")
        );
    }

    #[test]
    fn a_short_title_does_not_confirm_as_a_prefix() {
        let windows = [window("m1", "Sync with the risk team", (10, 0), (11, 0), None)];
        assert_eq!(
            match_live_meeting(&recap(at(10, 5), "Sync", None), &windows),
            LiveMatch::Ambiguous(vec!["m1".into()])
        );
    }

    #[test]
    fn a_fitting_window_with_no_evidence_stays_ambiguous() {
        // The recap may belong to a meeting double-booked over this one.
        let windows = [window("m1", "Budget review", (10, 0), (11, 0), None)];
        assert_eq!(
            match_live_meeting(&recap(at(10, 31), "Vendor call", Some(THREAD_A)), &windows),
            LiveMatch::Ambiguous(vec!["m1".into()])
        );
    }

    #[test]
    fn a_double_booked_slot_goes_to_the_meeting_the_evidence_names() {
        let windows = [
            window("budget", "Budget review", (10, 0), (11, 0), None),
            window("vendor", "Vendor call", (10, 30), (11, 0), None),
            window("other-thread", "Vendor call", (10, 30), (11, 0), Some(JOIN_B)),
        ];
        assert_eq!(
            match_live_meeting(&recap(at(10, 31), "Vendor call", Some(THREAD_A)), &windows),
            attach("vendor")
        );
    }

    #[test]
    fn a_recording_restarted_in_one_call_stays_ambiguous() {
        let windows = [
            window("part1", "Weekly sync", (10, 0), (10, 30), Some(JOIN_A)),
            window("part2", "Weekly sync", (10, 0), (10, 30), Some(JOIN_A)),
        ];
        assert_eq!(
            match_live_meeting(&recap(at(10, 5), "Weekly sync", Some(THREAD_A)), &windows),
            LiveMatch::Ambiguous(vec!["part1".into(), "part2".into()])
        );
    }

    #[test]
    fn back_to_back_meetings_with_one_title_go_to_the_closer_slot() {
        let windows = [
            window("first", "Sync", (10, 0), (10, 30), None),
            window("second", "Sync", (10, 30), (11, 0), None),
        ];
        assert_eq!(match_live_meeting(&recap(at(10, 32), "Sync", None), &windows), attach("second"));
        assert_eq!(match_live_meeting(&recap(at(10, 20), "Sync", None), &windows), attach("first"));
    }

    #[test]
    fn an_exact_title_beats_a_closer_prefix() {
        // People join a minute early, so the recording of the second meeting
        // starts inside the first one's slot.
        let windows = [
            window("first", "Weekly sync", (10, 0), (10, 30), None),
            window("second", "Weekly sync with vendors", (10, 30), (11, 0), None),
        ];
        assert_eq!(
            match_live_meeting(&recap(at(10, 29), "Weekly sync with vendors", None), &windows),
            attach("second")
        );
        assert_eq!(
            match_live_meeting(&recap(at(10, 31), "Weekly sync", None), &windows),
            attach("first")
        );
    }

    #[test]
    fn a_thread_match_beats_a_closer_title_match() {
        let windows = [
            window("by-title", "Weekly sync", (10, 0), (10, 30), None),
            window("by-thread", "Renamed by hand", (10, 25), (11, 0), Some(JOIN_A)),
        ];
        assert_eq!(
            match_live_meeting(&recap(at(10, 10), "Weekly sync", Some(THREAD_A)), &windows),
            attach("by-thread")
        );
    }

    #[test]
    fn a_later_time_is_tried_only_when_the_earlier_fits_nothing() {
        let windows = [
            window("morning", "Weekly sync", (10, 0), (10, 30), Some(JOIN_A)),
            window("afternoon", "Weekly sync", (17, 0), (17, 30), Some(JOIN_A)),
        ];
        let facts = |times| RecapFacts {
            times,
            titles: vec!["Weekly sync".into()],
            thread_id: Some(THREAD_A.into()),
        };
        // The row says morning; a stamp in another zone must not win with its
        // exact fit on the afternoon occurrence.
        assert_eq!(match_live_meeting(&facts(vec![at(10, 20), at(17, 2)]), &windows), attach("morning"));
        // The row showed only a date, which parses to midnight: the stamp decides.
        assert_eq!(match_live_meeting(&facts(vec![at(0, 0), at(17, 2)]), &windows), attach("afternoon"));
    }

    #[test]
    fn a_recap_with_no_time_never_matches() {
        let windows = [window("m1", "Weekly sync", (10, 0), (10, 30), Some(JOIN_A))];
        let facts = RecapFacts {
            times: vec![],
            titles: vec!["Weekly sync".into()],
            thread_id: Some(THREAD_A.into()),
        };
        assert_eq!(match_live_meeting(&facts, &windows), LiveMatch::None);
    }

    #[test]
    fn thread_ids_come_from_recap_links_and_join_links_only() {
        assert_eq!(thread_id_from_url(JOIN_A).as_deref(), Some(THREAD_A));
        assert_eq!(
            thread_id_from_url("https://teams.microsoft.com/l/meetingrecap?threadId=19%3Ameeting_AAA%40thread.v2&fileUrl=x")
                .as_deref(),
            Some(THREAD_A)
        );
        assert_eq!(
            thread_id_from_url("https://teams.microsoft.com/l/meetup-join/19:meeting_AAA@thread.v2/0").as_deref(),
            Some(THREAD_A)
        );
        assert_eq!(thread_id_from_url("https://teams.microsoft.com/meet/123456789?p=abc"), None);
        assert_eq!(thread_id_from_url("https://teams.live.com/meet/123456789"), None);
        assert_eq!(thread_id_from_url("https://teams.microsoft.com/l/meetingrecap?threadId=test"), None);
        assert_eq!(thread_id_from_url("not a url"), None);
    }

    #[test]
    fn a_file_stamp_reads_in_the_given_zone() {
        let cst = FixedOffset::west_opt(6 * 3600).unwrap();
        assert_eq!(
            stamp_to_instant("2026-11-03T04:02:00", &cst),
            Some(Utc.with_ymd_and_hms(2026, 11, 3, 10, 2, 0).unwrap())
        );
        assert_eq!(stamp_to_instant("2026-11-03", &cst), None);
        assert_eq!(stamp_to_instant("not a stamp", &cst), None);
    }

    #[test]
    fn facts_come_from_the_link_the_row_and_the_file_name() {
        let link = "https://teams.microsoft.com/l/meetingrecap?threadId=19%3Ameeting_AAA%40thread.v2&fileUrl=https%3A%2F%2Fc-my.sharepoint.com%2Fpersonal%2Fj%2FDocuments%2FRecordings%2FWeekly%20sync-20261103_100200-Meeting%20Recording.mp4";
        let facts = RecapFacts::from_link(
            link,
            "Weekly sync (renamed)",
            Some("2026-11-03T10:02:00"),
            Some("2026-11-03T10:00:00.000Z"),
        );
        assert_eq!(facts.thread_id.as_deref(), Some(THREAD_A));
        assert_eq!(facts.titles, vec!["Weekly sync (renamed)", "Weekly sync"]);
        assert_eq!(facts.times.len(), 2);
        assert_eq!(facts.times[0], at(10, 0), "the row's time comes first");

        let bare = RecapFacts::from_link(
            "https://teams.microsoft.com/l/meetingrecap?threadId=19%3Ax",
            UNTITLED_ROW_TITLE,
            None,
            Some("not a time"),
        );
        assert!(bare.times.is_empty() && bare.titles.is_empty());
    }

    /// The time is given directly, so the machine's zone does not matter here.
    fn recording(time: DateTime<Utc>, title: &str) -> RecordingFacts {
        RecordingFacts { times: vec![time], title: title.to_owned() }
    }

    #[test]
    fn a_recording_attaches_on_the_exact_subject() {
        let windows = [window("m1", "Q4 Review: Planning", (10, 0), (11, 0), Some(JOIN_A))];
        // Teams dropped the punctuation from the file name.
        assert_eq!(
            match_live_recording(&recording(at(10, 5), "Q4 Review Planning"), &windows),
            attach("m1")
        );
        assert_eq!(match_live_recording(&recording(at(11, 15), "Q4 Review Planning"), &windows), attach("m1"));
        assert_eq!(match_live_recording(&recording(at(11, 16), "Q4 Review Planning"), &windows), LiveMatch::None);
    }

    #[test]
    fn a_short_prefix_never_confirms_a_recording_though_it_confirms_a_recap() {
        let windows = [window("m1", "Weekly sync with vendors", (10, 0), (10, 30), None)];
        assert_eq!(
            match_live_recording(&recording(at(10, 5), "Weekly sync"), &windows),
            LiveMatch::Ambiguous(vec!["m1".into()])
        );
        assert_eq!(match_live_meeting(&recap(at(10, 5), "Weekly sync", None), &windows), attach("m1"));
    }

    #[test]
    fn a_long_file_title_may_be_the_start_of_the_subject_but_not_the_reverse() {
        let subject = format!("{} and the rest of the title", "Design session ".repeat(9));
        let cut: String = subject.chars().take(subject.len() - 12).collect();
        assert!(title_key(&cut).len() >= MIN_TRUNCATED_FILE_TITLE_CHARS);
        let windows = [window("m1", &subject, (10, 0), (11, 0), None)];
        assert_eq!(match_live_recording(&recording(at(10, 5), &cut), &windows), attach("m1"));

        // The file's title runs longer than the subject: not a truncation.
        let shorter_subject = [window("m2", &cut, (10, 0), (11, 0), None)];
        assert_eq!(
            match_live_recording(&recording(at(10, 5), &subject), &shorter_subject),
            LiveMatch::Ambiguous(vec!["m2".into()])
        );
    }

    #[test]
    fn a_recording_with_no_confirmed_window_stays_ambiguous_and_a_restart_ties() {
        let double_booked = [window("m1", "Budget review", (10, 0), (11, 0), Some(JOIN_A))];
        assert_eq!(
            match_live_recording(&recording(at(10, 5), "Vendor call"), &double_booked),
            LiveMatch::Ambiguous(vec!["m1".into()])
        );
        let restarted = [
            window("part1", "Weekly sync", (10, 0), (10, 30), None),
            window("part2", "Weekly sync", (10, 0), (10, 30), None),
        ];
        assert_eq!(
            match_live_recording(&recording(at(10, 5), "Weekly sync"), &restarted),
            LiveMatch::Ambiguous(vec!["part1".into(), "part2".into()])
        );
    }

    #[test]
    fn the_named_meeting_wins_a_double_booked_slot_for_a_recording() {
        let windows = [
            window("budget", "Budget review", (10, 0), (11, 0), None),
            window("vendor", "Vendor call", (10, 30), (11, 0), None),
        ];
        assert_eq!(match_live_recording(&recording(at(10, 31), "Vendor call"), &windows), attach("vendor"));
    }

    #[test]
    fn a_recording_tries_its_later_times_only_when_the_earlier_fit_nothing() {
        let windows = [
            window("morning", "Weekly sync", (10, 0), (10, 30), None),
            window("afternoon", "Weekly sync", (17, 0), (17, 30), None),
        ];
        let facts = |times| RecordingFacts { times, title: "Weekly sync".into() };
        // A colleague's stamp reads as the afternoon, but the instant is morning.
        assert_eq!(match_live_recording(&facts(vec![at(10, 20), at(17, 2)]), &windows), attach("morning"));
        // The file appeared long after the meeting: the stamp decides.
        assert_eq!(match_live_recording(&facts(vec![at(21, 0), at(17, 2)]), &windows), attach("afternoon"));
        assert_eq!(match_live_recording(&facts(vec![]), &windows), LiveMatch::None);
    }

    #[test]
    fn recording_facts_come_from_the_file_url_and_the_creation_time() {
        let created = "2026-11-03T10:40:00Z";
        let facts = RecordingFacts::from_file(
            "https://c-my.sharepoint.com/personal/j/Documents/Recordings/Weekly%20sync-20261103_100200-Meeting%20Recording%202.mp4",
            created,
        )
        .unwrap();
        assert_eq!(facts.title, "Weekly sync");
        assert_eq!(facts.times[0], at(10, 40), "the instant comes before the unmarked stamp");
        assert_eq!(facts.times[1], stamp_to_instant("2026-11-03T10:02:00", &Local).unwrap());

        let utc = RecordingFacts::from_file(
            "https://c-my.sharepoint.com/personal/j/Documents/Recordings/Weekly%20sync-20261103_100200UTC-Meeting%20Recording.mp4",
            created,
        )
        .unwrap();
        assert_eq!(utc.times, vec![at(10, 2), at(10, 40)], "a stamp marked UTC is an instant too");

        let no_created = RecordingFacts::from_file(
            "https://c-my.sharepoint.com/personal/j/Documents/Recordings/Weekly%20sync-20261103_100200UTC-Meeting%20Recording.mp4",
            "not a time",
        )
        .unwrap();
        assert_eq!(no_created.times, vec![at(10, 2)]);
    }

    #[test]
    fn a_video_that_is_not_a_teams_recording_has_no_facts() {
        for file in [
            "https://c.sharepoint.com/sites/s/Videos/Town%20hall.mp4",
            "https://c.sharepoint.com/sites/s/Videos/20261016_Service%20Offer%20(Recording).mp4",
            "not a url",
        ] {
            assert_eq!(RecordingFacts::from_file(file, "2026-11-03T10:40:00Z"), None, "{file}");
        }
    }
}
