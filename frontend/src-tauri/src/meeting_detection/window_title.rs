//! Teams meeting window title capture.
//!
//! New Teams opens each call in its own top-level window titled
//! `<meeting subject> | Microsoft Teams`. The main window uses the same suffix
//! with a navigation section in front ("Chat | Microsoft Teams") or no prefix at
//! all. Enumerating the windows is Windows only; deciding which title is a
//! meeting is pure and tested everywhere.

const TEAMS_APP_NAME: &str = "Microsoft Teams";

/// Navigation sections of the main Teams window. A title whose prefix is one of
/// these is the app shell, not a meeting.
const MAIN_WINDOW_SECTIONS: &[&str] = &[
    "Activity", "Chat", "Teams", "Calendar", "Calls", "Files", "OneDrive", "Apps",
];

/// Returns the meeting subject when `title` is a Teams meeting window title.
pub fn meeting_subject_from_title(title: &str) -> Option<String> {
    let title = title.trim();
    let app_name_at = title.len().checked_sub(TEAMS_APP_NAME.len())?;
    if !title.is_char_boundary(app_name_at)
        || !title[app_name_at..].eq_ignore_ascii_case(TEAMS_APP_NAME)
    {
        return None;
    }

    let subject = title[..app_name_at].trim_end().strip_suffix('|')?.trim();
    if subject.is_empty()
        || MAIN_WINDOW_SECTIONS
            .iter()
            .any(|section| section.eq_ignore_ascii_case(subject))
    {
        return None;
    }

    Some(subject.to_string())
}

/// The first meeting subject among `titles`, in enumeration order.
pub fn first_meeting_subject<I, S>(titles: I) -> Option<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    titles
        .into_iter()
        .find_map(|title| meeting_subject_from_title(title.as_ref()))
}

/// The subject of the Teams meeting window owned by one of `process_ids`, if a
/// visible one exists. Always `None` off Windows.
pub fn capture_meeting_window_title(process_ids: &[u32]) -> Option<String> {
    if process_ids.is_empty() {
        return None;
    }
    first_meeting_subject(platform::visible_window_titles(process_ids))
}

#[cfg(windows)]
mod platform {
    use windows::{
        core::BOOL,
        Win32::{
            Foundation::{HWND, LPARAM},
            UI::WindowsAndMessaging::{
                EnumWindows, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId,
                IsWindowVisible,
            },
        },
    };

    struct Enumeration<'a> {
        process_ids: &'a [u32],
        titles: Vec<String>,
    }

    /// Titles of the visible top-level windows owned by `process_ids`, in
    /// Z order (foreground first).
    pub fn visible_window_titles(process_ids: &[u32]) -> Vec<String> {
        let mut enumeration = Enumeration {
            process_ids,
            titles: Vec::new(),
        };
        let lparam = LPARAM(&mut enumeration as *mut Enumeration as isize);
        // The callback always returns TRUE, so an Err here means EnumWindows
        // itself failed; whatever was collected before that is still usable.
        if let Err(error) = unsafe { EnumWindows(Some(collect_title), lparam) } {
            log::warn!("EnumWindows failed while looking for a Teams meeting window: {error}");
        }
        enumeration.titles
    }

    unsafe extern "system" fn collect_title(hwnd: HWND, lparam: LPARAM) -> BOOL {
        // SAFETY: lparam is the pointer passed to EnumWindows above, which
        // outlives the synchronous enumeration.
        let enumeration = unsafe { &mut *(lparam.0 as *mut Enumeration) };

        if !unsafe { IsWindowVisible(hwnd) }.as_bool() {
            return BOOL(1);
        }

        let mut process_id = 0u32;
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process_id)) };
        if !enumeration.process_ids.contains(&process_id) {
            return BOOL(1);
        }

        let length = unsafe { GetWindowTextLengthW(hwnd) };
        if length <= 0 {
            return BOOL(1);
        }
        let mut buffer = vec![0u16; length as usize + 1];
        let copied = unsafe { GetWindowTextW(hwnd, &mut buffer) };
        if copied > 0 {
            enumeration
                .titles
                .push(String::from_utf16_lossy(&buffer[..copied as usize]));
        }

        BOOL(1)
    }
}

#[cfg(not(windows))]
mod platform {
    pub fn visible_window_titles(_process_ids: &[u32]) -> Vec<String> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_the_teams_suffix_from_a_meeting_title() {
        assert_eq!(
            meeting_subject_from_title("Weekly Sync | Microsoft Teams"),
            Some("Weekly Sync".to_string())
        );
    }

    #[test]
    fn keeps_separators_inside_the_subject() {
        assert_eq!(
            meeting_subject_from_title("Q3 Review | Finance | Microsoft Teams"),
            Some("Q3 Review | Finance".to_string())
        );
    }

    #[test]
    fn ignores_the_main_window_and_its_sections() {
        for title in [
            "Microsoft Teams",
            "| Microsoft Teams",
            "Chat | Microsoft Teams",
            "Calendar | Microsoft Teams",
            "Activity | Microsoft Teams",
            "Teams | Microsoft Teams",
            "calls | microsoft teams",
        ] {
            assert_eq!(meeting_subject_from_title(title), None, "{title}");
        }
    }

    #[test]
    fn ignores_windows_without_the_suffix() {
        assert_eq!(meeting_subject_from_title("Weekly Sync"), None);
        assert_eq!(
            meeting_subject_from_title("Microsoft Teams | Weekly Sync"),
            None
        );
        assert_eq!(meeting_subject_from_title(""), None);
        assert_eq!(meeting_subject_from_title("Teams"), None);
    }

    #[test]
    fn tolerates_surrounding_whitespace_and_non_ascii_subjects() {
        assert_eq!(
            meeting_subject_from_title("  Réunion d'équipe |  Microsoft Teams "),
            Some("Réunion d'équipe".to_string())
        );
    }

    #[test]
    fn picks_the_first_meeting_window_in_enumeration_order() {
        let titles = [
            "Chat | Microsoft Teams",
            "Notification",
            "Weekly Sync | Microsoft Teams",
            "Standup | Microsoft Teams",
        ];
        assert_eq!(
            first_meeting_subject(titles),
            Some("Weekly Sync".to_string())
        );
        assert_eq!(first_meeting_subject(["Chat | Microsoft Teams"]), None);
    }

    #[test]
    fn capture_is_none_without_teams_processes() {
        assert_eq!(capture_meeting_window_title(&[]), None);
    }
}
