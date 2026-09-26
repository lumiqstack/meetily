//! Choose a readable filename without replacing an unrelated note.

pub fn available_filename(candidate: &str, available: impl Fn(&str) -> bool) -> String {
    if available(candidate) {
        return candidate.to_owned();
    }
    let stem = if candidate.to_ascii_lowercase().ends_with(".md") {
        &candidate[..candidate.len() - 3]
    } else {
        candidate
    };
    for number in 2u64.. {
        let next = format!("{stem} ({number}).md");
        if available(&next) {
            return next;
        }
    }
    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unused_name_needs_no_suffix() {
        assert_eq!(
            available_filename("2026-09-26 Weekly.md", |_| true),
            "2026-09-26 Weekly.md"
        );
    }

    #[test]
    fn existing_notes_get_readable_number_instead_of_random_id() {
        let occupied = ["Weekly.md", "Weekly (2).md"];
        assert_eq!(
            available_filename("Weekly.md", |name| !occupied.contains(&name)),
            "Weekly (3).md"
        );
    }

    #[test]
    fn owned_numbered_name_is_reused() {
        assert_eq!(
            available_filename("Weekly.MD", |name| name == "Weekly (2).md"),
            "Weekly (2).md"
        );
    }
}
